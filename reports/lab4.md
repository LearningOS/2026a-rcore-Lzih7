# Lab4：文件系统与文件状态管理

## 1. 实验要求

本实验对应 rCore 第六章。在已有 easy-fs 文件系统和文件描述符管理机制的基础上，新增文件状态查询、硬链接创建和名称删除三个系统调用。实验重点不在于重新实现文件读写，而是建立目录项、inode 和打开文件对象之间的联系，使用户程序能够查询文件身份，并通过不同名称访问同一文件。

| 功能 | 系统调用 | 要求 |
| --- | --- | --- |
| 查询文件状态 | `sys_fstat(fd, st)`，编号 `80` | 根据当前进程的 fd 获取设备编号、inode 编号、文件类型和硬链接数，写回用户地址；成功返回 `0`，失败返回 `-1` |
| 创建硬链接 | `sys_linkat(old_name, new_name)`，编号 `37` | 新增一个指向已有 inode 的名称入口；旧名称不存在或新名称已被占用时返回 `-1` |
| 删除名称入口 | `sys_unlinkat(name)`，编号 `35` | 删除指定名称的目录项，不影响指向同一 inode 的其他硬链接；成功返回 `0`，名称不存在时返回 `-1` |
| 保持前序功能 | 时间查询、内存映射、进程创建等 | 迁移前章实现，保证第六章综合测例能够继续运行 |

实验提供的 `open`、`close`、`read`、`write`、磁盘块缓存和 inode 分配机制保持不变。本次新增工作集中在 `os/src/syscall/fs.rs`、内核文件抽象层和 `easy-fs/src/vfs.rs`，没有改变 `DiskInode` 的磁盘布局。

## 2. 方案设计

### 2.1 区分目录项、磁盘 inode 与打开文件对象

实现三个系统调用前，需要明确各层对象的职责。

| 对象 | 保存内容 | 作用 |
| --- | --- | --- |
| `DirEntry` | 名称和 `inode_id` | 在目录中建立“名称到 inode 编号”的映射 |
| `DiskInode` | 类型、大小、直接和间接块索引 | 描述文件或目录的元数据，并定位其数据块 |
| `Inode` | inode 所在块号、块内偏移、文件系统与设备引用 | 作为内存句柄，访问磁盘 inode 并提供目录与内容操作 |
| `OSInode` | 可读、可写标志，以及包含偏移和 `Arc<Inode>` 的内部对象 | 表示一次打开的文件，供内核通过 `File` trait 操作 |
| `fd_table` | `Option<Arc<dyn File>>` 数组 | 将当前进程的文件描述符映射到打开文件对象 |

文件名不存放在 `DiskInode` 中。目录自身也是一种文件，其数据块保存多条 `DirEntry`；普通文件的数据块保存正文。`DiskInode` 本身只保存索引，不直接容纳这些目录项或正文。

```text
根目录的 DiskInode
  -> 根目录数据块
     -> DirEntry("fname2", 7)
     -> DirEntry("linkname0", 7)
        -> DiskInode 7
           -> 文件数据块
```

这两个名称指向同一个 inode，所以看到的是同一份内容，而不是两个独立的副本。

### 2.2 硬链接只新增目录项

创建硬链接时，先在根目录检查新名称是否已经存在，再查找旧名称的 inode 编号。检查通过后，扩展目录数据并追加 `DirEntry::new(new_name, old_inode_id)`。

不能直接复用 `Inode::create()` 创建新名称，因为该方法会申请新的 inode 编号，得到独立文件，不符合硬链接语义。本次 `link` 只复用旧编号，不申请目标文件的新 inode，也不复制文件正文。

`DiskInode` 没有持久化的硬链接计数字段。为了避免修改磁盘格式，本次在查询状态时扫描根目录，统计所有指向目标 inode 的目录项。

### 2.3 unlink 删除名称，而不是立即销毁文件

`unlink` 的对象是一个名称入口。同一 inode 可以对应多个目录项，删除其中一个不能清空该 inode 的内容，否则其他名称和已打开文件都会受到影响。

本次按名称筛选目录项，保留其他记录，然后重建目录数据。目标文件的 inode 和数据块不回收，这是第六章测例允许的简化方案。它保证已有硬链接和打开文件继续可用，但删除最后一个名称后也会留下分配的文件空间。

目录数据块与目标文件数据块必须分开处理：重建时回收的是目录自身的块，不能对被删除名称指向的文件调用 `clear()`。

### 2.4 通过 File trait 查询状态

`fd_table` 中保存的是 `Arc<dyn File>`，系统调用不能直接访问 `OSInode` 的私有字段，也不能假设所有 fd 都是普通文件。因此，在 `File` trait 增加 `stat()` 方法，让实际文件对象提供自己的状态。

