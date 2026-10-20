# 第五章 `sys_spawn` 实现详解

## 1. 功能与调用约定

`sys_spawn` 创建一个新进程，让它去执行指定的程序，调用者继续运行。它与 `fork` 的区别在于调用者的命运，与 `exec` 的区别在于是否会创建新进程。

用户库的封装如下，系统调用编号为 `400`：

```rust
pub fn spawn(path: &str) -> isize {
    sys_spawn(path)
}
```

| 参数或返回值 | 含义 |
| --- | --- |
| `path` | 用户态字符串的地址，内容是要执行的程序名，以 NUL 结尾 |
| 返回正数 | 新建子进程的 pid |
| 返回 `-1` | 没有找到名为 `path` 的程序，此时不创建任何进程 |

### 1.1 与 `fork`、`exec` 的关系

| 操作 | 调用者 | 是否创建新进程 | 新进程执行的程序 | 返回值 |
| --- | --- | --- | --- | --- |
| `fork` | 继续运行 | 是，复制当前进程 | 与原进程相同 | 父进程得到子进程 pid，子进程得到 `0` |
| `exec` | 自身被替换 | 否 | 指定程序 | 成功时不返回，从新程序入口开始执行 |
| `spawn` | 继续运行 | 是，新建进程 | 指定程序 | 子进程 pid |

`spawn` 的组合方式是"先 fork，再让子进程 exec"：fork 负责得到一个独立的进程，exec 负责把这个进程换成目标程序。缺少任何一步都不对：只 fork 不 exec，子进程会继续跑调用者的程序；直接对调用者 exec，就是把调用者自己换掉了。

### 1.2 实现中的顺序

本次实现按下面的顺序完成，这个顺序本身是有意义的：

```text
翻译路径字符串
  -> 查表取得 ELF 数据
     -> fork 得到子进程
        -> 在子进程上 exec 目标程序
           -> 加入调度器的就绪队列
              -> 返回子进程 pid
```

先把子进程准备完整，再交给调度器，避免出现"已经可以被调度、但内容还没准备好"的中间状态。

## 2. 完整调用链

从用户程序调用 `spawn` 到子进程真正在用户态执行，中间分成两条线。第一条在系统调用里同步完成，第二条由调度器稍后触发。

### 2.1 创建线：内核同步完成

```text
用户 spawn("ch5_getpid\0")
  -> ecall，a7 = 400
trap_handler
  -> syscall(400, [path_ptr, 0, 0])                 [syscall/mod.rs]
     -> sys_spawn                                   [syscall/process.rs]
        -> current_user_token()                     取当前进程的页表 token
        -> translated_str(token, path_ptr)          按页表逐字节读出字符串
        -> get_app_data_by_name(name)               从内核内嵌的程序镜像中查名字
        -> current_task()                           当前进程的 Arc<TaskControlBlock>
        -> TaskControlBlock::fork(self: &Arc<Self>)  [task/task.rs]
           -> MemorySet::from_existed_user(父地址空间)
           -> pid_alloc() / kstack_alloc()
           -> 组装子进程 TCB，task_status = Ready
           -> parent_inner.children.push(子进程)
           -> 子进程 trap_cx.kernel_sp = 子进程内核栈顶
        -> TaskControlBlock::exec(子进程, elf_data)
           -> MemorySet::from_elf(elf_data)
           -> 替换子进程的 memory_set / trap_cx_ppn / base_size
           -> *trap_cx = TrapContext::app_init_context(...)
        -> add_task(子进程)                          [task/manager.rs]
           -> TASK_MANAGER.ready_queue.push_back(子进程)
        -> 返回子进程 pid
```

`add_task` 只是把任务放进就绪队列，没有唤醒动作：

```rust
pub fn add(&mut self, task: Arc<TaskControlBlock>) {
    self.ready_queue.push_back(task);
}
```

### 2.2 运行线：调度器稍后触发

```text
run_tasks 循环（当前 hart 回到调度点）
  -> fetch_task()                                   从就绪队列取出子进程
     -> __switch(当前任务上下文, 子进程 task_cx)       [task/switch.S]
        -> 子进程 task_cx.ra = trap_return，sp = 子进程内核栈顶
           -> trap_return                            [trap/mod.rs]
              -> current_user_token()                取子进程的页表 token
              -> 跳进 trampoline 中的 __restore
                 -> csrw satp, 子进程 token           切换到子进程地址空间
                 -> sfence.vma
                 -> 从 TrapContext 恢复 sepc / sstatus / x[2]
                    用户态从 ch5_getpid 的入口开始执行
```

### 2.3 两条线的衔接点

衔接靠的是子进程 TCB 里的两个上下文：

