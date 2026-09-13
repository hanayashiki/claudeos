//! Networking.
//!
//! The card and the protocols meet here and nowhere else. A driver implements
//! `Interface`, registers itself with `attach`, and hands every frame it
//! receives to `receive`. The stack parses those frames and sends its replies
//! back out through whatever was attached.
//!
//! Frames arrive at `receive` from ordinary task context, not from the
//! interrupt that took them off the card. Protocol work is far too much to do
//! with interrupts masked, so the driver's handler does nothing but move the
//! frame off the ring and wake the task that calls in here.

pub mod arp;
pub mod arptest;
pub mod e1000;
pub mod ether;
/// The Raspberry Pi 4's wired Ethernet, which exists on one machine only.
#[cfg(target_arch = "aarch64")]
pub mod genet;
#[cfg(target_arch = "aarch64")]
pub mod genettest;
pub mod icmp;
pub mod ip;
pub mod selftest;
pub mod socket;
pub mod tcp;
pub mod udp;

use crate::abi::Errno;
use crate::sync::Spinlock;
use alloc::collections::VecDeque;
use alloc::vec::Vec;
use ip::Ipv4Addr;

/// A network card the stack can send through.
pub trait Interface: Sync {
    /// The card's hardware address.
    fn mac(&self) -> [u8; 6];

    /// Queue one complete Ethernet frame, headers included. Called from task
    /// context, and must not sleep.
    fn transmit(&self, frame: &[u8]) -> Result<(), Errno>;
}

/// Bring up whatever card this machine has, before the first process exists.
///
/// Each driver's own probe reports whether it found anything, and finding
/// nothing is the ordinary outcome rather than an error: a machine booted
/// without a card boots without a network. The first one that answers is the
/// one the stack gets, because the stack holds one interface.
pub fn probe() -> bool {
    if e1000::probe() {
        return true;
    }
    #[cfg(target_arch = "aarch64")]
    if genet::probe() {
        return true;
    }
    false
}

/// Start the kernel task belonging to whichever driver attached. After init,
/// because process ids are handed out in order.
pub fn start_task() {
    if e1000::device().is_some() {
        e1000::start_task();
        return;
    }
    #[cfg(target_arch = "aarch64")]
    if genet::device().is_some() {
        genet::start_task();
    }
}

/// Log every frame that arrives, for the boot-time test.
pub fn trace_received(on: bool) {
    e1000::trace_received(on);
    #[cfg(target_arch = "aarch64")]
    genet::trace_received(on);
}

/// What the card has to say for itself: interrupts it raised, times it ran out
/// of receive descriptors, and frames the queue behind it had no room for.
pub fn counters() -> (u64, u64, u64) {
    #[cfg(target_arch = "aarch64")]
    if genet::device().is_some() {
        return (genet::interrupts(), genet::overruns(), genet::dropped());
    }
    (e1000::interrupts(), e1000::overruns(), e1000::dropped())
}

static INTERFACE: Spinlock<Option<&'static dyn Interface>> = Spinlock::new(None);

/// Register the card the stack sends through. Called once, by the driver.
pub fn attach(nic: &'static dyn Interface) {
    *INTERFACE.lock() = Some(nic);
}

pub fn interface() -> Option<&'static dyn Interface> {
    *INTERFACE.lock()
}

/// Send one frame, if there is a card to send it through.
pub fn transmit(frame: &[u8]) -> Result<(), Errno> {
    match interface() {
        Some(nic) => nic.transmit(frame),
        None => Err(Errno::ENODEV),
    }
}

/// The hardware address the stack answers to, which is the card's.
pub fn mac() -> [u8; 6] {
    match interface() {
        Some(nic) => nic.mac(),
        None => [0u8; 6],
    }
}

/// The addresses this machine uses.
///
/// The protocols read this rather than naming any address themselves; what is
/// in it comes from the kernel, which takes it from the command line or falls
/// back to what QEMU's user mode network hands out.
#[derive(Clone, Copy)]
pub struct Config {
    pub address: Ipv4Addr,
    pub netmask: Ipv4Addr,
    pub gateway: Ipv4Addr,
    /// Where a name would be looked up. Nothing in the kernel resolves names;
    /// this is here so a program can be told.
    pub nameserver: Ipv4Addr,
}

impl Config {
    /// The default for QEMU's user mode network: the guest is 10.0.2.15 on a
    /// /24, the gateway is 10.0.2.2 and the name server 10.0.2.3.
    pub const QEMU_USER: Config = Config {
        address: Ipv4Addr::new(10, 0, 2, 15),
        netmask: Ipv4Addr::new(255, 255, 255, 0),
        gateway: Ipv4Addr::new(10, 0, 2, 2),
        nameserver: Ipv4Addr::new(10, 0, 2, 3),
    };

