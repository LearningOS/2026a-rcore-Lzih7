# 第八章 Trap、跳板与陷入返回详解

## 1. Trap 的含义与模块分工

Trap 是 CPU 将当前执行流转交给异常或中断处理程序的机制，通常译作“陷入”。它包括两类事件：

| 类型 | 特征 | 本章例子 |
| --- | --- | --- |
| 同步异常 | 由当前指令直接引起 | 用户 `ecall`、非法指令、访存页异常 |
| 异步中断 | 与当前指令的具体操作无直接关系 | Supervisor 定时器中断 |

系统调用是应用主动执行 `ecall` 引发的同步异常，不是普通函数调用，也不是外设中断。

本文以当前 RV64、Sv39、单核实现为准。进程提供用户地址空间，线程拥有独立执行现场、用户栈和内核栈；陷入处理针对当前线程进行。

| 文件 | 职责 |
| --- | --- |
| `os/src/trap/context.rs` | 定义 TrapContext，构造初始用户现场 |
| `os/src/trap/trap.S` | 保存用户现场、切页表与栈、恢复现场、执行 sret |
| `os/src/trap/mod.rs` | 设置入口，区分原因，分发系统调用、中断和异常 |
| `os/src/linker.ld` | 安排跳板代码的位置与页对齐 |
| `os/src/mm/memory_set.rs` | 在用户与内核页表映射共享跳板 |
| `os/src/task/id.rs` | 分配线程用户栈、TrapContext 页和内核栈 |
| `os/src/task/context.rs`、`switch.S` | 保存与切换内核执行流的 TaskContext |
| `os/src/task/processor.rs` | 调度线程，取得当前线程的现场与用户页表 |

## 2. 特权级、页表与栈是三件事

用户程序在 U 模式运行，内核在 S 模式运行。CPU 的特权级决定能执行哪些指令，页表决定虚拟地址映射到哪里，sp 决定当前使用的栈位置。

不能将“进入 S 模式”理解为硬件已经自动切换了内核页表与内核栈。本章这两项工作由汇编显式完成。

```text
用户执行：
  特权级 U + 进程用户页表 + 线程用户栈

刚陷入：
  特权级 S + 原用户页表 + 原用户 sp

__alltraps 准备完成：
  特权级 S + 公共内核页表 + 线程内核栈

__restore 的中间阶段：
  特权级 S + 进程用户页表 + sp 暂指 TrapContext

sret 返回完成：
  特权级 U + 进程用户页表 + 线程用户栈
```

`satp` 存放分页模式和根页表物理页号等信息。当前 token 为 `8usize << 60 | root_ppn.0`，表示 Sv39、ASID 为 0。token 不是裸的页表地址。

## 3. 硬件陷入时完成什么

对于已配置为交由 S 模式处理的 U 模式异常或中断，CPU 主要完成：

1. 将异常对应或中断返回位置写入 `sepc`。
2. 将原因写入 `scause`，必要的辅助信息写入 `stval`。
3. 将先前特权级记录到 `sstatus.SPP`。从 U 模式陷入时，SPP 为 User。
4. 将原 SIE 保存到 SPIE，并清除 SIE，禁止常规 S 模式中断嵌套。
5. 进入 S 模式，按 `stvec` 指定的入口开始执行。

硬件不会自动保存所有通用寄存器，不会给本章线程自动分配 TrapContext，不会改写 satp，也不会自动将用户 sp 换成内核 sp。

| CSR | 本章用途 |
| --- | --- |
| `stvec` | 陷入入口地址，当前使用 Direct 模式 |
| `sepc` | 后续 sret 的返回 PC |
| `scause` | 异常或中断类型 |
| `stval` | 页异常地址等辅助信息，具体含义由异常类型决定 |
| `sstatus` | SPP、SIE、SPIE 等特权与中断状态 |
| `sscratch` | 用户运行期间保存当前线程 TrapContext 的用户虚拟地址 |
| `satp` | 指定当前页表与分页模式 |
| `sie` | 各类 S 模式中断的使能位，例如 STIE |

并非所有 scause 都需要递增 sepc。用户 ecall 的 sepc 指向 ecall 本身；中断的 sepc 应直接用于恢复被中断的执行位置。

## 4. TrapContext 的布局与归属

