# 第六章 `sys_linkat` 实现详解

## 1. 实现目标

`sys_linkat` 用于建立硬链接：为一个已经存在的文件增加新的名称，新名称和旧名称指向同一个磁盘 inode。

例如，已有文件 `hello` 的 inode 编号为 7，执行：

```rust
link("hello\0", "alias\0");
```

操作前后，根目录中的关系如下：

```text
操作前：
("hello", 7) -> DiskInode 7 -> 文件数据块

操作后：
("hello", 7) --+
              +-> DiskInode 7 -> 同一组文件数据块
("alias", 7) --+
```

本操作只增加目录项，不创建新的文件 inode，不复制原文件的数据块，也不把旧文件名写成新文件的内容。

这与符号链接不同。符号链接保存目标路径，访问时需要解析路径；硬链接直接记录同一个 inode 编号，不依赖旧名称继续存在。

## 2. 实现涉及的文件

| 文件 | 实现内容 |
| --- | --- |
| `user/src/lib.rs` | 提供用户接口 `link` |
| `user/src/syscall.rs` | 封装系统调用号和寄存器参数 |
| `os/src/syscall/mod.rs` | 将系统调用分发到内核 `sys_linkat` |
| `os/src/syscall/fs.rs` | 翻译用户字符串，返回成功或失败 |
| `os/src/fs/mod.rs` | 导出 `link_file` |
| `os/src/fs/inode.rs` | 通过 `link_file` 转发到根目录 inode |
| `easy-fs/src/vfs.rs` | `Inode::link` 检查名称并追加目录项 |
| `easy-fs/src/layout.rs` | 提供 `DiskInode`、`DirEntry` 和块级读写 |
| `easy-fs/src/block_cache.rs` | 加载、修改并同步相关磁盘块 |

系统调用层负责用户态与内核态之间的接口转换；easy-fs 层负责目录和磁盘布局操作。这样，系统调用不需要自行计算目录项的块号和偏移。

## 3. 完整调用链

```text
用户态 link(old_path, new_path)
  -> 用户库 sys_linkat(AT_FDCWD, old_path, AT_FDCWD, new_path, 0)
     -> syscall6，系统调用号 37
        -> ecall

内核 trap_handler
  -> syscall(37, args)
     -> 内核 sys_linkat(args[1], args[3])
        -> current_user_token()
        -> translated_str(token, old_name)
        -> translated_str(token, new_name)
        -> link_file(&old_name, &new_name)
           -> ROOT_INODE.link(old_name, new_name)
              -> 锁定 EasyFileSystem
              -> read_disk_inode
                 -> 检查新名称未被占用
                 -> 查找旧名称对应的 inode 编号
              -> modify_disk_inode
                 -> 扩展目录大小
                 -> 追加 (new_name, old_inode_id)
              -> block_cache_sync_all()
              -> 返回目标 inode 的内存句柄
        -> 成功返回 0，普通查找失败返回 -1

trap_handler 将结果写入返回值寄存器
  -> 返回用户态
     -> 用户 link() 得到结果
```

这里的 `read_disk_inode` 和 `modify_disk_inode` 操作的是根目录的磁盘 inode。目录项则写入根目录 inode 所管理的数据块。目标普通文件的正文不需要修改。

## 4. 用户参数为什么是 `args[1]` 和 `args[3]`

用户库保留了类似 Linux `linkat` 的参数形式：

```rust
pub fn sys_linkat(
    old_dirfd: usize,
    old_path: &str,
    new_dirfd: usize,
    new_path: &str,
    flags: usize,
) -> isize
```

通过 `syscall6` 传递的参数顺序为：

| 参数位置 | 寄存器 | 内容 |
| --- | --- | --- |
| 0 | `a0` | `old_dirfd` |
| 1 | `a1` | 旧名称字符串指针 |
| 2 | `a2` | `new_dirfd` |
| 3 | `a3` | 新名称字符串指针 |
| 4 | `a4` | `flags` |

因此内核分发代码是：

```rust
SYSCALL_LINKAT => sys_linkat(args[1] as *const u8, args[3] as *const u8),
```

内核函数只接收两个名称指针，不能误用 `args[0]` 和 `args[1]`。

当前内核分发只处理四个参数，且建链逻辑固定使用 `ROOT_INODE`，没有处理目录 fd 和 `flags`。用户接口虽然具有五参数形式，但本章实现不等同于完整 Linux `linkat`。

## 5. 系统调用层逐步分析

当前实现为：

```rust
pub fn sys_linkat(old_name: *const u8, new_name: *const u8) -> isize {
    trace!("kernel:pid[{}] sys_linkat", current_task().unwrap().pid.0);
    let token = current_user_token();
    let old_name = translated_str(token, old_name);
    let new_name = translated_str(token, new_name);
    if link_file(old_name.as_str(), new_name.as_str()) {
        0
    } else {
        -1
    }
}
```

