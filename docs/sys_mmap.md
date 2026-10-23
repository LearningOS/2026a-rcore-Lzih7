# 第四章 `sys_mmap` 实现详解

## 1. 功能与调用约定

`sys_mmap` 在用户程序指定的虚拟地址处新建立一段可用的内存。它与 `sys_get_time` 的差别在于方向：`sys_get_time` 是访问一块已经存在的用户内存，而 mmap 的目标地址通常还没有映射，它的工作是分配物理页、填写页表项，并把这段区间登记下来，供之后的 `munmap` 或任务退出时回收。

接口定义在 `os/src/syscall/process.rs`：

```rust
pub fn sys_mmap(start: usize, len: usize, port: usize) -> isize
```

| 参数或返回值 | 含义 |
| --- | --- |
| `start` | 用户虚拟地址，必须是页大小的整数倍 |
| `len` | 区间长度，不要求按页对齐 |
| `port` | 权限位：第 0 位可读、第 1 位可写、第 2 位可执行 |
| 返回 `0` | 映射建立成功 |
| 返回 `-1` | 参数非法，或目标区间已经被占用 |

形式参数名 `port` 是仓库预留的写法，语义与用户库中的 `prot` 相同。用户库封装如下，系统调用编号为 222：

```rust
pub fn mmap(start: usize, len: usize, prot: usize) -> isize {
    sys_mmap(start, len, prot)
}
```

## 2. 权限位与标志的对应关系

用户传入的 `port` 只有低三位有意义，它最终要转成页表项里的标志。仓库中两组标志的位定义完全相同：

| 标志 | `MapPermission` | `PTEFlags` | 数值 |
| --- | --- | --- | --- |
| 可读 | `R = 1 << 1` | `R = 1 << 1` | 2 |
| 可写 | `W = 1 << 2` | `W = 1 << 2` | 4 |
| 可执行 | `X = 1 << 3` | `X = 1 << 3` | 8 |
| 用户可访问 | `U = 1 << 4` | `U = 1 << 4` | 16 |

因为数值一致，`MapArea::map_one()` 里可以直接做 `PTEFlags::from_bits(self.map_perm.bits).unwrap()`，不需要额外的转换表。

`port` 的取值与典型测例：

| `port` | 二进制 | 含义 | 相关测例 |
| --- | --- | --- | --- |
| 1 | `0b001` | 只读 | ch4_mmap1、ch4_trace1 |
| 2 | `0b010` | 只写 | ch4_mmap2 |
| 3 | `0b011` | 可读可写 | ch4_mmap0、ch4_mmap3、ch4_unmap |
| 0 | `0b000` | 非法 | ch4_mmap3 |
| 11 | `0b1011` | 非法，含第 3 位 | ch4_mmap3 |

## 3. 为什么不能沿用访问已有内存的写法

`sys_get_time` 用 `translate()` 查目标地址对应的物理页，这套写法用在 mmap 上必然失败：mmap 的目标页本来就还没建立映射，查询结果只会是无效项，于是函数会立刻返回 `-1`。

两者的基本动作不同：

```text
sys_get_time：translate()   查询已有映射，再写物理页
sys_mmap    ：map()         新建映射，为地址分配物理页
```

页表的修改只能通过 `MemorySet` 完成，因为页帧的生命周期由它管理。所以 `sys_mmap` 本身只负责参数语义，真正的映射工作交给 `MemorySet`。

## 4. 参数校验

### 4.1 三条拒绝条件

```rust
let page_size = crate::config::PAGE_SIZE;
if start % page_size != 0 || len == 0 || port == 0 || port & !0b111 != 0 {
    return -1;
}
```

- `start % page_size != 0`：起始地址必须落在页边界上。
- `len == 0`：长度为 0 的映射没有意义。
- `port == 0 || port & !0b111 != 0`：权限不能为空，也不能出现第 3 位以上的位。

`crate::config::PAGE_SIZE` 为 `0x1000`，即 4096 字节。

