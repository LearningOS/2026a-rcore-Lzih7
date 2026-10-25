# 第六章 inode 的作用与实现

## 1. inode 解决什么问题

块设备只提供按块读写的能力。例如，读取第 100 号块，可以得到 512 字节，但块设备不知道这些字节属于哪个文件，也不知道文件有多大、下一段内容在哪里。

文件系统需要建立下面的关系：

```text
文件名 -> inode 编号 -> 文件元数据 -> 文件的数据块 -> 实际内容
```

inode 是文件或目录的元数据记录。它描述对象的类型、大小以及数据块的位置，使文件系统能够把“读取文件的第几个字节”转换为“读取设备的哪个块、块内哪个位置”。

在当前 easy-fs 中，inode 不保存文件名，也不直接保存文件内容：

- 文件名保存在目录项 `DirEntry` 中。
- 文件大小、类型、数据块索引保存在 `DiskInode` 中。
- 文件内容保存在数据块中。

因此，文件名更像查找入口，inode 才是这个文件在文件系统内的身份。

## 2. 三种相关结构

本章代码中的 `DiskInode`、`Inode` 和 `OSInode` 处于不同层次，不能把它们理解为三个独立文件。

| 结构 | 位置 | 作用 |
| --- | --- | --- |
| `DiskInode` | `easy-fs/src/layout.rs` | 定义保存在磁盘上的文件元数据 |
| `Inode` | `easy-fs/src/vfs.rs` | 保存磁盘 inode 的位置，提供查找、创建和读写接口 |
| `OSInode` | `os/src/fs/inode.rs` | 包装文件系统 inode，保存一次打开的权限和当前读写偏移 |

它们的关系可以概括为：

```text
进程 fd_table[fd]
  -> Arc<dyn File>，普通文件实际指向 OSInode
     -> OSInodeInner
        offset：当前读写位置
        inode：Arc<Inode>
          -> block_id、block_offset：磁盘 inode 的位置
             -> 块缓存中的 DiskInode
                -> 数据块索引
                   -> 文件内容
```

### 2.1 DiskInode：磁盘上的记录

当前定义为：

```rust
pub struct DiskInode {
    pub size: u32,
    pub direct: [u32; 28],
    pub indirect1: u32,
    pub indirect2: u32,
    type_: DiskInodeType,
}
```

`size` 表示有效内容的字节数，而不是分配的磁盘空间大小。`type_` 区分普通文件和目录。其余字段负责记录数据块的位置。

这里的块号不是内存指针，也不是用户虚拟地址，而是块设备上的块编号。

一般文件系统的 inode 还会记录权限、所有者、时间戳和硬链接数。当前 easy-fs 的 `DiskInode` 没有这些字段，因此不能直接套用完整 Unix inode 的布局。

### 2.2 Inode：操作磁盘记录的句柄

`Inode` 的主要字段是：

```rust
pub struct Inode {
    block_id: usize,
    block_offset: usize,
    fs: Arc<Mutex<EasyFileSystem>>,
    block_device: Arc<dyn BlockDevice>,
}
```

`block_id` 和 `block_offset` 指向的是磁盘 inode 本身的位置，不是文件内容的起始位置。

读取元数据时，`read_disk_inode` 根据这两个字段取到块缓存，再把块内对应字节解释为 `DiskInode`：

```text
get_block_cache(block_id, block_device)
  -> lock()
     -> read(block_offset, 回调函数)
        -> 回调函数访问 &DiskInode
```

修改时使用 `modify_disk_inode`，缓存会被标记为已修改，之后写回设备。

`Inode` 没有直接持有一整份 `DiskInode`，也没有长期持有它所在块的缓存。它保存定位信息，在操作时按需获取缓存。多个 `Inode` 对象可以指向同一个磁盘 inode。

### 2.3 OSInode：一次打开文件的状态

`OSInode` 保存 `readable`、`writable`，其内部的 `OSInodeInner` 保存：

```rust
pub struct OSInodeInner {
    offset: usize,
    inode: Arc<Inode>,
}
```

`offset` 属于打开文件的状态，不属于磁盘 inode。读取或写入后，内核会累加它，使下一次操作从后续位置开始。

分别调用两次 `open_file`，通常会得到两个 `OSInode`，各自从偏移 0 开始，但访问同一个文件的数据。`fork` 则克隆 fd 表中的 `Arc`，父子进程共享已有的 `OSInode`，因此也共享该打开文件的偏移。

