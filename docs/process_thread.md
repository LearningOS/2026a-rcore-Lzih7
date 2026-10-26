# 第八章进程、线程数据结构与系统调用详解

## 1. 进程与线程的分工

本文以当前 ch8 分支为准。前几章一个进程只有一个执行流，任务控制块同时承担资源管理和执行状态管理。ch8 将这两项职责分开：

- 进程是资源管理单位，由 `ProcessControlBlock` 表示。
- 线程是 CPU 调度单位，由 `TaskControlBlock` 表示。

一个进程可以包含多个线程。这些线程共享进程地址空间、文件描述符表和同步对象，但每个线程都有自己的用户栈、内核栈、陷入上下文和任务上下文。

```text
ProcessControlBlock：进程 P
  |
  +-- memory_set：共享地址空间
  +-- fd_table：共享打开文件表
  +-- mutex_list / semaphore_list / condvar_list：共享同步对象
  +-- deadlock：本进程线程的资源分配记录
  |
  +-- tasks[0] -> 主线程 TCB：自己的栈和上下文
  +-- tasks[1] -> 工作线程 TCB：自己的栈和上下文
  +-- tasks[2] -> 工作线程 TCB：自己的栈和上下文
```

例如同一进程中两个线程调用 `getpid()`，得到相同 PID；调用 `gettid()`，得到不同 TID。它们访问同一个全局变量时，访问的是同一进程地址空间中的同一份数据，不需要像 `fork` 那样复制物理页。

用户栈虽然按线程单独分配，仍然映射在共享地址空间中。因此“每线程独有栈”是分配和使用约定，不表示其他线程在硬件权限上无法访问它。

## 2. 进程控制块

实现位置：`os/src/task/process.rs`。

### 2.1 外层 `ProcessControlBlock`

```rust
pub struct ProcessControlBlock {
    pub pid: PidHandle,
    inner: UPSafeCell<ProcessControlBlockInner>,
}
```

外层保留进程身份和内部可变状态的访问入口。

| 字段 | 作用 |
| --- | --- |
| `pid` | 进程编号的拥有者；`PidHandle` 析构时回收编号 |
| `inner` | 包装可变的进程资源，通过运行时借用检查进行独占访问 |

进程通常以 `Arc<ProcessControlBlock>` 形式被引用。`Arc::clone` 只复制共享句柄并增加引用计数，不创建新进程。

### 2.2 内层 `ProcessControlBlockInner`

| 字段 | 含义与使用位置 |
| --- | --- |
| `is_zombie` | 进程是否已经退出，供 `sys_waitpid` 判断 |
| `memory_set` | 用户地址空间，包含页表和映射区域 |
| `parent` | 指向父进程的弱引用 |
| `children` | 持有子进程的强引用，保留其退出信息以供回收 |
| `exit_code` | 进程退出码，当前主要由主线程退出路径设置 |
| `fd_table` | 打开文件对象表，所有线程共享 |
| `signals` | 本进程待处理的信号位集合 |
| `tasks` | 线程槽位表，通常以 TID 作为下标 |
| `task_res_allocator` | 本进程的 TID 分配与回收器 |
| `mutex_list` | 本进程的互斥锁对象表 |
| `semaphore_list` | 本进程的信号量对象表 |
| `condvar_list` | 本进程的条件变量对象表 |
| `deadlock` | 同步资源的可用量、持有量和待满足请求 |

`fd_table` 的元素类型为：

```rust
Option<Arc<dyn File + Send + Sync>>
```

`Some(file)` 表示这个 fd 槽位有打开对象，`None` 表示空槽。`dyn File` 允许同一张表存放普通文件、管道和标准输入输出。`Send + Sync` 是类型约束，并不意味着整个内核已经支持多核安全执行。

`tasks` 的类型为：

```rust
Vec<Option<Arc<TaskControlBlock>>>
```

线程回收后可以留下 `None`，无需搬移其他槽位，否则 TID 与下标的对应关系会改变。

