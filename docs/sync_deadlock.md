# 第八章 mutex、信号量、条件变量与死锁检测详解

## 1. 本章要解决的问题

本章引入多线程后，同一进程的线程共享地址空间和文件表。共享数据的读写可能交错，因此需要两类协作机制：

- 互斥：限制同时进入临界区的线程数量。
- 同步：当资源或条件尚未满足时，让线程等待，并在条件变化后唤醒它。

mutex、信号量和条件变量解决不同的问题，不能简单互相替代。

| 机制 | 核心语义 | 等待时是否释放相关 mutex |
| --- | --- | --- |
| mutex | 同一时刻只允许一个线程持有锁 | 不涉及自动释放其他锁 |
| semaphore | 消耗或增加计数，表示资源数或通知数 | 不会自动释放其他锁 |
| condvar | 等待共享状态满足某个条件 | `wait` 释放指定 mutex，唤醒后重新获取 |

同步对象能阻止不正确的交错，但并不自动防止死锁。例如两个线程分别持有一把锁，再请求对方的锁，就可能互相等待。

本章新增死锁检测器，在获取 mutex 或执行信号量 `down` 前分析当前资源依赖；发现本次请求会使模型不安全时，拒绝请求并返回 `-0xdead`。

该机制拒绝的是新的请求，不会自动杀死线程、强制解锁，也不会自动撤销调用者此前已经完成的操作。

## 2. 实现文件与调用层次

| 文件 | 职责 |
| --- | --- |
| `user/src/lib.rs` | 用户使用的同步接口 |
| `user/src/syscall.rs` | 系统调用参数和返回值封装 |
| `os/src/syscall/mod.rs` | 按编号分发系统调用 |
| `os/src/syscall/sync.rs` | 查找当前进程的同步对象并转发 |
| `os/src/sync/mutex.rs` | 自旋式和阻塞式 mutex |
| `os/src/sync/semaphore.rs` | 信号量计数、等待和交接 |
| `os/src/sync/condvar.rs` | 条件变量等待队列 |
| `os/src/task/process.rs` | 进程同步对象表及死锁检测器 |
| `os/src/task/mod.rs` | 线程暂停、阻塞、退出 |
| `os/src/task/manager.rs` | 就绪队列与唤醒 |
| `os/src/sync/up.rs` | 内部可变数据的独占借用 |

一次 mutex 获取的完整路径为：

```text
用户 mutex_lock(id)
  -> 用户 sys_mutex_lock(id)
  -> ecall
  -> trap_handler
  -> syscall 分发
  -> 内核 sys_mutex_lock(id)
     -> current_process()
     -> mutex_list[id]
     -> 克隆 Arc，释放 PCB 内部借用
     -> MutexSpin::lock 或 MutexBlocking::lock
        -> deadlock.request
        -> 获取成功，或等待，或返回 -0xdead
  -> trap_handler 将结果写入 a0
  -> 返回用户态
```

## 3. 同步对象属于进程，等待对象是线程

PCB 中保存：

```rust
pub mutex_list: Vec<Option<Arc<dyn Mutex>>>,
pub semaphore_list: Vec<Option<Arc<Semaphore>>>,
pub condvar_list: Vec<Option<Arc<Condvar>>>,
pub deadlock: DeadlockDetector,
```

用户传入的对象 ID 是相应表的下标。它不是对象地址，也不是系统全局编号。

同一进程的线程可通过相同 ID 访问同一个同步对象。不同进程各自的 `mutex_id = 0` 不代表同一把锁。

等待队列保存的是：

```rust
VecDeque<Arc<TaskControlBlock>>
```

原因是实际被阻塞和唤醒的调度单位为线程。队列中的 `Arc` 让等待对象在离开就绪队列后仍然存在；克隆它不复制线程。

### 3.1 为什么转发前要 `drop(process_inner)`

系统调用先取得独立的对象句柄：

```rust
let mutex = Arc::clone(process_inner.mutex_list[mutex_id].as_ref().unwrap());
drop(process_inner);
mutex.lock()
```

`lock` 内部要再次借用 PCB，以更新 `deadlock`。如果仍持有之前的 `RefMut`，再次独占借用会发生运行时借用冲突。