`Arc` 的引用计数表示内存对象被多少引用持有，不能作为磁盘文件的硬链接数。

## 3. 目录项为什么与 inode 分开

目录也是一个 inode，但它的数据块保存的是目录项，而不是普通文件内容。

当前 `DirEntry` 由两个字段构成：

```rust
pub struct DirEntry {
    name: [u8; 28],
    inode_id: u32,
}
```

一条目录项占 32 字节，保存文件名和对应的 inode 编号。文件名按以零字节结尾的形式使用，当前正常名称长度上限为 27 字节。

假设根目录中有：

```text
目录项 ("hello", inode_id = 7)
目录项 ("data",  inode_id = 9)
```

查找 `hello` 时，文件系统先读取根目录的数据，找到名称匹配的目录项，再用编号 7 定位目标 `DiskInode`。它不是直接拿文件名计算数据块位置。

本章 `open_file` 直接调用 `ROOT_INODE.find(name)`，实现的是根目录下的名称查找，没有通用的多级路径解析过程。

## 4. inode 编号如何定位磁盘记录

文件系统的磁盘布局为：

```text
超级块 | inode 位图 | inode 区域 | 数据位图 | 数据区域
```

inode 位图记录哪些 inode 编号已经分配。inode 区域保存对应的 `DiskInode`。数据位图和数据区域分别管理数据块的分配状态与内容，其中间接索引块也从数据区域分配。

`EasyFileSystem::get_disk_inode_pos(inode_id)` 的计算逻辑是：

```rust
let inode_size = core::mem::size_of::<DiskInode>();
let inodes_per_block = (BLOCK_SZ / inode_size) as u32;
let block_id = self.inode_area_start_block + inode_id / inodes_per_block;
let block_offset = (inode_id % inodes_per_block) as usize * inode_size;
```

当前布局下 `DiskInode` 为 128 字节，一块为 512 字节，所以每块容纳 4 个 inode。

例如，假设 inode 区域从设备第 2 号块开始，查找编号 7：

```text
block_id     = 2 + 7 / 4 = 3
block_offset = (7 % 4) * 128 = 384
```

也就是说，编号 7 的磁盘 inode 位于设备第 3 号块的第 384 字节处。这个位置存放的是元数据，文件内容还需要通过其中的数据块索引继续查找。

根目录的 inode 编号固定为 0。格式化文件系统时，`EasyFileSystem::create` 首先分配编号 0，并将其类型初始化为 `Directory`。

## 5. inode 如何找到文件内容

### 5.1 直接索引

`direct` 有 28 项，每项保存一个数据块号。因此，文件的逻辑数据块 0 到 27 可以直接定位：

```text
文件第 3 个逻辑数据块 -> direct[3] -> 设备数据块
```

这里编号从 0 开始。直接索引可以覆盖 28 × 512 = 14336 字节。

### 5.2 一级间接索引

`indirect1` 保存一个索引块的块号。这个索引块不保存文件正文，而是保存 128 个 `u32` 数据块号，因为 512 / 4 = 128。

它对应逻辑数据块 28 到 155：

```text
逻辑块号 inner_id
  -> indirect1 指向的索引块
     -> 索引项 [inner_id - 28]
        -> 数据块号
```

例如，逻辑块 30 对应一级间接索引块中的第 2 项。

### 5.3 二级间接索引

逻辑数据块从 156 开始走二级间接索引。`indirect2` 指向的块保存一级索引块的块号，每个一级索引块再保存数据块号。

`get_block_id` 先计算：

```rust
let last = inner_id - 156;
```

再用 `last / 128` 选择一级索引块，用 `last % 128` 选择其中的数据块号。

例如逻辑块 300：

```text
last = 300 - 156 = 144
二级索引中的位置 = 144 / 128 = 1
一级索引中的位置 = 144 % 128 = 16

indirect2 -> 第 1 项指向的一级索引块 -> 第 16 项 -> 数据块号
```

三类索引最多表示 28 + 128 + 128 × 128 = 16540 个数据块，即 8468480 字节的文件内容。间接索引块自身还会额外占用磁盘空间。

`get_block_id` 返回的是数据块号，并没有读取文件正文。真正读取正文由 `DiskInode::read_at` 随后完成。

## 6. 打开和读取文件的调用链

### 6.1 打开文件