| 结构 | 字段 | 值 | 作用 |
| --- | --- | --- | --- |
| `TaskContext`（`task_cx`） | `ra` | `trap_return` 的地址 | 第一次被 `__switch` 切进来时跳到 `trap_return` |
| `TaskContext`（`task_cx`） | `sp` | 子进程内核栈顶 | 子进程自己的内核栈 |
| `TrapContext` | `sepc` | 目标程序入口 | 从内核返回用户态时的起始指令 |
| `TrapContext` | `x[2]` | 目标程序的用户栈顶 | 用户态栈指针 |
| `TrapContext` | `kernel_satp` / `kernel_sp` / `trap_handler` | 内核页表 token、子进程内核栈顶、`__alltraps` | 下次陷入内核时的落脚点 |

`exec` 负责的是后三行，`fork` 负责的是前两行。两者都不调用 `__switch`：改状态与切执行流分开，由调度器在合适的时刻完成切换。

## 3. 逐步说明

### 3.1 读取路径字符串

```rust
let token = current_user_token();
let path = translated_str(token, path);
```

`translated_str` 用当前进程的页表逐字节翻译地址，直到读到 `0`：

```rust
pub fn translated_str(token: usize, ptr: *const u8) -> String {
    let page_table = PageTable::from_token(token);
    let mut string = String::new();
    let mut va = ptr as usize;
    loop {
        let ch: u8 = *(page_table.translate_va(VirtAddr::from(va)).unwrap().get_mut());
        if ch == 0 {
            break;
        } else {
            string.push(ch as char);
            va += 1;
        }
    }
    string
}
```

这一步必须在 fork 之前做。fork 之后如果拿的还是父进程的 token，读的是父进程的地址空间，而路径字符串本来就存在父进程的用户栈上，看似也能读到；但语义上养成"谁的用户地址就用谁的 token 翻译"的习惯更安全，也避免了跨进程后再回头读父进程内存的混乱。

字符串只在用户栈上，长度由 `0` 结束符决定，函数本身没有长度上限，也不会检查地址是否有效——`translate_va()` 内部会 `unwrap()`。也就是说，用户如果传一个没有结束符或者越界的地址，内核会 panic 而不是返回 `-1`。这是当前实现的边界。

### 3.2 查表取得 ELF

```rust
if let Some(data) = get_app_data_by_name(path.as_str()) {
```

`get_app_data_by_name` 在编译期由 `build.rs` 嵌入内核的程序镜像里按名字查找，返回 `Option<&'static [u8]>`。找不到就返回 `-1`，此时 fork 还没有发生，没有任何进程被创建。

### 3.3 fork 复制地址空间

```rust
let task = current_task().unwrap();
let new_task = task.fork();
let new_pid = new_task.pid.0;
```

`fork` 的签名是 `pub fn fork(self: &Arc<Self>) -> Arc<Self>`，要求 `Arc` 是因为它需要做两件事：把子进程放进父进程的 `children` 列表，并给子进程一个指向父进程的 `Weak` 引用。引用计数在这里决定了进程树的形状。

内部分三步：

1. `MemorySet::from_existed_user(&parent_inner.memory_set)` 复制父进程的整个地址空间。它遍历父进程的每个 `MapArea`，用 `MapArea::from_another` 新建同范围、同权限的区域，然后逐页把父进程物理页的内容复制到新的物理页。程序段、用户栈、TrapContext 页都会被复制一份。
2. `pid_alloc()` 分配 pid，`kstack_alloc()` 在**内核地址空间**里插入一段内核栈（`KERNEL_STACK_SIZE` 为 8192 字节，即两页），并把这段区域登记到 `KERNEL_SPACE`。
3. 组装新的 `TaskControlBlock`，`task_status` 为 `Ready`，`task_cx` 用 `TaskContext::goto_trap_return(kernel_stack_top)` 初始化，然后把子进程压进父进程的 `children`。

最后还有一行容易被忽略：

```rust
let trap_cx = task_control_block.inner_exclusive_access().get_trap_cx();
trap_cx.kernel_sp = kernel_stack_top;
```

子进程的 TrapContext 是从父进程复制来的，里面的 `kernel_sp` 还指向父进程的内核栈，必须改成子进程自己的。否则子进程下一次陷入内核时会踩到父进程的栈。exec 之所以不需要这一步，是因为它会重建整个 TrapContext。

### 3.4 exec 替换子进程的程序

```rust
new_task.exec(data);
add_task(new_task);
```

`exec` 只做三件事：

- `MemorySet::from_elf(elf_data)` 建立新程序的地址空间，包含代码段、用户栈、TrapContext 页与 trampoline；
- 用新的 `memory_set`、`trap_cx_ppn`、`base_size` 覆盖子进程内部结构的对应字段；
- 用 `TrapContext::app_init_context(entry_point, user_sp, KERNEL_SPACE 的 token, 子进程内核栈顶, trap_handler)` 重建 TrapContext。

`app_init_context` 会把 `x[0..32]` 全部清零，再设置 `sepc = entry_point`、`x[2] = user_sp`，因此新程序启动时 `a0`（`x[10]`）为 `0`，不需要额外清零。

注意 exec 没有改动 `children` 与 `parent`，也没有改动 `task_cx`。前者意味着 exec 不会切断进程树关系，后者意味着 exec 不负责切换执行流——这正是它不需要调用 `__switch` 的原因。