`OSInode` 实现该方法；标准输入、标准输出采用默认的 `None`。系统调用对这些当前不支持查询的对象返回 `-1`。

查询 fd 不需要重新按名称打开文件。即使名称已经被删除，只要 fd 仍有效，打开文件对象依然持有原 inode，可以查询原文件的编号和剩余链接数。

### 2.5 用户结构体按页安全写回

`sys_fstat` 的 `st` 是用户虚拟地址，不能直接按内核页表解引用。当前 `Stat` 采用 `repr(C)` 布局，包含两个 `u64`、一个 `u32` 模式字段、一个 `u32` 链接数字段和七个 `u64` 保留字段，在 RV64 下共 `80` 字节。

写回前校验完整地址范围和每个涉及的页表项，再按物理页分段复制。这样即使结构体跨越两个虚拟页，且两页对应的物理页不连续，也能正确写入。

## 3. 文件修改与实现

### 3.1 `easy-fs/src/vfs.rs`：新增 Inode::link

第一步读取当前目录的元数据，检查名称并获得旧 inode 编号：

```rust
let old_inode_id = self.read_disk_inode(|dir_inode| {
    assert!(dir_inode.is_dir());
    if self.find_inode_id(new_name, dir_inode).is_some() {
        return None;
    }
    self.find_inode_id(old_name, dir_inode)
})?;
```

`read_disk_inode` 根据 `self.block_id` 和 `self.block_offset` 取得缓存中的 `&DiskInode`，交给闭包访问。`find_inode_id` 再通过目录的块索引读取目录数据，逐条比较 `DirEntry` 中的名称。

闭包返回 `Option<u32>`。新名称已存在或旧名称不存在时得到 `None`，末尾的 `?` 使 `link` 提前返回；找到时解包为 `old_inode_id`。闭包中的 `return None` 只退出闭包，外层提前返回由 `?` 完成。

第二步扩展目录并写入新记录：

```rust
self.modify_disk_inode(|dir_inode| {
    let file_count = dir_inode.size as usize / DIRENT_SZ;
    let new_size = (file_count + 1) * DIRENT_SZ;
    self.increase_size(new_size as u32, dir_inode, &mut fs);
    let dirent = DirEntry::new(new_name, old_inode_id);
    dir_inode.write_at(
        file_count * DIRENT_SZ,
        dirent.as_bytes(),
        &self.block_device,
    );
});
```

一条目录项为 `32` 字节。例如目录原有三条记录，大小为 `96` 字节，新增记录从偏移 `96` 开始写，目录大小变为 `128` 字节。原有块还有空闲空间时不需要申请新块；跨过块边界或索引范围时，`increase_size` 负责申请相应的数据块和索引块。

最后通过原 inode 编号构造返回句柄，并调用 `block_cache_sync_all()` 写回修改。操作期间持有文件系统锁，保证名称检查与目录追加之间不会被其他采用同一锁的操作插入。

### 3.2 `easy-fs/src/vfs.rs`：新增 Inode::unlink

该函数返回 `bool`，名称不存在时返回 `false`，删除成功返回 `true`。实现分为四步：

1. 扫描目录数据，逐条读取 `DirEntry`。
2. 按名称排除目标记录，将其余记录保存到 `Vec<DirEntry>`。
3. 清空目录自身的块索引，回收目录块，再按剩余记录数量重新扩容和写入。
4. 同步块缓存，返回成功。

核心删除判断是：

```rust
if dirent.name() == name {
    found = true;
} else {
    entries.push(dirent);
}
```

必须比较名称，不能把所有 `inode_id` 相同的目录项一并删除。例如 `fname2` 和 `linkname0` 都指向 inode 7，删除 `fname2` 后应保留 `linkname0`。

目录重建的核心逻辑如下：

```rust
for block_id in dir_inode.clear_size(&self.block_device) {
    fs.dealloc_data(block_id);
}
let new_size = entries.len() * DIRENT_SZ;
self.increase_size(new_size as u32, dir_inode, &mut fs);
for (i, dirent) in entries.iter().enumerate() {
    dir_inode.write_at(i * DIRENT_SZ, dirent.as_bytes(), &self.block_device);
}
```

采用重建方案是因为现有接口支持扩容与完整清空，没有保持块分配一致性的部分缩容方法。只减小 `size` 会让文件逻辑大小与保留的块分配状态不一致，后续扩容还可能覆盖旧索引并遗留块。

该实现保留剩余记录的顺序，并能处理跨块缩容及删除最后一条记录的情况。代价是每次 unlink 都要读取整个目录、保存剩余项并重写目录；它适合实验规模，不是高效的大目录删除方案，也没有断电恢复机制。

