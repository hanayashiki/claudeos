//! The signal frame, as the aarch64 Linux ABI defines it.
//!
//! When a task has installed a handler, the kernel builds the same
//! `rt_sigframe` Linux does on the user stack, points the link register at the
//! address the handler returns through, and re-enters user mode at the
//! handler. `rt_sigreturn` reads that frame back.
//!
//! The shape is not the x86-64 one. The saved registers sit in a `sigcontext`
//! reached through the `ucontext`, and the vector registers are not written at
//! a fixed offset but in a record inside a four-kilobyte area at the end of
//! it. That area is a chain: each record says what it is and how long it is,
//! and a record of length zero ends the chain. Only one record is written
//! here, so the chain is that record and its terminator.
//!
//! The address a handler returns through is the restorer the program
//! registered, when it registered one. Linux on this machine does not read
//! that field at all: it maps a page of its own holding the return sequence
//! into every program and sends a handler back through that. So a program
//! built for this machine has no reason to fill the field in, and Go does not.
//! `map_signal_trampoline` builds the same page here, and it is where a
//! handler goes back through when the field is empty.

use super::paging::{AddressSpace, FreshPage, PRESENT, USER};
use super::task::VECTOR_BYTES;
use super::trap::TrapFrame;
use crate::abi::{Errno, SysResult, MAP_PRIVATE, PROT_EXEC, PROT_READ};
use crate::mm::{PAGE_SIZE_U64, USER_TRAMPOLINE};
use crate::signal::{SigAction, Signal, SA_NODEFER, SA_ONSTACK};
use crate::task::Task;
use crate::uaccess;

/// `movz x8, #0`, with room in the immediate for the call number.
const MOVZ_X8: u32 = 0xD280_0008;
/// `svc #0`.
const SVC_0: u32 = 0xD400_0001;

/// The sequence a handler returns through: the number of the call that reads
/// the signal frame back in the register that carries a call number, then the
/// instruction that makes the call. It is the same pair Linux maps, and the
/// immediate comes from the number the dispatcher matches on rather than
/// being written out again, so the two cannot come to name different calls.
const RETURN_SEQUENCE: [u32; 2] = [MOVZ_X8 | ((super::nr::RT_SIGRETURN as u32) << 5), SVC_0];

const _: () = assert!(super::nr::RT_SIGRETURN < 1 << 16, "movz carries 16 bits");

/// Give `space` the page a handler returns through, and record the region it
/// occupies so that an mmap with no address of its own is placed past it.
///
/// One page per address space rather than one frame shared by all of them.
/// The frame would be cheaper, but a program may call `mprotect` on any
/// address it owns, and this kernel grants write access to a page that asks
/// for it unless the page carries the copy-on-write mark. That mark is not
/// available here: it means a page the program may write once it has a copy
/// of its own, and a write to a page of code is a fault rather than a silent
/// copy. A shared frame would therefore be one a program could get write
/// access to and then change underneath every other program on the machine.
/// A page apiece costs a frame per exec -- a fork shares its parent's
/// read-only through the same path as the rest of the image -- and leaves
/// what a program does to its own trampoline its own business.
///
/// Every address space gets one: whether a program will install a handler is
/// not known when its address space is being built. It goes in at exec, where
/// the rest of the address space is laid out, and a fork inherits it with
/// everything else.
pub fn map_signal_trampoline(task: &Task, space: &AddressSpace) -> Result<(), Errno> {
    let mut page = FreshPage::new().ok_or(Errno::ENOMEM)?;
    let mut at = 0;
    for instruction in RETURN_SEQUENCE {
        page.bytes()[at..at + 4].copy_from_slice(&instruction.to_le_bytes());
        at += 4;
    }
    // The kernel stored these bytes through the direct map and the program
    // fetches them as instructions through its own address. The two caches are
    // not coherent here, so what was stored has to be pushed down to where the
    // fetch will look before anything jumps there. Left out, a program runs
    // whatever the instruction cache was holding for that frame, which
    // emulation never shows.
    super::sync_instruction_cache(page.bytes().as_ptr() as u64, at);

    // Read-only and executable, which is what it stays. It is never reachable
    // any wider: the contents are finished before the page is published, and
    // publishing is what makes it reachable at all.
    space.publish(USER_TRAMPOLINE, page, PRESENT | USER).map_err(|_| Errno::ENOMEM)?;
    task.add_vma(
        USER_TRAMPOLINE,
        USER_TRAMPOLINE + PAGE_SIZE_U64,
        PROT_READ | PROT_EXEC,
        MAP_PRIVATE,
    );
    Ok(())
}

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

