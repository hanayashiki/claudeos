//! The signal frame, as the x86-64 Linux ABI defines it.
//!
//! When a task has installed a handler, the kernel builds the same
//! `rt_sigframe` Linux does on the user stack, points the return address at
//! the libc restorer, and re-enters user mode at the handler. `rt_sigreturn`
//! reads that frame back.
//!
//! That address is the program's to supply. There is no page of kernel code
//! mapped into a program on this machine to return through -- aarch64 is the
//! one with such a page -- so a disposition naming no restorer is refused
//! where the signal would be delivered, which is what Linux does here too.

use super::cpu::idt::TrapFrame;
use super::paging::PageTables;
use super::task::FPU_STATE_SIZE;
use crate::abi::{Errno, SysResult};
use crate::signal::{Fault, SigAction, Signal, SA_NODEFER, SA_ONSTACK};
use crate::task::Task;
use crate::uaccess;

/// Nothing to map here. A handler on this machine returns through the address
/// the program registered and there is no other place it could go: Linux
/// refuses to deliver a signal whose disposition names no restorer, rather
/// than supplying one. The name exists because the exec path calls it on
/// whichever machine it is built for; aarch64 is the one with a page to map.
pub fn map_signal_trampoline(_task: &Task, _space: &PageTables) -> Result<(), Errno> {
    Ok(())
}

// Frame layout, matching the x86_64 kernel ABI.
const UC_OFFSET: usize = 8;
const UC_FLAGS: usize = UC_OFFSET;
const UC_LINK: usize = UC_OFFSET + 8;
const UC_STACK: usize = UC_OFFSET + 16;
const MCONTEXT: usize = UC_OFFSET + 40;
const UC_SIGMASK: usize = MCONTEXT + 256;
const INFO_OFFSET: usize = UC_SIGMASK + 8;
/// The x87 and SSE registers, saved where `struct sigcontext`'s `fpstate`
/// points. The frame is placed at an address 8 past a 16-byte boundary, and
/// this offset is 8 past a multiple of 16, so the image lands aligned the way
/// `fxsave` needs it.
const FPSTATE_OFFSET: usize = INFO_OFFSET + 128;
const FRAME_SIZE: usize = FPSTATE_OFFSET + FPU_STATE_SIZE;

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
/// The fault address, which Linux also copies here for a page fault.
const SC_CR2: usize = 176;
/// Where `struct sigcontext` keeps the pointer to the saved x87/SSE image.
const SC_FPSTATE: usize = 184;

/// The least an alternate stack may be and still be accepted, which is what
/// Linux asks for on this machine. The frame this file writes is smaller than
/// it, so a stack that passes has room for one.
pub const MIN_ALT_STACK: u64 = 2048;

