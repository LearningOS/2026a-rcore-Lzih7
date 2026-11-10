# ELF 加载与进程地址空间建立

## 1. 文档范围

本文说明当前仓库中 `MemorySet::from_elf()` 的实现，重点介绍内核如何依据 ELF 文件建立进程地址空间，以及该过程与线程初始化、用户态执行的衔接关系。

主要实现位于 [`os/src/mm/memory_set.rs`](../os/src/mm/memory_set.rs)，相关模块如下：

| 文件 | 职责 |
| --- | --- |
| [`os/src/mm/page_table.rs`](../os/src/mm/page_table.rs) | 建立 Sv39 页表与虚拟页到物理页的映射 |
| [`os/src/mm/frame_allocator.rs`](../os/src/mm/frame_allocator.rs) | 分配、清零和回收物理页帧 |
| [`os/src/task/process.rs`](../os/src/task/process.rs) | 创建进程，保存地址空间并初始化主线程 |
| [`os/src/task/id.rs`](../os/src/task/id.rs) | 分配线程用户栈和 TrapContext |
| [`os/src/trap/context.rs`](../os/src/trap/context.rs) | 构造初始用户寄存器现场 |
| [`os/src/trap/trap.S`](../os/src/trap/trap.S) | 切换页表，恢复寄存器并返回用户态 |
| [`user/src/linker.ld`](../user/src/linker.ld) | 指定用户程序的节布局和入口符号 |

本文中的地址均应区分为文件偏移、虚拟地址和物理地址。三者属于不同范围，不能直接互换。

## 2. 地址空间与程序节

### 2.1 用户地址空间与内核地址空间

当前实现使用独立的用户页表和内核页表。每个进程持有自己的 `MemorySet`，内核地址空间由 `KERNEL_SPACE` 管理。

内核和应用分别编译、链接，各自具有 `.text`、`.rodata`、`.data` 和 `.bss` 等节。同名节表示相似的内容分类，不表示它们属于同一块内存。

| 节 | 用户程序中的内容 | 内核中的内容 | 常见权限 |
| --- | --- | --- | --- |
| `.text` | 应用机器指令 | 内核机器指令 | 读、执行 |
| `.rodata` | 应用只读数据 | 内核只读数据 | 只读 |
| `.data` | 应用已初始化的可写数据 | 内核已初始化的可写数据 | 读、写 |
| `.bss` | 应用零初始化数据 | 内核零初始化数据 | 读、写 |

用户程序的可加载区域具有 `U` 权限。内核代码和数据不具有用户访问权限。进程页表中还会映射 trampoline、TrapContext 等供特权态代码使用的区域，因此“映射在进程页表中”并不等于“允许用户态访问”。

### 2.2 虚拟连续与物理连续

一个程序段通常占据连续的虚拟地址区间，但该区间内的不同虚拟页可以映射到不连续的物理页。例如：

```text
连续的用户虚拟页             分配的物理页
0x10000 -----------------> 0x80412000
0x11000 -----------------> 0x80503000
0x12000 -----------------> 0x80418000
```

不同段之间可以存在页对齐空隙，代码、用户栈和其他映射之间也可以存在未映射区域。进程地址空间不要求整体连续占用物理内存，也不要求所有虚拟地址均具有有效映射。

## 3. ELF 中的节与可加载段

ELF 使用 Section（节）组织代码和数据，使用 Segment（段）描述运行时加载布局。

| 概念 | 示例 | 本加载流程中的作用 |
| --- | --- | --- |
| Section | `.text`、`.rodata`、`.data`、`.bss` | 描述代码和数据的组织方式 |
| Segment | `PT_LOAD` | 指定需要装入内存的文件内容、虚拟地址、内存大小和权限 |

`from_elf()` 遍历程序头表，仅处理 `PT_LOAD`，不按节名分别加载。一个可加载段可以包含多个节，例如同一个读写段可以包含 `.data` 和 `.bss`。实际映射权限来自程序头的标志，而非节名。

加载过程使用的程序头字段如下：

| ELF 字段 | 代码接口 | 含义 |
| --- | --- | --- |
| `p_vaddr` | `virtual_addr()` | 段在进程中的起始虚拟地址 |
| `p_memsz` | `mem_size()` | 段在运行时需要的字节数 |
| `p_offset` | `offset()` | 初始内容在 ELF 文件中的偏移 |
| `p_filesz` | `file_size()` | 从 ELF 文件复制的字节数 |
| `p_flags` | `flags()` | 段的读、写、执行权限 |

