# Lab3：进程管理与优先级调度

## 1. 实验要求

本实验对应 rCore 第五章。与前两章预先装入固定数量任务的方式相比，本章允许进程在运行期间创建子进程、替换程序，并等待子进程退出。实验需要在已有进程管理框架上完成 `sys_spawn`，使调用者创建一个执行指定程序的子进程；同时完成 `sys_set_priority`，把原有的先进先出调度改为带优先级的 stride 调度。

本章的主要目标如下：

| 功能 | 接口或配置 | 要求 |
| --- | --- | --- |
| 创建进程 | `sys_spawn(path)`，系统调用编号 `400` | 创建执行指定程序的子进程，父进程继续运行；成功返回子进程 PID，程序不存在时返回 `-1` |
| 设置优先级 | `sys_set_priority(prio)`，系统调用编号 `140` | 修改当前进程的优先级；`prio <= 1` 返回 `-1`，合法值返回所设置的优先级 |
| 初始化调度状态 | 每个新进程的 `priority`、`stride` | 初始优先级为 `16`，初始 stride 为 `0` |
| 按优先级调度 | `pass = BIG_STRIDE / priority` | 每次从就绪进程中选择 stride 最小者，并为选中进程累加 pass |
| 前序功能迁移 | 时间查询、内存映射与取消映射 | 适配第五章的任务控制块结构，保持原有测例可运行 |

本次没有重新编写进程管理框架。PID 分配、内核栈、`fork`、`exec`、`waitpid`、上下文切换及 ELF 装载均由第五章脚手架提供。新增工作主要是连接这些机制，实现 spawn 语义，并增加调度器所需的数据和选择逻辑。

## 2. 方案设计

### 2.1 创建进程与替换程序分开

`fork` 创建一个独立的子进程，并复制父进程的用户地址空间；`exec` 不创建进程，而是替换被调用对象的程序。`spawn` 需要的是“父进程保留原程序，子进程执行目标程序”，因此当前实现采用：

```text
查找目标 ELF -> 创建子进程 -> 替换子进程程序 -> 加入就绪队列
```

这里复用的是 `TaskControlBlock::fork()` 与子进程上的 `exec()` 方法，不是在用户态简单调用 `fork()` 后由父子两条执行流分别判断返回值。`sys_spawn` 在一次系统调用内准备好子进程，子进程第一次运行时直接进入目标程序。

查找 ELF 必须放在创建子进程之前。目标程序不存在时直接返回 `-1`，避免已经分配 PID、内核栈和地址空间后再处理失败。加入就绪队列必须放在 `exec` 之后，保证调度器看到的是已经准备完整的子进程。

### 2.2 用两个上下文衔接子进程的第一次运行

创建完成并不意味着立即执行。子进程的第一次运行依赖两个不同的上下文：

| 上下文 | 保存内容 | 本次创建中的作用 |
| --- | --- | --- |
| `TaskContext` | 内核态的 `ra`、`sp`、被调用者保存寄存器 | 由 `fork` 初始化，使第一次切入子进程时进入 `trap_return`，并使用子进程内核栈 |
| `TrapContext` | 用户寄存器、`sepc`、`sstatus`及再次陷入内核所需的信息 | 由 `exec` 重建，使返回用户态时进入目标 ELF 的入口，并使用目标程序的用户栈 |

因此，`exec` 不需要调用 `__switch`。它改变的是将来恢复的执行现场；真正的执行流切换由 `run_tasks()` 和 `__switch` 完成，随后 `trap_return` 经 trampoline 的 `__restore` 返回用户态。

### 2.3 为每个进程独立保存调度状态

stride 表示进程累计获得调度的“长度”，priority 决定每次调度增加的步长。两者都属于进程的可变运行状态，因此放入 `TaskControlBlockInner`，不使用全局公共值。

本次取：

$$
\mathrm{BIG\_STRIDE}=2^{16}=65536,\qquad
\mathrm{pass}=\left\lfloor\frac{65536}{\mathrm{priority}}\right\rfloor
$$

例如，优先级为 `16` 时，pass 为 `4096`；优先级为 `8` 时，pass 为 `8192`。在两者持续就绪、每次获得相近运行时间的条件下，优先级较大的进程 stride 增长较慢，因此被选择的次数更多。

初始 stride 为零的两个进程 A、B，优先级分别为 `16` 和 `8`。假设相等时先选 A，前三次调度如下：