注意：当前 `thread_count()` 返回 `tasks.len()`，它是槽位向量长度，不是对活跃线程数量的精确统计。后文的 `fork/exec` 检查使用的就是这个长度。

### 2.3 为什么父引用用 `Weak`

进程和线程之间的引用关系如下：

```text
父进程 -- children 中的 Arc --> 子进程
父进程 <-- parent 中的 Weak --- 子进程

进程 -- tasks 中的 Arc ------> 线程
进程 <-- process 中的 Weak --- 线程
```

如果双向均使用 `Arc`，对象可能因循环强引用而永远无法析构。`Weak` 不增加强引用计数；需要访问目标时通过 `upgrade()` 临时取得 `Arc`。

因此：

```rust
current_task().unwrap().process.upgrade().unwrap()
```

表示从当前线程找到其所属进程，不是创建进程或切换执行流。

### 2.4 `UPSafeCell` 与 `drop(inner)`

`inner_exclusive_access()` 返回 `RefMut<ProcessControlBlockInner>`。其底层是 `RefCell::borrow_mut()`，同一对象再次发生冲突借用会 panic，不是等待锁释放。

典型写法：

```rust
let inner = process.inner_exclusive_access();
let mutex = Arc::clone(inner.mutex_list[id].as_ref().unwrap());
drop(inner);
mutex.lock();
```

先取得同步对象的独立共享引用，再释放 PCB 内部借用。后面的 `lock()` 会再次访问 PCB 中的死锁检测状态，因此不能一直持有 `inner`。

`drop(inner)` 释放的是借用守卫，不是进程，也不会删除文件表或同步对象。

`UPSafeCell` 依赖单核、内核态不可抢占等约束，不是通用多核互斥锁。

## 3. 线程控制块与用户资源

实现位置：`os/src/task/task.rs`、`os/src/task/id.rs`。

### 3.1 `TaskControlBlock`

```rust
pub struct TaskControlBlock {
    pub process: Weak<ProcessControlBlock>,
    pub kstack: KernelStack,
    inner: UPSafeCell<TaskControlBlockInner>,
}
```

| 字段 | 作用 |
| --- | --- |
| `process` | 所属进程的弱引用，访问共享资源时使用 |
| `kstack` | 该线程独立的内核栈 |
| `inner` | 线程自身的可变执行状态 |

内核栈按独立的内核栈编号分配，不应把它与 PID 或 TID 混为一谈。

### 3.2 `TaskControlBlockInner`

| 字段 | 作用 |
| --- | --- |
| `res: Option<TaskUserRes>` | 线程的 TID、用户栈及 TrapContext 映射管理；退出时置为 `None` |
| `trap_cx_ppn` | 陷入上下文所在物理页号，供内核取得 `TrapContext` |
| `task_cx` | 内核执行流切换时保存的上下文 |
| `task_status` | `Ready`、`Running` 或 `Blocked` |
| `exit_code: Option<i32>` | `None` 表示尚未退出，`Some(code)` 表示已记录退出码 |

当前线程状态枚举没有 `Zombie`。线程退出通过 `exit_code` 表达；进程退出通过 PCB 的 `is_zombie` 表达，不能直接套用前几章的任务状态解释。

### 3.3 `TaskUserRes`

`TaskUserRes` 保存 `tid`、`ustack_base` 和所属进程的 `Weak`，负责分配与回收线程用户资源。

用户栈位置按 TID 计算：

```text
栈底 = ustack_base + tid * (PAGE_SIZE + USER_STACK_SIZE)
栈顶 = 栈底 + USER_STACK_SIZE
```

相邻线程栈之间留出一个页大小的间隔。线程的 TrapContext 虚拟地址为：

```text
TRAP_CONTEXT_BASE - tid * PAGE_SIZE
```

因此线程共享一张用户页表，但各自使用不同位置的栈和 TrapContext。

TrapContext 区域只有内核读写权限，不带用户态 `U` 位。它位于进程地址空间中，不表示用户代码能够直接访问。

