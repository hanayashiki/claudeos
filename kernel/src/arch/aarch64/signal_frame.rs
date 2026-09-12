//! The signal frame, as the aarch64 Linux ABI defines it.
//!
//! Not written yet. The layout is not the x86-64 one: `sigcontext` here holds
//! the thirty-one general registers, the stack pointer, the program counter
//! and the processor state, followed by a chain of variable-length records
//! carrying the floating point and vector state, ending in a terminator. Until
//! that is built, a task that installs a handler and then takes the signal
//! stops the machine rather than being sent somewhere unpredictable.

use super::trap::TrapFrame;
use crate::abi::SysResult;
use crate::signal::SigAction;
use crate::task::Task;

pub fn enter_signal_handler(
    _task: &mut Task,
    _signal: i32,
    _action: &SigAction,
    _frame: &mut TrapFrame,
) -> bool {
    unimplemented!("signal delivery on aarch64")
}

pub fn leave_signal_handler(_task: &mut Task, _frame: &mut TrapFrame) -> SysResult {
    unimplemented!("returning from a signal handler on aarch64")
}