| 调度次数 | 选中进程 | A 的 stride | B 的 stride |
| --- | --- | --- | --- |
| 初始 | 无 | `0` | `0` |
| 第一次 | A | `4096` | `0` |
| 第二次 | B | `4096` | `8192` |
| 第三次 | A | `8192` | `8192` |

三次调度中 A 获得两次、B 获得一次，体现了 `16:8` 的优先级比例。实际实现使用整数除法，比例会存在取整误差；按调度次数分配也不等于对实际运行时间做精确记账。

### 2.4 保留既有调度与退出路径

就绪队列仍使用 `VecDeque<Arc<TaskControlBlock>>`，`add_task()` 仍将任务放到队尾。只修改 `fetch()` 的选择方式，不重新实现时间片中断、主动让出 CPU 或退出逻辑。

进程让出 CPU 后，既有路径将其状态设为 `Ready` 并重新入队；退出进程不会继续参与就绪队列选择。`run_tasks()` 取得任务后将其设为 `Running`，再切换上下文。因此，本次遍历的对象是就绪队列中的进程，而不是所有已创建的进程，也不包括等待回收的僵尸进程。

## 3. 文件修改与实现

### 3.1 `os/src/syscall/process.rs`：完成 `sys_spawn`

核心实现如下，省略日志输出：

```rust
let token = current_user_token();
let path = translated_str(token, path);
if let Some(data) = get_app_data_by_name(path.as_str()) {
    let task = current_task().unwrap();
    let new_task = task.fork();
    let new_pid = new_task.pid.0;
    new_task.exec(data);
    add_task(new_task);
    new_pid as isize
} else {
    -1
}
```

各步骤的职责如下。

1. 路径来自父进程的用户地址空间，先取得父进程的页表 token，再通过 `translated_str` 读出 NUL 结尾的程序名。不能直接按内核页表解引用用户指针。
2. `get_app_data_by_name` 从编译期嵌入内核的程序镜像中取得 ELF。本章尚未引入文件系统，路径在这里实际用于匹配程序名。
3. `task.fork()` 创建子进程，复用 PID 分配、内核栈分配、地址空间复制及父子关系登记。
4. `new_task.exec(data)` 替换子进程的地址空间，更新 `trap_cx_ppn`、`base_size`，并重建目标程序的 `TrapContext`。必须在子进程上调用，不能对当前父进程调用。
5. `add_task(new_task)` 把已经准备完成的子进程交给调度器，父进程得到子进程 PID 并继续运行。

与 `sys_fork` 不同，spawn 子进程不会返回父进程的 fork 调用点。`exec` 会重新初始化 TrapContext，因此不需要照搬 `sys_fork` 中的 `trap_cx.x[10] = 0`；新程序的用户寄存器现场由 ELF 装载后的初始化流程决定。

完整创建调用链如下：

```text
用户 spawn
  -> ecall，系统调用编号 400
  -> trap_handler -> syscall -> sys_spawn
  -> current_user_token -> translated_str
  -> get_app_data_by_name
  -> TaskControlBlock::fork
     -> MemorySet::from_existed_user
     -> pid_alloc / kstack_alloc
     -> 初始化子进程 TCB 与 TaskContext
     -> 登记 parent / children
  -> 子进程 TaskControlBlock::exec
     -> MemorySet::from_elf
     -> 替换地址空间并重建 TrapContext
  -> add_task -> 就绪队列
  -> 向父进程返回子进程 PID
```

子进程随后运行的调用链如下：

```text
run_tasks -> fetch_task -> TaskManager::fetch
  -> 设置当前进程与 Running 状态
  -> __switch（idle 上下文 -> 子进程 TaskContext）
  -> trap_return -> __restore
  -> 写入子进程 satp、刷新 TLB、恢复用户现场
  -> sret -> 目标程序入口
```

子进程退出与回收继续复用已有机制：退出时进入 `Zombie` 状态并记录退出码，父进程通过 `waitpid` 将其从 `children` 中移除；最后一个强引用释放后，相关资源随对象析构回收。spawn 本身不新增一套手动释放流程。

### 3.2 `os/src/task/task.rs`：增加优先级与 stride

新增两个常量：

```rust
pub const INIT_PRIORITY: usize = 16;
pub const BIG_STRIDE: usize = 1 << 16;
```