`TaskUserRes` 析构时回收 TID，并取消用户栈与 TrapContext 映射。当前退出路径在 `waittid` 前就释放它，因此 TID 可能较早被复用，不能把 TID 当作永久身份。

## 4. 两种上下文与调度器

### 4.1 `TaskContext` 不等于 `TrapContext`

| 对象 | 保存内容 | 使用者 |
| --- | --- | --- |
| `TaskContext` | `ra`、内核栈 `sp`、`s0..s11` | `__switch` |
| `TrapContext` | 用户通用寄存器、`sepc`、`sstatus`、内核页表和内核栈信息等 | `__alltraps`、`__restore` |

`TaskContext` 用于暂停和恢复内核执行流，`TrapContext` 用于从用户态陷入内核以及返回用户态。它们解决的是不同的切换问题。

新线程的 `TaskContext::goto_trap_return(kstack_top)` 将 `ra` 设置为 `trap_return`，将 `sp` 设置为自己的内核栈顶。因此线程首次被调度时会进入返回用户态的路径。

### 4.2 `Processor` 与 `TaskManager`

`Processor.current` 保存：

```rust
Option<Arc<TaskControlBlock>>
```

这就是当前运行线程。`current_task()` 克隆这里的 `Arc`；`take_current_task()` 则取走它，并将该位置留下为 `None`。

`TaskManager.ready_queue` 保存就绪线程。当前实现使用 FIFO：

```text
add_task -> push_back
fetch_task -> pop_front
```

当前 ch8 没有沿用 ch5 的 stride 调度器。

正常调度路径：

```text
run_tasks
  -> fetch_task：取得一个就绪线程
  -> 将线程状态改为 Running
  -> Processor.current = Some(task)
  -> __switch(idle_task_cx, task.task_cx)
```

暂停、阻塞与唤醒的差别：

| 操作 | 状态变化 | 是否立即加入就绪队列 |
| --- | --- | --- |
| `suspend_current_and_run_next` | `Running -> Ready` | 是 |
| `block_current_and_run_next` | `Running -> Blocked` | 否 |
| `wakeup_task` | `Blocked -> Ready` | 是 |

阻塞线程需要由锁、信号量、条件变量或定时器等对象保留引用，并在条件满足时唤醒。

`PID2PCB` 则是 PID 到 PCB 的查找表，用于 `kill` 等进程操作。它不是线程就绪队列。

## 5. 系统调用如何连接这些对象

相关入口：`os/src/trap/mod.rs`、`os/src/syscall/mod.rs`。

```text
用户库封装
  -> a7 放系统调用编号，参数放 a0..a3
  -> ecall
  -> __alltraps 保存线程的 TrapContext，切换到内核页表
  -> trap_handler
     -> sepc += 4
     -> syscall(id, args)
        -> 对应 sys_* 函数
     -> 重新取得当前 TrapContext，将结果写入 x[10]
  -> trap_return
  -> __restore 切回用户页表，恢复寄存器
  -> 用户态继续执行
```

重新取得 TrapContext 是必要的：`exec` 会替换地址空间，先前的 TrapContext 可能已失效。

系统调用中的指针通常仍是用户虚拟地址。类型转换成 `*mut T` 不会自动进行地址翻译；访问时必须使用当前进程页表和相应翻译工具。

`current_user_token()` 的路径为：

```text
当前线程 -> 所属进程 -> memory_set.token()
```

同一进程的线程得到相同页表 token，但返回用户态时传给跳板的 TrapContext 虚拟地址取决于各自 TID。

`PageTable::from_token` 只是构造查询现有页表的对象，不复制页表，也不切换 `satp`。

## 6. 进程系统调用

实现位置：`os/src/syscall/process.rs`。

### 6.1 当前接口状态