/// The least an alternate stack may be and still be accepted, which is what
/// Linux asks for on this machine. It is larger than x86-64's because the
/// frame is: the vector registers go in a four-kilobyte area rather than at a
/// fixed offset. The frame this file writes is smaller than it, so a stack
/// that passes has room for one.
pub const MIN_ALT_STACK: u64 = 5120;

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
/// `action.handler`. Returns false if the user stack could not be written, in
/// which case the caller should kill the task.
pub fn enter_signal_handler(
    task: &Task,
    signal: Signal,
    action: &SigAction,
    frame: &mut TrapFrame,
) -> bool {
    // A disposition that asked for its own stack gets it, unless a handler is
    // already running on it -- nesting continues down the same stack rather
    // than starting again at its top, which would write over the frame the
    // outer handler is using.
    let alt = task.sig_stack.get();
    let on_alt = action.flags & SA_ONSTACK != 0 && alt.installed() && !alt.contains(frame.sp);
    let below = if on_alt { alt.ss_sp + alt.ss_size } else { frame.sp };
    let sp = (below.saturating_sub((FRAME_SIZE + RECORD_SIZE) as u64)) & !0xFu64;

    let mut buf = [0u8; WRITTEN_SIZE];

    // siginfo: si_signo, si_errno, si_code.
    put32(&mut buf, INFO, signal.number() as u32);
    put32(&mut buf, INFO + 4, 0);
    put32(&mut buf, INFO + 8, 0);

    put64(&mut buf, UC_FLAGS, 0);
    put64(&mut buf, UC_LINK, 0);
    // uc_stack: the alternate stack as it stood when the handler was entered,
    // which is what `rt_sigreturn` puts back and what a handler asking where
    // it is reads.
    put64(&mut buf, UC_STACK, alt.ss_sp);
    put64(&mut buf, UC_STACK + 8, alt.flags_at(frame.sp) as u32 as u64);
    put64(&mut buf, UC_STACK + 16, alt.ss_size);
    put64(&mut buf, UC_SIGMASK, task.signal_mask.get());

    put64(&mut buf, SC_FAULT_ADDRESS, frame.far);
    for (i, value) in frame.x.iter().enumerate() {
        put64(&mut buf, SC_REGS + i * 8, *value);
    }
    put64(&mut buf, SC_SP, frame.sp);
    put64(&mut buf, SC_PC, frame.elr);
    put64(&mut buf, SC_PSTATE, frame.spsr);

    // The handler runs on the same registers the interrupted code was using,
    // and nothing else would put the vector ones back.
    task.with_cpu(|cpu| {
        cpu.fpu.save();
        put32(&mut buf, FPSIMD, FPSIMD_MAGIC);
        put32(&mut buf, FPSIMD + 4, FPSIMD_SIZE as u32);
        put32(&mut buf, FPSIMD_STATUS, cpu.fpu.status_word());
        put32(&mut buf, FPSIMD_CONTROL, cpu.fpu.control_word());
        buf[FPSIMD_VECTORS..FPSIMD_VECTORS + VECTOR_BYTES].copy_from_slice(cpu.fpu.vectors());
    });
    put32(&mut buf, CHAIN_END, 0);
    put32(&mut buf, CHAIN_END + 4, 0);

    if uaccess::write_bytes_in(task, sp, &buf).is_err() {
        return false;
    }

    // The frame record above the frame, holding what the interrupted code had
    // in the two registers a stack walk follows.
    let mut record = [0u8; RECORD_SIZE];
    put64(&mut record, 0, frame.x[29]);
    put64(&mut record, 8, frame.x[30]);
    if uaccess::write_bytes_in(task, sp + FRAME_SIZE as u64, &record).is_err() {
        return false;
    }

    // Block this signal for the duration of the handler unless asked not to.
    task.signal_mask.set(task.signal_mask.get() | action.mask);
    if action.flags & SA_NODEFER == 0 {
        task.signal_mask.set(task.signal_mask.get() | signal.bit());
    }

    frame.elr = action.handler;
    frame.sp = sp;
    frame.x[0] = signal.number() as u64;
    frame.x[1] = sp + INFO as u64;
    frame.x[2] = sp + UC as u64;
    frame.x[29] = sp + FRAME_SIZE as u64;
    // Where the handler returns to. A program that registered a restorer is
    // sent back through it, which is what everything built against musl does;
    // one that registered none goes through the page above, which is what a
    // program built for Linux on this machine expects and never asked for.
    frame.x[30] = if action.restorer != 0 { action.restorer } else { USER_TRAMPOLINE };
    true
}

/// Read the signal frame back and restore the register state the handler was
/// entered with.
pub fn leave_signal_handler(task: &Task, frame: &mut TrapFrame) -> SysResult {
    // The restorer was reached by branching to it rather than by returning
    // through the stack, so the stack pointer is still at the frame.
    let base = frame.sp;
    let mut buf = [0u8; WRITTEN_SIZE];
    uaccess::read_bytes_in(task, base, &mut buf)?;

    for (i, value) in frame.x.iter_mut().enumerate() {
        *value = get64(&buf, SC_REGS + i * 8);
    }
    frame.sp = get64(&buf, SC_SP);
    frame.elr = get64(&buf, SC_PC);

    // Only the condition flags are taken from the frame. The rest of the
    // processor state says which level the program runs at and which
    // exceptions are masked, and neither is a program's to choose.
    frame.spsr = get64(&buf, SC_PSTATE) & 0xF000_0000;

    task.signal_mask.set(get64(&buf, UC_SIGMASK));
    restore_alt_stack(task, &buf, base);

    // Put the interrupted code's vector registers back, if the record saying
    // where they are is the one this kernel wrote.
    if get32(&buf, FPSIMD) == FPSIMD_MAGIC && get32(&buf, FPSIMD + 4) as usize == FPSIMD_SIZE {
        task.with_cpu(|cpu| {
            if cpu.fpu.load(
                &buf[FPSIMD_VECTORS..FPSIMD_VECTORS + VECTOR_BYTES],
                get32(&buf, FPSIMD_CONTROL),
                get32(&buf, FPSIMD_STATUS),
            ) {
                cpu.fpu.restore();
            }
        });
    }

    // rt_sigreturn does not set a return value; the first register comes back
    // from the frame like every other one.
    Ok(frame.x[0])
}
