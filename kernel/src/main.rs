#![no_std]
#![no_main]
#![allow(dead_code)]

extern crate alloc;

use core::panic::PanicInfo;

#[macro_use]
mod serial;
pub mod abi;
mod arch;
mod boot;
mod console;
mod elf;
mod fs;
mod futex;
mod integrity;
mod itimer;
mod mm;
#[cfg(target_arch = "aarch64")]
mod mmc;
mod net;
mod pci;
mod reboot;
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
    /// The words that say where the protocol stack's address comes from, and
    /// whether to run its own checks instead of booting.
    net: NetWords,
    net_test: bool,
    /// Which cards the network may use: any, or with `net=wifi` the WiFi alone.
    cards: net::Cards,
    /// Throw one frame in this many away in each direction, so what the
    /// protocols do about loss can be seen on the real card path. Zero is
    /// off, and it is zero unless the command line says otherwise.
    net_loss: u32,
    /// Serve the console over TCP port 23 once the network has an address.
    /// On unless the command line says `telnet=off`.
    telnet: bool,
    /// Arguments for init, after its own path.
    args: Vec<String>,
    /// Init's environment: a fixed set, with any `name=value` word the kernel
    /// does not use added or replacing the entry of the same name.
    env: Vec<String>,
}

fn parse_cmdline(cmdline: &str) -> BootOptions {
    let mut options = BootOptions {
        init: "/bin/init".to_string(),
        trace: syscall::TRACE_OFF,
        nettest: false,
        net: NetWords::default(),
        net_test: false,
        cards: net::Cards::Any,
        net_loss: 0,
        telnet: true,
        args: Vec::new(),
        env: ["PATH=/bin:/usr/bin", "HOME=/root", "TERM=linux", "USER=root", "PWD=/"]
            .iter()
            .map(|entry| entry.to_string())
            .collect(),
    };
    let mut words = cmdline.split_whitespace();
    for word in words.by_ref() {
        if word == "--" {
            break;
        }
        if let Some(value) = word.strip_prefix("init=") {
            options.init = value.to_string();
        } else if word.starts_with("panic=") || word.starts_with("watchdog=") {
            // The kernel's, and read by `reboot::configure` at the start of
            // boot, before there is a heap; not a setting for init.
        } else if let Some(value) = word.strip_prefix("trace=") {
            options.trace = if value == "all" {
                syscall::TRACE_ALL
            } else {
                value.parse().unwrap_or(syscall::TRACE_OFF)
            };
        } else if word == "nettest" {
            options.nettest = true;
        } else if let Some(value) = word.strip_prefix("net=") {
            // `net=test`: the protocols against a card that only records what
            // it is asked to send, in place of booting anything. `net=wifi`:
            // the wired cards left alone and the WiFi used, so that what
            // reaches the network is known to have gone over the air.
            options.net_test = value == "test";
            if value == "wifi" {
                options.cards = net::Cards::WifiOnly;
            }
        } else if let Some(value) = word.strip_prefix("netloss=") {
            options.net_loss = value.parse().unwrap_or(0);
        } else if let Some(value) = word.strip_prefix("telnet=") {
            match value {
                "on" => options.telnet = true,
                "off" => options.telnet = false,
                _ => println!("telnet={} is neither on nor off, so it is ignored", value),
            }
        } else if let Some(value) = word.strip_prefix("ip=") {
            options.net.ip = Some(value.to_string());
        } else if let Some(value) = word.strip_prefix("netmask=") {
            options.net.netmask = address_word(word, value);
        } else if let Some(value) = word.strip_prefix("gateway=") {
            options.net.gateway = address_word(word, value);
        } else if let Some(value) = word.strip_prefix("nameserver=") {
            options.net.nameserver = address_word(word, value);
        } else if let Some((name, _)) = word.split_once('=') {
            // A setting the kernel does not use. Linux puts these in init's
            // environment and ignores names with a dot, which are settings
            // for its own modules. The Raspberry Pi firmware puts several of
            // both in front of cmdline.txt, such as coherent_pool=1M and
            // 8250.nr_uarts=1, and none of them is meant for init.
            if !name.is_empty() && !name.contains('.') {
                let prefix = alloc::format!("{}=", name);
                options.env.retain(|entry| !entry.starts_with(&prefix));
                options.env.push(word.to_string());
            }
        } else {
            options.args.push(word.to_string());
        }
    }
    // After "--" every word is an argument to init, whatever it looks like.
    options.args.extend(words.map(|word| word.to_string()));
    options
}