| 接口 | 编号 | 当前行为 |
| --- | --- | --- |
| `sys_getpid` | 172 | 返回当前线程所属进程的 PID |
| `sys_fork` | 220 | 复制单线程进程，父返回子 PID，子返回 0 |
| `sys_exec` | 221 | 从文件系统读取 ELF，替换当前进程程序并设置参数 |
| `sys_waitpid` | 260 | 查找并回收已退出子进程 |
| `sys_exit` | 93 | 退出当前线程；主线程退出会结束进程 |
| `sys_yield` | 124 | 当前线程主动让出 CPU |
| `sys_kill` | 129 | 向目标进程记录信号位 |
| `sys_get_time` | 169 | 校验用户缓冲区并跨页写回秒与微秒 |
| `sys_mmap` / `sys_munmap` | 222 / 215 | 仍为占位实现，返回 -1 |
| `sys_spawn` | 400 | 仍为占位实现，返回 -1 |
| `sys_set_priority` | 140 | 仍为占位实现，返回 -1 |

这里的状态描述针对当前分支；累积保存的 ch5、ch6 文档不代表相应实现已自动迁移到 ch8。

### 6.2 `fork`：复制进程资源并创建子主线程

```text
sys_fork
  -> current_process()
  -> ProcessControlBlock::fork
     -> 检查 tasks.len() == 1
     -> MemorySet::from_existed_user：深拷贝用户地址空间
     -> 分配新 PID
     -> 克隆 fd_table 中的文件 Arc
     -> 创建子 PCB，挂到 parent.children
     -> 创建子主线程 TCB，TID 为 0
     -> 修正子 TrapContext.kernel_sp
     -> 插入 PID2PCB
     -> add_task：子线程入就绪队列
  -> 将子 TrapContext.x[10] 设置为 0
  -> 返回子 PID
```

地址空间是深拷贝，不是写时复制；文件对象则共享 `Arc`，因此文件内容和同一个打开对象的偏移可能共享。

子进程的同步对象表为空，信号为空，死锁检测器重新创建且默认关闭，不是复制父进程的锁持有关系。

用户代码中同一次 `fork()` 看似返回两次，实际是两个执行流分别恢复各自的 TrapContext。父的 `a0` 被设置为子 PID，子的 `a0` 被设置为 0。复制发生前 `sepc` 已前进到 `ecall` 的下一条指令，子进程不会重新执行同一次系统调用。

`fork` 内部没有直接调用 `__switch`。子线程虽已入队，但在当前单核不可抢占的系统调用路径内不会立即运行；真正运行由后续调度决定。

### 6.3 `exec`：替换程序，不创建进程

```text
sys_exec(path, args)
  -> 翻译路径和以空指针结尾的参数指针数组
  -> open_file(path, RDONLY)
  -> read_all：取得 ELF 字节
  -> ProcessControlBlock::exec
     -> 检查 tasks.len() == 1
     -> MemorySet::from_elf
     -> 替换 memory_set
     -> 为主线程重新建立用户栈和 TrapContext
     -> 将参数字符串、argv 指针数组写到新用户栈
     -> 设置 sepc、用户 sp、a0 = argc、a1 = argv
  -> 返回 argc，供 trap_handler 写入新 TrapContext.a0
```

用户原程序的调用位置不会在成功后继续执行，而是从新 ELF 的入口开始执行。但内核里的 `sys_exec` 确实会返回，以完成陷入返回流程。

PID、PCB 和主线程内核栈保持原对象；当前代码也没有清空文件表、同步对象表或重置死锁检测器。不能把“替换用户程序”理解成“重新创建全部进程资源”。

### 6.4 `waitpid`：回收子进程

`sys_waitpid` 操作的是 PCB 的 `children`：

- 没有匹配子进程：返回 `-1`。
- 有匹配子进程，但尚未退出：返回 `-2`。
- 找到僵尸子进程：移除 `children` 中的引用，写回退出码，返回子 PID。

`pid == -1` 表示匹配任意子进程。退出码写到父进程用户地址，通过父进程 token 翻译。

等待不是这段内核函数内部持续阻塞完成的。用户库通常遇到 `-2` 就 `yield`，再发起查询。

子进程退出时先释放主要资源，但 PCB 和退出码仍由父进程保留。`waitpid` 才完成这部分回收，PID 在 `PidHandle` 析构时被归还。