### 4.2 为什么 start 必须对齐而 len 不必

`MapArea::new()` 内部对起点取 `floor()`、对终点取 `ceil()`，终点不在页边界上时会被自动向上取整。也就是说 `len = 4097` 与 `len = 4096` 得到的是同一个页区间。

仓库里两条测例正好从两个方向固定了这个语义：

```text
ch4_mmap3: mmap(start + len + 1, len, prot)  -> -1   起始地址未对齐
ch4_unmap: mmap(start - len, len + 1, prot)  ->  0   长度未对齐，但可以取整
```

当前实现自己算取整后的 `end`，而不是交给 `MapArea::new()` 的 `ceil()`，原因是后面的重叠检查循环也要按取整后的区间逐页走，两处使用同一个 `end` 才能保持一致。

### 4.3 溢出检查

```rust
let end = match start.checked_add(len).and_then(|end| end.checked_add(page_size - 1)) {
    Some(end) => end / page_size * page_size,
    None => return -1,
};
```

区间端点分两步计算：先算 `start + len`，再补上 `page_size - 1` 以便向上取整。两步都用 `checked_add()`，因为用户传入的地址和长度都是任意数值。

例如：

```text
start = 0xFFFF_FFFF_FFFF_F000
len   = 0x2000

start + len 溢出  ->  checked_add() 返回 None  ->  返回 -1
```

如果先算 `start + len` 就溢出，函数在取整之前就返回，不会因为加法回绕得到一个偏小的 `end`。

## 5. 重叠检查必须在插入之前

`PageTable::map()` 的开头有一句断言：

```rust
assert!(!pte.is_valid(), "vpn {:?} is mapped before mapping", vpn);
```

也就是说，对一个已经映射过的虚拟页再次调用 `map()` 会让内核 panic。而测例要求这种情况返回 `-1`，所以检查必须在调用 `insert_framed_area` 之前完成：

```rust
let mut address = start;
while address < end {
    if self
        .memory_set
        .translate(VirtAddr::from(address).floor())
        .map_or(false, |pte| pte.is_valid())
    {
        return -1;
    }
    address += page_size;
}
```

这里有一个容易踩的坑：`translate()` 会沿三级页表一直走到末级并返回那个页表项，即使这一项是无效的，它同样返回 `Some`。所以判据是 `pte.is_valid()`，不能只判断 `is_some()`。这一点和 `sys_get_time` 里必须显式检查 `V` 标志是同一种情况。

循环按页步进而不是按字节，因为 `start` 已对齐、`end` 已取整，每个页只需要查询一次。

## 6. 权限拼装

```rust
let mut permission = MapPermission::U;
if port & 0b1 != 0 {
    permission |= MapPermission::R;
}
if port & 0b10 != 0 {
    permission |= MapPermission::W;
}
if port & 0b100 != 0 {
    permission |= MapPermission::X;
}
```

三个要点：

第一，`U` 必须无条件加上。页表项里的 `U` 位决定这条映射能否在用户态访问；不带 `U` 的映射属于 S 态，用户程序一访问就触发页错误。

第二，`port` 的位直接对应 `R`、`W`、`X`，不额外补齐。例如 `port = 1` 得到 `U|R`，写这一页会触发页错误，这正是 ch4_mmap1 期望程序被杀死的依据。

第三，`port = 2` 得到的页表项是 `R = 0, W = 1`。RISC-V 规定叶子页表项处于这种组合时是保留编码，任何访问都会产生页错误。也就是说，"只写页"在硬件上其实完全不可用。ch4_mmap2 正是利用这一点，验证内核没有擅自把 `R` 补上：

```text
mmap(0x10000000, 4096, 2)  -> 0
读取该页                    -> 页错误，程序被杀死
```

## 7. 物理页在哪里分配

映射工作复用了 `MemorySet::insert_framed_area()`：

```rust
pub fn insert_framed_area(
    &mut self,
    start_va: VirtAddr,
    end_va: VirtAddr,
    permission: MapPermission,
) {
    self.push(
        MapArea::new(start_va, end_va, MapType::Framed, permission),
        None,
    );
}
```

