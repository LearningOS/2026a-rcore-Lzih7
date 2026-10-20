//!Implementation of [`TaskManager`]
use super::task::BIG_STRIDE;
use super::TaskControlBlock;
use crate::sync::UPSafeCell;
use alloc::collections::VecDeque;
use alloc::sync::Arc;
use lazy_static::*;
///A array of `TaskControlBlock` that is thread-safe
pub struct TaskManager {
    ready_queue: VecDeque<Arc<TaskControlBlock>>,
}

/// A stride scheduler: the process with the smallest stride runs next.
impl TaskManager {
    ///Creat an empty TaskManager
    pub fn new() -> Self {
        Self {
            ready_queue: VecDeque::new(),
        }
    }
    /// Add process back to ready queue
    pub fn add(&mut self, task: Arc<TaskControlBlock>) {
        self.ready_queue.push_back(task);
    }
    /// Take the process with the smallest stride out of the ready queue
    ///
    /// The chosen process then advances its own stride by `pass`, so a process
    /// with a larger priority (a smaller pass) is picked more often.
    pub fn fetch(&mut self) -> Option<Arc<TaskControlBlock>> {
        let idx = self
            .ready_queue
            .iter()
            .enumerate()
            .min_by_key(|(_, task)| task.inner_exclusive_access().stride)
            .map(|(idx, _)| idx)?;
        // `idx` comes from the traversal above, so it is always in range.
        let task = self.ready_queue.remove(idx).unwrap();
        {
            // The borrow ends here, so that `task` can be moved out afterwards.
            let mut inner = task.inner_exclusive_access();
            inner.stride += BIG_STRIDE / inner.priority;
        }
        Some(task)
    }
}

lazy_static! {
    /// TASK_MANAGER instance through lazy_static!
    pub static ref TASK_MANAGER: UPSafeCell<TaskManager> =
        unsafe { UPSafeCell::new(TaskManager::new()) };
}

/// Add process to ready queue
pub fn add_task(task: Arc<TaskControlBlock>) {
    //trace!("kernel: TaskManager::add_task");
    TASK_MANAGER.exclusive_access().add(task);
}

/// Take a process out of the ready queue
pub fn fetch_task() -> Option<Arc<TaskControlBlock>> {
    //trace!("kernel: TaskManager::fetch_task");
    TASK_MANAGER.exclusive_access().fetch()
}