在 `TaskControlBlockInner` 中增加：

```rust
pub priority: usize,
pub stride: usize,
```

在 RV64 环境中，两个 `usize` 字段合计为每个进程增加 `16` 字节的数据。没有另外保存 pass 字段，而是在调度时由 priority 计算，避免修改优先级后还需要同步更新另一份状态。

`TaskControlBlock::new()` 与 `TaskControlBlock::fork()` 的初始化位置均补充：

```rust
priority: INIT_PRIORITY,
stride: 0,
```

两处都必须修改。`new()` 用于创建初始进程，`fork()` 用于动态创建子进程；只修改一处会使不同创建路径的初始化不一致。本次子进程使用默认优先级 `16`，不继承父进程已经调整过的 priority，也不继承父进程累计的 stride。

`exec()` 不修改 priority 和 stride。它替换程序，但保留进程身份及调度状态；spawn 子进程的这两个字段已经在 fork 创建阶段初始化，因此无需在 exec 中再次清零。

### 3.3 `os/src/syscall/process.rs`：完成 `sys_set_priority`

核心实现如下：

```rust
if prio <= 1 {
    return -1;
}
let task = current_task().unwrap();
let mut inner = task.inner_exclusive_access();
inner.priority = prio as usize;
prio
```

参数类型为 `isize`，先检查再转成 `usize`，避免负数转换为很大的无符号值。合法请求只修改当前进程的 priority，返回新值；非法请求直接返回 `-1`，不改变原优先级。

这里不清零 stride，也不立即切换任务。stride 保存已经累计的调度长度，设置优先级只改变下一次被调度时的增量。若每次设置优先级都把 stride 清零，进程可以通过反复调用接口获得额外的调度机会。

调用链为：

```text
用户 set_priority(prio)
  -> ecall，系统调用编号 140
  -> trap_handler -> syscall -> sys_set_priority
  -> 检查 prio > 1
  -> current_task -> inner_exclusive_access
  -> 修改当前进程 priority
  -> 返回 prio
```

系统调用分发层已提供 `140` 和 `400` 对应的分支，本次补齐处理函数，不需要新增一套分发入口。

### 3.4 `os/src/task/manager.rs`：将 FIFO 改为 stride 调度

原有队列按入队先后取任务。本次 `fetch()` 改为遍历队列，取得 stride 最小项的下标：

```rust
let idx = self
    .ready_queue
    .iter()
    .enumerate()
    .min_by_key(|(_, task)| task.inner_exclusive_access().stride)
    .map(|(idx, _)| idx)?;
```

队列为空时，`?` 使函数返回 `None`；非空时，根据遍历得到的下标移出对应任务：

```rust
let task = self.ready_queue.remove(idx).unwrap();
{
    let mut inner = task.inner_exclusive_access();
    inner.stride += BIG_STRIDE / inner.priority;
}
Some(task)
```

步长只累加到选中进程上，未选中的进程保持原值。累加发生在返回任务、实际切换之前，表示这次调度已经计入该进程。

内部借用放在独立作用域中，是为了在返回 `task` 之前释放 `RefMut`，避免仍持有内部可变借用时移动任务对象。`remove(idx)` 使用的下标来自同一次队列遍历，期间没有修改队列，因此该下标有效。

当前选择过程为线性扫描，每次调度的时间复杂度为 `O(n)`。与原 FIFO 相比增加了遍历成本，但不需要引入新的队列数据结构，适合本实验规模。

### 3.5 前序系统调用的迁移

第五章把任务管理改为 `Arc<TaskControlBlock>` 与 `TaskControlBlockInner`，当前进程通过 `current_task()` 获取，地址空间通过 `inner_exclusive_access().memory_set` 访问。前序功能据此迁移到当前结构：

| 文件 | 本次工作 | 原因 |
| --- | --- | --- |
| `os/src/syscall/process.rs` | 补齐 `sys_get_time` 的页表校验与跨页写回 | 时间结果仍写入当前进程的用户地址空间，不能直接解引用用户指针 |
| `os/src/syscall/process.rs` | 补齐 `sys_mmap` 的参数校验、重叠检查和权限拼装 | 在当前进程的 `MemorySet` 上建立持有物理页帧的映射 |
| `os/src/syscall/process.rs` | 补齐 `sys_munmap` 的逐页检查与取消映射转发 | 复用第五章脚手架的 `remove_area_with_start_vpn`，不修改其签名 |
| `os/src/mm/mod.rs` | 公开导出 `PageTable`、`PTEFlags` | 使系统调用层能够进行页表有效性与权限检查 |
| `os/src/mm/page_table.rs` | 为公开的权限位常量补充文档注释 | 满足内核对公开接口的文档检查 |

