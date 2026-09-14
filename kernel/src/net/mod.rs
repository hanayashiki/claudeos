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
use alloc::string::String;
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

/// One frame in this many is thrown away in each direction. Zero is off,
/// which is what it is unless `netloss=` says otherwise on the command line.
///
/// This is here because there is nowhere else to put it. QEMU's user mode
/// network cannot be asked to lose a packet: its netfilters can delay, dump,
/// mirror, redirect and rewrite, and none of them drops one. So the link this
/// kernel sees is made lossy at the seam every driver sends and receives
/// through, which leaves everything below it -- the card, its rings, its
/// interrupt -- doing exactly what it does on a link that loses nothing.
static LOSS: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
static SENT: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
static ARRIVED: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

pub fn set_loss(one_in: u32) {
    LOSS.store(one_in, core::sync::atomic::Ordering::Relaxed);
}

/// Whether this frame is one of the ones that goes missing. Counted rather
/// than drawn at random, so a run loses the same frames as the run before it
/// and a failure can be looked at twice.
fn lost(counter: &core::sync::atomic::AtomicU32) -> bool {
    use core::sync::atomic::Ordering;
    let one_in = LOSS.load(Ordering::Relaxed);
    if one_in == 0 {
        return false;
    }
    counter.fetch_add(1, Ordering::Relaxed) % one_in == one_in - 1
}

/// Send one frame, if there is a card to send it through.
pub fn transmit(frame: &[u8]) -> Result<(), Errno> {
    if lost(&SENT) {
        // As far as everything above here is concerned it went out, because
        // a frame the link loses is one that went out.
        return Ok(());
    }
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

// ---- the addresses this machine uses --------------------------------------

/// As many name servers as a resolver reads: musl and Go both stop at three.
pub const MAX_NAMESERVERS: usize = 3;

/// The addresses this machine uses on its link, which change together.
///
/// The protocols read this rather than naming any address themselves. It comes
/// from the kernel command line, and it is only ever replaced whole, through
/// `configure`: a reader takes one copy and makes every decision about a
/// packet from that copy, so nothing pairs a new address with an old netmask.
///
/// The fields are private and `new` is the only way to make one, so a value
/// of this type is an address a host can hold, with a netmask that is a
/// netmask and a gateway on the same subnet. A machine with no address holds
/// no `Config` at all, which is what `config` returning `None` says; there is
/// no 0.0.0.0 standing in for an address, for a reader to take as one.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Config {
    address: Ipv4Addr,
    netmask: Ipv4Addr,
    gateway: Option<Ipv4Addr>,
    /// Where a name would be looked up. Nothing in the kernel resolves names;
    /// these are here so programs can be told, through /etc/resolv.conf. The
    /// slots past `nameserver_count` are always zero, so two configurations
    /// naming the same servers compare equal.
    nameservers: [Ipv4Addr; MAX_NAMESERVERS],
    nameserver_count: usize,
}

/// An address a host can have as its own. 0/8 means "this network" and
/// 127/8 means "this machine" (RFC 1122 section 3.2.1.3), 224/4 is multicast,
/// and 240/4 is reserved and includes the all-ones broadcast.
fn is_host_address(address: Ipv4Addr) -> bool {
    let first = address.0 >> 24;
    first != 0 && first != 127 && first < 224
}

impl Config {
    /// A configuration, or the reason these addresses cannot be one.
    ///
    /// Name servers past the third are left out, and so is any entry nothing
    /// could answer on -- zero, a broadcast, a multicast address -- rather
    /// than refusing an otherwise usable lease over one bad entry.
    pub fn new(
        address: Ipv4Addr,
        netmask: Ipv4Addr,
        gateway: Option<Ipv4Addr>,
        nameservers: &[Ipv4Addr],
    ) -> Result<Config, &'static str> {
        if !is_host_address(address) {
            return Err("the address is not one a host can hold");
        }
        // A netmask is ones and then zeroes, which is what makes the host
        // part plus one a power of two. All zeroes passes that test and would
        // put every address in the world on the link, so it is refused.
        let host_bits = !netmask.0;
        if netmask.0 == 0 || host_bits & host_bits.wrapping_add(1) != 0 {
            return Err("the netmask is not a run of ones followed by zeroes");
        }
        // A /31 or a /32 has no network or broadcast address (RFC 3021); on
        // anything wider, those two are not addresses a host can hold.
        if host_bits > 1 && (address.0 & host_bits == 0 || address.0 & host_bits == host_bits) {
            return Err("the address is its subnet's network or broadcast address");
        }
        let mut config = Config {
            address,
            netmask,
            gateway: None,
            nameservers: [Ipv4Addr::UNSPECIFIED; MAX_NAMESERVERS],
            nameserver_count: 0,
        };
        if let Some(gateway) = gateway {
            if gateway == address || !is_host_address(gateway) || !config.on_link(gateway) {
                return Err("the gateway is not another host on the address's subnet");
            }
            config.gateway = Some(gateway);
        }
        for &server in nameservers {
            if config.nameserver_count == MAX_NAMESERVERS {
                break;
            }
            // 127.0.0.1 is a name server a program can be told about, so only
            // the addresses nothing answers on are left out.
            if server.is_unspecified() || server.is_broadcast() || server.is_multicast() {
                continue;
            }
            config.nameservers[config.nameserver_count] = server;
            config.nameserver_count += 1;
        }
        Ok(config)
    }

    /// The netmask an address implies when nothing names one: the one its
    /// class had before addresses were classless, which is what Linux assumes
    /// for an `ip=` given without a netmask.
    pub fn class_netmask(address: Ipv4Addr) -> Ipv4Addr {
        match address.0 >> 24 {
            0..=127 => Ipv4Addr::new(255, 0, 0, 0),
            128..=191 => Ipv4Addr::new(255, 255, 0, 0),
            _ => Ipv4Addr::new(255, 255, 255, 0),
        }
    }

    pub fn address(&self) -> Ipv4Addr {
        self.address
    }

    pub fn netmask(&self) -> Ipv4Addr {
        self.netmask
    }

    pub fn gateway(&self) -> Option<Ipv4Addr> {
        self.gateway
    }

    pub fn nameservers(&self) -> &[Ipv4Addr] {
        &self.nameservers[..self.nameserver_count]
    }

    /// The netmask as a count of its ones, which is how it is printed.
    pub fn prefix_len(&self) -> u32 {
        self.netmask.0.leading_ones()
    }

    /// True when `other` is on this machine's own subnet, so a datagram for it
    /// goes straight there rather than through the gateway.
    pub fn on_link(&self, other: Ipv4Addr) -> bool {
        (other.0 & self.netmask.0) == (self.address.0 & self.netmask.0)
    }

    /// The address every host on this subnet answers to.
    pub fn broadcast(&self) -> Ipv4Addr {
        Ipv4Addr(self.address.0 | !self.netmask.0)
    }

    /// The address a datagram for `destination` is handed to: the destination
    /// itself when it is on this subnet or is a broadcast, the gateway when it
    /// is anywhere else, and nobody when there is no gateway.
    pub fn next_hop(&self, destination: Ipv4Addr) -> Option<Ipv4Addr> {
        if destination.is_broadcast() || destination.is_multicast() || self.on_link(destination) {
            Some(destination)
        } else {
            self.gateway
        }
    }
}