### 4.1 Rust 结构与汇编偏移

`TrapContext` 使用 `#[repr(C)]`，使字段布局能与汇编中的固定偏移匹配：

```rust
pub struct TrapContext {
    pub x: [usize; 32],
    pub sstatus: Sstatus,
    pub sepc: usize,
    pub kernel_satp: usize,
    pub kernel_sp: usize,
    pub trap_handler: usize,
}
```

当前 RV64 中一个字宽为 8 字节：

| 字段 | 相对 TrapContext 起点的字节偏移 |
| --- | --- |
| `x[n]` | `n * 8` |
| `sstatus` | `32 * 8 = 256` |
| `sepc` | `33 * 8 = 264` |
| `kernel_satp` | `34 * 8 = 272` |
| `kernel_sp` | `35 * 8 = 280` |
| `trap_handler` | `36 * 8 = 288` |

当前布局共 296 字节，放在一张独立的 4 KiB 页中。修改字段顺序或宽度时必须同步修改汇编，不能只改 Rust 结构。

`x[2]` 是用户 sp，`kernel_sp` 是内核栈顶，两者不可混淆。最后三个字段由内核事先初始化，不是每次陷入时由用户寄存器保存得到的内容。

### 4.2 每线程一份，用户态不能访问

线程 tid 对应的 TrapContext 用户虚拟地址为：

```text
TRAP_CONTEXT_BASE - tid * PAGE_SIZE
```

当前地址为：

```text
TRAMPOLINE        = 0xfffffffffffff000
TRAP_CONTEXT_BASE = 0xffffffffffffe000

tid 0 TrapContext：0xffffffffffffe000
tid 1 TrapContext：0xffffffffffffd000
```

这张页在进程用户页表中设置 R、W，不设置 U。所谓“用户页表中的地址”不等于“用户态可访问”：进入 S 模式后，跳板才能通过这个地址读写现场。

TCB 的 `trap_cx_ppn: PhysPageNum` 保存它所在的物理页号，而不是结构体内容。内核使用 `trap_cx_ppn.get_mut()`，通过物理内存的恒等映射访问同一张页。

```text
用户页表中的高虚拟地址 ──> TrapContext 物理页
内核页表中的恒等映射地址 ─> 同一张物理页
```

因此 `current_trap_cx()` 不需要切回用户页表，也不需要复制 TrapContext。此实现中的原始指针转换仍依赖有效映射、对象存活和访问纪律，返回 `'static mut` 并不意味着可以任意制造多个可变别名。

## 5. 为什么需要共享跳板

页表切换时，CPU 后续取指也会使用新页表。若切换前后的当前代码虚拟地址不再有效，下一条指令就无法执行。

解决办法是在每个进程用户页表与公共内核页表中，将同一个 TRAMPOLINE 虚拟页映射到同一个物理代码页：

```text
进程 A：TRAMPOLINE ─┐
进程 B：TRAMPOLINE ─┼─> strampoline 所在物理页
内核：  TRAMPOLINE ─┘
```

`map_trampoline()` 直接建立 R、X 映射，没有 U，不分配新代码副本，也不加入普通 `areas`。

链接脚本先按 4 KiB 对齐，设置 `strampoline`，再收集 `.text.trampoline`。当前该节以 `__alltraps` 为起点，包含 `__restore`；页表映射覆盖一页，因此跳板代码必须保持在这一页内。

内核普通 text 还有恒等映射，所以同一物理跳板代码在内核页表中可以有低地址与 TRAMPOLINE 高地址两个别名。切页表时必须从双方共有的高地址别名执行。

跳板是共享代码，TrapContext 是每线程独立数据，内核栈也是每线程独立区域。

## 6. 入口设置与运行前约定

`init()` 实际调用 `set_kernel_trap_entry()`，将 stvec 设为 `__trap_from_kernel`，不是立即设置用户跳板入口。

每次返回用户态前，`trap_return()` 调用 `set_user_trap_entry()`，将 stvec 设为 TRAMPOLINE。用户运行期间的约定是：

```text
stvec    = TRAMPOLINE
sscratch = 当前线程 TrapContext 的用户虚拟地址
satp     = 所属进程的用户页表 token
sp       = 用户栈的当前栈指针
```