### 6.5 `exit`：主线程与其他线程不同

`sys_exit` 调用 `exit_current_and_run_next`，首先执行线程级清理：

```text
移出 Processor.current
  -> TCB.exit_code = Some(code)
  -> res = None，释放用户栈、TrapContext 和 TID
  -> deadlock.exit_thread(tid)
```

若退出的是非主线程，进程仍可继续运行，线程 TCB 通常保留在 `tasks` 中等待 `waittid` 回收。

若 `tid == 0`，还会执行进程级清理：移出 PID 查找表、设置 `is_zombie`、记录进程退出码、将子进程交给 `INITPROC`、移除其他线程的就绪或定时器记录，回收用户资源和数据页，清空文件表及线程表。主线程退出会结束整个进程，不等待其他线程自然结束。

正在执行的内核栈不能当场被释放。`stop_task` 机制保留退出主线程的引用，直到其已经离开该栈后才允许回收。

### 6.6 `kill` 与信号

`sys_kill` 通过 `pid2process(pid)` 找到 PCB，将传入的有效信号位并入 `signals`。当前参数按 `SignalFlags` 位掩码解释，例如 `SIGSEGV` 对应 `1 << 11`，不是直接把整数 11 当成该位。

这一步只是记录信号，不在 `sys_kill` 内直接销毁目标进程。目标执行到陷入处理的信号检查时，可能调用 `exit_current_and_run_next`。

当前分支只实现简化的致命信号检查，不包含完整用户信号处理器机制。尤其在多线程情况下，检查后调用的仍是“退出当前线程”的路径；若当前不是主线程，不应声称该实现总能立即结束整个进程。

### 6.7 `get_time` 与用户内存

`TimeVal` 由两个 `usize` 字段组成，在 RV64 上为 16 字节：

```rust
pub struct TimeVal {
    pub sec: usize,
    pub usec: usize,
}
```

`sys_get_time` 检查地址计算不溢出、缓冲区位于允许的低地址范围，并逐页确认页表项有效、可读、可写且带 `U` 位。然后将时间编码成字节，通过 `translated_byte_buffer` 按物理页分段写回。

例如用户缓冲区从页内偏移 4090 开始，16 字节分为当前页的 6 字节与下一页的 10 字节。两个虚拟页的物理页不必相邻，因此不能只翻译起点后直接写一个结构体。

## 7. 线程系统调用

实现位置：`os/src/syscall/thread.rs`。

| 接口 | 编号 | 行为 |
| --- | --- | --- |
| `sys_thread_create(entry, arg)` | 460 | 在当前进程中创建新线程，返回 TID |
| `sys_gettid()` | 178 | 返回当前线程 TID |
| `sys_waittid(tid)` | 462 | 查询退出状态并回收线程 TCB，成功返回退出码 |

### 7.1 创建线程的完整过程

```text
sys_thread_create
  -> 从当前线程找到 PCB
  -> TaskControlBlock::new
     -> TaskUserRes::new
        -> alloc_tid，登记 deadlock 的线程行
        -> 在原 memory_set 中映射新用户栈和 TrapContext
     -> kstack_alloc
     -> task_cx = goto_trap_return
     -> task_status = Ready
  -> add_task：加入线程就绪队列
  -> tasks[tid] = Some(new_task)
  -> 初始化 TrapContext
     -> sepc = entry
     -> 用户 sp = 新线程栈顶
     -> kernel_sp = 新线程内核栈顶
     -> x[10] = arg
  -> 返回 TID
```

当前源码先入队，再完成 `tasks` 登记和 TrapContext 初始化。这依赖当前单核、内核态不抢占的约束；系统调用执行完前不会切入这个新线程。不能把此顺序直接推广到多核或可抢占内核。

新线程不会从创建者的 `ecall` 下一条指令开始执行，而是从传入的 `entry` 开始。它也不复制创建者的整个地址空间。