完整的分配路径：

```text
MemorySet::insert_framed_area
  -> MemorySet::push
     -> MapArea::map
        -> MapArea::map_one
           -> frame_alloc()                  分配一个物理页帧
           -> data_frames.insert(vpn, frame) 页帧由 MapArea 持有
           -> page_table.map(vpn, ppn, pte_flags)
     -> areas.push(map_area)                 登记区间
```

两点说明：

第一，不要自己 `frame_alloc()` 再手动 `map()`。物理页帧必须有人持有，`frame_alloc()` 返回的 `FrameTracker` 一旦析构就会把页交还分配器，映射会指向已经回收的页。走 `insert_framed_area` 时，页帧存进 `MapArea` 的 `data_frames`，生命周期与这段映射绑定；任务退出时 `MemorySet` 被析构，页帧随之释放。

第二，新分配的页是干净的。`frame_alloc()` 在返回前会经过 `FrameTracker::new()`，其中把整页字节写为 0。所以 mmap 得到的页初始内容是确定的零，而不是上一任使用者留下的数据。

## 8. 调用链

从用户程序到页表项被写入，链路如下：

```text
用户 mmap(start, len, prot)
  -> ecall，a7 = 222
trap_handler
  -> syscall(222, [start, len, prot])
     -> sys_mmap                            [syscall/process.rs]
        -> task::mmap_current_task
           -> TaskManager::mmap_current_task  [task/mod.rs]
              -> inner.current_task
              -> TaskControlBlock::mmap       [task/task.rs]
                 -> 参数校验
                 -> 重叠检查
                 -> 权限拼装
                 -> MemorySet::insert_framed_area
```

`sys_mmap` 只留了一行转发，因为内核里改页表的行为都需要"当前任务"这个前提，而任务管理器不在 `syscall` 模块的可见范围内。`TASK_MANAGER` 是 `task` 模块私有的 `lazy_static`，别的模块无法直接访问，所以由 `task` 模块提供 `mmap_current_task()` 作为入口。

取 `MemorySet` 的三行是：

```rust
let mut inner = self.inner.exclusive_access();
let cur = inner.current_task;
inner.tasks[cur].mmap(start, len, port)
```

`TaskControlBlock::memory_set` 是公开字段，因此 `TaskManager` 只需要定位当前任务，参数语义和页表操作都留在 `TaskControlBlock::mmap()` 里。

`exclusive_access()` 返回的 `RefMut` 在方法返回时释放，所以 mmap 的全部逻辑都在持有这次借用期间完成。如果写成返回 `&'static mut MemorySet` 的辅助函数，就会把借用检查绕过去，之后别处再访问 `TASK_MANAGER` 时可能出现同一份数据的两个可变引用。

## 9. 具体例子

### 9.1 成功建立一段可读写映射

调用 `mmap(0x10000000, 4096, 3)`，执行过程如下：

| 步骤 | 结果 |
| --- | --- |
| 对齐与取值检查 | `0x10000000 % 0x1000 == 0`，`len` 与 `port` 合法 |
| 计算区间 | `end = (0x10000000 + 0x1000 + 0xfff) / 0x1000 * 0x1000 = 0x10001000` |
| 重叠检查 | 页 `0x10000` 的 `translate()` 无效，通过 |
| 权限拼装 | `port = 3` 得到 `U \| R \| W` |
| 建立映射 | `insert_framed_area(0x10000000, 0x10001000, U\|R\|W)` |

假设分配到的物理页号是 `0x80700`，写进页表项的编码是：

```text
PPN = 0x80700, flags = V | R | W | U
```

用户态可以正常读写这一页，ch4_mmap0 的整页写入再读回即通过。

### 9.2 ch4_mmap3 的四条失败用例

