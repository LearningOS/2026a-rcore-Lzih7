//! File and filesystem-related syscalls
use crate::fs::{link_file, open_file, unlink_file, OpenFlags, Stat};
use crate::mm::{
    translated_byte_buffer, translated_str, PageTable, PTEFlags, UserBuffer, VirtAddr,
};
use crate::task::{current_task, current_user_token};

pub fn sys_write(fd: usize, buf: *const u8, len: usize) -> isize {
    trace!("kernel:pid[{}] sys_write", current_task().unwrap().pid.0);
    let token = current_user_token();
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    if fd >= inner.fd_table.len() {
        return -1;
    }
    if let Some(file) = &inner.fd_table[fd] {
        if !file.writable() {
            return -1;
        }
        let file = file.clone();
        // release current task TCB manually to avoid multi-borrow
        drop(inner);
        file.write(UserBuffer::new(translated_byte_buffer(token, buf, len))) as isize
    } else {
        -1
    }
}

pub fn sys_read(fd: usize, buf: *const u8, len: usize) -> isize {
    trace!("kernel:pid[{}] sys_read", current_task().unwrap().pid.0);
    let token = current_user_token();
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    if fd >= inner.fd_table.len() {
        return -1;
    }
    if let Some(file) = &inner.fd_table[fd] {
        let file = file.clone();
        if !file.readable() {
            return -1;
        }
        // release current task TCB manually to avoid multi-borrow
        drop(inner);
        trace!("kernel: sys_read .. file.read");
        file.read(UserBuffer::new(translated_byte_buffer(token, buf, len))) as isize
    } else {
        -1
    }
}

pub fn sys_open(path: *const u8, flags: u32) -> isize {
    trace!("kernel:pid[{}] sys_open", current_task().unwrap().pid.0);
    let task = current_task().unwrap();
    let token = current_user_token();
    let path = translated_str(token, path);
    if let Some(inode) = open_file(path.as_str(), OpenFlags::from_bits(flags).unwrap()) {
        let mut inner = task.inner_exclusive_access();
        let fd = inner.alloc_fd();
        inner.fd_table[fd] = Some(inode);
        fd as isize
    } else {
        -1
    }
}

pub fn sys_close(fd: usize) -> isize {
    trace!("kernel:pid[{}] sys_close", current_task().unwrap().pid.0);
    let task = current_task().unwrap();
    let mut inner = task.inner_exclusive_access();
    if fd >= inner.fd_table.len() {
        return -1;
    }
    if inner.fd_table[fd].is_none() {
        return -1;
    }
    inner.fd_table[fd].take();
    0
}

/// 获取打开文件的状态，检查用户写权限并支持跨页写回。
pub fn sys_fstat(fd: usize, st: *mut Stat) -> isize {
    trace!("kernel:pid[{}] sys_fstat", current_task().unwrap().pid.0);
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();

    if fd >= inner.fd_table.len() {
        return -1;
    }
    let file = match &inner.fd_table[fd] {
        Some(file) => file.clone(),
        None => return -1,
    };
    let token = inner.memory_set.token();
    drop(inner);

    let start = st as usize;
    let len = core::mem::size_of::<Stat>();
    let end = match start.checked_add(len) {
        Some(end) if start != 0 && end <= (1usize << 38) => end,
        _ => return -1,
    };
    let page_table = PageTable::from_token(token);
    let mut address = start;
    while address < end {
        let va = VirtAddr::from(address);
        match page_table.translate(va.floor()) {
            Some(pte)
                if pte.flags().contains(
                    PTEFlags::V | PTEFlags::U | PTEFlags::R | PTEFlags::W,
                ) => {}
            _ => return -1,
        }
        address += (crate::config::PAGE_SIZE - va.page_offset()).min(end - address);
    }
    let stat = match file.stat() {
        Some(stat) => stat,
        None => return -1,
    };
    // 按 repr(C) 字段布局编码，不读取结构体的潜在填充字节；保留字段全部写零。
    let mut bytes = [0u8; core::mem::size_of::<Stat>()];
    bytes[0..8].copy_from_slice(&stat.dev.to_ne_bytes());
    bytes[8..16].copy_from_slice(&stat.ino.to_ne_bytes());
    bytes[16..20].copy_from_slice(&stat.mode.bits().to_ne_bytes());
    bytes[20..24].copy_from_slice(&stat.nlink.to_ne_bytes());
    let mut offset = 0;
    for buffer in translated_byte_buffer(token, st as *const u8, len) {
        let next = offset + buffer.len();
        buffer.copy_from_slice(&bytes[offset..next]);
        offset = next;
    }
    0
}

/// YOUR JOB: Implement linkat.
///
/// Create a hard link: `new_name` and `old_name` then refer to the same file.
pub fn sys_linkat(old_name: *const u8, new_name: *const u8) -> isize {
    trace!("kernel:pid[{}] sys_linkat", current_task().unwrap().pid.0);
    let token = current_user_token();
    let old_name = translated_str(token, old_name);
    let new_name = translated_str(token, new_name);
    if link_file(old_name.as_str(), new_name.as_str()) {
        0
    } else {
        -1
    }
}

/// YOUR JOB: Implement unlinkat.
pub fn sys_unlinkat(_name: *const u8) -> isize {
    trace!("kernel:pid[{}] sys_unlinkat", current_task().unwrap().pid.0);
    let token = current_user_token();
    let name = translated_str(token, _name);
    if unlink_file(name.as_str()) {
        0
    } else {
        -1
    }
}
