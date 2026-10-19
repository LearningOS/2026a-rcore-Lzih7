//! Types related to task management

use super::TaskContext;

/// Number of syscall IDs tracked per task, including trace (410).
pub const SYSCALL_COUNT: usize = 411;

/// The task control block (TCB) of a task.
#[derive(Copy, Clone)]
pub struct TaskControlBlock {
    /// The task status in it's lifecycle
    pub task_status: TaskStatus,
    /// The task context
    pub task_cx: TaskContext,
    /// Invocation counts indexed by syscall ID.
    pub syscall_counts: [usize; SYSCALL_COUNT],
}

/// The status of a task
#[derive(Copy, Clone, PartialEq)]
pub enum TaskStatus {
    /// uninitialized
    UnInit,
    /// ready to run
    Ready,
    /// running
    Running,
    /// exited
    Exited,
}