/// The `ip=`, `netmask=`, `gateway=` and `nameserver=` words, as given.
#[derive(Default)]
struct NetWords {
    ip: Option<String>,
    netmask: Option<net::ip::Ipv4Addr>,
    gateway: Option<net::ip::Ipv4Addr>,
    nameserver: Option<net::ip::Ipv4Addr>,
}

/// Where this machine's address comes from.
enum Addressing {
    /// `ip=` named an address and the words beside it the rest. DHCP does
    /// not run.
    Fixed(net::Config),
    /// No `ip=`, or `ip=dhcp`: ask the network. `nameserver=` still takes the
    /// place of the name servers the lease names.
    Dhcp { nameserver: Option<net::ip::Ipv4Addr> },
    /// `ip=off`, or an `ip=` that cannot be used: no address, and nothing asks
    /// for one.
    Off,
}

impl NetWords {
    /// The words taken together.
    ///
    /// An `ip=` that names an address wins over DHCP, as it does on Linux, and
    /// one that cannot be used leaves the machine with no address rather than
    /// quietly asking the network for a different one. `netmask=` and
    /// `gateway=` describe the address `ip=` names, so without it they are
    /// reported and ignored. With it, a netmask left out is the one the
    /// address's class implies and a gateway left out is none, which is what
    /// Linux does with the same words missing.
    fn addressing(&self) -> Addressing {
        match self.ip.as_deref() {
            None | Some("dhcp") => {
                if self.netmask.is_some() || self.gateway.is_some() {
                    println!("net: netmask= and gateway= mean nothing without ip=, so they are ignored");
                }
                Addressing::Dhcp { nameserver: self.nameserver }
            }
            Some("off") | Some("none") => Addressing::Off,
            Some(value) => {
                let Some(address) = net::ip::parse_address(value) else {
                    println!("net: ip={} is not an address, dhcp or off", value);
                    return Addressing::Off;
                };
                let netmask = self.netmask.unwrap_or_else(|| net::Config::class_netmask(address));
                let nameservers: Vec<net::ip::Ipv4Addr> = self.nameserver.into_iter().collect();
                match net::Config::new(address, netmask, self.gateway, &nameservers) {
                    Ok(config) => Addressing::Fixed(config),
                    Err(reason) => {
                        println!("net: ip={} netmask={} cannot be used: {}", address, netmask, reason);
                        Addressing::Off
                    }
                }
            }
        }
    }
}

/// The address an address word gives, or a line saying it gives none.
fn address_word(word: &str, value: &str) -> Option<net::ip::Ipv4Addr> {
    let address = net::ip::parse_address(value);
    if address.is_none() {
        println!("net: {} is not an address, so it is ignored", word);
    }
    address
}

/// How long init is held back for a DHCP lease, in seconds.
///
/// Init starts once there is an address, so what it runs straight away finds
/// the network configured. A machine with no cable, or on a network with no
/// DHCP server, has to boot all the same, so the wait ends. On the Pi 4 the
/// link took several seconds to negotiate, and a server may check that the
/// address it means to offer is not in use before offering it, which can take
/// a few seconds more; fifteen covers both with a retransmission to spare.
const ADDRESS_WAIT_SECONDS: u64 = 15;

/// Let the network task run, holding init back, until there is an address or
/// the wait is over. Interrupts are on while it waits, which is what lets the
/// timer drive the network task, and off again afterwards.
fn wait_for_address(init: &str) {
    println!(
        "dhcp: waiting up to {} s for an address before starting {}",
        ADDRESS_WAIT_SECONDS, init
    );
    let deadline = trap::ticks() + ADDRESS_WAIT_SECONDS * arch::TICK_HZ as u64;
    if !sched::idle_until(deadline, || net::config().is_some()) {
        println!(
            "dhcp: no address after {} s; starting {} without one, and the lease is taken when it comes",
            ADDRESS_WAIT_SECONDS, init
        );
    }
}