对一个可加载段，加载器需要建立以下关系：

```text
ELF 文件 [p_offset, p_offset + p_filesz)
                    |
                    | 复制初始内容
                    v
             新分配的物理页
                    ^
                    | 页表映射
                    |
用户虚拟地址 [p_vaddr, p_vaddr + p_memsz)
```

以上区间均为左闭右开区间。

## 4. 函数接口与职责

```rust
pub fn from_elf(elf_data: &[u8]) -> (Self, usize, usize)
```

输入 `elf_data` 是已读入内核内存的 ELF 文件字节。函数不负责从文件系统读取文件。

| 返回值 | 含义 |
| --- | --- |
| `Self` | 已建立 ELF 可加载段和 trampoline 映射的 `MemorySet` |
| 第一个 `usize` | 后续安排线程用户栈的基准虚拟地址 `user_stack_base` |
| 第二个 `usize` | ELF 入口虚拟地址 `entry_point` |

当前函数不分配用户栈或 TrapContext，也不切换 `satp`、执行应用或建立用户堆。函数顶部关于包含用户栈和 TrapContext 的注释，以及末尾关于映射用户栈的注释，比实际实现覆盖的职责更广，应以代码和后续调用链为准。

## 5. 地址空间建立流程

### 5.1 创建空页表

```rust
let mut memory_set = Self::new_bare();
```

`new_bare()` 创建新的 `PageTable` 和空的 `areas` 向量。`PageTable::new()` 分配并持有根页表物理页，此时尚未映射应用代码和数据。

`MemorySet` 中两个主要成员的职责如下：

| 成员 | 职责 |
| --- | --- |
| `page_table` | 保存硬件地址转换所需的页表 |
| `areas` | 保存映射区域及其物理页所有权等管理信息 |

### 5.2 映射 trampoline

```rust
memory_set.map_trampoline();
```

此操作将 `TRAMPOLINE` 所在虚拟页映射到内核 `strampoline` 所在物理页，设置 `R | X` 权限，不设置 `U`。该映射直接写入页表，不加入 `areas`。

用户页表与内核页表在相同虚拟地址映射同一跳板物理页，确保切换页表前后，跳板中的指令仍可继续执行。跳板用于特权态下的陷入和返回过程，用户态不能直接执行。

该操作只共享跳板页，不会把内核的全部 `.text` 映射到用户页表。

### 5.3 解析 ELF 文件

```rust
let elf = xmas_elf::ElfFile::new(elf_data).unwrap();
let elf_header = elf.header;
let magic = elf_header.pt1.magic;
assert_eq!(magic, [0x7f, 0x45, 0x4c, 0x46], "invalid elf!");
let ph_count = elf_header.pt2.ph_count();
```

函数通过 `xmas_elf` 解析文件，检查 ELF 魔数，并读取程序头数量。魔数检查用于识别 ELF 格式，不构成对所有加载约束的完整验证。

### 5.4 筛选可加载段

```rust
for i in 0..ph_count {
    let ph = elf.program_header(i).unwrap();
    if ph.get_type().unwrap() == xmas_elf::program::Type::Load {
        // 建立该段的内存映射并装入内容
    }
}
```

只有 `PT_LOAD` 参与后续映射。函数不读取节头表，也不查询 `.text`、`.data` 或 `.bss` 名称。

### 5.5 确定虚拟地址范围与权限

```rust
let start_va: VirtAddr = (ph.virtual_addr() as usize).into();
let end_va: VirtAddr =
    ((ph.virtual_addr() + ph.mem_size()) as usize).into();
```

段的虚拟地址范围由 ELF 决定，分配大小使用 `p_memsz`。随后以 `U` 为基础，按程序头标志添加 `R`、`W` 和 `X`：

```rust
let mut map_perm = MapPermission::U;
let ph_flags = ph.flags();
if ph_flags.is_read() {
    map_perm |= MapPermission::R;
}
if ph_flags.is_write() {
    map_perm |= MapPermission::W;
}
if ph_flags.is_execute() {
    map_perm |= MapPermission::X;
}
```

