//! The signal frame, as the aarch64 Linux ABI defines it.
//!
//! When a task has installed a handler, the kernel builds the same
//! `rt_sigframe` Linux does on the user stack, points the link register at the
//! restorer libc registered, and re-enters user mode at the handler.
//! `rt_sigreturn` reads that frame back.
//!
//! The shape is not the x86-64 one. The saved registers sit in a `sigcontext`
//! reached through the `ucontext`, and the vector registers are not written at
//! a fixed offset but in a record inside a four-kilobyte area at the end of
//! it. That area is a chain: each record says what it is and how long it is,
//! and a record of length zero ends the chain. Only one record is written
//! here, so the chain is that record and its terminator.
//!
//! Nothing here is executable. The kernel writes the frame and nothing else,
//! and the address a handler returns through is one inside the program's own
//! image, which the loader wrote and made fetchable when it loaded it. There
//! is no trampoline generated at run time and no page of kernel-supplied code
//! mapped into a program, so no cache maintenance is owed on this path. A
//! program that registered no restorer is refused rather than sent somewhere.

use super::task::VECTOR_BYTES;
use super::trap::TrapFrame;
use crate::abi::SysResult;
use crate::signal::{SigAction, SA_NODEFER};
use crate::task::Task;
use crate::uaccess;

/// `struct siginfo` comes first and is a fixed 128 bytes.
const INFO: usize = 0;
const INFO_SIZE: usize = 128;

/// `struct ucontext` follows it.
const UC: usize = INFO + INFO_SIZE;
const UC_FLAGS: usize = UC;
const UC_LINK: usize = UC + 8;
const UC_STACK: usize = UC + 16;
const UC_SIGMASK: usize = UC + 40;
/// `uc_mcontext` sits past a hundred and twenty bytes of room the ABI reserves
/// for a wider signal mask, rounded up to the sixteen bytes a `sigcontext`
/// is aligned to.
const MCONTEXT: usize = UC + 176;

/// Offsets inside `struct sigcontext`.
const SC_FAULT_ADDRESS: usize = MCONTEXT;
const SC_REGS: usize = MCONTEXT + 8;
const SC_SP: usize = MCONTEXT + 256;
const SC_PC: usize = MCONTEXT + 264;
const SC_PSTATE: usize = MCONTEXT + 272;
/// The four-kilobyte area holding the chain of records, sixteen-byte aligned
/// inside the `sigcontext` and so eight bytes past where the fields end.
const SC_RESERVED: usize = MCONTEXT + 288;
const SC_RESERVED_SIZE: usize = 4096;

/// The record that carries the vector registers.
const FPSIMD_MAGIC: u32 = 0x4650_8001;
const FPSIMD: usize = SC_RESERVED;
const FPSIMD_SIZE: usize = 16 + VECTOR_BYTES;
const FPSIMD_STATUS: usize = FPSIMD + 8;
const FPSIMD_CONTROL: usize = FPSIMD + 12;
const FPSIMD_VECTORS: usize = FPSIMD + 16;
/// A record of length zero, which is how the chain ends.
const CHAIN_END: usize = FPSIMD + FPSIMD_SIZE;

/// How much of the frame is ever written. The rest of the reserved area is
/// left alone: nothing reads past the end of the chain.
const WRITTEN_SIZE: usize = CHAIN_END + 8;
/// How much of the user stack the frame occupies.
const FRAME_SIZE: usize = SC_RESERVED + SC_RESERVED_SIZE;
/// A frame pointer and a return address placed just above the frame, so that
/// something walking the stack from inside a handler can step over it.
const RECORD_SIZE: usize = 16;

