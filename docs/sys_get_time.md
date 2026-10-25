# 第四章 `sys_get_time` 实现详解

## 1. 功能与调用约定

`sys_get_time` 将当前时钟计数换算为秒和微秒，写入用户程序提供的 `TimeVal`。它的重点是把时间数据从内核写回用户地址空间，并处理目标结构体跨越页边界的情况。

接口定义在 `os/src/syscall/process.rs`：

```rust
pub fn sys_get_time(_ts: *mut TimeVal, _tz: usize) -> isize
```

| 参数或返回值 | 含义 |
| --- | --- |
| `_ts` | 用户虚拟地址，指向用于接收时间的 `TimeVal` |
| `_tz` | 当前实现未使用的时区参数 |
| 返回 `0` | 时间数据写入成功 |
| 返回 `-1` | 地址范围或页表权限检查失败 |

参数名前的下划线不影响传参，也不代表参数没有参与计算；当前 `_ts` 实际用于定位输出缓冲区，只有 `_tz` 未使用。

需要区分两个返回结果：内核 `sys_get_time` 返回执行状态，时间本身通过 `_ts` 写回；用户库中的 `get_time()` 再读取结构体，将时间换算为毫秒并返回。

## 2. 数据布局

```rust
#[repr(C)]
#[derive(Debug)]
pub struct TimeVal {
    pub sec: usize,
    pub usec: usize,
}
```

在当前 RV64 目标上，`usize` 为 8 字节，结构体按 C 布局排列：

| 相对地址 | 长度 | 字段 |
| --- | --- | --- |
| `0` 至 `7` | 8 字节 | `sec` |
| `8` 至 `15` | 8 字节 | `usec` |

两个字段类型相同，当前布局中没有额外的字段间填充，`TimeVal` 总长度为 16 字节。代码使用 `size_of::<TimeVal>()` 和 `size_of::<usize>()` 获取大小，避免在复制过程中直接写死 `16` 和 `8`。

`usec` 表示当前整秒内的微秒余数，范围为 `0` 至 `999999`，不是从起点累计的微秒总数。

## 3. 第四章为什么需要地址翻译

第四章启用虚拟内存后，用户程序与内核使用不同的页表。同一个数值地址在不同地址空间中，可能对应不同物理页，也可能根本没有映射。

因此，不能直接沿用第三章的写法：

```rust
unsafe {
    *_ts = TimeVal { sec, usec };
}
```

这段代码会按当前内核页表解释 `_ts`，不能保证写入的是用户缓冲区。即使先翻译起始地址，也不能默认后续 16 字节在物理内存中连续。

例如，用户虚拟页 `0x1000` 和 `0x2000` 相邻，但它们可以分别映射到物理页 `0x81000000` 和 `0x82000000`。从第一个物理页末尾继续向后写，不会自动跳到第二个物理页。

当前方案分为两步：先检查目标区间覆盖的全部用户页，再通过页表将区间拆分成物理内存切片，逐片写入。

## 4. 检查输出地址范围

### 4.1 使用半开区间表示缓冲区

```rust
let start = _ts as usize;
let len = core::mem::size_of::<TimeVal>();
let end = match start.checked_add(len) {
    Some(end) => end,
    None => return -1,
};
```

目标缓冲区表示为 `[start, end)`：包含 `start`，不包含 `end`。最后一个要写入的字节位于 `end - 1`。

`checked_add()` 在加法溢出时返回 `None`。用户可以传入任意地址数值，不能假定 `start + len` 总能得到正确结果。检查溢出后，后续计算中的区间长度和循环条件才有可靠依据。

### 4.2 检查空地址和 Sv39 低半区范围

```rust
if start == 0 || end > (1usize << 38) {
    return -1;
}
```

Sv39 的虚拟地址需要满足规范地址要求：位 63 至位 39 应与位 38 相同。低半区规范地址范围是：

```text
0x0000000000000000 至 0x0000003fffffffff
```

当前函数只接受这一区域内的非空用户缓冲区。这是本实现采用的地址范围策略，并不意味着所有 Sv39 用户映射在体系结构上都只能位于低半区。

这里比较的是缓冲区的尾后地址，所以允许 `end == 1 << 38`；此时最后一个实际访问字节仍位于低半区内部。

