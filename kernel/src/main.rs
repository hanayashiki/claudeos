#![no_std]
#![no_main]
#![allow(dead_code)]

extern crate alloc;

use core::panic::PanicInfo;

#[macro_use]
mod serial;
mod abi;
mod arch;
mod boot;
mod console;
mod elf;
mod fs;
mod futex;
mod mm;
mod net;
mod pci;
mod rng;
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
    /// Throw one frame in this many away in each direction, so what the
    /// protocols do about loss can be seen on the real card path. Zero is
    /// off, and it is zero unless the command line says otherwise.
    net_loss: u32,
    args: Vec<String>,
}

fn parse_cmdline(cmdline: &str) -> BootOptions {
    let mut options = BootOptions {
        init: "/bin/init".to_string(),
        trace: syscall::TRACE_OFF,
        nettest: false,
        net: net::Config::QEMU_USER,
        net_test: false,
        net_loss: 0,
        args: Vec::new(),
    };
    for word in cmdline.split_whitespace() {
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
        } else if let Some(value) = word.strip_prefix("netloss=") {
            options.net_loss = value.parse().unwrap_or(0);
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

/// Where every architecture's entry code arrives, once it has a console to
/// print on and the machine described in terms the rest of the kernel reads.
pub fn start(boot: &boot::BootInfo) -> ! {
    println!();
    println!("claudeos: booting");

    arch::init_traps();
    trap::init();
    arch::init_cpu();

    mm::frame::init(boot);
    mm::heap::init();
    let (used, total) = mm::frame::stats();
    println!(
        "memory: {} MiB total, {} MiB in use, {} MiB kernel heap",
        total * 4096 / (1024 * 1024),
        used * 4096 / (1024 * 1024),
        mm::KERNEL_HEAP_SIZE / (1024 * 1024)
    );

    // Parsing needs the heap, so it cannot happen any earlier than this.
    let options = parse_cmdline(boot.cmdline());

    arch::init_interrupt_controller();
    arch::init_timer(arch::TICK_HZ);
    time::init();
    console::init();
    arch::unmask_irq(arch::TIMER_IRQ);
    // Measure the cycle counter against the tick. This has to happen before
    // there is anything to schedule, or the calibration loop is preempted and
    // measures the whole system instead of itself.
    sync::enable_interrupts();
    time::calibrate();
    sync::disable_interrupts();

    // The generator is seeded here rather than on first use, because the
    // sources it draws on are boot timing and the machine only boots once.
    rng::init(boot);

    syscall::init();
    unsafe { core::ptr::write_volatile(core::ptr::addr_of_mut!(syscall::TRACE), options.trace) };

    fs::init();
    mount_initramfs(boot);

    // Nothing reads low memory through the identity map from here on.
    unsafe { arch::paging::drop_identity_map() };

    sched::init();

    // Find and bring up the network card before the first process exists. Its
    // registers are mapped into the kernel half of the page tables, and a new
    // address space copies the kernel half as it stands at the moment it is
    // created, so a mapping made later would be missing from it. Finding no
    // card is the ordinary outcome on a machine booted without one.
    let nic = net::probe();

    // The protocol stack takes its addresses from here rather than naming any
    // of its own; a driver that attaches later does not change them.
    net::configure(options.net);
    if options.net_loss != 0 {
        net::set_loss(options.net_loss);
        println!("net: losing one frame in {} in each direction", options.net_loss);
    }
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
        arch::power_off();
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
        net::start_task();
        if options.nettest {
            net::arptest::start();
        }
    }

    sync::enable_interrupts();
    sched::idle_loop();
}

fn mount_initramfs(boot: &boot::BootInfo) {
    let Some(module) = boot.modules().first() else {
        println!("claudeos: no initramfs module supplied");
        return;
    };
    let virt = mm::phys_to_virt(module.start);
    let archive = unsafe { core::slice::from_raw_parts(virt as *const u8, module.len()) };
    match fs::cpio::extract(archive) {
        Ok(stats) => {
            // The dates on the archive are the only evidence of the real time
            // that reaches a board with no clock of its own.
            time::set_floor(stats.newest);
            println!(
                "initramfs: {} files, {} dirs, {} links, {} KiB",
                stats.files,
                stats.dirs,
                stats.symlinks,
                stats.bytes / 1024
            );
        }
        Err(err) => println!("initramfs: failed to unpack: {}", err),
    }
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    sync::disable_interrupts();
    println!();
    println!("KERNEL PANIC: {}", info);
    if sched::has_current() {
        let task = sched::current();
        println!("  in pid {} ({})", task.pid, task.name());
    }
    loop {
        arch::halt();
    }
}