/// Where every architecture's entry code arrives, once it has a console to
/// print on and the machine described in terms the rest of the kernel reads.
pub fn start(boot: &boot::BootInfo) -> ! {
    println!();
    println!("claudeos: booting");
    // First, so that the watchdog covers as much of boot as it can, and a
    // panic from here on restarts the way the command line says.
    reboot::configure(boot.cmdline());

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
    // Printed whole, because on the board most of it is written by the
    // firmware and this is the only place it can be seen.
    println!("command line: {}", boot.cmdline());
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
    // Straight after unpacking, so what is checked is what the image held
    // before the kernel or any program has changed a file in it.
    integrity::check();

    // Nothing reads low memory through the identity map from here on.
    unsafe { arch::paging::drop_identity_map() };

    sched::init();

    // Find and bring up the network card before the first process exists. Its
    // registers are mapped into the kernel half of the page tables, and a new
    // address space copies the kernel half as it stands at the moment it is
    // created, so a mapping made later would be missing from it. Finding no
    // card is the ordinary outcome on a machine booted without one.
    let nic = net::probe(options.cards);

    if options.net_loss != 0 {
        net::set_loss(options.net_loss);
        println!("net: losing one frame in {} in each direction", options.net_loss);
    }
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
    let envp = options.env.clone();

    // Init is built now, so it takes pid 1 before the network task takes the
    // next one: a good deal of the system takes pid 1 to be init. It is not
    // started until the network has had its chance to configure itself.
    let init = task::prepare(&options.init, argv, envp, 0);

    // The protocol stack takes its addresses from here rather than naming any
    // of its own.
    let asking = match options.net.addressing() {
        Addressing::Fixed(config) => {
            net::configure(Some(config));
            println!("net: {}, from the command line", config);
            false
        }
        Addressing::Dhcp { nameserver } if nic => {
            net::dhcp::start(nameserver);
            true
        }
        Addressing::Dhcp { .. } => {
            println!("net: no network card, so no address");
            false
        }
        Addressing::Off => {
            println!("net: no address");
            false
        }
    };

    // After init's pid, because the scheduler hands out process ids in order
    // and a good deal of the system takes pid 1 to be init.
    if nic {
        // Before the network task, which is what runs it. Nothing listens
        // until the network has a configuration.
        if options.telnet {
            console::telnet::enable();
        } else {
            println!("telnet: off, from the command line");
        }
        net::start_task();
        if options.nettest {
            net::arptest::start();
        }
    }

    if asking && init.is_ok() {
        wait_for_address(&options.init);
    }

    match init {
        Ok(task) => {
            let pid = sched::register(task);
            sched::set_foreground(pid);
            println!("claudeos: starting {} as pid {}", options.init, pid);
        }
        Err(err) => {
            println!("claudeos: cannot start {}: {:?}", options.init, err);
            println!("claudeos: filesystem contents:");
            syscall::file::dump_tree("/", 1);
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

/// Set by the first panic.
static PANICKING: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    sync::disable_interrupts();
    // A second panic is one raised by the code below, reporting the first.
    // Running that code again would raise it again, so this one stops where
    // it is, and on a machine with a watchdog the watchdog, which nothing
    // feeds any more, restarts it.
    if PANICKING.swap(true, core::sync::atomic::Ordering::Relaxed) {
        loop {
            arch::halt();
        }
    }
    // Before the first print, because a panic reached while the console lock
    // or the log lock was held would otherwise spin for ever on it with
    // interrupts off and print nothing at all.
    unsafe { serial::force_release() };
    println!();
    println!("KERNEL PANIC: {}", info);
    if sched::has_current() {
        // The pid is a plain field. The task's name is behind a lock of its
        // own and is copied onto the heap to be read, which are two more
        // places this could hang before it has said anything.
        println!("  in pid {}", sched::current().pid);
    }
    reboot::after_panic()
}
