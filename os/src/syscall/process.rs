//! Process management syscalls
use crate::mm::{translated_byte_buffer, PageTable, PTEFlags, VirtAddr};
use crate::task::{
    change_program_brk, current_user_token, exit_current_and_run_next, suspend_current_and_run_next,
};
use crate::timer::get_time_us;

#[repr(C)]
#[derive(Debug)]
pub struct TimeVal {
    pub sec: usize,
    pub usec: usize,
}

/// task exits and submit an exit code
pub fn sys_exit(_exit_code: i32) -> ! {
    trace!("kernel: sys_exit");
    exit_current_and_run_next();
    panic!("Unreachable in sys_exit!");
}

/// current task gives up resources for other tasks
pub fn sys_yield() -> isize {
    trace!("kernel: sys_yield");
    suspend_current_and_run_next();
    0
}

/// Write seconds and microseconds to a writable user buffer, including across pages.
pub fn sys_get_time(_ts: *mut TimeVal, _tz: usize) -> isize {
    trace!("kernel: sys_get_time");
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

/// Read or write one byte in the current task's user memory, or report how many
/// times the current task has called a system call.
///
/// `trace_request` selects the operation: `0` reads the byte at `id`, `1` writes
/// the low byte of `data` to `id`, `2` returns the call count of the syscall
/// numbered `id`, and any other value returns `-1`.
pub fn sys_trace(trace_request: usize, id: usize, data: usize) -> isize {
    trace!("kernel: sys_trace");
    if trace_request == 2 {
        // The dispatch of this very call has already been counted.
        return crate::task::syscall_count(id) as isize;
    }
    // Translate `id` through the current task's page table; a user pointer
    // cannot be dereferenced directly here.
    let va = VirtAddr::from(id);
    let token = current_user_token();
    let page_table = PageTable::from_token(token);
    let pte = match page_table.translate(va.floor()) {
        Some(pte) if pte.flags().contains(PTEFlags::V | PTEFlags::U) => pte,
        _ => return -1,
    };
    let byte = &mut pte.ppn().get_bytes_array()[va.page_offset()];
    match trace_request {
        0 if pte.flags().contains(PTEFlags::R) => *byte as isize,
        1 if pte.flags().contains(PTEFlags::W) => {
            *byte = data as u8;
            0
        }
        _ => -1,
    }
}

// YOUR JOB: Implement mmap.
// prot：第 0 位表示是否可读，第 1 位表示是否可写，第 2 位表示是否可执行。其他位无效且必须为 0
pub fn sys_mmap(start: usize, len: usize, port: usize) -> isize {
    trace!("kernel: sys_mmap");
    crate::task::mmap_current_task(start, len, port)
}

// YOUR JOB: Implement munmap.
pub fn sys_munmap(start: usize, len: usize) -> isize {
    trace!("kernel: sys_munmap");
    crate::task::munmap_current_task(start, len)
}

/// change data segment size
pub fn sys_sbrk(size: i32) -> isize {
    trace!("kernel: sys_sbrk");
    if let Some(old_brk) = change_program_brk(size) {
        old_brk as isize
    } else {
        -1
    }
}