同时，阻塞或让出 CPU 前也不能一直持有同步对象的内部借用，否则下一线程访问同一对象时可能 panic。

`UPSafeCell` 基于 `RefCell`，不是硬件自旋锁。这份代码依赖单核、内核态不可抢占的运行条件；不能直接视为一般多核同步实现。

### 3.2 暂停、阻塞、唤醒的区别

| 操作 | 状态变化 | 就绪队列 |
| --- | --- | --- |
| `suspend_current_and_run_next` | `Running -> Ready` | 当前线程重新入队 |
| `block_current_and_run_next` | `Running -> Blocked` | 当前线程不入队 |
| `wakeup_task` | `Blocked -> Ready` | 等待线程重新入队 |

唤醒只使线程可以被调度，不保证立即执行。等到调度器取出它，`__switch` 恢复其内核上下文后，它才会从此前暂停的内核调用位置继续。

## 4. mutex：保护临界区

### 4.1 两种实现

```rust
pub trait Mutex: Sync + Send {
    fn lock(&self) -> isize;
    fn unlock(&self);
}
```

`sys_mutex_create(blocking)` 根据参数创建：

- `blocking == false`：`MutexSpin`。
- `blocking == true`：`MutexBlocking`。

每把 mutex 在检测器中登记一类数量为 1 的资源。

`MutexSpin` 的状态是 `locked: UPSafeCell<bool>`；`MutexBlocking` 的状态还包含等待队列。

### 4.2 `MutexSpin::lock`：让出 CPU 后重试

执行顺序：

```text
登记本次资源请求
  -> 不安全：返回 -0xdead
  -> 可接受：进入循环
     -> locked == true
        -> 释放 locked 的借用
        -> suspend_current_and_run_next
        -> 下次恢复后重新检查
     -> locked == false
        -> locked = true
        -> deadlock.acquire
        -> 返回 0
```

这不是一直占据 CPU 执行空循环的纯忙等。它仍属于反复竞争型获取，但每次失败主动让出 CPU；线程一直处于可运行状态，没有进入锁的等待队列。

`request` 只在循环前调用一次，因此等待过程中不会重复增加 Need。

释放时，`unlock` 将 `locked` 改为 false，并调用 `deadlock.release`。没有指定唤醒者，后续就绪线程通过重试竞争锁。

因此 `MutexSpin` 不提供严格 FIFO 的锁获取顺序。

### 4.3 `MutexBlocking::lock`：排队睡眠

若锁空闲：

```text
locked = true
deadlock.acquire(tid, resource)
返回 0
```

若锁已经被持有：

```text
将当前线程放到 wait_queue 尾部
释放 mutex_inner 的借用
block_current_and_run_next
线程不再参与就绪队列调度
```

唤醒后，`lock()` 从阻塞调用后继续并返回 0，不再次竞争锁。这个行为成立的关键在 `unlock` 的资源交接。

### 4.4 阻塞 mutex 的释放与直接交接

`MutexBlocking::unlock` 先登记当前线程释放资源，再检查等待队列：

```text
等待队列非空：
  取出队首线程 B
  deadlock.acquire(B, resource)
  wakeup_task(B)
  locked 保持 true

等待队列为空：
  locked = false
```

有等待者时，锁不是先变为空闲再由大家抢，而是直接交给队首线程。

例如：

```text
A 持有锁，B 和 C 等待
  -> A unlock
  -> B 被登记为持有者，进入 Ready
  -> locked 仍为 true
  -> 新到达的 D 不能在 B 运行前抢走锁
```

等待者按队列顺序取得交接，但真正运行时间仍由调度器决定。

### 4.5 使用约定

典型逻辑：

```text
若 mutex_lock 返回 0：
  访问共享数据
  mutex_unlock
否则：
  不进入临界区，处理获取失败
```

当前 mutex 没有独立 owner 字段，也不支持递归加锁。程序应由持有线程执行一次对应的 unlock，不应重复解锁或替其他线程解锁。

阻塞 mutex 只断言锁处于 locked 状态，不能保证解锁者确为持有者。错误使用还可能使实际锁状态与检测器记录不一致。

## 5. semaphore：计数资源与通知

### 5.1 数据结构

