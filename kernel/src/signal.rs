//! Signal dispositions.
//!
//! What a signal does when it arrives is decided here; how the registers are
//! handed to a handler and taken back afterwards is decided by the
//! architecture's ABI, and lives behind `crate::arch`.

use crate::abi::*;

pub const SIG_DFL: u64 = 0;
pub const SIG_IGN: u64 = 1;

pub const SA_SIGINFO: u64 = 0x0000_0004;
pub const SA_RESTORER: u64 = 0x0400_0000;
pub const SA_ONSTACK: u64 = 0x0800_0000;
pub const SA_NODEFER: u64 = 0x4000_0000;
pub const SA_RESETHAND: u64 = 0x8000_0000;

#[derive(Debug, Clone, Copy, Default)]
pub struct SigAction {
    pub handler: u64,
    pub flags: u64,
    pub restorer: u64,
    pub mask: u64,
}

/// Signals whose default action is to do nothing.
pub fn default_is_ignore(signal: i32) -> bool {
    matches!(signal, SIGCHLD | SIGCONT | 23 /* SIGURG */ | 28 /* SIGWINCH */)
}

/// Redirect `frame` into `action.handler`. Returns false if the user stack
/// could not be written, in which case the caller should kill the task.
pub fn deliver(
    task: &crate::task::Task,
    signal: i32,
    action: &SigAction,
    frame: &mut crate::arch::TrapFrame,
) -> bool {
    crate::arch::enter_signal_handler(task, signal, action, frame)
}

/// Restore the register state a handler was entered with.
pub fn sigreturn(task: &crate::task::Task, frame: &mut crate::arch::TrapFrame) -> SysResult {
    crate::arch::leave_signal_handler(task, frame)
}