先检查原始数值地址也很重要：本仓库的 `VirtAddr::from()` 会截取低 39 位。如果不先检查，非法高位可能被截掉，使一个不合法的原始地址被误认为另一个地址。

## 5. 检查所有目标页

### 5.1 使用当前任务的页表

```rust
let token = current_user_token();
let page_table = PageTable::from_token(token);
```

`current_user_token()` 获取当前任务地址空间的页表 token。`PageTable::from_token()` 根据 token 中的根页表物理页号建立一个临时页表访问对象。

这里没有切换 CPU 当前使用的页表，也没有为任务创建新的映射。内核通过这个对象遍历用户页表，找到输出缓冲区对应的物理页。

#### 5.1.1 token 是什么

token 并不是一个普通句柄或索引，它就是可以写进 `satp` 寄存器的那个值。它由 `os/src/mm/page_table.rs` 中的 `PageTable::token()` 生成：

```rust
pub fn token(&self) -> usize {
    8usize << 60 | self.root_ppn.0
}
```

按 Sv39 的 `satp` 字段定义，这个 `usize` 可以拆成三段：

| 位段 | 长度 | 本仓库取值 | 含义 |
| --- | --- | --- | --- |
| `63..60` | 4 位 | `8` | `MODE`，`8` 表示 Sv39 三级页表 |
| `59..44` | 16 位 | `0` | `ASID`，地址空间标识，本仓库未使用 |
| `43..0` | 44 位 | 根页表物理页号 | `PPN`，指向该地址空间的根页表 |

因此 token 里没有任何虚拟地址信息，只有一个物理页号和分页模式。它唯一回答的问题是：这个地址空间的根页表在哪个物理页上。

token 的值在每个任务的地址空间建立时就确定了。`TaskControlBlock::new()` 调用 `MemorySet::from_elf()` 得到该任务的 `MemorySet`，其中包含一个新建的 `PageTable`，于是 `memory_set.token() -> page_table.token()` 就返回 `8 << 60 | root_ppn`。只要这个任务的根页表不被替换，token 就一直不变。

反过来，`PageTable::from_token()` 只做一件事：

```rust
pub fn from_token(satp: usize) -> Self {
    Self {
        root_ppn: PhysPageNum::from(satp & ((1usize << 44) - 1)),
        frames: Vec::new(),
    }
}
```

它用掩码取出低 44 位当作根页表物理页号，并让 `frames` 为空。这个细节很重要：这个对象只是对已有页表的一次只读式访问，析构时不会释放任何页帧，也不会影响原任务的页表结构。

#### 5.1.2 完整调用链

从用户程序调用 `get_time()` 到内核拿到 token，整条链路如下。

第一阶段，用户态发出系统调用：

```text
用户程序
  -> user_lib::get_time()
  -> user_lib::sys_get_time(&mut time, 0)
  -> ecall，a7 = 169（SYSCALL_GET_TIME）
```

第二阶段，陷入内核并切换地址空间：

```text
trap.S: __alltraps
  -> 把通用寄存器、sstatus、sepc 保存到用户页中的 TrapContext
  -> 从 TrapContext 读出内核页表 token，csrw satp 切到内核地址空间
  -> jr trap_handler
```

第三阶段，内核分发并处理系统调用：

```text
trap_handler()                       [trap/mod.rs]
  -> 识别 Trap::Exception(Exception::UserEnvCall)
  -> cx.x[10] = syscall(cx.x[17], [cx.x[10], cx.x[11], cx.x[12]])
     -> syscall(169, args)           [syscall/mod.rs]
        -> sys_get_time(args[0] as *mut TimeVal, args[1])
```

第四阶段，也就是当前这行代码所在的位置，取得用户页表：

```text
current_user_token()                                    [task/mod.rs]
  -> TASK_MANAGER.get_current_token()
     -> inner.tasks[inner.current_task].get_user_token() [task/task.rs]
        -> self.memory_set.token()                       [mm/memory_set.rs]
           -> self.page_table.token()                    [mm/page_table.rs]
              -> 8usize << 60 | root_ppn.0
```

第五阶段，用 token 翻译用户地址：

