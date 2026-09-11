//! Exception and IRQ handling.

use crate::cpu::idt::{register, TrapFrame, EXCEPTION_NAMES};
use crate::cpu::{pic, read_cr2};
use core::sync::atomic::{AtomicU64, Ordering};

pub static TICKS: AtomicU64 = AtomicU64::new(0);

pub fn init() {
    for vector in 0..32u8 {
        register(vector, exception);
    }
    register(14, page_fault);
    register(pic::PIC1_OFFSET, timer);
    for irq in 1..16u8 {
        register(pic::PIC1_OFFSET + irq, spurious);
    }
}

pub fn ticks() -> u64 {
    TICKS.load(Ordering::Relaxed)
}

fn timer(_frame: &mut TrapFrame) {
    TICKS.fetch_add(1, Ordering::Relaxed);
    pic::end_of_interrupt(0);
    crate::sched::on_tick();
}

fn spurious(frame: &mut TrapFrame) {
    let irq = (frame.vector - pic::PIC1_OFFSET as u64) as u8;
    if irq == 4 || irq == 3 {
        crate::console::serial_irq();
    } else if irq == 1 {
        crate::console::keyboard_irq();
    }
    pic::end_of_interrupt(irq);
}

fn page_fault(frame: &mut TrapFrame) {
    let addr = read_cr2();
    let code = frame.error_code;

    if frame.from_user() {
        if crate::sched::handle_user_page_fault(addr, code, frame) {
            return;
        }
        println!(
            "[trap] user page fault at {:#x} rip={:#x} code={:#x}{}{}{}",
            addr,
            frame.rip,
            code,
            if code & 1 != 0 { " protection" } else { " not-present" },
            if code & 2 != 0 { " write" } else { " read" },
            if code & 16 != 0 { " instruction-fetch" } else { "" },
        );
        crate::sched::kill_current(11); // SIGSEGV
        return;
    }

    // A kernel fault is not recoverable; dump everything useful and stop.
    println!();
    println!("KERNEL PAGE FAULT");
    println!("  address : {:#018x}", addr);
    println!("  rip     : {:#018x}", frame.rip);
    println!(
        "  cause   : {}{}{}{}",
        if code & 1 != 0 { "protection-violation " } else { "not-present " },
        if code & 2 != 0 { "write " } else { "read " },
        if code & 4 != 0 { "user " } else { "kernel " },
        if code & 16 != 0 { "instruction-fetch" } else { "" },
    );
    dump(frame);
    panic!("unrecoverable kernel page fault");
}

fn exception(frame: &mut TrapFrame) {
    let vector = frame.vector as usize;
    let name = EXCEPTION_NAMES.get(vector).copied().unwrap_or("unknown");

    if frame.from_user() {
        println!(
            "[trap] user exception {} ({}) rip={:#x} err={:#x}",
            vector, name, frame.rip, frame.error_code
        );
        let signal = match vector {
            0 => 8,   // SIGFPE
            3 => 5,   // SIGTRAP
            4 => 8,   // SIGFPE
            6 => 4,   // SIGILL
            _ => 11,  // SIGSEGV
        };
        crate::sched::kill_current(signal);
        return;
    }

    println!();
    println!("KERNEL EXCEPTION {} ({})", vector, name);
    println!("  error code: {:#x}", frame.error_code);
    dump(frame);
    panic!("unhandled kernel exception {}", vector);
}

pub fn unhandled(frame: &mut TrapFrame) {
    let vector = frame.vector;
    if (32..48).contains(&vector) {
        pic::end_of_interrupt((vector - 32) as u8);
        return;
    }
    println!("[trap] unhandled vector {} rip={:#x}", vector, frame.rip);
    if frame.from_user() {
        crate::sched::kill_current(11);
    }
}

pub fn dump(frame: &TrapFrame) {
    println!("  rip {:#018x}  cs  {:#06x}  rflags {:#018x}", frame.rip, frame.cs, frame.rflags);
    println!("  rsp {:#018x}  ss  {:#06x}", frame.rsp, frame.ss);
    println!("  rax {:#018x}  rbx {:#018x}  rcx {:#018x}", frame.rax, frame.rbx, frame.rcx);
    println!("  rdx {:#018x}  rsi {:#018x}  rdi {:#018x}", frame.rdx, frame.rsi, frame.rdi);
    println!("  rbp {:#018x}  r8  {:#018x}  r9  {:#018x}", frame.rbp, frame.r8, frame.r9);
    println!("  r10 {:#018x}  r11 {:#018x}  r12 {:#018x}", frame.r10, frame.r11, frame.r12);
    println!("  r13 {:#018x}  r14 {:#018x}  r15 {:#018x}", frame.r13, frame.r14, frame.r15);
}