`trap_handler()` 开头再将 stvec 改为内核异常入口。因为用户陷入入口依赖上述 sscratch 约定，不能直接用于一般内核态异常。

## 7. __alltraps：逐段分析陷入汇编

### 7.1 汇编宏与代码节

```asm
.macro SAVE_GP n
    sd x\n, \n*8(sp)
.endm
.macro LOAD_GP n
    ld x\n, \n*8(sp)
.endm
```

宏在汇编阶段展开，不是运行时函数调用。`SAVE_GP 5` 等价于 `sd x5, 40(sp)`。`.rept 27` 从 x5 开始展开到 x31；`.altmacro` 配合 `%n` 使用循环计数值。

`.section .text.trampoline` 将代码放入跳板节，`.globl` 导出符号。当前 RISC-V 汇编中的 `.align 2` 表示按 4 字节对齐，不是按 2 字节对齐。

### 7.2 交换 sp 与 sscratch

```asm
csrrw sp, sscratch, sp
```

该指令把原 sscratch 读入 sp，并将原 sp 写入 sscratch：

```text
交换前：sp = 用户栈位置，sscratch = TrapContext 地址
交换后：sp = TrapContext 地址，sscratch = 用户栈位置
```

CPU 此时已经在 S 模式，但仍用用户页表。sp 暂时是保存区的基址，不是正式的内核栈；本段使用固定偏移存储，没有在这里调用 Rust 函数。

### 7.3 保存通用寄存器

```asm
sd x1, 1*8(sp)
sd x3, 3*8(sp)
# 宏循环保存 x5 到 x31
```

x1 是 ra，x3 是 gp。当前实现跳过：

- x0：恒为零，不必保存。
- x2：已经被交换，原值在 sscratch，稍后单独保存。
- x4：tp，当前实现假设应用不使用，保存与恢复都跳过。

因此不能说当前代码无条件完整保存了全部 32 个寄存器。需要 TLS 或依赖 tp 的应用不能直接套用这一假设。当前结构也没有浮点、向量寄存器现场，不能据此宣称支持它们的完整线程切换。

保存 x5 等寄存器后，才能放心把 t0、t1、t2 作为临时变量。

### 7.4 保存特权状态和用户 sp

```asm
csrr t0, sstatus
csrr t1, sepc
sd t0, 32*8(sp)
sd t1, 33*8(sp)
csrr t2, sscratch
sd t2, 2*8(sp)
```

保存的 sstatus 是硬件陷入更新后的状态，包含返回所需的 SPP、SPIE 等信息。sscratch 中的原用户 sp 存入 x[2]。

scause、stval 没有保存到 TrapContext 中，本章 Rust handler 在进入后读取它们，并将值保存在本次处理流程中。

### 7.5 读取内核环境并切换

```asm
ld t0, 34*8(sp)
ld t1, 36*8(sp)
ld sp, 35*8(sp)
csrw satp, t0
sfence.vma
jr t1
```

顺序含义：

1. 在用户页表仍有效时读取 kernel_satp、trap_handler、kernel_sp。
2. 将 sp 设为当前线程内核栈顶。
3. 将 satp 改为公共内核页表 token。
4. 用 `sfence.vma` 同步地址翻译，避免使用旧页表的缓存结果。
5. 跳到内核的 Rust `trap_handler()`。

`ld sp` 后、切 satp 前，sp 所代表的内核栈还不一定在用户页表中有映射。该短暂阶段没有通过新 sp 访存；代码仍在共享跳板执行。

`jr t1` 不设置返回地址，它不是普通 call。handler 的返回类型是 `!`，最后转入 trap_return，不会通过 ret 返回跳板调用点。

## 8. Rust trap_handler 的处理

### 8.1 用户系统调用

```rust
cx.sepc += 4;
let result = syscall(cx.x[17], [cx.x[10], cx.x[11], cx.x[12], cx.x[13]]);
cx = current_trap_cx();
cx.x[10] = result as usize;
```

用户约定 a7/x17 存系统调用号，a0/x10 起存参数，a0 也接收返回值。当前 handler 提取 a0 到 a3 四个参数；用户库即使有六参数封装，也不代表本 handler 自动传递了 a4、a5。

ecall 固定为 4 字节，所以 sepc 加 4 跳过这次调用；即使启用压缩指令，也不应把这里改成统一加 2。