线程入口结束时应通过用户封装明确退出；当前初始化不是为任意入口函数设置一个自动退出的返回地址。

### 7.2 `waittid` 与 `waitpid` 的区别

| 比较项 | `waittid` | `waitpid` |
| --- | --- | --- |
| 查询对象 | 本进程的线程 | 本进程的子进程 |
| 查找表 | `tasks[tid]` | `children` |
| 退出判断 | TCB 的 `exit_code` | PCB 的 `is_zombie` |
| 成功返回 | 线程退出码 | 子进程 PID |
| 退出码写回指针 | 不需要 | 需要 |

`waittid` 拒绝等待自身；目标槽位为空返回 `-1`；目标未退出返回 `-2`；已退出则把槽位置为 `None` 并返回退出码。

用户库遇到 `-2` 会让出 CPU 并重试。当前内核实现直接访问 `tasks[tid]`，没有先检查 `tid < tasks.len()`，越界 TID 仍可能 panic，不能描述成任意非法 TID 都可靠地返回 `-1`。

## 8. 同步资源与死锁检测

同步系统调用位于 `os/src/syscall/sync.rs`，对象实现位于 `os/src/sync/`。

### 8.1 资源归进程，请求来自线程

mutex、semaphore、condvar 的外部编号分别是各自 PCB 表的下标。例如 `mutex_id = 0` 与 `sem_id = 0` 属于不同表，不代表同一个对象。

死锁检测器另外分配统一的 `resource` 编号，每一种 mutex 或 semaphore 对应矩阵的一列。这使跨两种对象的依赖也能放在同一次检查中分析。

### 8.2 `DeadlockDetector` 字段

| 字段 | 含义 |
| --- | --- |
| `enabled` | 是否拒绝检测为不安全的请求，初值为 false |
| `available[j]` | 第 j 类资源当前尚未分配的数量 |
| `allocation[i][j]` | 线程 i 已经持有的第 j 类资源数量 |
| `need[i][j]` | 线程 i 已登记、尚未实际取得的请求数量 |
| `live[i]` | 该 TID 行是否对应尚未退出的线程 |

这里的 `Need` 记录当前未完成请求，不是用户提前申报的最大资源需求。因此算法不能预测用户未来还会申请什么，也不覆盖条件变量、管道或 `waittid` 引起的所有等待关系。

### 8.3 一次获取请求如何更新状态

```text
request(tid, resource)
  -> 临时增加 Need
  -> enabled 时执行 is_safe
     -> 不安全：撤销此次 Need，返回失败
     -> 安全或未开启：保留请求
  -> 资源可用：acquire
     -> Available -= 1
     -> Need -= 1
     -> Allocation += 1
  -> 资源不可用：保留 Need，并等待
```

不安全请求返回 `-0xdead`，不会改变真实锁状态、信号量计数或等待队列。

释放时调用 `release`。阻塞锁或信号量若存在等待者，则在唤醒之前调用 `acquire(next_tid, resource)` 完成资源交接。被唤醒线程可能尚未获得 CPU，但它已取得资源承诺，因此必须立即计入 Allocation，不能等它运行后再记录。

信号量允许未持有资源的线程调用 `up`，例如生产者发出通知。检测器对此增加可用量，而不把该线程的 Allocation 减成负数；它不是严格的“只有持有者才能释放”模型。

### 8.4 安全性模拟

`is_safe` 使用独立工作副本：

```text
Work = Available
Finish[i] = !live[i]

反复寻找：
  未完成的活跃线程 i
  且所有资源满足 Need[i][j] <= Work[j]

假设该线程能完成：
  Work[j] += Allocation[i][j]
  Finish[i] = true

没有新的进展时：
  全部 Finish 为 true -> 当前模型下安全
  否则 -> 当前模型下不安全
```

已经退出的线程在模拟中不参与继续执行，也不会自动释放其遗留 Allocation。实际代码没有因为线程退出就自动解锁，因此模拟不能凭空把这些资源加回 Work。

举例，两线程各持有一把锁：

