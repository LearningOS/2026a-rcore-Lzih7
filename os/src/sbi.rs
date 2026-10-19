//! SBI call wrappers

use core::arch::asm;

const SBI_TIME: usize = 0x54494D45;
const SBI_DEBUG_CONSOLE: usize = 0x4442434E;
const SBI_SYSTEM_RESET: usize = 0x53525354;

/// Call an SBI extension, returning its error code and value.
#[inline(always)]
fn sbi_call(
    extension: usize,
    function: usize,
    arg0: usize,
    arg1: usize,
    arg2: usize,
) -> (isize, usize) {
    let error;
    let value;
    unsafe {
        asm!(
            "ecall",
            inlateout("a0") arg0 => error,
            inlateout("a1") arg1 => value,
            in("a2") arg2,
            in("a6") function,
            in("a7") extension,
        );
    }
    (error, value)
}

/// use sbi call to set timer
pub fn set_timer(timer: usize) {
    sbi_call(SBI_TIME, 0, timer, 0, 0);
}

/// use sbi call to putchar in console (qemu uart handler)
pub fn console_putchar(c: usize) {
    // DBCN function 2 writes one byte without a shared-memory buffer.
    sbi_call(SBI_DEBUG_CONSOLE, 2, c, 0, 0);
}

/// use sbi call to shutdown the kernel
pub fn shutdown() -> ! {
    // SBI System Reset: shutdown (type 0), no failure reason (reason 0).
    sbi_call(SBI_SYSTEM_RESET, 0, 0, 0, 0);
    panic!("It should shutdown!");
}