第五章版本的 `sys_munmap` 先确认请求覆盖的每一页有效，再按起始虚拟页号删除区域。它不是第四章报告中“起始页号与页数精确匹配”的同一实现，也不支持一般意义上的任意子区间释放。前序报告保留当时章节的实现记录，不应将其中的代码签名直接视为第五章当前代码。

本章重点与 spawn 的详细分析见 `docs/sys_spawn.md`。该文档作为调用链说明的补充，本报告以当前代码和实际测试结果为准。

## 4. 测试与结果

### 4.1 构建问题与验证方法

最初运行 `make -C ci-user test CHAPTER=5` 时，用户测例编译出现以下错误：

```text
error[E0460]: found possibly newer version of crate `spin`
error: crate `buddy_system_allocator` required to be available in rlib format
```

这些错误发生在用户库与测例的构建阶段，尚未进入内核运行。随后的 `println`、分配器及 `panic_handler` 缺失是用户库加载失败引起的连带错误，不能据此判断 spawn 或 stride 实现失败。

使用相同工具链与依赖，在容器 `/tmp` 下构建全部用户测例能够成功；在 macOS 共享挂载目录 `/mnt` 的 target 下则出现上述错误。仅移走旧缓存不能稳定解决，且使用 target 符号链接会被评测脚本的 `cargo clean` 删除。因此，本次通过环境变量和 Make 参数，把 Cargo、用户测例复制步骤和 QEMU 的内核路径统一指向容器本地目录：

```sh
CARGO_TARGET_DIR=/tmp/rcore-ch5-build make -C ci-user test CHAPTER=5 \
  TARGET_DIR=/tmp/rcore-ch5-build/riscv64gc-unknown-none-elf/release \
  KERNEL_ELF=/tmp/rcore-ch5-build/riscv64gc-unknown-none-elf/release/os
```

以上命令在容器的 `/mnt` 目录执行，不需要修改内核源码或评测脚本。对照构建说明问题与共享目录上的构建产物有关，但没有进一步确定 Docker 文件共享层的具体内部原因。

### 4.2 功能测试

第五章输出检查器共有 `15` 项检查，包含 `13` 条预期输出及 `2` 条不应出现的输出。实际运行全部通过：

```text
Test spawn0 OK!
Test wait OK!
Test waitpid OK!
Test set_priority OK!
ch5 Usertests passed!
Test passed: 15/15
```

这里省略评测脚本添加的随机后缀。`15/15` 是输出检查项数量，不是独立用户程序的数量。

| 测例或测例组 | 验证内容 | 实际结果 |
| --- | --- | --- |
| `ch5_spawn0` | 连续创建 40 个 `ch5_getpid` 子进程，并逐个等待、确认退出码 | 正常结束，输出 spawn 成功标记 |
| `ch5_spawn1` | 新建子进程以及 wait、waitpid 行为 | 输出 wait 和 waitpid 成功标记 |
| `ch5_setprio` | 合法值 `10`、`isize::MAX`，非法值 `0`、`1`、`-10` | 所有返回值断言通过，进程退出码为 `0` |
| `ch5_stride` | 创建六个不同优先级的子进程，观察单位优先级的计算量 | 六个子进程完成，汇总进程退出码为 `0` |
| `ch3_sleep`、`ch3_sleep1` | 时间查询及让出 CPU 后的等待 | 正常结束，时间相关输出检查通过 |
| `ch4_mmap*`、`ch4_unmap*` | 内存映射、权限错误和取消映射 | 相关输出检查通过；权限异常测例按预期被终止 |

`ch4_mmap1` 与 `ch4_mmap2` 的退出码为 `-2`，属于非法权限访问的预期结果，而不是正常程序执行失败。所有应用结束后的 `All applications completed!` 来自当前脚手架的结束路径，评测随后完成输出检查。

### 4.3 stride 调度结果

本次运行中，六个子进程输出的结果如下：