```text
PageTable::from_token(token)
  -> root_ppn = token & ((1 << 44) - 1)
page_table.translate(vpn)
  -> find_pte() 逐级读取三级页表，返回末级页表项
translated_byte_buffer(token, ptr, len)
  -> 内部再次 from_token，再 translate 得到 ppn
  -> ppn.get_bytes_array()，得到可直接写入的物理内存切片
```

同一条 token 在内核中还有另一处用途。`trap_return()` 把 `current_user_token()` 作为 `a1` 传给 `__restore`，由 `__restore` 执行 `csrw satp, a1` 切回用户地址空间。任务创建时写入 `TrapContext` 的是内核页表 token（`KERNEL_SPACE.token()`），`trap_return()` 传入的是用户页表 token，两者方向相反，构成一次完整的地址空间往返。

#### 5.1.3 简要分析

第一，为什么必须用 token 重新构造页表对象，而不是直接解引用用户指针。此刻 CPU 正在使用内核页表，`_ts` 只是一个用户虚拟地址，在内核页表中未必有对应映射，直接 `*_ts = ...` 会按内核的映射关系解释这个地址。token 让内核在不切换 `satp` 的前提下，按用户页表的映射关系查找地址。

第二，为什么不用切换 `satp` 的方式。给系统调用临时切换地址空间需要保存现场、刷新 TLB，代价高且容易出错。本仓库采用的做法是让内核地址空间本身也包含物理内存的直接映射（`KERNEL_SPACE` 以恒等映射覆盖物理内存），于是内核只要用 token 查出物理页号，就能通过内核自己的映射访问那一页。

第三，token 不含 ASID，也不保证页表内容稳定。`from_token()` 只解析低 44 位，因此 `ASID` 字段在本仓库中没有参与翻译。这也意味着 token 只是定位根页表，权限和映射内容仍需在 `translate()` 之后单独检查，这正是 5.2 节检查 `V`、`U`、`W` 三个标志的原因。

第四，这个方案依赖单核与稳定的页表状态。`PageTable::from_token()` 返回的对象借用了外层页表的物理页，一旦有其他执行流在同一时刻修改该任务的页表，这里读到的中间状态就可能失效。当前实验环境是单核、系统调用期间无人并发修改地址空间，因此这个前提成立。

### 5.2 校验有效位与权限

```rust
let mut address = start;
while address < end {
    let va = VirtAddr::from(address);
    match page_table.translate(va.floor()) {
        Some(pte) if pte.flags().contains(PTEFlags::V | PTEFlags::U | PTEFlags::W) => {}
        _ => return -1,
    }
    address += (crate::config::PAGE_SIZE - va.page_offset()).min(end - address);
}
```

`va.floor()` 返回该地址所在的虚拟页号，`translate()` 查找对应页表项。所需标志如下：

| 标志 | 含义 | 检查原因 |
| --- | --- | --- |
| `V` | 页表项有效 | 防止访问尚未映射或已经取消映射的页 |
| `U` | 用户页 | 防止将内核专用映射作为用户输出缓冲区 |
| `W` | 可写 | 输出参数需要写入，不能接受只读页 |

`contains(V | U | W)` 要求三个标志同时存在。

本仓库的 `translate()` 可能返回一个无效的末级页表项，因此仅判断 `Some(pte)` 不够，仍需要检查 `V`。

内核接下来通过物理内存映射执行复制，用户页的写权限不会自动替这个复制过程完成检查，因此必须显式检查 `W` 和 `U`。

### 5.3 循环如何前进

```rust
(PAGE_SIZE - va.page_offset()).min(end - address)
```

这一表达式在两个长度中取较小值：

1. 当前页从 `address` 开始还剩多少字节。
2. 整个缓冲区从 `address` 开始还剩多少字节。

如果区间跨页，`address` 前进到下一页起点；如果剩余数据都在当前页，`address` 直接前进到 `end`，循环结束。

这种写法按页检查，不需要逐字节查询页表。虽然 `TimeVal` 只有 16 字节，写法仍能处理字段自身跨页或整个结构体跨页的情况。

### 5.4 为什么先全部检查再写

如果第一段可写、第二段没有映射，函数应在复制前返回 `-1`。先遍历全部目标页，可避免因为后续页检查失败而留下半个时间结构体。

这是一种当前单核、系统调用执行期间地址空间保持稳定条件下的预检查方案，并不等同于支持任意并发页表修改的原子事务。后续引入多线程或并发取消映射时，需要额外的地址空间同步。

