# Lab5：进程、线程与同步互斥

## 1. 实验要求

本实验对应 rCore 第八章。在前章进程管理的基础上，将进程与线程的职责分开：进程负责管理地址空间、文件表等共享资源，线程负责保存执行现场并参与 CPU 调度。同一进程中的线程能够并发访问共享数据，因此本章提供 mutex、semaphore 和 condvar 三种同步机制，并在 mutex 与 semaphore 的获取路径上加入死锁检测。

本报告重点说明本章的数据结构与同步过程，不重复前章文件系统的实现。

| 内容 | 主要接口或结构 | 要求 |
| --- | --- | --- |
| 进程资源管理 | `ProcessControlBlock`、`ProcessControlBlockInner` | 集中保存地址空间、父子关系、文件描述符、线程和同步对象 |
| 线程执行管理 | `TaskControlBlock`、`TaskControlBlockInner` | 每个线程拥有独立的内核栈、用户执行现场和调度状态 |
| 互斥锁 | `MutexSpin`、`MutexBlocking` | 同一时刻只有一个线程进入对应临界区，支持让出 CPU 后重试或排队阻塞 |
| 信号量 | `Semaphore::down/up` | 用计数表示许可，许可不足时阻塞，释放许可时唤醒等待者 |
| 条件变量 | `Condvar::wait/signal` | 等待时释放 mutex，唤醒后重新获取 mutex 并检查共享条件 |
| 死锁检测 | `DeadlockDetector` | 根据 Available、Allocation、Need 检查资源请求，不安全时返回 `-0xdead` |
| 检测开关 | `sys_enable_deadlock_detect` | 默认关闭，参数为 `0` 或 `1` 时设置开关，其他值返回 `-1` |

PCB、TCB、调度器和三类同步对象的基本框架由本章已有代码提供。本次实验扩展主要是维护进程内的资源分配矩阵，将检测接入锁和信号量操作，并让获取失败的返回值传递到系统调用层。

## 2. 方案设计

### 2.1 进程管理资源，线程参与调度

前章主要围绕单线程进程组织任务。本章引入多线程后，不能再把进程资源与单条执行流混在一个控制块中。

| 对象 | 负责的内容 | 是否由同进程线程共享 |
| --- | --- | --- |
| PCB | PID、地址空间、文件表、父子关系、同步对象、死锁检测器 | 是 |
| TCB | 内核栈、用户栈资源、TrapContext、TaskContext、运行状态 | 每个线程独立 |
| `TaskManager` | 就绪线程队列 | 系统统一管理 |
| `Processor` | 当前运行线程与调度器上下文 | 当前单核统一管理 |

例如，一个进程包含主线程 T0 和工作线程 T1。两者访问相同的全局变量和文件描述符表，但各自拥有独立的栈和寄存器现场；T0 在信号量上阻塞时，T1 仍然可以被调度。

因此，`current_task()` 返回当前线程的 `Arc<TaskControlBlock>`，不是复制出一个新线程。`current_process()` 通过该线程的 `process` 弱引用取得所属 PCB。

### 2.2 用 Arc 与 Weak 表示对象关系

进程的 `tasks` 表强引用线程，线程通过 `Weak<ProcessControlBlock>` 指向进程：

```text
ProcessControlBlock
  -> tasks[0]: Arc<TaskControlBlock>
  -> tasks[1]: Arc<TaskControlBlock>
                 -> process: Weak<ProcessControlBlock>
```

如果两个方向都使用 Arc，进程与线程会形成循环强引用，外部引用释放后仍不能自动析构。反向使用 Weak，可以访问所属进程而不增加其强引用计数。

父子进程也采用类似设计：父进程通过 `children` 强引用子进程，子进程通过 `parent` 弱引用父进程。

同步对象的等待队列保存 `Arc<TaskControlBlock>`。线程离开就绪队列后仍有对象持有它，唤醒时可以直接将同一线程放回就绪队列。

### 2.3 内部可变性与调度前释放借用

PCB、TCB 和同步对象的可变状态由 `UPSafeCell<T>` 包装，其内部是：