| priority | 计算计数 count | `count / priority` |
| --- | --- | --- |
| `5` | `6655200` | `1331040` |
| `6` | `8138800` | `1356466` |
| `7` | `9623600` | `1374800` |
| `8` | `10816400` | `1352050` |
| `9` | `12152800` | `1350311` |
| `10` | `13861600` | `1386160` |

优先级从 `5` 增至 `10` 时，计算计数大体翻倍；归一化后的结果分布在约 `133` 万至 `139` 万之间，最大值与最小值相差约 `4.1%`。这与持续竞争时计算量近似按优先级分配的目标一致。

上述 count 是测例记录的计算量，不是内核直接统计的 CPU 时间；差异还受进程启动先后、整数取整和实际调度时机影响。另外，`ch5.py` 的 `15/15` 没有单独检查这些比例数值，因此这里只将日志作为调度行为的观测证据，不把它表述为对所有优先级情况的证明。

### 4.4 报告检查

首次功能评测通过后，命令因缺少 `reports/lab1.md` 在报告检查阶段返回失败。该次结果说明功能检查通过，但不能称为完整评测退出码为零。

本次报告整理将第四章分支已有的 `lab1.md`、`lab2.md` 原样带入 `reports/`，并新增本报告 `lab3.md`。三份报告分别记录第三章追踪功能、第四章地址空间以及第五章进程创建与调度实现。报告文件是否存在由评测脚本检查，报告内容则依据各章节代码与实际验证记录编写。

三份报告加入后，使用第 4.1 节的容器本地构建命令重新执行完整评测，最终日志为：

```text
Test passed4047725074: 15/15
Report for lab1 found.
Report for lab2 found.
Report for lab3 found.
```

该次命令退出码为 `0`，功能检查与三份报告的存在性检查均通过，评测脚本也完成了原始构建文件与测例的恢复。

## 5. 遗留问题

1. spawn 存在重复复制。当前先 fork 复制父地址空间，再 exec 建立目标地址空间并丢弃复制结果，功能可用，但不如直接从目标 ELF 创建子进程高效。
2. exec 的堆元数据没有完整重置。当前脚手架更新 `memory_set`、`trap_cx_ppn` 和 `base_size`，但没有同步更新 `heap_bottom`、`program_brk`。spawn 经 fork 后会留下父进程继承的堆边界，若目标程序的用户栈位置不同，后续 `sbrk` 存在不一致风险；本轮测试通过不代表这一情况已经覆盖。
3. 路径读取缺少完整验证。沿用的 `translated_str` 没有字符串长度上限，也不能把所有非法用户地址转换为 `-1`；无效地址可能使内核 panic。
4. 极大优先级使步长变成零。当 priority 大于 `65536` 时，整数除法结果为 `0`。`set_priority(isize::MAX)` 的返回值符合测例要求，但不能据此认为该值下仍能公平调度；持续就绪的零步长进程可能使其他进程饥饿。
5. stride 没有处理溢出。当前使用普通 `usize` 加法和最小值比较，没有回绕比较或统一归一化；长期运行后溢出会破坏顺序，具体溢出行为还取决于编译配置。
6. 调度按次数计账。每次被选中都累加固定 pass，不根据实际消耗时间调整；主动提前 yield 的进程也承担一次完整增量，因此比例结论需要近似等时长的调度条件。
7. 内存取消映射接口较简化。第五章按起始页号删除整个 MapArea，逐页有效性检查不能替代精确区域匹配，也不支持任意区间拆分。

这些问题不影响本次列出的测试结果，但限定了当前实现的适用范围。实验测试通过与完整、长期运行的进程管理实现之间仍有差别。

## 6. 实验总结

本章新增了两部分相互配合的逻辑：`sys_spawn` 将已有的地址空间复制、ELF 装载和父子关系管理连接起来，使父进程能够创建执行指定程序的子进程；stride 调度则在任务控制块中增加 priority、stride，在系统调用中更新优先级，并在就绪队列取任务时选择累计长度最小者。

实现中的关键分工是：创建路径准备进程状态与上下文，调度路径负责实际切换；系统调用修改 priority，调度器读取 priority 并计算 pass。保持这两个分工后，不需要改动既有的上下文切换与退出机制。

容器本地构建目录下的第五章功能检查通过 `15/15`，六个竞争进程的计算量也呈现与优先级近似成比例的结果。前两章报告随本章一起保留在 `reports/`，保证实验记录连续。
