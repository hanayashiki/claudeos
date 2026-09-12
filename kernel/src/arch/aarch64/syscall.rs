//! The system call entry path: how a program asks for a call, where the
//! number and the arguments are found, and where the result goes back.

use super::trap::TrapFrame;
use crate::abi::{Errno, SysResult};

/// Nothing to arrange. A system call is an exception here, taken through the
/// same vector table as everything else, so it is already caught by the time
/// this could run.
pub fn init_syscall_entry() {}

/// Which call the program asked for.
#[inline]
pub fn syscall_number(frame: &TrapFrame) -> u64 {
    frame.x[8]
}

/// The six argument registers, in the order the Linux calling convention
/// names them.
#[inline]
pub fn syscall_args(frame: &TrapFrame) -> [u64; 6] {
    [frame.x[0], frame.x[1], frame.x[2], frame.x[3], frame.x[4], frame.x[5]]
}

/// Where the result goes on the way back out.
#[inline]
pub fn set_syscall_result(frame: &mut TrapFrame, value: u64) {
    frame.x[0] = value;
}

/// What is currently in the result register.
#[inline]
pub fn syscall_result(frame: &TrapFrame) -> u64 {
    frame.x[0]
}

/// Make a child's frame resume where its parent's did, returning zero, and on
/// `stack` if one was named.
pub fn fork_child_frame(child: &mut TrapFrame, parent: &TrapFrame, stack: u64) {
    *child = *parent;
    child.x[0] = 0;
    if stack != 0 {
        child.sp = stack;
    }
}

/// `arch_prctl` does not exist on this architecture: a program addresses its
/// thread-local storage through a register it writes itself, so there is
/// nothing to ask the kernel for.
pub fn arch_prctl(_code: u64, _addr: u64) -> SysResult {
    Err(Errno::EINVAL)
}
