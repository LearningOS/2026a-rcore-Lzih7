//! Process management syscalls
use alloc::sync::Arc;

use crate::{
    loader::get_app_data_by_name,
    mm::{
        translated_byte_buffer, translated_refmut, translated_str, MapPermission, PageTable,
        PTEFlags, VirtAddr,
    },
    task::{
        add_task, current_task, current_user_token, exit_current_and_run_next,
        suspend_current_and_run_next,
    },
    timer::get_time_us,
};

#[repr(C)]
#[derive(Debug)]
pub struct TimeVal {
    pub sec: usize,
    pub usec: usize,
}

/// task exits and submit an exit code
pub fn sys_exit(exit_code: i32) -> ! {
    trace!("kernel:pid[{}] sys_exit", current_task().unwrap().pid.0);
    exit_current_and_run_next(exit_code);
    panic!("Unreachable in sys_exit!");
}

/// current task gives up resources for other tasks
pub fn sys_yield() -> isize {
    trace!("kernel:pid[{}] sys_yield", current_task().unwrap().pid.0);
    suspend_current_and_run_next();
    0
}

pub fn sys_getpid() -> isize {
    trace!("kernel: sys_getpid pid:{}", current_task().unwrap().pid.0);
    current_task().unwrap().pid.0 as isize
}

pub fn sys_fork() -> isize {
    trace!("kernel:pid[{}] sys_fork", current_task().unwrap().pid.0);
    let current_task = current_task().unwrap();
    let new_task = current_task.fork();
    let new_pid = new_task.pid.0;
    // modify trap context of new_task, because it returns immediately after switching
    let trap_cx = new_task.inner_exclusive_access().get_trap_cx();
    // we do not have to move to next instruction since we have done it before
    // for child process, fork returns 0
    trap_cx.x[10] = 0;
    // add new task to scheduler
    add_task(new_task);
    new_pid as isize
}

pub fn sys_exec(path: *const u8) -> isize {
    trace!("kernel:pid[{}] sys_exec", current_task().unwrap().pid.0);
    let token = current_user_token();
    let path = translated_str(token, path);
    if let Some(data) = get_app_data_by_name(path.as_str()) {
        let task = current_task().unwrap();
        task.exec(data);
        0
    } else {
        -1
    }
}

/// If there is not a child process whose pid is same as given, return -1.
/// Else if there is a child process but it is still running, return -2.
pub fn sys_waitpid(pid: isize, exit_code_ptr: *mut i32) -> isize {
    trace!("kernel::pid[{}] sys_waitpid [{}]", current_task().unwrap().pid.0, pid);
    let task = current_task().unwrap();
    // find a child process

    // ---- access current PCB exclusively
    let mut inner = task.inner_exclusive_access();
    if !inner
        .children
        .iter()
        .any(|p| pid == -1 || pid as usize == p.getpid())
    {
        return -1;
        // ---- release current PCB
    }
    let pair = inner.children.iter().enumerate().find(|(_, p)| {
        // ++++ temporarily access child PCB exclusively
        p.inner_exclusive_access().is_zombie() && (pid == -1 || pid as usize == p.getpid())
        // ++++ release child PCB
    });
    if let Some((idx, _)) = pair {
        let child = inner.children.remove(idx);
        // confirm that child will be deallocated after being removed from children list
        assert_eq!(Arc::strong_count(&child), 1);
        let found_pid = child.getpid();
        // ++++ temporarily access child PCB exclusively
        let exit_code = child.inner_exclusive_access().exit_code;
        // ++++ release child PCB
        *translated_refmut(inner.memory_set.token(), exit_code_ptr) = exit_code;
        found_pid as isize
    } else {
        -2
    }
    // ---- release current PCB automatically
}

/// Write seconds and microseconds to a writable user buffer, including across pages.
pub fn sys_get_time(_ts: *mut TimeVal, _tz: usize) -> isize {
    trace!("kernel:pid[{}] sys_get_time", current_task().unwrap().pid.0);
    let start = _ts as usize;
    let len = core::mem::size_of::<TimeVal>();
    let end = match start.checked_add(len) {
        Some(end) => end,
        None => return -1,
    };
    // User buffers are in the lower canonical half of the Sv39 address space.
    if start == 0 || end > (1usize << 38) {
        return -1;
    }
    // 获取当前任务的用户页表 token（包含分页模式和根页表物理页号），用于翻译用户地址。
    let token = current_user_token();
    // 用 token 里的根页表物理页号重建页表对象，从而在 S 态直接查用户页表。
    let page_table = PageTable::from_token(token);
    // 待检查的用户虚拟地址，每次前进到当前页末尾，逐页验证映射。
    let mut address = start;
    while address < end {
        let va = VirtAddr::from(address);
        match page_table.translate(va.floor()) {
            Some(pte) if pte.flags().contains(PTEFlags::V | PTEFlags::U | PTEFlags::W) => {}
            _ => return -1,
        }
        address += (crate::config::PAGE_SIZE - va.page_offset()).min(end - address);
    }

    let us = get_time_us();
    let mut bytes = [0u8; core::mem::size_of::<TimeVal>()];
    let word_size = core::mem::size_of::<usize>();
    bytes[..word_size].copy_from_slice(&(us / 1_000_000).to_ne_bytes());
    bytes[word_size..].copy_from_slice(&(us % 1_000_000).to_ne_bytes());
    let mut offset = 0;
    for buffer in translated_byte_buffer(token, _ts as *const u8, len) {
        let next = offset + buffer.len();
        buffer.copy_from_slice(&bytes[offset..next]);
        offset = next;
    }
    0
}