fn put64(buf: &mut [u8], offset: usize, value: u64) {
    buf[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn get64(buf: &[u8], offset: usize) -> u64 {
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&buf[offset..offset + 8]);
    u64::from_le_bytes(bytes)
}

/// Take the alternate stack back from `uc_stack`, which is where the handler
/// could have changed it. A handler that left the field alone puts back what
/// was already there.
fn restore_alt_stack(task: &Task, buf: &[u8], sp: u64) {
    // A handler still standing on the stack cannot have it taken away or
    // moved, so what the frame says is not acted on there.
    if task.sig_stack.get().contains(sp) {
        return;
    }
    let ss_sp = get64(buf, UC_STACK);
    let ss_flags = get64(buf, UC_STACK + 8) as u32 as i32;
    let ss_size = get64(buf, UC_STACK + 16);
    let stack = if ss_flags & crate::abi::SS_DISABLE != 0 || ss_size < MIN_ALT_STACK {
        crate::abi::SigAltStack::default()
    } else {
        crate::abi::SigAltStack { ss_sp, ss_flags: 0, _pad: 0, ss_size }
    };
    task.sig_stack.set(stack);
}

/// Build a signal frame on the user stack and redirect `frame` into
/// `action.handler`. Returns false if the user stack could not be written or
/// no restorer was registered, in which case the caller should kill the task.
/// `fault` is what the signal was raised for, when a fault raised it.
pub fn enter_signal_handler(
    task: &Task,
    signal: Signal,
    action: &SigAction,
    frame: &mut TrapFrame,
    fault: Option<Fault>,
) -> bool {
    // The return address on the frame is the only way back from a handler on
    // this machine, and a program that registered no restorer has given
    // nothing to put there. Linux refuses the delivery in that case rather
    // than sending the handler somewhere, and so does this. Say which program
    // and which signal, because from outside it looks like a program that took
    // a fault it never executed an instruction for.
    if action.restorer == 0 {
        crate::println!(
            "[signal] pid={} registered no restorer for signal {}, \
             and this machine has no return sequence of its own to supply",
            crate::sched::current().pid,
            signal.number()
        );
        return false;
    }

    // A disposition that asked for its own stack gets it, unless a handler is
    // already running on it -- nesting continues down the same stack rather
    // than starting again at its top, which would write over the frame the
    // outer handler is using.
    let alt = task.sig_stack.get();
    let on_alt = action.flags & SA_ONSTACK != 0 && alt.installed() && !alt.contains(frame.rsp);
    // Leave the red zone alone, then place the frame so that the handler sees
    // the alignment a `call` would have produced. The red zone belongs to the
    // interrupted function's own stack, so a frame that starts at the top of a
    // fresh one has nothing below it to leave alone.
    let mut sp = if on_alt { alt.ss_sp + alt.ss_size } else { frame.rsp.saturating_sub(128) };
    sp = sp.saturating_sub(FRAME_SIZE as u64);
    sp &= !0xFu64;
    sp = sp.wrapping_sub(8);

    let mut buf = [0u8; FRAME_SIZE];

    // Return address: the restorer libc registered, which calls rt_sigreturn.
    put64(&mut buf, 0, action.restorer);

    put64(&mut buf, UC_FLAGS, 0);
    put64(&mut buf, UC_LINK, 0);
    // uc_stack: the alternate stack as it stood when the handler was entered,
    // which is what `rt_sigreturn` puts back and what a handler asking where
    // it is reads.
    put64(&mut buf, UC_STACK, alt.ss_sp);
    put64(&mut buf, UC_STACK + 8, alt.flags_at(frame.rsp) as u32 as u64);
    put64(&mut buf, UC_STACK + 16, alt.ss_size);

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

    put64(&mut buf, UC_SIGMASK, task.blocked());

    // The handler runs on the same registers the interrupted code was using,
    // and nothing else would put the floating point and vector ones back.
    task.with_cpu(|cpu| {
        cpu.fpu.save();
        buf[FPSTATE_OFFSET..FPSTATE_OFFSET + FPU_STATE_SIZE].copy_from_slice(cpu.fpu.bytes());
    });
    put64(&mut buf, m + SC_FPSTATE, sp + FPSTATE_OFFSET as u64);

    // siginfo: si_signo, si_errno, si_code, and for a fault `si_addr`, the
    // first word of the union after the three ints and their padding. A
    // handler for a fault reads what went wrong and where from here: Go's
    // prints "unexpected fault address" with it, and tells a nil dereference
    // it can turn into a panic from a signal sent with kill by the code.
    let (code, address) = fault.map_or((0, 0), |fault| (fault.code, fault.address));
    buf[INFO_OFFSET..INFO_OFFSET + 4].copy_from_slice(&signal.number().to_le_bytes());
    buf[INFO_OFFSET + 4..INFO_OFFSET + 8].copy_from_slice(&0i32.to_le_bytes());
    buf[INFO_OFFSET + 8..INFO_OFFSET + 12].copy_from_slice(&code.to_le_bytes());
    put64(&mut buf, INFO_OFFSET + 16, address);
    put64(&mut buf, m + SC_CR2, address);

    if uaccess::write_bytes_in(task, sp, &buf).is_err() {
        return false;
    }

    // Block this signal for the duration of the handler unless asked not to.
    task.block(action.mask);
    if action.flags & SA_NODEFER == 0 {
        task.block(signal.bit());
    }

    frame.rip = action.handler;
    frame.rsp = sp;
    frame.rdi = signal.number() as u64;
    frame.rsi = sp + INFO_OFFSET as u64;
    frame.rdx = sp + UC_OFFSET as u64;
    frame.rax = 0;
    true
}

/// Read the signal frame back and restore the register state the handler was
/// entered with.
pub fn leave_signal_handler(task: &Task, frame: &mut TrapFrame) -> SysResult {
    // The restorer was reached by returning into it, so the frame starts one
    // word below the current stack pointer.
    let base = frame.rsp.wrapping_sub(8);
    let mut buf = [0u8; FRAME_SIZE];
    uaccess::read_bytes_in(task, base, &mut buf)?;

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

    task.set_blocked(get64(&buf, UC_SIGMASK));
    restore_alt_stack(task, &buf, base);

    // Put the interrupted code's floating point and vector registers back.
    if get64(&buf, m + SC_FPSTATE) != 0 {
        task.with_cpu(|cpu| {
            if cpu.fpu.from_bytes(&buf[FPSTATE_OFFSET..FPSTATE_OFFSET + FPU_STATE_SIZE]) {
                cpu.fpu.restore();
            }
        });
    }

    // rt_sigreturn does not set a return value; rax comes back from the frame.
    let rax = get64(&buf, m + SC_RAX);
    frame.rax = rax;
    Ok(rax)
}
