#![no_std]
#![no_main]
#![allow(dead_code)]

extern crate alloc;

use core::arch::global_asm;
use core::panic::PanicInfo;

#[macro_use]
mod serial;
mod console;
mod cpu;
mod io;
mod mm;
mod multiboot;
mod sched;
mod sync;
mod trap;

global_asm!(include_str!("boot.s"), options(att_syntax));
global_asm!(include_str!("cpu/interrupts.s"), options(att_syntax));

extern "C" {
    static kernel_stack_top: u8;
}

#[no_mangle]
pub extern "C" fn kmain(mb_info_phys: u64, magic: u64) -> ! {
    serial::init();
    println!();
    println!("=== claudeos ===");

    if magic as u32 != multiboot::MULTIBOOT_BOOTLOADER_MAGIC {
        panic!("bad multiboot magic {:#x}", magic);
    }

    let boot = unsafe { multiboot::parse(mb_info_phys) };

    cpu::gdt::init();
    cpu::gdt::set_kernel_stack(unsafe { core::ptr::addr_of!(kernel_stack_top) as u64 });
    cpu::idt::init();
    trap::init();
    cpu::init_per_cpu();
    cpu::init_sse();

    mm::frame::init(&boot);
    let (used, total) = mm::frame::stats();
    println!(
        "memory: {} MiB usable, {} MiB in use by the kernel image",
        total * 4096 / (1024 * 1024),
        used * 4096 / (1024 * 1024)
    );

    mm::heap::init();
    println!("heap: {} MiB at {:#x}", mm::KERNEL_HEAP_SIZE / (1024 * 1024), mm::KERNEL_HEAP_BASE);

    cpu::pic::init();
    cpu::pit::init(cpu::pit::TICK_HZ);
    console::init();
    cpu::pic::unmask(0);

    // Everything the loader left in low memory has been consumed.
    unsafe { mm::paging::drop_identity_map() };

    sync::enable_interrupts();

    selftest(&boot);

    println!("idle");
    loop {
        cpu::halt();
    }
}

fn selftest(boot: &multiboot::BootInfo) {
    use alloc::string::String;
    use alloc::vec::Vec;

    let mut v: Vec<u64> = Vec::new();
    for i in 0..1000 {
        v.push(i * i);
    }
    let sum: u64 = v.iter().sum();
    let mut s = String::new();
    s.push_str("heap");
    println!("selftest: {} vec sum={} len={}", s, sum, v.len());

    let space = mm::paging::AddressSpace::current();
    let probe = 0xFFFF_D000_0000_0000u64;
    space
        .map_new(probe, mm::paging::PRESENT | mm::paging::WRITABLE | mm::paging::NO_EXECUTE)
        .expect("probe map");
    unsafe {
        core::ptr::write_volatile(probe as *mut u64, 0xDEAD_BEEF);
        assert_eq!(core::ptr::read_volatile(probe as *const u64), 0xDEAD_BEEF);
    }
    space.unmap(probe);
    println!("selftest: paging map/unmap ok");

    let ticks_before = trap::ticks();
    let mut spins = 0u64;
    while trap::ticks() == ticks_before && spins < 500_000_000 {
        spins += 1;
        core::hint::spin_loop();
    }
    println!("selftest: timer ticking ({} ticks)", trap::ticks());

    for m in &boot.modules[..boot.module_count] {
        println!("module: {:#x}..{:#x} ({} bytes)", m.start, m.end, m.len());
    }
}

/// Ask QEMU's isa-debug-exit device to terminate with `code`.
pub fn qemu_exit(code: u32) -> ! {
    unsafe {
        io::outl(0xf4, code);
    }
    loop {
        cpu::halt();
    }
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    sync::disable_interrupts();
    println!();
    println!("KERNEL PANIC: {}", info);
    loop {
        cpu::halt();
    }
}
