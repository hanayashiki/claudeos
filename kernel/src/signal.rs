//! Signal dispositions.
//!
//! What a signal does when it arrives is decided here; how the registers are
//! handed to a handler and taken back afterwards is decided by the
//! architecture's ABI, and lives behind `crate::arch`.

use crate::abi::SysResult;

pub const SIG_DFL: u64 = 0;
pub const SIG_IGN: u64 = 1;

/// A signal number that named a signal when it was made.
///
/// Two things are asked of a signal number: the bit it occupies in a pending
/// or blocked set, and where its disposition sits in the table. Both used to
/// be taken from a raw number with a mask, which turned a number outside the
/// range into another signal's: 73 masked to six bits is 9, so a `kill` no
/// system accepts set the bit for the one signal a task cannot survive, while
/// every comparison against the number itself still saw 73 and did nothing
/// about it -- including the one that restarts a stopped task for a kill. The
/// number is checked here instead, once, where it arrives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Signal(u8);

impl Signal {
    /// The signal `number` names, or `None` when no signal has that number.
    pub fn from_number(number: i32) -> Option<Signal> {
        if (1..64).contains(&number) {
            Some(Signal(number as u8))
        } else {
            None
        }
    }

    /// The bit this signal occupies in a pending or blocked set.
    ///
    /// Signal n is bit n - 1, as in Linux's `sigset_t`. A program hands the
    /// kernel its sets in that layout -- the mask `rt_sigprocmask` changes, the
    /// one `rt_sigaction` blocks for a handler, `uc_sigmask` in a signal frame
    /// -- and they are stored as they arrive, so a bit that is not the one the
    /// program meant is a signal it did not name. With n at bit n, a thread
    /// that blocked SIGUSR1 set the bit this kernel read as SIGKILL, which
    /// cannot be blocked, and SIGUSR1 stayed deliverable.
    pub const fn bit(self) -> u64 {
        1u64 << (self.0 - 1)
    }

    /// Where this signal's disposition sits in a task's table.
    pub const fn index(self) -> usize {
        self.0 as usize
    }

    /// The number itself, for a report, for the status a kill leaves behind,
    /// and for the handler's own first argument.
    pub const fn number(self) -> i32 {
        self.0 as i32
    }

    /// Signals whose default action is to stop the task.
    pub fn stops(self) -> bool {
        matches!(self, SIGSTOP | SIGTSTP | SIGTTIN | SIGTTOU)
    }

    /// Every signal there is, lowest number first.
    pub fn all() -> impl Iterator<Item = Signal> {
        (1..64u8).map(Signal)
    }
}

pub const SIGHUP: Signal = Signal(1);
pub const SIGINT: Signal = Signal(2);
pub const SIGQUIT: Signal = Signal(3);
pub const SIGILL: Signal = Signal(4);
pub const SIGTRAP: Signal = Signal(5);
pub const SIGABRT: Signal = Signal(6);
pub const SIGFPE: Signal = Signal(8);
pub const SIGKILL: Signal = Signal(9);
pub const SIGSEGV: Signal = Signal(11);
pub const SIGPIPE: Signal = Signal(13);
pub const SIGALRM: Signal = Signal(14);
pub const SIGTERM: Signal = Signal(15);
pub const SIGCHLD: Signal = Signal(17);
pub const SIGCONT: Signal = Signal(18);
pub const SIGSTOP: Signal = Signal(19);
pub const SIGTSTP: Signal = Signal(20);
pub const SIGTTIN: Signal = Signal(21);
pub const SIGTTOU: Signal = Signal(22);
pub const SIGURG: Signal = Signal(23);
pub const SIGVTALRM: Signal = Signal(26);
pub const SIGPROF: Signal = Signal(27);
pub const SIGWINCH: Signal = Signal(28);

/// The same value on both machines, as asm-generic and x86-64 define it.
pub const SA_NOCLDWAIT: u64 = 0x0000_0002;
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
pub fn default_is_ignore(signal: Signal) -> bool {
    matches!(signal, SIGCHLD | SIGCONT | SIGURG | SIGWINCH)
}

/// Redirect `frame` into `action.handler`. Returns false if the user stack
/// could not be written, in which case the caller should kill the task.
pub fn deliver(
    task: &crate::task::Task,
    signal: Signal,
    action: &SigAction,
    frame: &mut crate::arch::TrapFrame,
) -> bool {
    crate::arch::enter_signal_handler(task, signal, action, frame)
}

/// Restore the register state a handler was entered with.
pub fn sigreturn(task: &crate::task::Task, frame: &mut crate::arch::TrapFrame) -> SysResult {
    crate::arch::leave_signal_handler(task, frame)
}