### 3.3 `easy-fs/src/vfs.rs`：新增 Inode::stat

底层接口返回 `(inode 编号, 是否为目录, 硬链接数)`：

```rust
pub fn stat(&self) -> (u64, bool, u32)
```

由于内存 `Inode` 只记录磁盘位置，没有单独保存 inode 编号，本次根据 inode 区起始块与结构体大小反算编号：

```text
每块 inode 数 = BLOCK_SZ / size_of::<DiskInode>()
inode 编号 = (当前块号 - inode 区起始块号) × 每块 inode 数
           + 块内偏移 / size_of::<DiskInode>()
```

当前一块为 `512` 字节，一个 `DiskInode` 为 `128` 字节，每块容纳四个 inode。例如相对于 inode 区起点的第二块、块内偏移 `384` 对应编号 `1 × 4 + 384 / 128 = 7`。

文件类型由目标 `DiskInode::is_dir()` 取得。链接数则遍历根目录所有记录，对 `dirent.inode_id() == inode_id` 的记录累计计数。

这里统计的是名称入口数，不是 `Arc::strong_count()`。打开一个文件或在内核中克隆引用会改变 Arc 引用计数，但不会增加硬链接。删除最后一个名称后，仍打开的普通文件可以返回 `nlink = 0`。

统计过程面向本实验只有根目录名称入口的结构，不是通用多级目录的链接计数实现；根目录自身的 Unix 链接数约定也不在本方案覆盖范围内。

### 3.4 `os/src/fs/mod.rs`：扩展 File trait 和导出接口

新增默认方法：

```rust
fn stat(&self) -> Option<Stat> {
    None
}
```

该方法允许通过 `dyn File` 查询状态，同时保持既有标准输入输出实现可编译。`Stat`、`StatMode` 继续使用脚手架已有定义，没有新增另一套用户 ABI。

将 `link_file` 和 `unlink_file` 加入模块的 `pub use` 列表，使系统调用层能够从 `crate::fs` 导入。仅在私有子模块中把函数标为 `pub`，并不能让 `crate::fs::unlink_file` 自动可见，因此需要显式重新导出。

### 3.5 `os/src/fs/inode.rs`：连接根目录与打开文件对象

新增根目录包装函数：

```rust
pub fn link_file(old_name: &str, new_name: &str) -> bool {
    ROOT_INODE.link(old_name, new_name).is_some()
}

pub fn unlink_file(name: &str) -> bool {
    ROOT_INODE.unlink(name)
}
```

`ROOT_INODE` 是磁盘 inode 0 对应的根目录内存句柄，不是目录项数组本身。这两个包装函数把操作交给它，使系统调用不必直接处理块号、块内偏移和目录扩容。

在 `impl File for OSInode` 中实现 `stat()`：先取得内部 `Arc<Inode>`，释放临时内部借用，再调用底层统计，构造：

```rust
Stat {
    dev: 0,
    ino,
    mode: if is_dir { StatMode::DIR } else { StatMode::FILE },
    nlink,
    pad: [0; 7],
}
```

当前只有一个文件系统，设备编号固定为 `0`；普通文件类型为 `0o100000`，目录类型为 `0o040000`。查询过程不修改打开文件的读写偏移。

### 3.6 `os/src/syscall/fs.rs`：完成三个系统调用

`sys_linkat` 和 `sys_unlinkat` 先取得调用者页表 token，再用 `translated_str` 读取 NUL 结尾的名称。包装接口返回 `true` 时转换为 `0`，失败转换为 `-1`。

它们只在根目录中匹配名称，不实现一般路径解析。`translated_str` 当前未完整检查非法地址和字符串长度，这也是这两个系统调用仍存在的安全边界。

`sys_fstat` 的处理更复杂：

1. 检查 fd 是否越界，以及对应槽位是否为 `Some`。
2. 克隆 `Arc<dyn File>`，保存用户页表 token，释放任务控制块内部借用。
3. 检查 `st` 非空、地址加法不溢出，完整范围位于 Sv39 低半区。
4. 逐页要求 `V | U | R | W` 权限。除可写外检查 `R`，避免 RISC-V 不合法的仅写叶子映射。
5. 调用 `file.stat()`，不支持查询时返回 `-1`。
6. 编码 `Stat`，经 `translated_byte_buffer` 按页写回，成功返回 `0`。

编码时先建立全零的 `80` 字节数组，再分别写入字段：

| 字节范围 | 字段 |
| --- | --- |
| `0..8` | `dev` |
| `8..16` | `ino` |
| `16..20` | `mode` |
| `20..24` | `nlink` |
| `24..80` | 保留字段，全部为零 |