impl core::fmt::Display for Config {
    /// `192.168.86.57/24 gateway 192.168.86.1 dns 192.168.86.1`, which is how
    /// the boot log names a configuration.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}/{}", self.address, self.prefix_len())?;
        match self.gateway {
            Some(gateway) => write!(f, " gateway {}", gateway)?,
            None => write!(f, " no gateway")?,
        }
        if self.nameservers().is_empty() {
            return write!(f, " no dns");
        }
        write!(f, " dns")?;
        for server in self.nameservers() {
            write!(f, " {}", server)?;
        }
        Ok(())
    }
}

static CONFIG: Spinlock<Option<Config>> = Spinlock::new(None);

/// Replace the configuration, whole. `None` takes the address away.
///
/// Called at boot with what the command line says, before there is anything
/// else that could publish one at the same moment.
pub fn configure(config: Option<Config>) {
    let previous = core::mem::replace(&mut *CONFIG.lock(), config);
    if previous == config {
        return;
    }
    // Anything learned under the old address is about a different network.
    arp::clear();
    publish_nameservers();
}

/// The configuration as it stands, or `None` while this machine has no
/// address.
pub fn config() -> Option<Config> {
    *CONFIG.lock()
}

/// The address to send from when talking to `destination`.
///
/// With no address, the only thing that can be sent is a broadcast from
/// 0.0.0.0: RFC 1122 section 3.2.1.3 allows that source only while a host is
/// learning its own address, and a broadcast is the only destination that
/// needs no route. Anything else has nowhere to go, which Linux reports as
/// ENETUNREACH.
pub fn source_for(destination: Ipv4Addr) -> Result<Ipv4Addr, Errno> {
    if destination.is_loopback() {
        return Ok(Ipv4Addr::new(127, 0, 0, 1));
    }
    match config() {
        Some(config) => Ok(config.address()),
        None if destination.is_broadcast() => Ok(Ipv4Addr::UNSPECIFIED),
        None => Err(Errno::ENETUNREACH),
    }
}

/// The name servers /etc/resolv.conf last named, so it is written only when
/// that changes.
static PUBLISHED_NAMESERVERS: Spinlock<Option<Vec<Ipv4Addr>>> = Spinlock::new(None);

/// Tell programs where names are looked up.
///
/// Programs with their own resolver -- Go's, musl's -- read /etc/resolv.conf,
/// so that is where the name servers go. The file is replaced rather than
/// rewritten in place, so a program opening it reads either the old one whole
/// or the new one whole. A configuration being taken away leaves it alone:
/// nothing can reach a name server without an address, and a configuration
/// that comes back naming the same servers then changes nothing a resolver
/// would reread.
fn publish_nameservers() {
    let Some(config) = config() else { return };
    let servers = config.nameservers();
    {
        let mut published = PUBLISHED_NAMESERVERS.lock();
        if published.as_deref() == Some(servers) {
            return;
        }
        *published = Some(servers.to_vec());
    }
    let mut text = String::from(
        "# Written by the kernel from its network configuration, and written\n\
         # again whenever that changes.\n",
    );
    for server in servers {
        text.push_str(&alloc::format!("nameserver {}\n", server));
    }
    if let Err(err) = crate::fs::replace_file("/etc/resolv.conf", text.as_bytes(), 0o644) {
        crate::println!("net: cannot write /etc/resolv.conf: {:?}", err);
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
    if lost(&ARRIVED) {
        // Gone, and nothing above is told. Time has still passed, and the
        // timers are what recovers from this.
        tick();
        return;
    }
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