```rust
inner: RefCell<T>
```

`exclusive_access()` 调用 `borrow_mut()`，返回 `RefMut<T>`。这使代码可以通过共享引用修改内部状态，但借用冲突仍在运行时检查：一个对象已经存在可变借用时，再次借用会 panic。

`UPSafeCell` 不是多核锁。本章依赖单核、内核态不可抢占的执行条件，`unsafe impl Sync` 也不能代替这一前提。

系统调用查到同步对象后，先克隆其 Arc，再释放 PCB 内部借用：

```rust
let mutex = Arc::clone(process_inner.mutex_list[mutex_id].as_ref().unwrap());
drop(process_inner);
mutex.lock()
```

否则 `lock()` 内部访问同一 PCB 的检测器时会重复可变借用。入队后切换线程前也必须释放对象借用，避免下一线程访问同一对象时冲突。这里 `drop` 释放的是借用守卫，不是销毁 PCB 或同步对象。

### 2.4 同步机制通过线程状态连接调度器

| 操作 | 线程状态变化 | 是否回到就绪队列 |
| --- | --- | --- |
| `suspend_current_and_run_next` | `Running -> Ready` | 是，等待下一次调度后重试 |
| `block_current_and_run_next` | `Running -> Blocked` | 否，由对应等待队列持有 |
| `wakeup_task` | `Blocked -> Ready` | 是，但不保证立刻运行 |

当前 `TaskManager` 使用 FIFO 就绪队列，不是前章报告中的 stride 调度。调度器取出的对象始终是线程 TCB；线程恢复后从此前暂停的内核执行位置继续。

## 3. 文件修改与实现

### 3.1 `os/src/task/process.rs`：进程数据结构

PCB 的外层保存不可变的 PID，内层集中保存可变资源：

```rust
pub struct ProcessControlBlock {
    pub pid: PidHandle,
    inner: UPSafeCell<ProcessControlBlockInner>,
}
```

`ProcessControlBlockInner` 的字段分为以下几组：

| 字段 | 类型或内容 | 作用 |
| --- | --- | --- |
| `is_zombie`、`exit_code` | 退出标志与退出码 | 记录进程退出状态，供父进程回收 |
| `memory_set` | `MemorySet` | 管理用户地址空间及用户页表 |
| `parent`、`children` | Weak 与 Arc 引用 | 管理父子进程关系 |
| `fd_table` | `Vec<Option<Arc<dyn File + Send + Sync>>>` | 保存进程共享的打开文件对象 |
| `signals` | `SignalFlags` | 保存进程信号标志 |
| `tasks` | `Vec<Option<Arc<TaskControlBlock>>>` | 按 TID 保存线程对象 |
| `task_res_allocator` | `RecycleAllocator` | 分配和回收进程内 TID |
| `mutex_list` | `Vec<Option<Arc<dyn Mutex>>>` | 保存两种 mutex 的统一接口对象 |
| `semaphore_list` | `Vec<Option<Arc<Semaphore>>>` | 保存信号量 |
| `condvar_list` | `Vec<Option<Arc<Condvar>>>` | 保存条件变量 |
| `deadlock` | `DeadlockDetector` | 保存进程内资源分配和请求状态 |

表中 `Option` 允许槽位为空。对象创建时优先寻找空槽位，否则追加新槽位，返回下标作为对象 ID。mutex、semaphore 与 condvar 分别编号，不应把它们的下标理解为全系统统一的资源号。

进程创建时初始化地址空间、标准输入输出、空线程表和同步表，并建立主线程。主线程 TID 为 0。创建后，PID 到 PCB 的全局映射负责按 PID 查找进程；调度队列则负责管理可运行线程，两者不是同一张表。

同进程线程共用 `memory_set`，但这并不意味着用户态与内核态共用一张页表。用户态使用所属进程的页表，陷入内核后通过跳板切换到公共 `KERNEL_SPACE` 页表。

### 3.2 `os/src/task/task.rs`：线程数据结构

线程外层保存所属进程弱引用与独立内核栈，内层保存可变执行状态：