```text
用户 open("hello", ...)
  -> sys_open
     -> translated_str：读取用户传来的文件名
     -> open_file
        -> ROOT_INODE.find("hello")
           -> find_inode_id：遍历根目录的目录项
           -> get_disk_inode_pos：定位磁盘 inode
           -> 构造 Arc<Inode>
        -> 构造 OSInode，offset = 0
     -> 放入当前进程 fd_table
     -> 返回 fd
```

打开文件主要是在建立访问对象，并不会自动把整个文件读进内存。`open_file` 本身也不分配 fd，fd 的分配与登记由 `sys_open` 完成。

### 6.2 读取文件

```text
用户 read(fd, buf, len)
  -> sys_read
     -> 从 fd_table 取得 File 对象
     -> translated_byte_buffer：处理用户缓冲区
     -> OSInode::read
        -> Inode::read_at(offset, ...)
           -> read_disk_inode
              -> DiskInode::read_at
                 -> get_block_id：逻辑块号转换为设备块号
                 -> get_block_cache：取得数据块缓存
                 -> 将数据复制到目标缓冲区
        -> 累加 offset
```

例如从文件偏移 600 开始读取，块大小为 512：

```text
逻辑块号 = 600 / 512 = 1
块内偏移 = 600 % 512 = 88
设备块号 = direct[1]
```

接着从该设备块的第 88 字节开始读取。如果读取范围跨块，`read_at` 会分段处理；如果超过文件大小，则只读取到 `size`。

块缓存未命中时，`get_block_cache` 会通过 `BlockCache::new` 调用设备的 `read_block` 加载该块。因此读取 inode、间接索引块和数据块都可以使用同一套缓存接口。

## 7. 创建、写入与硬链接

### 7.1 创建文件

`Inode::create(name)` 的核心步骤是：

1. 检查目录中是否已经存在该名称。
2. 调用 `alloc_inode`，在 inode 位图中分配编号。
3. 定位并初始化新的 `DiskInode`，类型为普通文件，大小为 0。
4. 扩展父目录的数据空间，追加 `(name, inode_id)` 目录项。
5. 同步块缓存，返回新 inode 的操作句柄。

新文件为空时，不需要立即分配正文数据块。目录追加一条记录是否需要新块，则取决于原目录数据块是否还有空间。

写入文件时，`Inode::write_at` 会先按目标大小计算需要增加的正文块和间接索引块，调用 `alloc_data` 分配，再更新 `DiskInode` 的大小和索引，最后写入内容。

### 7.2 硬链接

当前 `Inode::link(old_name, new_name)` 复用旧文件的 inode 编号，只新增目录项：

```text
目录项 ("hello", inode_id = 7) --+
                               +-> DiskInode 7 -> 同一组文件数据块
目录项 ("alias", inode_id = 7) --+
```

这不是复制文件，也不是让新名称保存旧名称字符串。通过任一名称打开，最终访问的都是同一个磁盘 inode，文件内容与大小也相同。

该操作不分配新的文件 inode，也不复制正文数据块；不过目录扩容时可能分配新的目录数据块或索引块。

当前 `DiskInode` 没有 `nlink` 字段，`link` 也没有更新磁盘链接计数。若要提供硬链接数，可以在本章单层目录模型下统计指向同一 inode 编号的目录项，或者设计额外的计数存储。

### 7.3 清空与删除不是同一件事

`Inode::clear` 清空文件数据、把大小变为 0，并回收正文块及间接索引块，但不移除目录项，也不释放 inode 编号。

删除一个硬链接应移除指定目录项，不能直接调用 `clear`：只要还有其他名称指向同一个 inode，文件数据就应继续存在。

完整文件系统还需要考虑打开文件的引用。最后一个名称被删除后，如果进程仍持有打开文件对象，通常也不能立即回收正文数据。当前代码中的 `Arc` 析构本身不等于磁盘 inode 或数据块回收。

## 8. 总结

inode 将文件的身份和内容布局与文件名分离。目录项负责“根据名称找到谁”，磁盘 inode 负责“这个对象是什么、数据在哪里”，数据块保存实际内容。

在本项目中，`DiskInode` 是持久化元数据，`Inode` 是文件系统操作句柄，`OSInode` 是一次打开文件的状态。理解这三层，可以解释文件查找、块索引、父子进程共享偏移以及硬链接为什么共享内容。