```rust
pub struct SemaphoreInner {
    pub count: isize,
    pub wait_queue: VecDeque<Arc<TaskControlBlock>>,
}
```

初始化 `count = res_count`。`down` 又称 P 操作，消耗一个许可；`up` 又称 V 操作，提供一个许可。

在正常使用下：

- `count > 0`：有许可可立即取得。
- `count == 0`：无剩余许可，也没有尚未获交接的等待者。
- `count < 0`：无剩余许可，绝对值反映等待数量。

`count` 可以为负，检测器的 `Available` 不可以为负。正常状态下，可立即分配量对应 `max(count, 0)`，两者不是同一个变量。

### 5.2 `down`：先检测，再减少计数

```text
deadlock.request(tid, resource)
  -> 被拒绝：返回 -0xdead，count 和队列不变
  -> 被接受：
     count -= 1
     -> count >= 0：acquire，返回 0
     -> count < 0：排队并阻塞
```

等待中的请求保留在 Need 中，但尚未计入 Allocation。

### 5.3 `up`：增加计数并交接一个许可

```text
deadlock.release(tid, resource)
count += 1
  -> count <= 0：
     取出一个等待者
     deadlock.acquire(next_tid, resource)
     wakeup_task
  -> count > 0：
     留下可供后续 down 使用的许可
```

和阻塞 mutex 一样，等待者在唤醒前已经获得许可记录。它运行后不会再次减 count 或再次 acquire。

### 5.4 具体数值推演

信号量初值为 2：

| 操作 | count | Available | 效果 |
| --- | --- | --- | --- |
| 初始化 | 2 | 2 | 两个可用许可 |
| A down | 1 | 1 | A 获得一个许可 |
| B down | 0 | 0 | B 获得一个许可 |
| C down，被接受 | -1 | 0 | C 阻塞，Need 增加 |
| A up | 0 | 0 | A 释放，许可直接交给 C |
| B up | 1 | 1 | 无等待者，留下一个许可 |

表格假设检测关闭或所有列出的请求通过检查。`A up` 内部 Available 短暂增加再被 C acquire 减去，但这段更新在当前内核模型下不会被其他线程插入观察。

### 5.5 信号量不要求 up 者之前执行过 down

这是信号量与 mutex 的重要差别。例如：

```text
初始 semaphore = 0
消费者：down，等待数据
生产者：准备数据后 up，发出通知
```

生产者并没有持有某个许可。检测器的 `release` 因此先检查 Allocation 是否非零，有持有量才减一，但始终增加 Available。

然而，启用当前资源型检测器时，“零初值通知信号量”的等待可能被保守拒绝：算法没有用户未来生产事件的模型，可能认为等待者无法完成。适合资源容量分析的模型，不等于通用通知协议分析器。

## 6. condvar：等待共享状态

### 6.1 条件不是存放在 condvar 中的

`CondvarInner` 只有等待队列，没有计数，也没有“条件为真”的字段。

条件应存放在由 mutex 保护的共享数据中，例如：

```text
ready == true
队列非空
缓冲区尚有空位
```

condvar 只是让检查出条件不满足的线程暂时睡眠，并允许其他线程通知它重新检查。

### 6.2 `wait` 的执行顺序

当前代码：

```rust
pub fn wait(&self, mutex: Arc<dyn Mutex>) -> isize {
    mutex.unlock();
    let mut inner = self.inner.exclusive_access();
    inner.wait_queue.push_back(current_task().unwrap());
    drop(inner);
    block_current_and_run_next();
    mutex.lock()
}
```

调用者应在进入 `wait` 前持有传入的 mutex：

```text
持锁检查条件
  -> 条件不满足
  -> wait 释放 mutex
  -> 当前线程加入 condvar 等待队列
  -> 当前线程 Blocked
  -> 收到 signal，进入 Ready
  -> 恢复后重新获取 mutex
  -> 获取成功后返回，重新检查条件
```

若不释放 mutex 就睡眠，负责改变条件的线程可能无法获得锁，也就不能使条件满足。

### 6.3 `signal` 只唤醒，不交接 mutex

`signal` 从条件变量等待队列头部取出一个线程并调用 `wakeup_task`。

它不自动解锁通知者的 mutex，不保证条件为真，也不保证被唤醒者立刻取得 mutex。