```rust
pub struct TaskControlBlock {
    pub process: Weak<ProcessControlBlock>,
    pub kstack: KernelStack,
    inner: UPSafeCell<TaskControlBlockInner>,
}
```

| 内层字段 | 作用 |
| --- | --- |
| `res: Option<TaskUserRes>` | 管理 TID、用户栈及 TrapContext 映射等线程资源 |
| `trap_cx_ppn` | 定位该线程 TrapContext 所在的物理页 |
| `task_cx` | 保存内核上下文，供 `__switch` 切换执行流 |
| `task_status` | 记录 Ready、Running 或 Blocked |
| `exit_code: Option<i32>` | 记录线程是否退出及其退出码 |

`TaskContext` 与 `TrapContext` 用途不同：前者保存内核执行流切换所需的寄存器，后者保存用户寄存器与陷入返回信息。线程阻塞时保留这两个现场，恢复时不需要重新进入线程入口函数。

`sys_thread_create` 在当前 PCB 中创建新 TCB，准备用户入口和参数，再加入就绪队列。线程退出后记录退出码，用户通过 `waittid` 查询并回收相应线程槽位。主线程退出则触发整个进程的终止流程。

本次在 TID 分配和退出路径中接入检测器：`alloc_tid()` 登记活跃线程，退出路径调用 `exit_thread(tid)` 清除未完成请求并标记非活跃。退出不等于解锁，不能直接把遗留资源加入 Available。

### 3.3 `os/src/sync/mutex.rs`：两种互斥锁

mutex 的公共接口为：

```rust
pub trait Mutex: Sync + Send {
    fn lock(&self) -> isize;
    fn unlock(&self);
}
```

`lock` 返回 `0` 表示成功，检测拒绝时返回 `-0xdead`。每把 mutex 在检测器中注册容量为 1 的资源。

`MutexSpin` 保存 `locked`。获取时先登记请求，再检查锁是否空闲；若已锁，释放借用并调用 `suspend_current_and_run_next()`，下次运行时重试。它不是一直占用 CPU 的纯忙等，也没有专门的阻塞等待队列。成功时设置 `locked = true` 并登记实际分配；unlock 设置 false 并登记释放。

`MutexBlocking` 额外维护 `VecDeque<Arc<TaskControlBlock>>`。锁已被占用时，线程入队并进入 Blocked；锁空闲时直接取得。

阻塞 mutex 的 unlock 采用直接交接：

```text
当前线程释放资源记录
  -> 有等待者：给队首登记分配，再将它唤醒，locked 保持 true
  -> 无等待者：locked = false，锁重新空闲
```

例如 A 持锁，B、C 排队。A 解锁后，锁交给 B，即使 B 尚未运行，新到达的 D 也不能抢先获取。B 恢复后的 `lock()` 不再次竞争，直接返回成功，因为资源已在唤醒前交接。

两种锁都没有独立 owner 校验，也不支持递归加锁。调用者必须在成功获取后由正确线程执行对应 unlock；检测失败后不能把请求当成已经成功。

### 3.4 `os/src/sync/semaphore.rs`：计数与等待队列

信号量内部保存：

```rust
pub struct SemaphoreInner {
    pub count: isize,
    pub wait_queue: VecDeque<Arc<TaskControlBlock>>,
}
```

`down` 先登记资源请求。不安全时立即返回 `-0xdead`，不修改 count，也不加入队列；接受后执行 `count -= 1`，非负表示取得许可，负数表示入队阻塞。

`up` 先登记释放，再执行 `count += 1`。结果小于或等于 0 时，取出队首等待者，在唤醒前为其登记分配；结果大于 0 时，许可留给未来的 down。

初值为 2 的正常操作过程如下：

| 操作 | count | Available | 结果 |
| --- | --- | --- | --- |
| 初始化 | 2 | 2 | 两个许可 |
| A down | 1 | 1 | A 获得一个 |
| B down | 0 | 0 | B 获得一个 |
| C down | -1 | 0 | C 等待 |
| A up | 0 | 0 | 许可直接交给 C |
| B up | 1 | 1 | 留下一个空闲许可 |