采用显式编码而不是直接把结构体转换成任意字节切片，避免读取潜在未初始化填充字节，并保证保留字段不包含内核旧数据。该布局与当前 RV64 的用户结构体保持一致。

### 3.7 三个系统调用的完整调用链

```text
用户 fstat(fd, st)
  -> ecall -> trap_handler -> syscall
  -> sys_fstat
  -> 当前进程 fd_table[fd]
  -> File::stat 动态分发 -> OSInode::stat
  -> Inode::stat
     -> 反算 inode 编号、读取文件类型
     -> 扫描根目录，统计相同 inode_id 的目录项
  -> 编码 Stat -> translated_byte_buffer -> 写回用户内存
```

```text
用户 link(old_name, new_name)
  -> ecall -> syscall -> sys_linkat
  -> translated_str -> link_file
  -> ROOT_INODE.link
  -> 查旧名称、检查新名称 -> 扩展目录
  -> 写入 DirEntry(new_name, old_inode_id) -> 同步块缓存
```

```text
用户 unlink(name)
  -> ecall -> syscall -> sys_unlinkat
  -> translated_str -> unlink_file
  -> ROOT_INODE.unlink
  -> 按名称筛选目录项 -> 回收并重建目录数据
  -> 保留目标文件 inode 和正文 -> 同步块缓存
```

分发层已经提供编号和参数提取：`linkat` 从 `args[1]`、`args[3]` 提取两个名称，`unlinkat` 从 `args[1]` 提取名称，`fstat` 使用 `args[0]` 和 `args[1]`。本实验采用简化接口，没有完整实现 Linux 的目录 fd 与 flags 语义。

## 4. 测试结果

### 4.1 新增功能的专项测试

| 测例 | 主要检查 | 已观察到的结果 |
| --- | --- | --- |
| `ch6_file1` | fstat 返回 `0`，类型为普通文件，初始 `nlink = 1` | 输出 `Test fstat OK` |
| `ch6_file2` | 创建多个硬链接、查询链接数、删除原名称后读取内容、比较 inode 身份 | 输出 `Test link OK` |
| `ch6_file3` | 十轮创建、写入、关闭和 unlink，删除后按原名称打开失败 | 输出 `Test mass open/unlink OK` |

`ch6_file2` 中链接数按以下过程变化：

```text
创建 fname2                         nlink = 1
新增 linkname0                      nlink = 2
再新增 linkname1、linkname2          nlink = 4
删除 fname2                         nlink = 3
删除 linkname1、linkname2            nlink = 1
最后删除 linkname0                   不再保留名称入口
```

删除 `fname2` 后，测例仍能通过 `linkname0` 读取原正文，并确认 `dev` 与 `ino` 不变，说明当前实现新增和删除的是名称入口，而不是复制或清空目标文件。

### 4.2 综合评测与环境问题

综合评测使用 `make -C ci-user test CHAPTER=6`。测试期间曾出现两类与新增系统调用无关的问题。

第一类是用户程序编译阶段的 `E0460`，提示 `spin` 与 `user_lib` 编译产物不一致，随后出现宏、分配器和 panic handler 缺失的连带错误。将编译产物放到容器 `/tmp`、避开 `/mnt` 共享目录后，评测能够进入运行阶段。

第二类是随机输出标记累积。实际输出曾为 `OK57482658925074!`，而本轮检查器期待 `OK5748!`。因此含有 `OK` 或 `passed` 的成功输出匹配失败，原始检查只统计为 `9/31`。

对同一份运行日志仅在输入流中去除这些数字后缀，再交给恢复后的检查器，结果为 `31/31`。这表明该轮日志满足功能输出检查，但不能把这次复核描述为原始完整命令成功：原命令仍因标记不一致返回失败，最终报告检查和完整命令退出码尚未在这次复核中验证。

后续正式评测前，应恢复用户测例与检查器中的基础标记，继续采用容器本地编译目录。评测脚本会临时覆盖 `os/Makefile` 和 `easy-fs-fuse/src/main.rs`，中断后还需检查是否正确恢复；这些模板覆盖不属于三个系统调用的实现改动。

## 5. 实验总结

本章将三个新增系统调用贯通到目录和 inode 层。`linkat` 复用 inode 编号建立新名称，`unlinkat` 只删除指定入口，`fstat` 从有效 fd 获取打开文件的身份、类型和实时链接数，并安全写回用户地址空间。

这套实现保持原有磁盘格式，沿用块缓存、目录读写和文件描述符框架。主要限制也较明确：链接数只扫描根目录，unlink 不回收目标文件资源，目录删除采用整体重建，字符串参数检查仍不完整。因此它满足当前实验规模下的文件语义与功能输出测试，但尚不是具有完整资源回收、路径解析和故障恢复能力的通用文件系统。
