//! Mutex (spin-like and blocking(sleep))

use super::UPSafeCell;
use crate::task::TaskControlBlock;
use crate::task::{block_current_and_run_next, suspend_current_and_run_next};
use crate::task::{current_process, current_task, wakeup_task};
use alloc::{collections::VecDeque, sync::Arc};

/// Mutex trait
pub trait Mutex: Sync + Send {
    /// Lock the mutex
    fn lock(&self) -> isize;
    /// Unlock the mutex
    fn unlock(&self);
}

/// Spinlock Mutex struct
pub struct MutexSpin {
    locked: UPSafeCell<bool>,
    resource: usize,
}

impl MutexSpin {
    /// Create a new spinlock mutex
    pub fn new(resource: usize) -> Self {
        Self {
            locked: unsafe { UPSafeCell::new(false) },
            resource,
        }
    }
}

impl Mutex for MutexSpin {
    /// Lock the spinlock mutex
    fn lock(&self) -> isize {
        trace!("kernel: MutexSpin::lock");
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
        loop {
            let mut locked = self.locked.exclusive_access();
            if *locked {
                drop(locked);
                suspend_current_and_run_next();
                continue;
            } else {
                *locked = true;
                process
                    .inner_exclusive_access()
                    .deadlock
                    .acquire(tid, self.resource);
                return 0;
            }
        }
    }

    fn unlock(&self) {
        trace!("kernel: MutexSpin::unlock");
        let mut locked = self.locked.exclusive_access();
        *locked = false;
        let tid = current_task()
            .unwrap()
            .inner_exclusive_access()
            .res
            .as_ref()
            .unwrap()
            .tid;
        current_process()
            .inner_exclusive_access()
            .deadlock
            .release(tid, self.resource);
    }
}

/// Blocking Mutex struct
pub struct MutexBlocking {
    inner: UPSafeCell<MutexBlockingInner>,
    resource: usize,
}

pub struct MutexBlockingInner {
    locked: bool,
    wait_queue: VecDeque<Arc<TaskControlBlock>>,
}

impl MutexBlocking {
    /// Create a new blocking mutex
    pub fn new(resource: usize) -> Self {
        trace!("kernel: MutexBlocking::new");
        Self {
            resource,
            inner: unsafe {
                UPSafeCell::new(MutexBlockingInner {
                    locked: false,
                    wait_queue: VecDeque::new(),
                })
            },
        }
    }
}

impl Mutex for MutexBlocking {
    /// lock the blocking mutex
    fn lock(&self) -> isize {
        trace!("kernel: MutexBlocking::lock");
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
        let mut mutex_inner = self.inner.exclusive_access();
        if mutex_inner.locked {
            mutex_inner.wait_queue.push_back(current_task().unwrap());
            drop(mutex_inner);
            block_current_and_run_next();
        } else {
            mutex_inner.locked = true;
            process
                .inner_exclusive_access()
                .deadlock
                .acquire(tid, self.resource);
        }
        0
    }

    /// unlock the blocking mutex
    fn unlock(&self) {
        trace!("kernel: MutexBlocking::unlock");
        let mut mutex_inner = self.inner.exclusive_access();
        assert!(mutex_inner.locked);
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
        if let Some(waking_task) = mutex_inner.wait_queue.pop_front() {
            // 在唤醒前完成所有权交接，避免尚未运行的线程被漏计。
            let next_tid = waking_task
                .inner_exclusive_access()
                .res
                .as_ref()
                .unwrap()
                .tid;
            process_inner.deadlock.acquire(next_tid, self.resource);
            drop(process_inner);
            wakeup_task(waking_task);
        } else {
            mutex_inner.locked = false;
        }
    }
}