### 5.1 翻译用户字符串

`old_name` 和 `new_name` 最初都是用户虚拟地址，不能按内核页表直接解引用。

`current_user_token` 获取调用者的用户页表 token；`translated_str` 根据这个 token 查询用户页表，逐字节读取，遇到零字节结束，并构造内核中的 `String`。

例如，用户传入的内容为：

```text
h e l l o \0
```

转换后的内核字符串为 `"hello"`，不包含末尾零字节。

用户库传递的是 `&str` 的指针，并没有自动追加终止符。因此当前接口的调用者需要提供以 `\0` 结束的名称，测例也采用这种写法。

### 5.2 同名变量绑定

下面的代码使用 Rust 的变量遮蔽：

```rust
let old_name = translated_str(token, old_name);
```

右侧 `old_name` 仍是原来的 `*const u8`，左侧新绑定则是 `String`。下一行处理新名称，原理相同。

### 5.3 转发与返回

`String::as_str()` 得到借用的 `&str`，不会再次复制名称。`link_file` 返回 `bool`，系统调用将它转换为约定的返回值：成功为 0，失败为 -1。

该过程不分配文件描述符，也不修改当前进程的 `fd_table`。建硬链接是目录操作，不是打开文件。

## 6. 内核文件系统包装层

`os/src/fs/inode.rs` 中的包装函数为：

```rust
pub fn link_file(old_name: &str, new_name: &str) -> bool {
    ROOT_INODE.link(old_name, new_name).is_some()
}
```

`ROOT_INODE` 表示根目录。通过它调用 `link`，说明两个名称都在根目录中查找或建立。

`Inode::link` 返回 `Option<Arc<Inode>>`：

- `Some` 表示建链完成，并返回指向目标磁盘 inode 的句柄。
- `None` 表示名称查找或冲突检查失败。

包装层只需要知道是否成功，所以使用 `.is_some()`。返回的临时 `Arc<Inode>` 随后释放，但目录项已经写入文件系统，硬链接不会随该内存句柄的释放而消失。

## 7. `Inode::link` 的核心实现

### 7.1 锁定文件系统

```rust
let mut fs = self.fs.lock();
```

这个锁覆盖查找名称、扩展目录、追加目录项和同步缓存的过程，避免遵守同一锁约定的其他文件系统操作在中间插入修改。

`fs` 的类型是 `MutexGuard<EasyFileSystem>`。通过该 guard 可以调用分配数据块、定位 inode 等方法；离开函数时锁自动释放。

锁只协调内存中的并发访问，不代表磁盘事务，也不能保证断电后目录更新一定完整。

### 7.2 检查名称并取得旧 inode 编号

```rust
let old_inode_id = self.read_disk_inode(|dir_inode| {
    assert!(dir_inode.is_dir());
    if self.find_inode_id(new_name, dir_inode).is_some() {
        return None;
    }
    self.find_inode_id(old_name, dir_inode)
})?;
```

这里 `self` 是根目录 inode。闭包接收根目录的 `&DiskInode`，先确认它是目录，再检查新名称是否已经存在，最后查找旧名称。

`find_inode_id` 通过目录大小计算目录项数量，逐条读取 `DirEntry`，比较名称，匹配时返回对应的 inode 编号。

闭包返回 `Option<u32>`。末尾 `?` 的作用是：

```text
Some(id) -> 取出 id，继续执行
None     -> 当前 Inode::link 直接返回 None
```

因此，新名称已存在或旧名称不存在时，代码不会进入后续目录修改过程。新旧名称相同且旧文件存在，也会因为名称被占用而失败。

闭包中的 `return None` 只结束闭包；是外面的 `?` 将失败继续传出 `Inode::link`。

### 7.3 扩展目录数据

```rust
let file_count = (dir_inode.size as usize) / DIRENT_SZ;
let new_size = (file_count + 1) * DIRENT_SZ;
self.increase_size(new_size as u32, dir_inode, &mut fs);
```

一条目录项占 32 字节，所以追加一条目录项需要将目录有效大小增加 32 字节。

`increase_size` 根据目标大小计算额外需要的块数，分配数据块，必要时建立间接索引，再更新目录 inode 的大小与索引。

并非每次建链都需要分配新块。例如目录已有 3 条目录项，占用 96 字节，追加后为 128 字节，原来的 512 字节块足以容纳。如果已有 16 条，占用 512 字节，追加后为 544 字节，就需要增加目录数据块。

### 7.4 追加复用旧编号的目录项

```rust
let dirent = DirEntry::new(new_name, old_inode_id);
dir_inode.write_at(file_count * DIRENT_SZ, dirent.as_bytes(), &self.block_device);
```

`DirEntry::new` 将新名称和旧 inode 编号组合为目录项。`as_bytes` 将这条记录作为 32 字节切片提供给写入函数。