| 调用 | 结果 | 命中哪条规则 |
| --- | --- | --- |
| `mmap(0x10000000, 4096, 3)` | `0` | 正常建立 |
| `mmap(0x0FFFF000, 4097, 3)` | `-1` | 取整后覆盖到已映射的 `0x10000000` 页 |
| `mmap(0x10001001, 4096, 3)` | `-1` | `start` 未页对齐 |
| `mmap(0x10001000, 4096, 0)` | `-1` | `port` 为 0 |
| `mmap(0x10001000, 4096, 11)` | `-1` | `port` 含第 3 位 |

第二条值得单独看一遍。`0x0FFFF000` 加上 4097 得到 `0x10000001`，向上取整后是 `0x10001000`，覆盖了页 `0x0FFFF000` 和 `0x10000000`；后一页已经由第一条调用映射，因此返回 `-1`。长度 4097 本身不是问题，冲突才是。

### 9.3 与 ch4_unmap 的对照

ch4_unmap 里同样出现 4097 这个长度，期望却是成功：

```text
mmap(0x10000000, 4096, 3)    -> 0
mmap(0x10001000, 8192, 3)    -> 0
munmap(0x10000000, 4096)     -> 0        先释放掉这一页
mmap(0x0FFFF000, 4097, 3)    -> 0        取整后的两页此时都空闲
```

同一个调用在两条测例里结果相反，说明判据从来不是长度是否对齐，而是目标区间是否与已有映射重叠。这也意味着 ch4_unmap 依赖 `munmap`，只实现 mmap 时它不会通过。

### 9.4 只读页与 trace 的配合

ch4_trace1 用 `prot = 1` 映射一段内存，然后依次做三件事：

```text
trace_read(start)        -> 某个字节值      页表项有 R
trace_write(start, 0)    -> -1              页表项没有 W
munmap(start, len) 之后
trace_read(start)        -> None            页表项已无效
```

`sys_trace` 的请求 0 要求页表项含 `R`、请求 1 要求含 `W`，因此它能准确区分只读页与可写页，也能识别出已经被取消映射的地址。

## 10. 错误情况与实现边界

| 情况 | 当前行为 |
| --- | --- |
| `start` 不是页大小的整数倍 | 返回 `-1` |
| `len` 为 0 | 返回 `-1` |
| `port` 为 0 或含第 3 位以上的位 | 返回 `-1` |
| `start + len` 溢出 | 返回 `-1` |
| 目标区间存在已映射的页 | 返回 `-1`，不建立任何映射 |
| 参数合法且区间空闲 | 建立映射，返回 `0` |

需要说明的实现边界：

- 取消映射要求区间完全吻合。`munmap` 只处理起始 VPN 与页数都一致的那一个区间，子区间或跨区间的释放会返回 `-1`，不会被部分执行。
- 不是文件映射。接口没有文件描述符和偏移量参数，页内容是分配后的零页，不涉及从文件读入数据。
- 没有按需分配。调用 mmap 时立刻分配并映射全部物理页，不存在惰性分配或写时复制。
- 不挑选地址。映射地址完全由用户给出的 `start` 决定，内核不回填空闲地址。
- 不做地址区间的额外限制。校验只有页对齐和重叠两项，没有专门检查 `start` 是否落在用户程序预留的地址范围内，因此把用户数据映射到这些地址以外的低地址用户空间也是允许的，只要不与他人冲突。
- 依赖单核。`exclusive_access()` 保证同一时刻只有一处访问任务管理器和该任务的 `MemorySet`，这不是通用的并发保护。

## 11. 取消映射

`munmap` 是 mmap 的逆操作。`MemorySet` 上原本只有插入接口，因此需要补一个删除入口：

```rust
pub fn remove_area_with_start_vpn(&mut self, start_vpn: VirtPageNum, pages: usize) -> bool
```

它的工作分三段：按起始虚拟页号在 `areas` 中找到下标，核对页数是否与请求一致，然后清理这段映射。

### 11.1 完整调用链

从用户调用到物理页真正归还，链路如下：