典型应用代码段具有 `U | R | X`，只读数据段具有 `U | R`，读写数据段具有 `U | R | W`。

### 5.6 创建页粒度的区域描述

```rust
let map_area =
    MapArea::new(start_va, end_va, MapType::Framed, map_perm);
```

`MapArea::new()` 将起始地址向下取整为虚拟页号，将结束地址向上取整为虚拟页号：

```rust
let start_vpn = start_va.floor();
let end_vpn = end_va.ceil();
```

例如，页大小为 `0x1000` 时，字节区间 `[0x10000, 0x12500)` 需要映射三个虚拟页，实际页覆盖范围为 `[0x10000, 0x13000)`。

此时仅创建了描述对象，尚未分配该段的数据物理页。`MapType::Framed` 表示后续为每个虚拟页独立分配物理页；它不要求虚拟地址等于物理地址，也不要求物理页连续。

### 5.7 分配物理页并建立页表映射

```rust
memory_set.push(
    map_area,
    Some(&elf.input[
        ph.offset() as usize
        ..(ph.offset() + ph.file_size()) as usize
    ]),
);
```

`push()` 的执行顺序为：

1. 调用 `MapArea::map()` 建立整个区域的映射。
2. 存在初始数据时，调用 `copy_data()` 装入内容。
3. 将区域加入 `MemorySet::areas`。

`map()` 对每个虚拟页调用 `map_one()`。对于 `Framed` 映射，核心操作为：

```rust
let frame = frame_alloc().unwrap();
ppn = frame.ppn;
self.data_frames.insert(vpn, frame);
page_table.map(vpn, ppn, pte_flags);
```

其中：

- `frame_alloc()` 分配物理页，`FrameTracker::new()` 将该页清零。
- `data_frames` 保存虚拟页与物理页拥有者之间的关系，以便后续管理和回收。
- `PageTable::map()` 按需分配中间层页表页，并在叶子页表项中写入物理页号、区域权限和有效位 `V`。

用于存放应用内容的物理页由 `MapArea::data_frames` 持有；用于存放页表项的物理页由 `PageTable::frames` 持有。两类物理页承担不同职责。

### 5.8 复制文件内容

`copy_data()` 从 ELF 文件提供的切片中按页取出数据，查询新页表得到目标物理页，再将数据写入该物理页。

```text
取出一页以内的 ELF 内容
          |
          v
查询新页表：VPN -> PPN
          |
          v
通过内核映射访问目标物理页并写入
          |
          v
处理下一个虚拟页，直到复制完成
```

此过程不需要先激活用户页表。当前内核通过自身对物理内存的恒等映射访问目标页，新页表只用于查询目标虚拟页对应的物理页。

### 5.9 保留零初始化区域

运行时分配大小与文件复制大小分别使用 `p_memsz` 和 `p_filesz`。当 `p_memsz > p_filesz` 时，段尾多出的内存应初始化为零。

例如，某读写段具有：

```text
p_vaddr  = 0x20000
p_filesz = 0x600
p_memsz  = 0x1800
```

其内容为：

| 虚拟地址区间 | 初始化方式 |
| --- | --- |
| `[0x20000, 0x20600)` | 从 ELF 文件复制 `0x600` 字节 |
| `[0x20600, 0x21800)` | 保持为零，共 `0x1200` 字节 |

由于新分配的页已经清零，复制文件内容后，段尾自然保持为零。包含在该区域中的 `.bss` 因而获得运行时存储空间，而无须在 ELF 文件中保存等量零字节。

### 5.10 计算用户栈布局基准并返回

加载每个段时，函数记录其页对齐结束位置：

```rust
max_end_vpn = map_area.vpn_range.get_end();
```

循环结束后，将其转换为虚拟地址，再跳过一页：

```rust
let max_end_va: VirtAddr = max_end_vpn.into();
let mut user_stack_base: usize = max_end_va.into();
user_stack_base += PAGE_SIZE;
```

在预期布局中，ELF 映射与主线程用户栈之间留出一页未映射区域，作为保护页。用户栈向低地址增长并越过边界进入该页时，会触发页异常。该保护机制依赖访问实际落入保护页，并不覆盖跳过保护页的任意越界访问。

函数最后返回地址空间、栈布局基准和 ELF 入口地址：

