//! Signal delivery.
//!
//! When a task has installed a handler, the kernel builds the same
//! `rt_sigframe` Linux does on the user stack, points the return address at
//! the libc restorer, and re-enters user mode at the handler. `rt_sigreturn`
//! reads that frame back.

use crate::abi::*;
use crate::cpu::idt::TrapFrame;
use crate::task::Task;
use crate::uaccess;

pub const SIG_DFL: u64 = 0;
pub const SIG_IGN: u64 = 1;

pub const SA_SIGINFO: u64 = 0x0000_0004;
pub const SA_RESTORER: u64 = 0x0400_0000;
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

// Frame layout, matching the x86_64 kernel ABI.
const UC_OFFSET: usize = 8;
const UC_FLAGS: usize = UC_OFFSET;
const UC_LINK: usize = UC_OFFSET + 8;
const UC_STACK: usize = UC_OFFSET + 16;
const MCONTEXT: usize = UC_OFFSET + 40;
const UC_SIGMASK: usize = MCONTEXT + 256;
const INFO_OFFSET: usize = UC_SIGMASK + 8;
const FRAME_SIZE: usize = INFO_OFFSET + 128;

/// Offsets of each saved register inside `struct sigcontext`.
const SC_R8: usize = 0;
const SC_R9: usize = 8;
const SC_R10: usize = 16;
const SC_R11: usize = 24;
const SC_R12: usize = 32;
const SC_R13: usize = 40;
const SC_R14: usize = 48;
const SC_R15: usize = 56;
const SC_RDI: usize = 64;
const SC_RSI: usize = 72;
const SC_RBP: usize = 80;
const SC_RBX: usize = 88;
const SC_RDX: usize = 96;
const SC_RAX: usize = 104;
const SC_RCX: usize = 112;
const SC_RSP: usize = 120;
const SC_RIP: usize = 128;
const SC_EFLAGS: usize = 136;
const SC_CS: usize = 144;

fn put64(buf: &mut [u8], offset: usize, value: u64) {
    buf[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn get64(buf: &[u8], offset: usize) -> u64 {
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&buf[offset..offset + 8]);
    u64::from_le_bytes(bytes)
}

/// Redirect `frame` into `action.handler`. Returns false if the user stack
/// could not be written, in which case the caller should kill the task.
pub fn deliver(task: &mut Task, signal: i32, action: &SigAction, frame: &mut TrapFrame) -> bool {
    // Leave the red zone alone, then place the frame so that the handler sees
    // the alignment a `call` would have produced.
    let mut sp = frame.rsp.saturating_sub(128);
    sp = sp.saturating_sub(FRAME_SIZE as u64);
    sp &= !0xFu64;
    sp = sp.wrapping_sub(8);

    let mut buf = [0u8; FRAME_SIZE];

    // Return address: the restorer libc registered, which calls rt_sigreturn.
    put64(&mut buf, 0, action.restorer);

    put64(&mut buf, UC_FLAGS, 0);
    put64(&mut buf, UC_LINK, 0);
    // uc_stack: no alternate stack is in use.
    put64(&mut buf, UC_STACK, 0);
    put64(&mut buf, UC_STACK + 8, 0);
    put64(&mut buf, UC_STACK + 16, 0);

    let m = MCONTEXT;
    put64(&mut buf, m + SC_R8, frame.r8);
    put64(&mut buf, m + SC_R9, frame.r9);
    put64(&mut buf, m + SC_R10, frame.r10);
    put64(&mut buf, m + SC_R11, frame.r11);
    put64(&mut buf, m + SC_R12, frame.r12);
    put64(&mut buf, m + SC_R13, frame.r13);
    put64(&mut buf, m + SC_R14, frame.r14);
    put64(&mut buf, m + SC_R15, frame.r15);
    put64(&mut buf, m + SC_RDI, frame.rdi);
    put64(&mut buf, m + SC_RSI, frame.rsi);
    put64(&mut buf, m + SC_RBP, frame.rbp);
    put64(&mut buf, m + SC_RBX, frame.rbx);
    put64(&mut buf, m + SC_RDX, frame.rdx);
    put64(&mut buf, m + SC_RAX, frame.rax);
    put64(&mut buf, m + SC_RCX, frame.rcx);
    put64(&mut buf, m + SC_RSP, frame.rsp);
    put64(&mut buf, m + SC_RIP, frame.rip);
    put64(&mut buf, m + SC_EFLAGS, frame.rflags);
    buf[m + SC_CS..m + SC_CS + 2].copy_from_slice(&(frame.cs as u16).to_le_bytes());

    put64(&mut buf, UC_SIGMASK, task.signal_mask);

    // siginfo: si_signo, si_errno, si_code.
    buf[INFO_OFFSET..INFO_OFFSET + 4].copy_from_slice(&signal.to_le_bytes());
    buf[INFO_OFFSET + 4..INFO_OFFSET + 8].copy_from_slice(&0i32.to_le_bytes());
    buf[INFO_OFFSET + 8..INFO_OFFSET + 12].copy_from_slice(&0i32.to_le_bytes());

    if uaccess::write_bytes(sp, &buf).is_err() {
        return false;
    }

    // Block this signal for the duration of the handler unless asked not to.
    task.signal_mask |= action.mask;
    if action.flags & SA_NODEFER == 0 {
        task.signal_mask |= 1u64 << (signal as u64 & 63);
    }

    frame.rip = action.handler;
    frame.rsp = sp;
    frame.rdi = signal as u64;
    frame.rsi = sp + INFO_OFFSET as u64;
    frame.rdx = sp + UC_OFFSET as u64;
    frame.rax = 0;
    true
}

/// Restore the register state a handler was entered with.
pub fn sigreturn(task: &mut Task, frame: &mut TrapFrame) -> SysResult {
    // The restorer was reached by returning into it, so the frame starts one
    // word below the current stack pointer.
    let base = frame.rsp.wrapping_sub(8);
    let mut buf = [0u8; FRAME_SIZE];
    uaccess::read_bytes(base, &mut buf)?;

    let m = MCONTEXT;
    frame.r8 = get64(&buf, m + SC_R8);
    frame.r9 = get64(&buf, m + SC_R9);
    frame.r10 = get64(&buf, m + SC_R10);
    frame.r11 = get64(&buf, m + SC_R11);
    frame.r12 = get64(&buf, m + SC_R12);
    frame.r13 = get64(&buf, m + SC_R13);
    frame.r14 = get64(&buf, m + SC_R14);
    frame.r15 = get64(&buf, m + SC_R15);
    frame.rdi = get64(&buf, m + SC_RDI);
    frame.rsi = get64(&buf, m + SC_RSI);
    frame.rbp = get64(&buf, m + SC_RBP);
    frame.rbx = get64(&buf, m + SC_RBX);
    frame.rdx = get64(&buf, m + SC_RDX);
    frame.rcx = get64(&buf, m + SC_RCX);
    frame.rsp = get64(&buf, m + SC_RSP);
    frame.rip = get64(&buf, m + SC_RIP);

    // Only the flags a program may set are taken from the frame.
    let flags = get64(&buf, m + SC_EFLAGS);
    frame.rflags = (flags & 0x0000_08D5) | 0x202;

    task.signal_mask = get64(&buf, UC_SIGMASK);

    // rt_sigreturn does not set a return value; rax comes back from the frame.
    let rax = get64(&buf, m + SC_RAX);
    frame.rax = rax;
    Ok(rax)
}