## 6. 读取时钟并拆分时间

```rust
let us = get_time_us();
```

`get_time_us()` 定义在 `os/src/timer.rs`：

```rust
pub fn get_time_us() -> usize {
    time::read() * MICRO_PER_SEC / CLOCK_FREQ
}
```

`time::read()` 读取 RISC-V 的 `time` 计数器。按配置中的每秒计数频率换算后，得到微秒数。它表示平台计时基准上的时间，不是 Unix 时间戳，也不是该任务实际占用 CPU 的时间。

当前仓库中 `CLOCK_FREQ = 12500000`。按这个配置，若时钟计数为 `29290112`，计算为：

```text
29290112 × 1000000 ÷ 12500000
= 2343208 微秒（整数除法取整）

sec  = 2343208 ÷ 1000000 = 2
usec = 2343208 % 1000000 = 343208
```

最终结构体表示 `2` 秒加 `343208` 微秒。

换算结果是否符合实际时间，取决于 `CLOCK_FREQ` 是否与运行平台的 timebase frequency 一致。本次函数实现沿用仓库配置，没有校准硬件频率。另外，现有 `get_time_us()` 的“先乘后除”在很长运行时间后可能出现整数溢出；本次实现没有修改这一辅助函数。

## 7. 将字段编码为字节

```rust
let mut bytes = [0u8; core::mem::size_of::<TimeVal>()];
let word_size = core::mem::size_of::<usize>();
bytes[..word_size].copy_from_slice(&(us / 1_000_000).to_ne_bytes());
bytes[word_size..].copy_from_slice(&(us % 1_000_000).to_ne_bytes());
```

在内核栈上建立一个 16 字节数组，前 8 字节写入秒数，后 8 字节写入微秒数。

`to_ne_bytes()` 使用目标平台的本机字节序。当前 RISC-V 目标为小端序，因此较低有效字节位于较低地址。用户程序与内核运行在同一目标平台，字段解释保持一致。

例如：

```text
sec = 2

前 8 字节：
02 00 00 00 00 00 00 00
```

字节数组方案不需要从用户指针构造 `&mut TimeVal`，也不要求用户地址满足结构体对齐条件。复制边界由页决定，字段可以被分成多段写入。

这段编码依赖当前 `TimeVal` 恰好由两个 `usize` 字段组成。如果以后改变字段类型、顺序或结构体布局，必须同步修改编码逻辑。

## 8. 跨页复制

```rust
let mut offset = 0;
for buffer in translated_byte_buffer(token, _ts as *const u8, len) {
    let next = offset + buffer.len();
    buffer.copy_from_slice(&bytes[offset..next]);
    offset = next;
}
```

`translated_byte_buffer()` 定义在 `os/src/mm/page_table.rs`。它按用户虚拟地址的顺序查找各页，返回对应物理内存中的可变字节切片：

```rust
Vec<&'static mut [u8]>
```

这些切片直接指向目标物理页，并不是用户数据的副本。调用 `copy_from_slice()` 时，数据才从内核的 `bytes` 写入用户页对应的物理内存。

该辅助函数的参数类型虽然是 `*const u8`，返回的却是可变切片，这是本仓库既有接口设计。可写性由调用者预先检查，不能根据参数中的 `const` 推断它只能读取。

`offset` 表示源字节数组中已经复制的长度。每次取得一个目标切片，就从源数组取出同样长度的区间复制，最后所有切片长度之和为 `len`。

### 8.1 两个字段分别位于不同页的例子

假设页面大小为 `0x1000`，用户传入：

```text
start = 0x1ff8
len   = 16
end   = 0x2008
```

预检查过程如下：

| 次数 | `address` | 页内偏移 | 本次跨度 | 下一地址 |
| --- | --- | --- | --- | --- |
| 第一次 | `0x1ff8` | `0xff8` | `min(8, 16) = 8` | `0x2000` |
| 第二次 | `0x2000` | `0` | `min(4096, 8) = 8` | `0x2008` |

假设两页分别映射到物理地址 `0x81000000`、`0x82000000`：

