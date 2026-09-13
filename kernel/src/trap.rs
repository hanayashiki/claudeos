//! Exception and IRQ handling.

use crate::arch::{self, TrapFrame};
use core::sync::atomic::{AtomicU64, Ordering};

pub static TICKS: AtomicU64 = AtomicU64::new(0);

pub fn init() {
    for vector in 0..arch::EXCEPTION_COUNT {
        arch::register_trap_handler(vector, exception);
    }
    arch::register_trap_handler(arch::PAGE_FAULT_VECTOR, page_fault);
    for irq in 0..arch::IRQ_COUNT {
        arch::register_irq_handler(irq, device);
    }
    // Last, because the timer's line is not always outside the range above:
    // on some machines it is an ordinary device interrupt like any other.
    arch::register_irq_handler(arch::TIMER_IRQ, timer);
}

pub fn ticks() -> u64 {
    TICKS.load(Ordering::Relaxed)
}

fn timer(frame: &mut TrapFrame) {
    TICKS.fetch_add(1, Ordering::Relaxed);
    arch::end_of_interrupt(arch::TIMER_IRQ);
    // A task killed while spinning in user mode notices here.
    if frame.from_user() {
        crate::sched::check_signals();
    }
    crate::sched::on_tick();
}

fn device(frame: &mut TrapFrame) {
    let Some(irq) = arch::vector_irq(arch::trap_vector(frame)) else { return };
    if irq == arch::SERIAL_IRQ || irq == arch::SERIAL_IRQ_ALT {
        crate::console::serial_irq();
    } else if irq == arch::KEYBOARD_IRQ {
        crate::console::keyboard_irq();
    }
    arch::end_of_interrupt(irq);
}

fn page_fault(frame: &mut TrapFrame) {
    let fault = arch::page_fault(frame);
    let addr = fault.address;

    if frame.from_user() {
        if crate::sched::handle_user_page_fault(&fault) {
            return;
        }
        println!(
            "[trap] user page fault at {:#x} rip={:#x} code={:#x}{}{}{}",
            addr,
            arch::instruction_pointer(frame),
            fault.raw,
            if fault.present { " protection" } else { " not-present" },
            if fault.write { " write" } else { " read" },
            if fault.instruction_fetch { " instruction-fetch" } else { "" },
        );
        dump(frame);
        // The registers name the memory the program was walking; show it, so
        // a fault on a structure says what the structure held.
        for (name, at) in arch::fault_probe_registers(frame) {
            dump_user(name, at);
        }
        dump_regions(addr);
        crate::sched::kill_current(11); // SIGSEGV
    }

    // A kernel fault is not recoverable; dump everything useful and stop.
    println!();
    println!("KERNEL PAGE FAULT");
    println!("  address : {:#018x}", addr);
    println!("  rip     : {:#018x}", arch::instruction_pointer(frame));
    println!(
        "  cause   : {}{}{}{}",
        if fault.present { "protection-violation " } else { "not-present " },
        if fault.write { "write " } else { "read " },
        if fault.user { "user " } else { "kernel " },
        if fault.instruction_fetch { "instruction-fetch" } else { "" },
    );
    // Every entry the walker reads for this address, out of the tables the
    // machine is pointed at. A fault the last entry says should not have
    // happened is one this is the only way to take further: what is left is a
    // level above it, a translation cached from an older entry, or tables that
    // are not the ones the access went through.
    let live = arch::paging::AddressSpace::current();
    live.dump_walk(addr);
    if crate::sched::has_current() {
        let task = crate::sched::current();
        println!("  faulted in pid {} ({})", task.pid, task.name());
        // The same tables in every path that reaches user memory through a
        // task. Printed when they are not, because a check made against one
        // and an access made through the other explains a fault that neither
        // on its own does.
        if task.space() != live {
            println!("  but the task is recorded on other tables:");
            task.space().dump_walk(addr);
        }
    }
    dump(frame);
    panic!("unrecoverable kernel page fault");
}

fn exception(frame: &mut TrapFrame) {
    let vector = arch::trap_vector(frame);
    let name = arch::exception_name(vector);

    if frame.from_user() {
        println!(
            "[trap] user exception {} ({}) rip={:#x} err={:#x}",
            vector,
            name,
            arch::instruction_pointer(frame),
            arch::trap_error_code(frame)
        );
        crate::sched::kill_current(arch::exception_signal(vector));
    }

    println!();
    println!("KERNEL EXCEPTION {} ({})", vector, name);
    println!("  error code: {:#x}", arch::trap_error_code(frame));
    dump(frame);
    panic!("unhandled kernel exception {}", vector);
}

pub fn unhandled(frame: &mut TrapFrame) {
    let vector = arch::trap_vector(frame);
    if let Some(irq) = arch::vector_irq(vector) {
        arch::end_of_interrupt(irq);
        return;
    }
    println!("[trap] unhandled vector {} rip={:#x}", vector, arch::instruction_pointer(frame));
    if frame.from_user() {
        crate::sched::kill_current(11);
    }
}

/// What the task believes it has mapped, so a fault says whether the address
/// was inside a region at all.
fn dump_regions(addr: u64) {
    let task = crate::sched::current();
    let mm = task.mm();
    let mm = mm.lock();
    println!(
        "  regions: brk {:#x}..{:#x}  mmap_top {:#x}  {} vmas",
        mm.brk_start,
        mm.brk,
        mm.mmap_top,
        mm.vmas.len()
    );
    for vma in mm.vmas.iter() {
        let hit = if addr >= vma.start && addr < vma.end { " <== fault" } else { "" };
        println!(
            "    {:#014x}..{:#014x} prot {:#x} flags {:#x} {}{}",
            vma.start,
            vma.end,
            vma.prot,
            vma.flags,
            if vma.file.is_some() { "file" } else { "anon" },
            hit
        );
    }
}

/// Four 16-byte lines of user memory at `at`, skipped if it is not mapped.
fn dump_user(name: &str, at: u64) {
    let start = at & !0xF;
    if start < 0x1000 {
        return;
    }
    for line in 0..4u64 {
        let address = start + line * 16;
        let mut bytes = [0u8; 16];
        if crate::uaccess::read_bytes(address, &mut bytes).is_err() {
            return;
        }
        let mut words = [0u64; 2];
        for (i, word) in words.iter_mut().enumerate() {
            let mut raw = [0u8; 8];
            raw.copy_from_slice(&bytes[i * 8..i * 8 + 8]);
            *word = u64::from_le_bytes(raw);
        }
        println!(
            "  {} {:#018x}: {:#018x} {:#018x}",
            if line == 0 { name } else { "   " },
            address,
            words[0],
            words[1]
        );
    }
}

pub fn dump(frame: &TrapFrame) {
    arch::dump_registers(frame);
}