对 spawn 来说，`self` 是刚 fork 出来的子进程，所以它的地址空间被换成了目标程序，而父进程的地址空间完全不受影响。代价是 fork 时复制的那份地址空间紧接着就被丢掉了；这是本实验采用的标准做法，直接构造新 TCB 会更省，但会把创建流程拆成两条分支。

### 3.5 加入就绪队列

```rust
add_task(new_task);
```

`add_task` 把子进程的 `Arc` 压入全局就绪队列。压入之后子进程就具备被调度的资格了，但由于此刻仍在系统调用中、没有发生调度，父进程会继续执行完 `sys_spawn` 并返回用户态。

### 3.6 子进程第一次运行

当父进程因为时间片或 `yield` 让出 CPU 时，调度循环取到子进程，执行 `__switch`。此时子进程的 `TaskContext` 里 `ra` 是 `trap_return`、`sp` 是它自己的内核栈顶，所以：

```text
__switch
  -> trap_return
     -> 取当前任务的 token 与 TRAP_CONTEXT_BASE 定位 TrapContext
     -> __restore：csrw satp, token; sfence.vma
     -> 恢复 sstatus / sepc / 通用寄存器
        -> 用户态从 entry_point 开始执行
```

`TRAP_CONTEXT_BASE` 在每个用户地址空间里是同一个虚拟地址，所以 `trap_return` 不用关心是哪个进程；具体落到哪一页，由 `satp` 里的 token 决定。

## 4. 一个具体例子

以 ch5 的 `ch5_spawn0` 为例，父进程循环 `spawn("ch5_getpid\0")` 共 40 次。

假设父进程 pid 为 `1`，第一次 spawn 得到的子进程 pid 为 `2`：

| 对象 | 内容 |
| --- | --- |
| 父进程返回值 | `a0 = 2`，用户库的 `spawn()` 返回 `2`，程序打印 `new child 2` |
| 子进程 pid | `2` |
| 子进程 `task_cx` | `ra = trap_return`，`sp = 子进程内核栈顶` |
| 子进程 `TrapContext.sepc` | `ch5_getpid` 的入口地址 |
| 子进程 `TrapContext.x[2]` | `ch5_getpid` 的用户栈顶 |
| 子进程 `TrapContext.x[10]` | `0` |
| 子进程 `TrapContext.kernel_satp` | 内核地址空间的 token |
| 子进程 `TrapContext.kernel_sp` | 子进程内核栈顶 |
| 子进程 `TrapContext.trap_handler` | `__alltraps` 的地址 |
| 父进程 `children` | 长度 1，元素是指向子进程的 `Arc` |

循环跑完后父进程已经有 40 个子进程，随后调用 `wait` 逐个回收。子进程执行 `ch5_getpid` 后退出，父进程的 `waitpid` 在 `children` 里找到状态为 `Zombie` 的子进程，取出它的 `exit_code` 写回用户地址，并返回它的 pid。`waitpid` 里有一句断言：

```rust
assert_eq!(Arc::strong_count(&child), 1);
```

它确认这个子进程在被移出 `children` 之后只剩当前这一份引用，随后的析构才会真正释放它的地址空间、内核栈和 pid。

## 5. 边界与注意事项

- `fork` 与 `exec` 的组合会先复制父进程的地址空间再丢弃，开销集中在 `MemorySet::from_existed_user` 的逐页复制上。子进程越多，这一步越明显。
- `translated_str` 没有长度上限，遇到没有 NUL 结束符的地址会一直读下去；地址无效时 `translate_va()` 内部的 `unwrap()` 会让内核 panic。这是仓库提供的既有辅助函数的行为，本次实现沿用它，没有额外加长度校验。
- 找不到程序时返回 `-1`，此时 fork 尚未发生，不会留下半成品子进程。
- 路径字符串必须是 NUL 结尾，且位于用户可读的页内。
- `exec` 不会清空 `children` 与 `parent`，进程树关系在换程序之后依然保留。
- 内核栈与 pid 由 `KernelStack`、`PidHandle` 的 `Drop` 负责回收，进程退出后自动归还，不需要在 spawn 路径上手工释放。
- 本章还没有文件描述符表，`spawn` 出来的子进程只共享内核里已嵌入的程序镜像；到第六章引入文件系统后，这两者才需要一起处理。

## 6. 小结

`spawn` 做的事情可以概括为：读出用户给的程序名，从内核镜像里取出对应 ELF，fork 出一个独立进程，把它的地址空间换成目标程序，最后交给调度器。

整条链路上有两个关键分工。一是"改状态"与"切执行流"分开：`fork` 与 `exec` 只修改 TCB 里的上下文和地址空间，真正的切换由调度器的 `__switch` 和返回用户态的 `__restore` 完成；二是"谁的用户地址用谁的 token"：路径字符串用父进程的 token 翻译，子进程第一次返回用户态时用子进程的 token 写 `satp`，两次都用 `TRAP_CONTEXT_BASE` 这个各地址空间共享的虚拟地址定位 TrapContext。
