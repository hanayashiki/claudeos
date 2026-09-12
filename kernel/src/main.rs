#![no_std]
#![no_main]
#![allow(dead_code)]

extern crate alloc;

use core::arch::global_asm;
use core::panic::PanicInfo;

#[macro_use]
mod serial;
mod abi;
mod console;
mod cpu;
mod elf;
mod fs;
mod futex;
mod io;
mod mm;
mod multiboot;
mod net;
mod pci;
mod sched;
mod signal;
mod sync;
mod syscall;
mod task;
mod time;
mod trap;
mod uaccess;

use alloc::string::{String, ToString};
use alloc::vec::Vec;

global_asm!(include_str!("boot.s"), options(att_syntax));
global_asm!(include_str!("cpu/interrupts.s"), options(att_syntax));
global_asm!(include_str!("switch.s"), options(att_syntax));
global_asm!(include_str!("syscall/entry.s"), options(att_syntax));

extern "C" {
    static kernel_stack_top: u8;
}

/// Options parsed out of the boot loader command line.
struct BootOptions {
    init: String,
    trace: i64,
    /// Send one hand-built ARP request for the gateway at boot and report
    /// what comes back. Off unless the word `nettest` is on the command line,
    /// so the ordinary suites never see it.
    nettest: bool,
    /// The addresses the protocol stack uses, and whether to run its own
    /// checks instead of booting.
    net: net::Config,
    net_test: bool,
    args: Vec<String>,
}

fn parse_cmdline(cmdline: Option<&str>) -> BootOptions {
    let mut options = BootOptions {
        init: "/bin/init".to_string(),
        trace: syscall::TRACE_OFF,
        nettest: false,
        net: net::Config::QEMU_USER,
        net_test: false,
        args: Vec::new(),
    };
    let Some(cmdline) = cmdline else { return options };
    // The boot loader puts the kernel's own path in the first word.
    for word in cmdline.split_whitespace().skip(1) {
        if let Some(value) = word.strip_prefix("init=") {
            options.init = value.to_string();
        } else if let Some(value) = word.strip_prefix("trace=") {
            options.trace = if value == "all" {
                syscall::TRACE_ALL
            } else {
                value.parse().unwrap_or(syscall::TRACE_OFF)
            };
        } else if word == "nettest" {
            options.nettest = true;
        } else if let Some(value) = word.strip_prefix("net=") {
            // The protocols against a card that only records what it is
            // asked to send, in place of booting anything.
            options.net_test = value == "test";
        } else if let Some(value) = word.strip_prefix("ip=") {
            if let Some(address) = net::ip::parse_address(value) {
                options.net.address = address;
            }
        } else if let Some(value) = word.strip_prefix("netmask=") {
            if let Some(address) = net::ip::parse_address(value) {
                options.net.netmask = address;
            }
        } else if let Some(value) = word.strip_prefix("gateway=") {
            if let Some(address) = net::ip::parse_address(value) {
                options.net.gateway = address;
            }
        } else if let Some(value) = word.strip_prefix("nameserver=") {
            if let Some(address) = net::ip::parse_address(value) {
                options.net.nameserver = address;
            }
        } else {
            // Anything else is handed to the init process as an argument.
            options.args.push(word.to_string());
        }
    }
    options
}