表格假设检测关闭或这些请求已通过检查。count 可以为负，Available 始终表示可分配量，不能为负；两者不能混用。

信号量允许没有执行过 down 的线程执行 up，用于生产者通知消费者。因此 detector 的 release 只在 Allocation 非零时扣减持有量，但总会增加 Available。零初值通知信号量的未来生产事件不在资源模型中，启用检测后可能发生保守拒绝。

### 3.5 `os/src/sync/condvar.rs`：释放锁、等待和重新加锁

条件变量只保存等待队列，不保存条件值，也不累计通知次数。条件本身应放在 mutex 保护的共享数据中。

当前 wait 的关键顺序为：

```rust
mutex.unlock();
let mut inner = self.inner.exclusive_access();
inner.wait_queue.push_back(current_task().unwrap());
drop(inner);
block_current_and_run_next();
mutex.lock()
```

先释放 mutex，其他线程才能进入临界区并改变条件。等待者入队后阻塞，被 signal 唤醒后重新获取 mutex，成功后才能继续访问共享状态。

signal 只唤醒队首线程，不自动释放通知者的锁，也不把锁直接交给等待者。这是 Mesa 风格的使用方式，应采用以下协议：

```text
等待方：
  获取 mutex，并检查获取结果
  while 共享条件不满足：
    condvar wait，并检查重新获取 mutex 的结果
  操作共享数据
  释放 mutex

通知方：
  获取 mutex
  修改共享条件
  condvar signal
  释放 mutex
```

使用 while 是因为通知只表示需要重新检查；等待者实际运行时，条件可能已被其他线程改变。signal 时没有等待者则通知不会保留，当前也没有 broadcast 接口。

源码中 unlock 在入队之前，二者之间没有另加原子操作。本章单核、内核不可抢占且该路径不主动切换线程，使等待登记能在其他用户线程运行前完成；多核或可抢占内核不能直接照搬。

wait 最后返回 mutex.lock 的结果，重新加锁可能被检测拒绝。内核系统调用会传播错误，但用户高层 `condvar_wait()` 当前丢弃返回值。需要检查时应使用返回 `isize` 的 `user_lib::sys_condvar_wait`，不能无条件假设高层 wait 返回后已经持锁。

### 3.6 `os/src/task/process.rs`：统一死锁检测状态

检测器包含开关与四组状态：

| 字段 | 含义 |
| --- | --- |
| `available` | 各类资源可立即分配的数量 |
| `allocation[tid]` | 线程已经持有的资源 |
| `need[tid]` | 线程已经提出但尚未满足的请求 |
| `live[tid]` | 对应线程是否活跃 |

mutex 与 semaphore 创建时调用 `add_resource`，取得统一的内部 resource ID。这样两把锁之间、多个信号量之间，以及锁与信号量混合形成的等待关系都可以在同一矩阵中分析。condvar 不登记固定容量资源。

三类更新操作为：

```text
request：Need += 1；若检查失败，Need -= 1 并拒绝
acquire：Available -= 1；Need -= 1；Allocation += 1
release：有持有量时 Allocation -= 1；Available += 1
```

安全性检查复制 Available 为 Work，并将非活跃线程的 Finish 初始化为 true。每轮寻找满足 `Need[i] <= Work` 的未完成线程，模拟它完成并释放原有持有量，即 `Work += Allocation[i]`。无法继续时，只有全部线程 Finish 为 true 才接受请求。

本实现的 Need 不是经典银行家算法中预先声明的最大剩余需求，而是当前未完成请求。因此它是对当前资源依赖进行安全性模拟，不能证明任意未来执行路径都不会死锁。

两线程两锁的例子：

```text
A 持有 L0，B 持有 L1：
Available = [0, 0]
Allocation[A] = [1, 0]
Allocation[B] = [0, 1]

A 请求 L1：
Need[A] = [0, 1]，Need[B] = [0, 0]
模拟 B 完成后 Work = [0, 1]，A 随后可完成，请求被接受

B 再请求 L0：
Need[A] = [0, 1]，Need[B] = [1, 0]
没有线程满足 Need <= [0, 0]，拒绝 B 的新请求
```

