//! Semaphore

use crate::sync::UPSafeCell;
use crate::task::{
    block_current_and_run_next, current_process, current_task, wakeup_task, TaskControlBlock,
};
use alloc::{collections::VecDeque, sync::Arc};

/// semaphore structure
pub struct Semaphore {
    /// semaphore inner
    pub inner: UPSafeCell<SemaphoreInner>,
    resource: usize,
}

pub struct SemaphoreInner {
    pub count: isize,
    pub wait_queue: VecDeque<Arc<TaskControlBlock>>,
}

impl Semaphore {
    /// Create a new semaphore
    pub fn new(res_count: usize, resource: usize) -> Self {
        trace!("kernel: Semaphore::new");
        Self {
            resource,
            inner: unsafe {
                UPSafeCell::new(SemaphoreInner {
                    count: res_count as isize,
                    wait_queue: VecDeque::new(),
                })
            },
        }
    }

    /// up operation of semaphore
    pub fn up(&self) {
        trace!("kernel: Semaphore::up");
        let mut inner = self.inner.exclusive_access();
        let tid = current_task()
            .unwrap()
            .inner_exclusive_access()
            .res
            .as_ref()
            .unwrap()
            .tid;
        let process = current_process();
        let mut process_inner = process.inner_exclusive_access();
        process_inner.deadlock.release(tid, self.resource);
        inner.count += 1;
        if inner.count <= 0 {
            if let Some(task) = inner.wait_queue.pop_front() {
                let next_tid = task.inner_exclusive_access().res.as_ref().unwrap().tid;
                process_inner.deadlock.acquire(next_tid, self.resource);
                drop(process_inner);
                wakeup_task(task);
            }
        }
    }

    /// down operation of semaphore
    pub fn down(&self) -> isize {
        trace!("kernel: Semaphore::down");
        let tid = current_task()
            .unwrap()
            .inner_exclusive_access()
            .res
            .as_ref()
            .unwrap()
            .tid;
        let process = current_process();
        if !process
            .inner_exclusive_access()
            .deadlock
            .request(tid, self.resource)
        {
            return -0xdead;
        }
        let mut inner = self.inner.exclusive_access();
        inner.count -= 1;
        if inner.count < 0 {
            inner.wait_queue.push_back(current_task().unwrap());
            drop(inner);
            block_current_and_run_next();
        } else {
            process
                .inner_exclusive_access()
                .deadlock
                .acquire(tid, self.resource);
        }
        0
    }
}