    /// True when `other` is on this machine's own subnet, so a datagram for it
    /// goes straight there rather than through the gateway.
    pub fn on_link(&self, other: Ipv4Addr) -> bool {
        !self.netmask.is_unspecified()
            && (other.0 & self.netmask.0) == (self.address.0 & self.netmask.0)
    }

    /// The address every host on this subnet answers to.
    pub fn broadcast(&self) -> Ipv4Addr {
        Ipv4Addr(self.address.0 | !self.netmask.0)
    }
}

static CONFIG: Spinlock<Config> = Spinlock::new(Config::QEMU_USER);

pub fn configure(config: Config) {
    *CONFIG.lock() = config;
    // Anything learned under the old address is about a different network.
    arp::clear();
}

pub fn config() -> Config {
    *CONFIG.lock()
}

/// The address to send from when talking to `destination`.
pub fn source_for(destination: Ipv4Addr) -> Ipv4Addr {
    if destination.is_loopback() {
        Ipv4Addr::new(127, 0, 0, 1)
    } else {
        config().address
    }
}

// ---- talking to ourselves -------------------------------------------------
//
// A datagram addressed to this machine never reaches a card. It cannot be
// delivered where it is built either: the socket that produced it is locked
// at that moment, and the socket it is for may answer straight back and want
// the same lock. So it is queued, and delivered from a point that holds no
// socket lock at all.

static LOOPBACK: Spinlock<VecDeque<Vec<u8>>> = Spinlock::new(VecDeque::new());
/// Datagrams held for local delivery. Past this the oldest is dropped, the
/// same as a card whose ring is full.
const LOOPBACK_LIMIT: usize = 256;

pub fn loop_back(datagram: Vec<u8>) {
    {
        let mut queue = LOOPBACK.lock();
        if queue.len() >= LOOPBACK_LIMIT {
            queue.pop_front();
        }
        queue.push_back(datagram);
    }
    // Whoever is asleep on a socket has to look again: the queue is delivered
    // by whichever task next has no lock held, and that may be one of them.
    crate::sched::io_ready();
}

pub fn loopback_pending() -> bool {
    !LOOPBACK.lock().is_empty()
}

fn deliver_loopback() {
    // Delivering a datagram can produce another -- an acknowledgement for
    // what was just delivered -- so this runs bounded rather than until
    // empty. The guard is for a caller that reaches here from inside
    // delivery, which would otherwise take a socket lock it already holds.
    static RUNNING: core::sync::atomic::AtomicBool =
        core::sync::atomic::AtomicBool::new(false);
    use core::sync::atomic::Ordering;
    if RUNNING.swap(true, Ordering::Acquire) {
        return;
    }
    for _ in 0..LOOPBACK_LIMIT {
        let Some(datagram) = LOOPBACK.lock().pop_front() else { break };
        ip::receive(&datagram, mac(), ip::Origin::Loopback);
    }
    RUNNING.store(false, Ordering::Release);
}

/// Deliver anything addressed to this machine. Cheap when there is nothing,
/// so a socket system call can call it on the way in.
pub fn poll() {
    if loopback_pending() {
        deliver_loopback();
    }
}

/// One received Ethernet frame.
pub fn receive(frame: &[u8]) {
    let Some(parsed) = ether::Frame::parse(frame) else { return };
    // The card may be in promiscuous mode, and a hub or a bridge will hand
    // over frames addressed elsewhere. Only ours and the broadcasts count.
    if parsed.destination != mac() && !ether::is_group(&parsed.destination) {
        return;
    }
    match parsed.ethertype {
        ether::ETHERTYPE_ARP => arp::receive(parsed.payload),
        ether::ETHERTYPE_IPV4 => {
            ip::receive(parsed.payload, parsed.source, ip::Origin::Card)
        }
        // No IPv6.
        _ => {}
    }
    // Traffic arriving is as good a moment as any to look at the clock, so a
    // stack whose timer is not being driven still makes progress whenever the
    // other end says anything.
    tick();
}

/// Anything that has come due: retransmissions, timeouts. Called once per
/// timer tick from the network task.
pub fn tick() {
    // Timers walk the socket table and lock what they find, so this must not
    // run inside anything that already holds one of those locks. Nothing
    // calls it from there, and this stops a future caller from finding out
    // the hard way.
    static RUNNING: core::sync::atomic::AtomicBool =
        core::sync::atomic::AtomicBool::new(false);
    use core::sync::atomic::Ordering;
    if RUNNING.swap(true, Ordering::Acquire) {
        return;
    }
    deliver_loopback();
    arp::expire();
    tcp::on_tick();
    RUNNING.store(false, Ordering::Release);
}

/// The next tick at which `tick` would have something to do, so a task that is
/// blocked on a socket knows how long it may sleep.
pub fn next_deadline() -> u64 {
    if loopback_pending() {
        // Already due: something is waiting to be handed to a socket here.
        return crate::trap::ticks();
    }
    arp::next_deadline().min(tcp::next_deadline())
}