必须重新调用 `current_trap_cx()`：exec 可能替换地址空间与 TrapContext 页，原引用不能继续作为返回现场使用。fork 则让子线程现场中的 a0 为 0，父线程的 a0 写入子 PID。

### 8.2 定时器中断与调度

处理顺序为：

```text
set_next_trigger：设置下一次定时事件
check_timer：唤醒到期的睡眠线程
suspend_current_and_run_next：当前线程变 Ready，入队并切调度器
```

定时配置为每秒 100 次，目标间隔约 10 ms，但不能认为实际每次运行精确占用 10 ms。

此分支不修改 sepc。线程再次调度后，先恢复内核 TaskContext，继续完成本次 handler，最后才恢复用户 TrapContext。

`enable_timer_interrupt()` 设置 sie.STIE。中断是否能够交付还与特权级、sstatus、底层委托及定时器条件有关，设置 STIE 不等于单独完成全部中断配置。本章常规 S 模式处理期间 SIE 关闭，不应描述为任意位置都可内核抢占。

### 8.3 访存异常、非法指令与信号

访存 access fault 和 page fault 分支打印 scause、stval、sepc，再设置进程 SIGSEGV；非法指令设置 SIGILL。结束分发后检查当前信号，存在致命信号时调用退出路径。

当前页异常处理没有缺页补映射、换页或写时复制恢复，不能把它理解为异常后自动修复并重试。

未知原因进入 panic。用户库中的信号相关接口也不能单凭名称说明当前 handler 已实现完整的用户信号处理函数与 sigreturn 协议。

## 9. trap_return：准备返回跳板

Rust 代码先设置用户陷入入口，再取得：

```text
a0 = 当前线程 TrapContext 的用户虚拟地址
a1 = 当前进程用户页表 token
```

跳板恢复入口的高地址按偏移计算：

```rust
let restore_va = __restore as usize - __alltraps as usize + TRAMPOLINE;
```

`__restore - __alltraps` 是同一代码节中的相对偏移。例如偏移为 0x80，则跳转地址是 `0xfffffffffffff080`。这是示例偏移，不是当前编译结果的实测值。

```asm
fence.i
jr restore_va
```

`fence.i` 同步先前写入的指令内容与后续取指，不负责刷新地址翻译；页表相关同步使用 sfence.vma。

跳到高地址时仍在 S 模式、内核页表下。这个地址在两套页表中均有效，后续才能在 __restore 内切换页表。

## 10. __restore：逐段分析返回汇编

### 10.1 切回用户页表

```asm
csrw satp, a1
sfence.vma
csrw sscratch, a0
mv sp, a0
```

此时只是切页表，尚未返回 U 模式。设置 sscratch 为下次陷入的准备地址，sp 也指向 TrapContext，以便用固定偏移恢复现场。

### 10.2 恢复 sstatus 与 sepc

```asm
ld t0, 32*8(sp)
ld t1, 33*8(sp)
csrw sstatus, t0
csrw sepc, t1
```

sepc 决定返回后的 PC；sstatus.SPP 决定 sret 返回的特权级。初始现场由 `app_init_context()` 设置 SPP=User，正常用户陷入也由硬件记录 User。

写 sepc 不等于立即跳转，写 sstatus.SPP 也不等于立即进入用户态。真正的控制转移在 sret。

### 10.3 恢复寄存器，最后恢复 sp

```asm
ld x1, 1*8(sp)
ld x3, 3*8(sp)
# 宏循环恢复 x5 到 x31
ld sp, 2*8(sp)
sret
```

必须最后恢复用户 sp：之前的 ld 都把 sp 当作 TrapContext 基址，提前改变它会读错位置。

恢复循环也会恢复 a0、a1，因此 Rust 传入的临时 a0/a1 被用户现场取代。a0 最终得到系统调用结果，而不是 TrapContext 指针。

sret 根据 SPP 返回 U 模式，将 PC 设为 sepc，并按规范恢复 SIE/SPIE 等状态。satp 不由 sret 切换，用户 sp 也不由 sret 自动恢复；它们已由前面的指令准备好。

## 11. TrapContext 与 TaskContext 的区别