#[no_mangle]
pub extern "C" fn kmain(mb_info_phys: u64, magic: u64) -> ! {
    serial::init();
    println!();
    println!("claudeos: booting");

    if magic as u32 != multiboot::MULTIBOOT_BOOTLOADER_MAGIC {
        panic!("bad multiboot magic {:#x}", magic);
    }
    let boot = unsafe { multiboot::parse(mb_info_phys) };

    cpu::gdt::init();
    cpu::gdt::set_kernel_stack(core::ptr::addr_of!(kernel_stack_top) as u64);
    cpu::idt::init();
    trap::init();
    cpu::init_per_cpu();
    cpu::init_sse();

    mm::frame::init(&boot);
    mm::heap::init();
    let (used, total) = mm::frame::stats();
    println!(
        "memory: {} MiB total, {} MiB in use, {} MiB kernel heap",
        total * 4096 / (1024 * 1024),
        used * 4096 / (1024 * 1024),
        mm::KERNEL_HEAP_SIZE / (1024 * 1024)
    );

    // The command line lives in low memory and needs the heap to parse.
    let cmdline = unsafe { multiboot::cstr_at(boot.cmdline_phys) };
    let options = parse_cmdline(cmdline);

    cpu::pic::init();
    cpu::pit::init(cpu::pit::TICK_HZ);
    time::init();
    console::init();
    cpu::pic::unmask(0);
    // Measure the timestamp counter against the tick. This has to happen
    // before there is anything to schedule, or the calibration loop is
    // preempted and measures the whole system instead of itself.
    sync::enable_interrupts();
    time::calibrate();
    sync::disable_interrupts();

    syscall::init();
    unsafe { core::ptr::write_volatile(core::ptr::addr_of_mut!(syscall::TRACE), options.trace) };

    fs::init();
    mount_initramfs(&boot);

    // Nothing reads low memory through the identity map from here on.
    unsafe { mm::paging::drop_identity_map() };

    sched::init();

    // Find and bring up the network card before the first process exists. Its
    // registers are mapped into the kernel half of the page tables, and a new
    // address space copies the kernel half as it stands at the moment it is
    // created, so a mapping made later would be missing from it. Finding no
    // card is the ordinary outcome on a machine booted without one.
    let nic = net::e1000::probe();

    // The protocol stack takes its addresses from here rather than naming any
    // of its own; a driver that attaches later does not change them.
    net::configure(options.net);
    println!(
        "net: {} netmask {} gateway {}",
        options.net.address, options.net.netmask, options.net.gateway
    );
    if options.net_test {
        println!();
        let passed = net::selftest::run();
        println!();
        println!(
            "claudeos: network self test {}",
            if passed { "passed" } else { "FAILED" }
        );
        power_off();
    }

    let mut argv = alloc::vec![options.init.clone()];
    argv.extend(options.args.iter().cloned());
    let envp = alloc::vec![
        "PATH=/bin:/usr/bin".to_string(),
        "HOME=/root".to_string(),
        "TERM=linux".to_string(),
        "USER=root".to_string(),
        "PWD=/".to_string(),
    ];

    match task::spawn(&options.init, argv, envp, 0) {
        Ok(pid) => {
            sched::set_foreground(pid);
            println!("claudeos: starting {} as pid {}", options.init, pid);
        }
        Err(err) => {
            println!("claudeos: cannot start {}: {:?}", options.init, err);
            println!("claudeos: filesystem contents:");
            syscall::file::dump_tree("/", 1);
        }
    }

    // After init, because the scheduler hands out process ids in order and a
    // good deal of the system takes pid 1 to be init.
    if nic {
        net::e1000::start_task();
        if options.nettest {
            net::arptest::start();
        }
    }

    sync::enable_interrupts();
    sched::idle_loop();
}

fn mount_initramfs(boot: &multiboot::BootInfo) {
    if boot.module_count == 0 {
        println!("claudeos: no initramfs module supplied");
        return;
    }
    let module = boot.modules[0];
    let virt = mm::phys_to_virt(module.start);
    let archive = unsafe { core::slice::from_raw_parts(virt as *const u8, module.len()) };
    match fs::cpio::extract(archive) {
        Ok(stats) => println!(
            "initramfs: {} files, {} dirs, {} links, {} KiB",
            stats.files,
            stats.dirs,
            stats.symlinks,
            stats.bytes / 1024
        ),
        Err(err) => println!("initramfs: failed to unpack: {}", err),
    }
}

/// Ask QEMU's isa-debug-exit device to terminate with `code`.
pub fn qemu_exit(code: u32) -> ! {
    unsafe { io::outl(0xf4, code) };
    loop {
        cpu::halt();
    }
}

/// Shut the machine down. Tries the ACPI sleep register QEMU exposes, then
/// the debug-exit device, then simply stops.
pub fn power_off() -> ! {
    unsafe {
        io::outw(0x604, 0x2000); // QEMU / modern ACPI
        io::outw(0xB004, 0x2000); // older QEMU
        io::outw(0x4004, 0x3400); // virt machines
        io::outl(0xf4, 0);
    }
    sync::disable_interrupts();
    loop {
        cpu::halt();
    }
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    sync::disable_interrupts();
    println!();
    println!("KERNEL PANIC: {}", info);
    if sched::has_current() {
        let task = sched::current();
        println!("  in pid {} ({})", task.pid, task.name);
    }
    loop {
        cpu::halt();
    }
}