写入偏移 `file_count * DIRENT_SZ` 恰好是原目录内容的末尾。

例如原目录有 3 条记录，旧文件 inode 编号为 7：

```text
原目录大小：96 字节
新记录偏移：96 字节
新目录大小：128 字节
追加内容：("alias", 7)
```

硬链接的关键就在 `old_inode_id`：这里没有调用 `alloc_inode`，新目录项与旧目录项使用同一个编号。

这也是不能直接调用 `create(new_name)` 的原因。`create` 会分配新 inode，得到独立文件，不满足共享 inode 的要求。

### 7.5 同步并返回

```rust
let (block_id, block_offset) = fs.get_disk_inode_pos(old_inode_id);
block_cache_sync_all();
Some(Arc::new(Self::new(
    block_id,
    block_offset,
    self.fs.clone(),
    self.block_device.clone(),
)))
```

`get_disk_inode_pos` 将旧 inode 编号转换为元数据所在块号和块内偏移，用于构造返回句柄。

`block_cache_sync_all` 同步缓存中的脏块，包括本次修改涉及的目录元数据、目录数据和可能的分配位图或索引块。它不是只同步目标普通文件，也不是日志事务提交。

最后创建的是新的内存 `Inode` 句柄，指向的仍是原来的磁盘 inode。新的 `Arc` 对象不意味着新分配了磁盘 inode。

## 8. 与文件内容、打开偏移的关系

建链后，通过旧名称写入内容，再通过新名称读取，可以看到相同数据，因为两个名称最终查询到同一个 `DiskInode`。

但是分别打开两个名称时，`open_file` 会分别构造 `OSInode`，各自维护读写偏移。因此：

```text
共享磁盘 inode -> 共享文件内容和大小
分别 open     -> 通常具有独立的当前偏移
fork 克隆 fd  -> 共享原有打开文件对象及其偏移
```

不要将“共享文件内容”和“共享打开偏移”混为一谈，也不要用 `Arc::strong_count` 计算硬链接数。

## 9. 当前实现边界

本文描述的是当前代码已经实现的行为，以下能力不能视为已完成：

1. 用户指针检查不完整。`translated_str` 没有严格检查页表有效性、用户可读权限和字符串长度；使用 `unwrap`，非法地址不保证返回 -1，可能引发 panic 或非法访问。
2. 名称没有充分校验。当前没有拒绝空名称或过长名称。`DirEntry::new` 使用固定数组，超过数组容量会 panic；28 字节名称还会缺少结束零字节，后续读取名称也可能越界。正常使用应限制在 27 字节以内。
3. 名称校验应在修改目录前完成。当前先扩展目录再构造目录项，异常名称导致 panic 时不能依赖回滚。
4. 没有完整路径解析。代码只按根目录中的名称查找，没有处理一般的多级路径、目录 fd 或 flags。
5. 没有显式维护硬链接数。`DiskInode` 不含 `nlink` 字段，建链只追加目录项。要支持 `fstat` 的链接数，需要额外统计或存储机制。
6. 没有实现一般性的目录硬链接限制。当前检查的是接收操作的 inode 是否为目录，没有额外验证旧名称指向的对象是否为普通文件。
7. 分配和写回缺少可恢复错误处理。底层分配可能 `unwrap`，块设备接口也没有提供本函数可传播的 I/O 错误；不能认为所有失败都会变成 -1。
8. 删除链接与回收尚需独立实现。当前 `sys_unlinkat` 和 `sys_fstat` 仍是占位函数，不能仅凭 `sys_linkat` 判断完整硬链接测例已经通过。

## 10. 测试应验证什么

`ci-user/user/src/bin/ch6_file2.rs` 同时使用建链、读写、`fstat` 和删除链接。其主要预期是：

- 原文件增加一个名称后，硬链接数为 2。
- 再增加两个名称后，硬链接数为 4。
- 通过原文件写入的数据，可以通过其他名称读出。
- 删除原名称后，其他名称仍指向同一个 inode，数据仍然可读。
- 删除名称后链接数随之减少，inode 编号保持不变。

独立检查 `sys_linkat` 时，还应验证旧名称不存在、新名称已存在以及新旧名称相同的失败情况，并确认失败不会改变目录。

这些是验证目标，不是本文新增的运行测试结果。完整测例还依赖正确实现 `fstat` 和 `unlinkat`。

## 11. 总结

`sys_linkat` 的系统调用层负责把两个用户名称转换成内核字符串；`link_file` 将操作交给根目录；`Inode::link` 查出旧 inode 编号，再追加一条复用该编号的新目录项。

理解实现时最重要的是区分三件事：新增目录项、创建内存 inode 句柄、分配磁盘 inode。当前建链会做前两件事，不会做第三件事，也不会复制文件正文。