```text
Available = [0, 0]
Allocation[0] = [1, 0]
Allocation[1] = [0, 1]
```

线程 0 请求锁 1 时，若线程 1 尚无未完成请求，模拟允许线程 1 先完成并释放锁 1，因此可接受这个等待。随后线程 1 请求锁 0，两个 Need 分别成为 `[0, 1]` 与 `[1, 0]`，Work 为 `[0, 0]`，无人能够推进，此次请求被拒绝并撤销。

算法只模拟，不真的结束线程，也不真的释放资源。

### 8.5 开关与生命周期

`sys_enable_deadlock_detect` 的系统调用编号为 469：

| 参数 | 结果 |
| --- | --- |
| 0 | 关闭当前进程检测，返回 0 |
| 1 | 开启当前进程检测，返回 0 |
| 其他值 | 返回 -1，保持原状态 |

关闭检测只关闭拒绝逻辑，资源记录仍继续更新。因此运行中开启检测时，不需要重新推测已有资源的分配情况。

创建线程时分配矩阵行，创建资源时扩展矩阵列；线程退出时将对应行标记为非活跃。新建进程和 `fork` 子进程使用新的检测器；当前 `exec` 并未重新初始化它。

条件变量本身没有分配检测器资源列，但 `wait` 会释放 mutex，并在唤醒后重新获取 mutex，因而仍会影响锁的分配记录。重新加锁若被拒绝，`condvar.wait` 会返回错误，调用者不能再假定自己持有 mutex。

## 9. 编号、地址与身份的区分

| 名称 | 属于哪个范围 | 表示什么 |
| --- | --- | --- |
| PID | 系统级进程编号 | 一个进程 |
| TID | 单个进程内的线程编号 | 一个线程槽位身份 |
| fd | 单个进程的文件表下标 | 一个打开文件对象入口 |
| mutex/sem/condvar ID | 单个进程的相应对象表下标 | 一个同步对象入口 |
| detector resource ID | 单个进程的统一检测器列号 | 一类被统计的同步资源 |
| kernel stack ID | 全局内核栈分配器 | 一段线程内核栈 |
| token | 页表寄存器格式的数值 | 用户地址空间的分页模式与根页表物理页号 |
| `trap_cx_ppn` | 物理页号 | 当前线程陷入上下文所在页 |

例如 PID 5 内部的 TID 1 与 PID 6 内部的 TID 1 是不同线程。相同数值不能跨不同编号范围比较身份。

## 10. 阅读代码的建议顺序与实现边界

建议按以下顺序阅读：

1. `task/process.rs`：明确资源在哪个 PCB 字段中。
2. `task/task.rs` 和 `task/id.rs`：明确线程自身的状态与栈布局。
3. `task/processor.rs` 和 `task/manager.rs`：跟踪当前线程、就绪队列和切换。
4. `trap/mod.rs` 和 `syscall/mod.rs`：理解系统调用参数与返回值。
5. `syscall/process.rs`、`syscall/thread.rs`：串起创建、替换、退出和回收。
6. `syscall/sync.rs` 与同步对象实现：跟踪请求、阻塞、释放和唤醒。

当前实现应保留以下边界认识：

- `fork/exec` 只接受线程槽位向量长度为 1 的进程，不是一般多线程语义。
- 部分接口仍是占位实现，不能因文档累计存在就认为已完成迁移。
- `waitpid` 的结构化写回及 `exec` 参数读取等路径仍依赖宽松的用户指针辅助函数，不能声称所有系统调用均具有完整跨页和权限校验。
- 同步对象查找常直接使用下标和 `unwrap`，非法编号并非全部转换成错误码。
- 死锁检测分析已登记的 mutex/semaphore 请求，不是对全部程序未来行为的证明。
- 线程退出不自动释放其持有的锁，程序仍应遵守正确的资源释放协议。

理解本章的核心是先判断每个操作作用于谁：资源归进程，执行和阻塞归线程；共享句柄复制不是对象复制，调度上下文切换也不是地址空间复制。