拒绝只撤销 B 本次 Need，不释放 B 已持有的 L1。B 必须处理失败并释放相应资源，A 才能继续。检测器不是自动终止线程或强制解锁的恢复机制。

### 3.7 `os/src/syscall/sync.rs`：连接对象与用户接口

创建系统调用在 PCB 对象表中保存 Arc 并返回 ID；操作系统调用按 ID 克隆对象，释放 PCB 借用后转发到实际方法。

```text
用户 mutex_lock / semaphore_down / condvar_wait
  -> ecall -> trap_handler -> syscall
  -> sys_mutex_lock / sys_semaphore_down / sys_condvar_wait
  -> 当前进程对象表 -> 克隆 Arc -> 释放 PCB 借用
  -> 同步对象方法
     -> 获取许可、阻塞等待或拒绝请求
  -> 返回值写入用户 a0
```

获取路径中的死锁错误由 mutex lock、semaphore down 和内核 condvar wait 返回。开关函数只接受 0、1，其他输入返回 -1。关闭检测时仍更新资源记录，开关只决定是否拒绝不安全请求；开启它不会自动修复已经存在的死锁。

当前对象查找仍直接使用下标和 unwrap，非法对象 ID 的处理并不完整。本次报告编写没有修改这些既有接口。

## 4. 测试结果

### 4.1 已有验证记录与本次范围

前期实现记录中，死锁检测代码已通过内核编译、第八章 25 项评测和 6 项算法边界测试。这是此前的验证记录，不是编写本报告时重新执行得到的结果，也不能说明所有等待协议和异常路径都已覆盖。

本次仅编写实验报告，并对照当前 PCB、TCB、mutex、semaphore、condvar、同步系统调用和调度器源码检查说明，没有重新启动编译或完整评测。因此不附新的测试日志或新的通过率。

### 4.2 重点检查的行为

| 检查内容 | 当前代码行为 |
| --- | --- |
| 同进程资源共享 | 多个 TCB 指向同一 PCB，地址空间、文件表和同步对象集中管理 |
| spin mutex 竞争 | 已锁时让出 CPU，回到就绪队列后重试 |
| blocking mutex 竞争 | 已锁时排队阻塞，unlock 在唤醒前直接交接 |
| semaphore 许可不足 | count 变负后阻塞，up 交接许可并唤醒队首 |
| condvar 等待 | 释放 mutex、入队、阻塞，恢复后重新获取 mutex |
| 不安全请求 | 返回 `-0xdead`，撤销本次请求，不污染实际队列和资源计数 |
| 非法检测开关 | 参数不为 0 或 1 时返回 `-1` |

这些是源码核对结果，不是新增运行时测试。尤其 condvar 的应用层协议、线程持锁退出和非法 ID，需要专项运行验证才能进一步评价。

## 5. 实验总结

本章的核心变化是将资源管理与执行调度分离。PCB 保存进程共享资源，TCB 保存线程独立现场；线程通过 Arc 和 Weak 与进程关联，再通过就绪队列与等待队列实现运行、阻塞和唤醒。

mutex 保护独占临界区，semaphore 管理计数许可，condvar 等待共享条件。阻塞 mutex 和 semaphore 在唤醒前完成资源交接，condvar 只通知等待者，恢复后仍需重新获取 mutex 并检查条件。这一区别决定了它们各自的使用协议。

本次新增的检测器将 mutex 和 semaphore 纳入统一资源矩阵，在请求执行前分析当前依赖，不安全时拒绝并回滚本次请求。它不替代正确的错误处理，也不覆盖 condvar、管道或 waittid 等全部等待关系。

当前实现还依赖单核、内核不可抢占，没有完善的 mutex 所有者检查与持锁退出恢复机制；TID 复用也不能恢复遗留锁的所有权。用户高层 condvar wait 丢弃重新加锁错误，是开启检测后需要特别注意的接口边界。实验中的同步机制可用于理解线程协作与资源等待，但不能直接视为完整的多核同步库。