| 对比 | TrapContext | TaskContext |
| --- | --- | --- |
| 保存什么 | 用户通用寄存器、返回 PC、特权状态、内核环境信息 | 内核 ra、sp、s0 到 s11 |
| 保存在哪里 | 线程独立的 TrapContext 物理页 | TCB 内层，调度器另有 idle 上下文 |
| 谁操作 | __alltraps、__restore 和 handler | __switch |
| 解决什么 | U 与 S 之间的现场保存和恢复 | 多条内核执行流之间的切换 |

__switch 是遵循调用约定的内核函数，保存被调用者保存寄存器即可；调用者保存寄存器由编译器与调用约定处理。异步陷入则不能假定用户正处于函数调用边界，需要保存更广的现场。

```text
用户线程 A
  -> 陷入，保存 A 的 TrapContext
  -> A 在内核中阻塞，保存 A 的 TaskContext
  -> 调度器选择 B
  -> B 运行，之后 A 被唤醒
  -> 恢复 A 的 TaskContext，继续阻塞调用之后的内核代码
  -> trap_return，恢复 A 的 TrapContext
  -> A 继续用户代码
```

__switch 本身不切 satp，也不执行 sret。内核切线程期间共用 KERNEL_SPACE，返回具体用户线程时才切到所属进程用户页表。

新线程第一次运行也是这一机制：TaskContext.ra 初始化为 trap_return，内核 sp 初始化为内核栈顶；TrapContext.sepc 初始化为入口，x[2] 初始化为用户栈顶。它不需要先执行一次 ecall 才能进入用户态。

## 12. 数值例子：一次 getpid 系统调用

以下地址为说明流程的假设值，不代表某次实测布局：

```text
用户 ecall 地址 = 0x10080
用户 sp         = 0x16000
TrapContext VA  = 0xffffffffffffe000
a7              = 172，getpid 系统调用号
内核 sp         = K，线程内核栈顶
```

执行过程：

1. ecall 触发异常，硬件保存 sepc=0x10080，进入 S 模式，PC 转到 TRAMPOLINE。
2. 交换后 sp=TrapContext VA，sscratch=0x16000。
3. 汇编保存 x[2]=0x16000、sepc=0x10080 等现场。
4. 读取内核环境，sp=K，satp=kernel token，跳到 handler。
5. handler 将 sepc 改为 0x10084，执行 getpid。假设结果为 7，则 x[10]=7。
6. trap_return 跳到 __restore 的高地址，切用户页表，恢复寄存器。
7. 恢复用户 sp=0x16000，sret 返回 U 模式，PC=0x10084，a0=7。

跳板并没有通过 ra 返回用户调用点；返回位置来自 sepc。

## 13. 内核态异常与紧急栈

本章内核异常入口为：

```asm
__trap_from_kernel:
    la sp, __emergency_end
    j trap_from_kernel
```

紧急栈位于 `.data`，预留 4 KiB。切换到它是为了在原内核栈受损时仍有机会打印异常信息。

这一入口不保存完整内核上下文，不尝试恢复原执行流。Rust `trap_from_kernel()` 打印辅助信息并 panic；它是故障诊断与终止路径，不是支持内核嵌套中断的通用处理机制。

另外，用户跳板执行期间尚未进入 Rust handler、或返回跳板准备期间，stvec 的状态正在过渡。不能据此保证任何时刻发生的 S 模式异常都能安全走紧急栈；当前设计要求这些关键指令和映射正常有效。

## 14. 总结与阅读要点

完整路径为：

```text
U 模式应用
  -> 硬件记录 CSR、进入 S 模式
  -> __alltraps 保存用户现场
  -> 切线程内核栈与公共内核页表
  -> trap_handler 分发事件，必要时调度线程
  -> trap_return 定位共享跳板恢复入口
  -> __restore 切用户页表、恢复 CSR 和寄存器
  -> sret 返回用户程序
```

理解代码时应始终追踪三个量：当前特权级、当前 satp、当前 sp。TrapContext 的用户虚拟地址、物理页号和内核恒等映射访问地址也是不同概念。

本文只对照当前源码分析，没有重新执行编译、QEMU 或汇编单步验证。关键边界包括：tp 未保存，浮点与向量现场未覆盖，页异常不做恢复，内核异常以 panic 终止，以及单核、内核不支持常规抢占的前提。