/// YOUR JOB: Implement mmap.
/// prot：第 0 位表示是否可读，第 1 位表示是否可写，第 2 位表示是否可执行。其他位无效且必须为 0
pub fn sys_mmap(start: usize, len: usize, port: usize) -> isize {
    trace!(
        "kernel:pid[{}] sys_mmap",
        current_task().unwrap().pid.0
    );
    let page_size = crate::config::PAGE_SIZE;
    if start % page_size != 0 || len == 0 || port == 0 || port & !0b111 != 0 {
        return -1;
    }
    // 长度向上取整到页边界，start 已对齐，取整后的 end 也落在页边界上
    let end = match start
        .checked_add(len)
        .and_then(|end| end.checked_add(page_size - 1))
    {
        Some(end) => end / page_size * page_size,
        None => return -1,
    };
    // 逐页确认区间空闲。translate 对未建立的中间页表返回 None，对无效的末级项
    // 返回 Some，两种情况都说明这一页没有被映射。
    let page_table = PageTable::from_token(current_user_token());
    let mut address = start;
    while address < end {
        let va = VirtAddr::from(address);
        if page_table
            .translate(va.floor())
            .map_or(false, |pte| pte.is_valid())
        {
            return -1;
        }
        address += page_size;
    }
    // 用户态要访问的映射必须带 U
    let mut permission = MapPermission::U;
    if port & 0b1 != 0 {
        permission |= MapPermission::R;
    }
    if port & 0b10 != 0 {
        permission |= MapPermission::W;
    }
    if port & 0b100 != 0 {
        permission |= MapPermission::X;
    }
    current_task()
        .unwrap()
        .inner_exclusive_access()
        .memory_set
        .insert_framed_area(VirtAddr::from(start), VirtAddr::from(end), permission);
    0
}

/// YOUR JOB: Implement munmap.
pub fn sys_munmap(start: usize, len: usize) -> isize {
    trace!(
        "kernel:pid[{}] sys_munmap",
        current_task().unwrap().pid.0
    );
    let page_size = crate::config::PAGE_SIZE;
    if start % page_size != 0 || len == 0 {
        return -1;
    }
    // 请求长度按页向上取整，取整后的 end 落在页边界上
    let end = match start
        .checked_add(len)
        .and_then(|end| end.checked_add(page_size - 1))
    {
        Some(end) => end / page_size * page_size,
        None => return -1,
    };
    // 区间内每一页都必须在当前用户页表中有效。只要有一页没有映射，说明请求
    // 范围超出了已有的区间，此时不能执行取消映射。
    let page_table = PageTable::from_token(current_user_token());
    let mut address = start;
    while address < end {
        let mapped = page_table
            .translate(VirtAddr::from(address).floor())
            .map_or(false, |pte| pte.is_valid());
        if !mapped {
            return -1;
        }
        address += page_size;
    }
    let task = current_task().unwrap();
    let mut inner = task.inner_exclusive_access();
    inner
        .memory_set
        .remove_area_with_start_vpn(VirtAddr::from(start).floor());
    0
}

/// change data segment size
pub fn sys_sbrk(size: i32) -> isize {
    trace!("kernel:pid[{}] sys_sbrk", current_task().unwrap().pid.0);
    if let Some(old_brk) = current_task().unwrap().change_program_brk(size) {
        old_brk as isize
    } else {
        -1
    }
}

/// YOUR JOB: Implement spawn.
/// HINT: fork + exec =/= spawn
pub fn sys_spawn(path: *const u8) -> isize {
    trace!(
        "kernel:pid[{}] sys_spawn",
        current_task().unwrap().pid.0
    );
    let token = current_user_token();
    let path = translated_str(token, path);
    if let Some(data) = get_app_data_by_name(path.as_str()) {
        let task = current_task().unwrap();
        let new_task = task.fork();
        let new_pid = new_task.pid.0;
        // 让子进程执行目标程序，exec 会为它重建整个 TrapContext
        new_task.exec(data);
        add_task(new_task);
        new_pid as isize
    } else {
        -1
    }
}

// YOUR JOB: Set task priority.
///
/// The priority drives the stride scheduler: a process with priority `p` gets a
/// stride step of `BIG_STRIDE / p`, so a larger priority means more CPU time.
/// A priority of 1 or below has no valid stride step and is rejected.
pub fn sys_set_priority(prio: isize) -> isize {
    trace!(
        "kernel:pid[{}] sys_set_priority",
        current_task().unwrap().pid.0
    );
    if prio <= 1 {
        return -1;
    }
    let task = current_task().unwrap();
    let mut inner = task.inner_exclusive_access();
    inner.priority = prio as usize;
    prio
}