```text
用户 munmap(start, len)
  -> ecall，a7 = 215
trap_handler
  -> syscall(215, [start, len])
     -> sys_munmap                                 [syscall/process.rs]
        -> task::munmap_current_task
           -> TaskManager::munmap_current_task     [task/mod.rs]
              -> inner.current_task
              -> TaskControlBlock::munmap          [task/task.rs]
                 -> 校验页对齐、len 非 0
                 -> pages = ceil(len / PAGE_SIZE)
                 -> MemorySet::remove_area_with_start_vpn(vpn, pages)
                    -> areas.iter().position(...)  定位下标 idx
                    -> 核对区间页数
                    -> MapArea::unmap(&mut page_table)
                       -> 对 vpn_range 中每个 vpn 调用 unmap_one
                          -> data_frames.remove(&vpn)
                             -> FrameTracker 析构
                                -> frame_dealloc(ppn)
                                   -> 页号回到分配器的 recycled 列表
                          -> PageTable::unmap(vpn)
                             -> *pte = PageTableEntry::empty()
                    -> areas.remove(idx)           区间记录被移除
```

`sys_munmap` 和 `sys_mmap` 一样只做转发，真正的页表操作在任务这一层完成，原因也相同：改页表必须先确定"当前任务"。

### 11.2 关键两行在做什么

```rust
self.areas[idx].unmap(&mut self.page_table);
self.areas.remove(idx);
```

第一行 `self.areas[idx]` 用 `IndexMut` 取出第 `idx` 个 `MapArea` 的可变引用，随后调用 `MapArea::unmap()`。这个方法按区间自己的 `vpn_range` 逐页调用 `unmap_one()`，每一页做两件事：

```rust
pub fn unmap_one(&mut self, page_table: &mut PageTable, vpn: VirtPageNum) {
    if self.map_type == MapType::Framed {
        self.data_frames.remove(&vpn);
    }
    page_table.unmap(vpn);
}
```

- `data_frames.remove(&vpn)` 把这个页的 `FrameTracker` 从映射表中摘出来。返回值是 `Option<FrameTracker>`，语句结束时就被丢弃，于是触发 `FrameTracker` 的析构，最终走到 `frame_dealloc(ppn)`，页号被放回分配器的回收列表。物理页在这里被真正释放，不需要额外调用任何释放函数。
- `page_table.unmap(vpn)` 先断言这一项当前有效，再把它写成 `PageTableEntry::empty()`，也就是 `bits = 0`，`V` 位随之清零。

这里有一个和 mmap 相呼应的细节：清空之后 `translate()` 依然会返回 `Some(pte)`，只是这个项的 `is_valid()` 为假。mmap 的重叠检查用 `is_valid()` 判断，trace 读取要求 `V | U` 同时成立，两边都不会把它当成有效映射。

第二行 `self.areas.remove(idx)` 把这段区间从 `areas` 中删掉。此时该 `MapArea` 的 `data_frames` 已经被清空，所以它自身的析构不会再释放任何页帧，不存在重复释放的问题。这一步同时也让区间记录与实际页表保持一致，后面的 `mmap` 或 `sbrk` 才能正确判断哪些地址空闲。

这段代码里两处借用是并存的：`self.areas` 的可变索引和 `self.page_table` 的可变借用分别落在两个不同的字段上，借用检查器允许同时存在；`remove(idx)` 放在最后，是因为它需要独占 `self.areas`。

### 11.3 两步缺一不可

只做其中一步都会出问题：

- 只清页表项、不删区间记录。`areas` 里会留下一个空壳 `MapArea`，后续按起始 VPN 查找区间的逻辑（例如 `sbrk` 用的 `append_to`）可能命中它，从而把已经释放的地址当成仍在使用。
- 只删区间记录、不清页表项。页表项还指向那块物理页，用户程序仍然可以访问；更严重的是页帧已经归还给分配器，随时可能被分配给别的用途，于是这段地址会读到别人的数据，或者写坏别人的页。

所以两行必须成对出现：先清页表并归还页帧，再移除区间记录。