fn put64(buf: &mut [u8], offset: usize, value: u64) {
    buf[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn put32(buf: &mut [u8], offset: usize, value: u32) {
    buf[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn get64(buf: &[u8], offset: usize) -> u64 {
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&buf[offset..offset + 8]);
    u64::from_le_bytes(bytes)
}

fn get32(buf: &[u8], offset: usize) -> u32 {
    let mut bytes = [0u8; 4];
    bytes.copy_from_slice(&buf[offset..offset + 4]);
    u32::from_le_bytes(bytes)
}

/// Build a signal frame on the user stack and redirect `frame` into
/// `action.handler`. Returns false if the user stack could not be written or
/// no restorer was registered, in which case the caller should kill the task.
pub fn enter_signal_handler(
    task: &mut Task,
    signal: i32,
    action: &SigAction,
    frame: &mut TrapFrame,
) -> bool {
    // There is no code in this kernel's address space a handler could return
    // through, so a program that registered no restorer has nowhere to go.
    if action.restorer == 0 {
        return false;
    }

    let sp = (frame.sp.saturating_sub((FRAME_SIZE + RECORD_SIZE) as u64)) & !0xFu64;

    let mut buf = [0u8; WRITTEN_SIZE];

    // siginfo: si_signo, si_errno, si_code.
    put32(&mut buf, INFO, signal as u32);
    put32(&mut buf, INFO + 4, 0);
    put32(&mut buf, INFO + 8, 0);

    put64(&mut buf, UC_FLAGS, 0);
    put64(&mut buf, UC_LINK, 0);
    // uc_stack: no alternate stack is in use.
    put64(&mut buf, UC_STACK, 0);
    put64(&mut buf, UC_STACK + 8, 0);
    put64(&mut buf, UC_STACK + 16, 0);
    put64(&mut buf, UC_SIGMASK, task.signal_mask);

    put64(&mut buf, SC_FAULT_ADDRESS, frame.far);
    for (i, value) in frame.x.iter().enumerate() {
        put64(&mut buf, SC_REGS + i * 8, *value);
    }
    put64(&mut buf, SC_SP, frame.sp);
    put64(&mut buf, SC_PC, frame.elr);
    put64(&mut buf, SC_PSTATE, frame.spsr);

    // The handler runs on the same registers the interrupted code was using,
    // and nothing else would put the vector ones back.
    task.cpu.fpu.save();
    put32(&mut buf, FPSIMD, FPSIMD_MAGIC);
    put32(&mut buf, FPSIMD + 4, FPSIMD_SIZE as u32);
    put32(&mut buf, FPSIMD_STATUS, task.cpu.fpu.status_word());
    put32(&mut buf, FPSIMD_CONTROL, task.cpu.fpu.control_word());
    buf[FPSIMD_VECTORS..FPSIMD_VECTORS + VECTOR_BYTES]
        .copy_from_slice(task.cpu.fpu.vectors());
    put32(&mut buf, CHAIN_END, 0);
    put32(&mut buf, CHAIN_END + 4, 0);

    if uaccess::write_bytes(sp, &buf).is_err() {
        return false;
    }

    // The frame record above the frame, holding what the interrupted code had
    // in the two registers a stack walk follows.
    let mut record = [0u8; RECORD_SIZE];
    put64(&mut record, 0, frame.x[29]);
    put64(&mut record, 8, frame.x[30]);
    if uaccess::write_bytes(sp + FRAME_SIZE as u64, &record).is_err() {
        return false;
    }

    // Block this signal for the duration of the handler unless asked not to.
    task.signal_mask |= action.mask;
    if action.flags & SA_NODEFER == 0 {
        task.signal_mask |= 1u64 << (signal as u64 & 63);
    }

    frame.elr = action.handler;
    frame.sp = sp;
    frame.x[0] = signal as u64;
    frame.x[1] = sp + INFO as u64;
    frame.x[2] = sp + UC as u64;
    frame.x[29] = sp + FRAME_SIZE as u64;
    frame.x[30] = action.restorer;
    true
}

/// Read the signal frame back and restore the register state the handler was
/// entered with.
pub fn leave_signal_handler(task: &mut Task, frame: &mut TrapFrame) -> SysResult {
    // The restorer was reached by branching to it rather than by returning
    // through the stack, so the stack pointer is still at the frame.
    let base = frame.sp;
    let mut buf = [0u8; WRITTEN_SIZE];
    uaccess::read_bytes(base, &mut buf)?;

    for (i, value) in frame.x.iter_mut().enumerate() {
        *value = get64(&buf, SC_REGS + i * 8);
    }
    frame.sp = get64(&buf, SC_SP);
    frame.elr = get64(&buf, SC_PC);

    // Only the condition flags are taken from the frame. The rest of the
    // processor state says which level the program runs at and which
    // exceptions are masked, and neither is a program's to choose.
    frame.spsr = get64(&buf, SC_PSTATE) & 0xF000_0000;

    task.signal_mask = get64(&buf, UC_SIGMASK);

    // Put the interrupted code's vector registers back, if the record saying
    // where they are is the one this kernel wrote.
    if get32(&buf, FPSIMD) == FPSIMD_MAGIC
        && get32(&buf, FPSIMD + 4) as usize == FPSIMD_SIZE
        && task.cpu.fpu.load(
            &buf[FPSIMD_VECTORS..FPSIMD_VECTORS + VECTOR_BYTES],
            get32(&buf, FPSIMD_CONTROL),
            get32(&buf, FPSIMD_STATUS),
        )
    {
        task.cpu.fpu.restore();
    }

    // rt_sigreturn does not set a return value; the first register comes back
    // from the frame like every other one.
    Ok(frame.x[0])
}