若通知者仍持有 mutex，被唤醒线程恢复后可能再次在 mutex 上等待。这与阻塞 mutex 的 unlock 直接交接锁不同。

这种“通知后重新竞争锁并检查条件”的使用方式符合 Mesa 风格。

### 6.4 为什么检查条件要用 while

推荐协议，以下是行为伪代码：

```text
等待方：
  获取 mutex，确认成功
  while ready == false:
    执行 condvar_wait
    若重新获取 mutex 失败：
      进入错误处理，不能继续按持锁状态访问共享数据
  使用共享数据
  释放 mutex

通知方：
  获取同一把 mutex，确认成功
  修改共享数据，将 ready 设置为 true
  condvar_signal
  释放 mutex
```

使用 while，不是因为当前实现会随机唤醒线程，而是唤醒与实际运行之间存在时间间隔；其他线程可能先消耗资源，使条件再次变为不满足。

`signal` 时若无等待者，调用没有记忆效果。以后进入 wait 的线程不能消费此前的 signal。若需要积累通知次数，应使用信号量或显式共享状态。

当前仅提供单线程唤醒的 signal，没有 broadcast 接口。

### 6.5 当前实现为什么没有解锁与入队间的通知空隙

源码先 `mutex.unlock()`，再把线程放到 condvar 队列。一般多核实现中，这两步之间可能发生通知，从而丢失唤醒。

当前代码依赖单核、内核态不可抢占：

- `unlock` 可以把其他线程变为 Ready，但不会立即切换过去。
- 当前线程在显式 `block_current_and_run_next` 前完成入队。
- 其他线程运行时，等待者已经被登记且进入 Blocked。

因此，正确使用 mutex 的情况下，这段序列在当前执行模型中完成必要的等待登记。若改为多核或可抢占内核，需要额外机制保证释放锁、登记等待和睡眠的协调，不能照搬这段代码。

### 6.6 重新加锁失败的返回值

`Condvar::wait` 最终返回 `mutex.lock()` 的结果。开启检测后，这次重新加锁也可能返回 `-0xdead`。

内核 `sys_condvar_wait` 会传播这个错误，但当前用户高层接口：

```rust
pub fn condvar_wait(condvar_id: usize, mutex_id: usize) {
    sys_condvar_wait(condvar_id, mutex_id);
}
```

丢弃了返回值。因此要处理检测拒绝，应调用能够返回 `isize` 的 `user_lib::sys_condvar_wait`，或调整用户封装让返回值传到调用者。`syscall` 模块本身是私有的，但其中接口通过 `pub use syscall::*` 在用户库根模块重导出。

不能声称当前高层 `condvar_wait()` 无条件保证返回时持锁。只有重新加锁成功时才成立。

## 7. 死锁检测器的数据结构

实现位于 `os/src/task/process.rs`：

```rust
pub struct DeadlockDetector {
    pub enabled: bool,
    available: Vec<usize>,
    allocation: Vec<Vec<usize>>,
    need: Vec<Vec<usize>>,
    live: Vec<bool>,
}
```

设 n 为线程行数，m 为资源类数：

| 字段 | 尺寸 | 含义 |
| --- | --- | --- |
| Available | m | 当前未分配的资源数量 |
| Allocation | n × m | 各线程已经取得的资源 |
| Need | n × m | 各线程尚未实际满足的请求 |
| live | n | 对应 TID 是否活跃 |

矩阵行按进程内 TID 标识，列按检测器内部 resource ID 标识。

### 7.1 对象 ID 与 resource ID 不相同

mutex 和 semaphore 各自有编号表，但共同使用一个检测器：

```text
mutex_list[0]       -> resource 0，初始 Available = 1
semaphore_list[0]   -> resource 1，初始 Available = 3
mutex_list[1]       -> resource 2，初始 Available = 1
```

统一资源列能识别 mutex 与 semaphore 混合形成的依赖。condvar 不登记资源列，因为它不是具有固定可分配数量的资源。

### 7.2 动态扩展

`add_resource(count)` 追加 Available 元素，并给每个 Allocation、Need 行追加 0。

`add_thread(tid)` 扩展线程行，设置 live，并清空该 TID 行的记录。