```text
用户区间 [0x1ff8, 0x2000)
    -> 物理区间 [0x81000ff8, 0x81001000)
    -> 写入 bytes[0..8]，即 sec

用户区间 [0x2000, 0x2008)
    -> 物理区间 [0x82000000, 0x82000008)
    -> 写入 bytes[8..16]，即 usec
```

两个物理区间不连续，但用户地址中的结构体仍连续。页表翻译保证了每段数据写入正确的物理位置。

### 8.2 一个字段自身跨页的例子

若 `start = 0x1ffc`，第一段只有 4 字节，第二段有 12 字节：

```text
第一段：bytes[0..4]
第二段：bytes[4..16]
```

此时 `sec` 字段的前半部分在第一页，后半部分在第二页，函数仍按字节顺序完成复制。`0x1ffc` 不满足通常的 `TimeVal` 对齐要求，这个例子用于说明内核复制算法能处理这种原始地址；普通 Rust 用户代码通过 `&mut TimeVal` 传参时仍应遵守类型的对齐规则。

## 9. 用户程序如何取得结果

本仓库用户库的调用链是：

```text
用户 get_time()
    -> sys_get_time(&mut time, 0)
    -> ecall，系统调用编号 169
    -> 内核 syscall()
    -> 内核 sys_get_time()
    -> 将时间写入用户页，返回状态码
    -> 用户 get_time() 读取字段并换算为毫秒
```

用户库当前实现为：

```rust
pub fn get_time() -> isize {
    let mut time = TimeVal::new();
    match sys_get_time(&mut time, 0) {
        0 => ((time.sec & 0xffff) * 1000 + time.usec / 1000) as isize,
        _ => -1,
    }
}
```

因此，如果内核写入 `sec = 2`、`usec = 343208`，用户得到：

```text
2 × 1000 + 343208 ÷ 1000 = 2343 毫秒
```

用户库在换算时对秒数使用了 `& 0xffff`，这是既有用户库行为，内核没有把秒数截成 16 位。微秒到毫秒的整数除法会舍去不足一毫秒的部分。

## 10. 错误情况与实现边界

| 情况 | 当前行为 |
| --- | --- |
| 地址为零 | 返回 `-1` |
| 地址加结构体长度发生溢出 | 返回 `-1` |
| 缓冲区超出低半区地址范围 | 返回 `-1` |
| 任意目标页未映射或末级项无效 | 返回 `-1` |
| 任意目标页不是用户页或不可写 | 返回 `-1` |
| 第二页检查失败 | 在复制前返回，不写入第一段 |
| 所有检查成功 | 写入时间，返回 `0` |

本实现使用仓库已有的三级、4 KiB 页映射接口，没有增加大页翻译能力。`translated_byte_buffer()` 内部使用 `unwrap()`，并不自行完整校验地址和权限；当前调用通过前置检查避免已知的无效目标页问题，但该辅助函数不是通用的容错用户内存复制接口。

此外，内核目前依赖单核实验环境和稳定的页表状态。如果未来存在其他执行流并发修改当前任务映射，仅靠“检查后复制”不能保证安全，需要增加同步或具有故障恢复能力的用户内存复制机制。

## 11. 验证状态与建议测例

当前代码已经通过 `cargo check --release`。编译检查确认类型、语法和接口调用能够通过构建，但不能代替运行时验证。第四章完整测试及下面列出的专项跨页测例尚未在本次实现后执行。

建议分别验证：

1. 正常栈上 `TimeVal`：返回 `0`，微秒字段小于 `1000000`。
2. 页末尾的结构体：相邻虚拟页均可写、物理页不连续时，两个字段仍正确。
3. 第二页缺失或只读：返回 `-1`，第一段的预置内容不改变。
4. 空地址、越界地址及加法溢出地址：返回 `-1`，内核不因访问目标缓冲区崩溃。
5. 连续时间查询：短时间内结果不倒退，并结合平台时钟频率核对时间尺度。

## 12. 小结

`sys_get_time` 的执行可以概括为：检查地址范围，遍历用户页表验证写权限，读取一次时钟并编码两个字段，最后按页翻译后的物理切片复制结果。

跨页支持来自逐段地址翻译和字节复制；权限检查保护输出目标；分发返回的 `0` 或 `-1` 表示系统调用状态。掌握这三点后，同样的思路也可以用于其他需要向用户空间写回结构体的系统调用。