顺序上还有一点值得注意：`remove_area_with_start_vpn` 把页数核对放在所有修改之前，核对不通过就直接返回 `false`，此时没有执行任何 `unmap`、也没有移除任何区间。这样失败时不会留下"释放了一半"的状态。

### 11.4 页数为什么必须精确匹配

这一点由 ch4_unmap2 固定：

```text
mmap(0x10000000, 4096, 3)    -> 0     建立一个 1 页的区间
munmap(0x10000000, 4097)     -> -1    请求取整后是 2 页，超出这个区间
munmap(0x10000001, 4095)     -> -1    起始地址未页对齐
```

如果只要求"首页已映射"就动手，第一条会把范围里并不存在的第二页也算进去，还可能悄悄删掉整段区间，因此这里选择返回失败而不是部分生效。

### 11.5 释放后地址可以再次使用

ch4_unmap 验证了区间被移除后地址重新变为空闲：

```text
mmap(0x10000000, 4096, 3)    -> 0
mmap(0x10001000, 8192, 3)    -> 0
munmap(0x10000000, 4096)     -> 0     这一页恢复空闲
mmap(0x0FFFF000, 4097, 3)    -> 0     取整后的两页此时都不冲突
```

最后一条能成功，说明 `areas` 与页表都被同步更新了：页表项已清空，重叠检查查不到冲突；区间记录已移除，后续查找也不会误命中。

取消映射本身不需要显式刷新 TLB。返回用户态时 `__restore` 会执行 `csrw satp` 并接一条 `sfence.vma`，地址翻译缓存随之失效，因此用户不会读到已经被清除的旧翻译。

## 12. 验证状态与建议测例

当前实现已通过 `cargo check --release`。编译检查只确认类型、语法和接口调用成立，不能代替运行验证，第四章完整测试尚未执行。

按测例划分的预期结果：

| 测例 | 预期 | 说明 |
| --- | --- | --- |
| ch4_mmap0 | 通过 | 可读写页的整页写入与读回 |
| ch4_mmap3 | 通过 | 四条失败用例都由参数校验或重叠检查覆盖 |
| ch4_mmap1 | 通过 | 只读页写入触发页错误，程序被杀死，不输出 error |
| ch4_mmap2 | 通过 | 只写页读取触发页错误，程序被杀死，不输出 error |
| ch4_unmap | 通过 | 释放后地址可再次映射 |
| ch4_unmap2 | 通过 | 未对齐与超出区间两种失败都被拦下 |
| ch4_trace1 | 通过 | 只读页可供 trace 读取，释放后 trace 读取得到 `None` |

建议按下面顺序自行验证：

1. `mmap` 后整页写入再读回，确认权限与地址翻译正确。
2. 同一区间重复 `mmap`，确认返回 `-1` 而不是让内核 panic。
3. 未对齐的 `start`、为 0 的 `len`、越权的 `port`，确认都返回 `-1`。
4. 用 `prot = 1` 和 `prot = 2` 分别映射一页，确认访问受限的方向与预期一致。
5. 在 mmap 得到的页上读取初始内容，确认是全零。
6. `munmap` 之后再访问原地址，确认变为地址错误；重复 `munmap` 同一区间应返回 `-1`。

## 13. 小结

`sys_mmap` 的执行可以概括为四步：校验参数，把长度按页向上取整得到目标区间，逐页确认区间内没有已映射的页，最后拼装权限并交给 `insert_framed_area` 分配页帧、写入页表项。

三个最容易出错的点：`start` 必须对齐而 `len` 不必；重叠检查必须放在插入之前，否则 `PageTable::map()` 的断言会让内核 panic；拼装权限时必须带上 `U`。

物理页的分配与回收都交给 `MemorySet` 统一管理，`sys_mmap` 与 `sys_munmap` 本身不直接接触页帧。两个系统调用的分工因此很清晰：mmap 负责申请区间并让 `MapArea` 持有页帧，munmap 负责找到区间、清掉页表项并把区间移除，页帧随 `FrameTracker` 析构回到分配器。