`exit_thread(tid)` 将 live 改为 false，清空未完成请求；不会假定线程已经释放所有持有资源。

## 8. 请求、分配与释放的更新规则

### 8.1 `request`

```text
Need[tid][resource] += 1

若 enabled 且不安全：
  Need[tid][resource] -= 1
  返回 false
否则：
  返回 true
```

分析的是包含本次请求的假设状态；拒绝后撤销的只有本次 Need。

这一步不更改 mutex.locked、semaphore.count 或真实等待队列。

### 8.2 `acquire`

```text
Available[resource] -= 1
Need[tid][resource] -= 1
Allocation[tid][resource] += 1
```

只有真正取得许可或完成交接时调用。请求被接受不等于资源已经分配。

### 8.3 `release`

```text
若 Allocation[tid][resource] > 0：
  Allocation[tid][resource] -= 1

Available[resource] += 1
```

允许信号量由未持有许可者 up。对 mutex 则必须遵守持有者解锁约定，否则这个通用更新不能代替 owner 检查。

### 8.4 为什么在唤醒前登记分配

若先唤醒 B，却等 B 实际运行后才增加 Allocation，则期间检测器可能把已承诺给 B 的资源仍看成可用，或遗漏 B 持有资源引起的依赖。

当前阻塞 mutex 和 semaphore 都采用：

```text
release(A) -> acquire(B) -> wakeup_task(B)
```

真实资源对象与检测器在交接时一起更新。

## 9. 安全性检查算法

`is_safe` 创建模拟副本：

```rust
let mut work = self.available.clone();
let mut finish: Vec<bool> = self.live.iter().map(|live| !live).collect();
```

活跃线程初值为未完成，已退出线程直接标记为不参与模拟。它们遗留的 Allocation 不会因此加入 Work。

算法反复扫描活跃且未完成的线程，判断所有资源列：

```text
Need[i][j] <= Work[j]
```

满足时假设该线程能执行完毕并释放现有持有量：

```text
Work[j] += Allocation[i][j]
Finish[i] = true
```

如果一轮没有任何进展，就返回所有 Finish 是否为 true。

### 9.1 为什么只加 Allocation

假设线程还需 Need 个资源：

```text
分配请求：Work -= Need
完成释放：Work += Need + Allocation
```

两步合起来，净效果为 `Work += Allocation`。所以代码没有漏算本次需求，而是将模拟的分配与释放合并了。

算法不改变实际 Available，不真正运行线程，也不真的解锁。

### 9.2 复杂度

每轮最多检查 n 个线程，每个线程检查或更新 m 个资源列；最多有 n 轮取得新进展，因此上界约为 O(n²m)。

检测器长期记录占用 O(nm)，一次检查额外创建 Work 和 Finish，占用 O(m+n)。

## 10. 两线程、两锁的手算示例

资源顺序为 `[L0, L1]`。初始：

```text
Available = [1, 1]
Allocation[A] = [0, 0]
Allocation[B] = [0, 0]
Need[A] = [0, 0]
Need[B] = [0, 0]
```

### 10.1 A 取得 L0，B 取得 L1

实际分配后：

```text
Available = [0, 0]
Allocation[A] = [1, 0]
Allocation[B] = [0, 1]
```

### 10.2 A 请求 L1

暂存请求：

```text
Need[A] = [0, 1]
Need[B] = [0, 0]
Work = [0, 0]
```

A 不能完成，但 B 当前没有未完成请求，模拟允许 B 先完成并释放 L1：

```text
B 完成：Work = [0, 1]
A 完成：Work = [1, 1]
```

状态安全，A 的请求被接受。真实 L1 仍被 B 持有，所以 A 等待；模拟结果不代表 B 真的已经执行了 unlock。

### 10.3 B 再请求 L0

暂存后：

```text
Need[A] = [0, 1]
Need[B] = [1, 0]
Work = [0, 0]
```

两个线程都不满足 Need <= Work，无法推进。本次 B 的请求被拒绝：

```text
B 的 mutex_lock(L0) 返回 -0xdead
Need[B] 恢复为 [0, 0]
B 不进入 L0 的等待队列
B 原先持有的 L1 不会自动释放
```

B 应处理失败，例如释放 L1。随后 A 可以获得 L1 并继续，循环等待才真正被解除。