```rust
(
    memory_set,
    user_stack_base,
    elf.header.pt2.entry_point() as usize,
)
```

`user_stack_base` 不是初始栈指针。初始栈指针由后续线程资源分配得到的栈顶确定。ELF 入口也不必等于 `main`；当前用户链接脚本通过 `ENTRY(_start)` 指定启动入口。

## 6. 从地址空间建立到首次执行

`ProcessControlBlock::new()` 接收 `from_elf()` 的返回值，保存地址空间并创建主线程。后续流程如下：

```text
MemorySet::from_elf(elf_data)
  +-- 创建页表
  +-- 映射 trampoline
  +-- 为各 PT_LOAD 分配物理页并建立映射
  +-- 复制初始内容，保留零初始化区域
  +-- 返回 memory_set、ustack_base、entry_point
          |
          v
ProcessControlBlock::new()
  +-- 保存 memory_set
  +-- 创建主线程 TaskControlBlock
          |
          v
TaskUserRes::alloc_user_res()
  +-- 映射用户栈：R | W | U
  +-- 映射 TrapContext：R | W，无 U
          |
          v
TrapContext::app_init_context()
  +-- sepc = entry_point
  +-- sp = 用户栈顶
  +-- 设置 sstatus、内核页表 token、内核栈和陷入处理入口
          |
          v
主线程获得调度，进入 trap_return / __restore
  +-- 写 satp，切换到用户页表
  +-- 执行 sfence.vma
  +-- 恢复用户寄存器和返回状态
  +-- 执行 sret，进入 U 模式并从 entry_point 开始执行
```

每个线程的用户栈位置按以下方式计算：

```text
栈底 = ustack_base + tid * (PAGE_SIZE + USER_STACK_SIZE)
栈顶 = 栈底 + USER_STACK_SIZE
```

当前配置的 `USER_STACK_SIZE` 为两页。主线程 TID 为 0 时，栈底即 `ustack_base`，初始 `sp` 指向栈顶。

主线程资源分配完成后的用户页表示意如下，地址由上向下递减：

```text
高地址
  +----------------------------------+
  | trampoline：R | X，无 U          |
  +----------------------------------+
  | 主线程 TrapContext：R | W，无 U  |
  +----------------------------------+
  | 大量未映射区域                   |
  +----------------------------------+ <- 初始用户 sp
  | 主线程用户栈：R | W | U          |
  +----------------------------------+ <- ustack_base
  | 一页未映射保护区                 |
  +----------------------------------+ <- ELF 映射结束位置
  | ELF 可加载段，按段设置权限        |
  | 包含 .text / .rodata / .data      |
  | 以及 .bss 对应的零初始化区域     |
  +----------------------------------+
低地址
```

图中 ELF 各段可以存在空隙，实际地址与段的组合由生成的 ELF 文件决定。内核完整的代码、数据和线程内核栈由内核页表映射，未在该图中列出。

## 7. 当前实现的假设与边界

### 7.1 加载顺序

`max_end_vpn` 每次直接赋值，没有显式执行最大值比较。该实现依赖可加载段按虚拟地址递增排列，并使最后记录的结束位置能够代表程序映射的最高结束位置；函数本身不验证这一条件。

### 7.2 页对齐与映射冲突

`copy_data()` 从目标区域首个页的页首开始写入，不处理 `p_vaddr` 的页内偏移。因此当前加载流程依赖可加载段的虚拟起始地址按页对齐。用户链接脚本对主要区域进行了 4 KiB 对齐，但不能据此认为该函数支持所有合法 ELF 布局。

不同 `MapArea` 的页范围还必须互不重叠。若两个段在取整后覆盖同一个虚拟页，`PageTable::map()` 的重复映射断言会失败。

### 7.3 错误处理与加载能力

当前实现使用 `unwrap()`、`assert_eq!()` 和切片索引处理解析、分配及数据访问。非法文件、越界字段或内存不足可能导致 panic，函数没有返回结构化加载错误。

该函数按 ELF 指定的虚拟地址直接装载，未实现动态链接器加载、动态重定位或通用地址随机化。程序头中除 `PT_LOAD` 之外的条目不会在此建立对应的运行时行为。

## 8. 相关文档

- [进程、线程数据结构与系统调用详解](process_thread.md)
- [Trap、跳板与陷入返回详解](trap.md)