若 B 忽略返回值并一直持有 L1，A 仍然可能长期等待。拒绝请求需要与用户正确的失败处理配合。

## 11. 检测开关与返回值

`sys_enable_deadlock_detect(enabled)`：

```rust
if enabled > 1 {
    return -1;
}
current_process().inner_exclusive_access().deadlock.enabled = enabled == 1;
0
```

| 参数 | 行为 |
| --- | --- |
| 0 | 关闭本进程检测，成功返回 0 |
| 1 | 开启本进程检测，成功返回 0 |
| 其他值 | 返回 -1，原状态不变 |

检测器默认关闭。关闭只是不拒绝不安全请求，资源记录仍继续更新；运行中开启不会从零开始记录。

开启动作本身不检查现有状态，也不唤醒已经死锁的线程。因此不是“开启后立即修复已有死锁”。

新进程和 fork 子进程创建新的检测器；当前 exec 没有重新初始化检测器。

同步系统调用编号与返回值：

| 系统调用 | 编号 | 正常返回 |
| --- | --- | --- |
| mutex_create | 463 | mutex ID |
| mutex_lock | 464 | 0，检测拒绝为 -0xdead |
| mutex_unlock | 466 | 0 |
| semaphore_create | 467 | semaphore ID |
| semaphore_up | 468 | 0 |
| enable_deadlock_detect | 469 | 0，非法参数为 -1 |
| semaphore_down | 470 | 0，检测拒绝为 -0xdead |
| condvar_create | 471 | condvar ID |
| condvar_signal | 472 | 0 |
| condvar_wait | 473 | 重新获取 mutex 的返回值 |

这些是合法对象 ID 的正常路径。当前许多查找直接使用下标与 `unwrap`，不保证非法对象 ID 都返回 -1。

## 12. 检测机制的适用范围与限制

### 12.1 Need 不是最大需求声明

当前 Need 只记录已经发出的未满足请求，没有程序未来需求的上界。这是依据当前等待关系进行的安全性模拟，不是完整的银行家最大需求模型。

模拟认为零 Need 的活跃线程可以完成并释放持有量，但该线程未来仍可能发出新的请求，届时需要再次检查。

### 12.2 不是所有等待都在模型中

直接覆盖：

- `MutexSpin::lock`。
- `MutexBlocking::lock`。
- `Semaphore::down`。

condvar 的等待队列本身、管道等待、waittid、waitpid、应用层条件依赖等没有被建成资源列。

例如 A 在 condvar 上等待 B 改变条件，但 B 正在等待 A 持有的另一把锁，此类混合依赖不能仅凭当前矩阵保证识别。

### 12.3 线程退出不自动释放锁

退出线程被标记为非活跃，不会在安全性模拟中被假设为还能完成并释放资源。这避免把遗留锁错误地当成可回收量。

但是当前实现没有健壮 mutex 或自动 owner 清理机制。TID 会复用，`add_thread` 也会清空相同行的记录；持锁退出后再复用该 TID，不能视为可靠的所有权恢复。程序应在线程退出前正确释放资源。

### 12.4 检测不代替正确的同步协议

即使所有资源请求都通过检查，也不说明共享数据没有竞争、条件变量没有错误通知、线程一定公平运行或程序一定终止。

更稳妥的程序设计仍包括：

- 对多把锁约定统一获取顺序。
- 缩短临界区，避免持锁等待不相关事件。
- 条件变量配合同一共享状态和 mutex，以 while 重查条件。
- 检查 lock、down 和低层 condvar wait 的失败返回。
- 退出或错误处理前释放已经取得的资源。

## 13. 总结

mutex 保护独占临界区；semaphore 表示可消耗的计数资源或通知；condvar 让线程等待由 mutex 保护的共享状态变化。

三者通过线程就绪与等待队列衔接调度器。阻塞 mutex 和信号量在唤醒前直接交接资源，条件变量只通知重新检查，恢复后仍需取得 mutex。

死锁检测器维护每进程的资源矩阵，在资源请求进入真实等待或分配流程前进行模拟，并拒绝会形成不安全状态的新请求。它的有效性依赖记录与实际分配保持一致，也依赖用户程序正确处理拒绝结果。
