//! DHCP: asking the network for an address.
//!
//! RFC 2131's client, far enough to take a lease and keep it: DISCOVER, OFFER,
//! REQUEST, then ACK or NAK; renewing at T1 with the server that granted the
//! lease, rebinding at T2 with any server that will answer, and giving the
//! address up when the lease runs out. The options read are RFC 2132's netmask
//! (1), routers (3), name servers (6), lease time (51), server identifier (54),
//! and the renewal and rebinding times (58, 59).
//!
//! It talks through an ordinary UDP socket bound to port 68, so what it sends
//! and what it hears go through the same IP and UDP code as every program's.
//! Two rules down there are what let that work before there is an address: a
//! broadcast may be sent from 0.0.0.0 while the machine has no address, and
//! while it has none the only datagrams it takes off the card are broadcasts.
//! That is why every message sent before a lease is held sets the broadcast
//! flag, which asks the server to broadcast its answer rather than send it to
//! an address the machine does not have yet.
//!
//! The client decides and its caller acts. `poll` reads what has arrived,
//! looks at the clock, and returns what should happen -- messages to send, a
//! configuration to publish -- in the order it should happen, having done none
//! of it. The order matters: a lease that has run out is given up before the
//! DISCOVER that follows it is sent, so that DISCOVER leaves from 0.0.0.0 and
//! not from the address just given up. And publishing a configuration writes
//! /etc/resolv.conf, which is not something to do under the client's lock; a
//! list the caller works through after letting go keeps it out from under it.
//!
//! Left out: probing an offered address with ARP before using it and declining
//! it if somebody answers (RFC 2131 section 4.4.1 says a client SHOULD);
//! reusing a remembered lease after a reboot (INIT-REBOOT); releasing the lease
//! at shutdown; and the random wait of one to ten seconds before the first
//! DISCOVER, which exists to spread out machines powering on together and
//! would add up to ten seconds to every boot.

use super::ether;
use super::ip::Ipv4Addr;
use super::socket::{Endpoint, InetSocket};
use super::Config;
use crate::abi::Errno;
use crate::sync::Spinlock;
use alloc::sync::Arc;
use alloc::vec::Vec;

pub const SERVER_PORT: u16 = 67;
pub const CLIENT_PORT: u16 = 68;

const OP_REQUEST: u8 = 1;
const OP_REPLY: u8 = 2;
const HARDWARE_ETHERNET: u8 = 1;
const HARDWARE_LEN: u8 = 6;
/// Set in `flags` to ask for the answer to be broadcast.
pub const FLAG_BROADCAST: u16 = 0x8000;

/// The fixed BOOTP header is 236 bytes, and the magic cookie that says what
/// follows is DHCP options takes four more.
const COOKIE_AT: usize = 236;
pub const OPTIONS_AT: usize = 240;
pub const MAGIC_COOKIE: [u8; 4] = [99, 130, 83, 99];
/// The server name and boot file fields, which option 52 can say hold options.
const SNAME: core::ops::Range<usize> = 44..108;
const FILE: core::ops::Range<usize> = 108..236;
/// BOOTP's messages were 300 bytes, and servers that still refuse anything
/// shorter exist, so a shorter message is padded out to this.
pub const MIN_MESSAGE_LEN: usize = 300;

pub const DISCOVER: u8 = 1;
pub const OFFER: u8 = 2;
pub const REQUEST: u8 = 3;
pub const ACK: u8 = 5;
pub const NAK: u8 = 6;

pub const OPTION_PAD: u8 = 0;
pub const OPTION_NETMASK: u8 = 1;
pub const OPTION_ROUTERS: u8 = 3;
pub const OPTION_NAMESERVERS: u8 = 6;
pub const OPTION_REQUESTED_ADDRESS: u8 = 50;
pub const OPTION_LEASE_TIME: u8 = 51;
pub const OPTION_OVERLOAD: u8 = 52;
pub const OPTION_MESSAGE_TYPE: u8 = 53;
pub const OPTION_SERVER: u8 = 54;
pub const OPTION_PARAMETERS: u8 = 55;
pub const OPTION_RENEWAL_TIME: u8 = 58;
pub const OPTION_REBINDING_TIME: u8 = 59;
pub const OPTION_CLIENT_ID: u8 = 61;
pub const OPTION_END: u8 = 255;

/// The options the client asks servers to include, in option 55.
pub const PARAMETERS: [u8; 6] = [
    OPTION_NETMASK,
    OPTION_ROUTERS,
    OPTION_NAMESERVERS,
    OPTION_LEASE_TIME,
    OPTION_RENEWAL_TIME,
    OPTION_REBINDING_TIME,
];

const HZ: u64 = crate::arch::TICK_HZ as u64;
/// RFC 2131 section 4.1: the first retransmission after four seconds, the
/// wait doubling each time to at most 64, and each wait moved by up to a
/// second either way so that clients started together do not stay together.
pub const FIRST_RETRANSMIT: u64 = 4 * HZ;
pub const MAX_RETRANSMIT: u64 = 64 * HZ;
pub const JITTER: u64 = HZ;
/// REQUESTs sent to the server whose offer was taken before starting over
/// with a DISCOVER. With the waits above, the last one is given up on a minute
/// after the first.
const REQUEST_ATTEMPTS: u32 = 4;
/// RFC 2131 section 4.4.5: in RENEWING and REBINDING the REQUEST goes again
/// after half the time left, but no sooner than a minute.
pub const MIN_RENEW_RETRANSMIT: u64 = 60 * HZ;
/// A server has to say how long a lease is (RFC 2131 table 3). A message that
/// does not is taken as granting an hour, which is what busybox's udhcpc
/// assumes.
const DEFAULT_LEASE_SECONDS: u32 = 3600;
/// The shortest lease taken at its word. Renewal comes at half the lease, so a
/// lease of a second or of none would have the client renewing as fast as the
/// server can answer; anything shorter is taken as this long.
const MIN_LEASE_SECONDS: u32 = 20;
/// All ones is a lease that never runs out (RFC 2131 section 3.3).
const INFINITE: u32 = 0xFFFF_FFFF;

// ---- what a server sends ---------------------------------------------------

/// A server's message, checked and taken apart. Only the fields and options
/// the client uses are kept.
pub struct Reply {
    pub kind: u8,
    pub xid: u32,
    pub chaddr: [u8; 6],
    pub yiaddr: Ipv4Addr,
    pub server: Option<Ipv4Addr>,
    pub netmask: Option<Ipv4Addr>,
    pub routers: Vec<Ipv4Addr>,
    pub nameservers: Vec<Ipv4Addr>,
    pub lease_seconds: Option<u32>,
    pub renewal_seconds: Option<u32>,
    pub rebinding_seconds: Option<u32>,
}

/// An option that is present but cannot mean what its code says.
struct Malformed;

/// An option that may be absent but has to decode when it is there.
fn optional<T>(
    value: Option<&[u8]>,
    decode: fn(&[u8]) -> Option<T>,
) -> Result<Option<T>, Malformed> {
    match value {
        None => Ok(None),
        Some(bytes) => decode(bytes).map(Some).ok_or(Malformed),
    }
}

fn one_byte(value: &[u8]) -> Option<u8> {
    match value {
        [byte] => Some(*byte),
        _ => None,
    }
}

fn address(value: &[u8]) -> Option<Ipv4Addr> {
    match value {
        [a, b, c, d] => Some(Ipv4Addr::new(*a, *b, *c, *d)),
        _ => None,
    }
}

/// A list of addresses: at least one, and nothing left over.
fn addresses(value: &[u8]) -> Option<Vec<Ipv4Addr>> {
    if value.is_empty() || value.len() % 4 != 0 {
        return None;
    }
    Some(value.chunks_exact(4).map(|a| Ipv4Addr::new(a[0], a[1], a[2], a[3])).collect())
}

fn seconds(value: &[u8]) -> Option<u32> {
    match value {
        [a, b, c, d] => Some(u32::from_be_bytes([*a, *b, *c, *d])),
        _ => None,
    }
}

/// Options as a message carries them. An option split over several entries
/// is joined back together in the order the pieces came (RFC 3396).
#[derive(Default)]
struct Options {
    entries: Vec<(u8, Vec<u8>)>,
}

impl Options {
    /// Read one area of options, as far as its end option or its last byte.
    /// `None` when an option's length runs past the area.
    fn read(&mut self, area: &[u8]) -> Option<()> {
        let mut at = 0;
        while at < area.len() {
            let code = area[at];
            if code == OPTION_END {
                return Some(());
            }
            if code == OPTION_PAD {
                at += 1;
                continue;
            }
            let length = *area.get(at + 1)? as usize;
            let value = area.get(at + 2..at + 2 + length)?;
            match self.entries.iter_mut().find(|(existing, _)| *existing == code) {
                Some((_, joined)) => joined.extend_from_slice(value),
                None => self.entries.push((code, value.to_vec())),
            }
            at += 2 + length;
        }
        // Run off the end with no end option. RFC 2131 asks for one, but the
        // area has already been measured by IP and UDP, so everything that
        // was sent has been read.
        Some(())
    }

    fn get(&self, code: u8) -> Option<&[u8]> {
        self.entries
            .iter()
            .find(|(existing, _)| *existing == code)
            .map(|(_, value)| value.as_slice())
    }
}

impl Reply {
    /// Take a message apart, or refuse it.
    ///
    /// Every length is checked against the bytes that are there before
    /// anything is read. An option this client uses whose length is not the
    /// one its meaning needs refuses the whole message: a server that sends a
    /// four-byte address in three bytes has said nothing about the rest that
    /// can be believed either.
    pub fn parse(bytes: &[u8]) -> Option<Reply> {
        if bytes.len() < OPTIONS_AT {
            return None;
        }
        if bytes[0] != OP_REPLY || bytes[1] != HARDWARE_ETHERNET || bytes[2] != HARDWARE_LEN {
            return None;
        }
        if bytes[COOKIE_AT..OPTIONS_AT] != MAGIC_COOKIE {
            return None;
        }
        let mut options = Options::default();
        options.read(&bytes[OPTIONS_AT..])?;
        // Option 52 says the file and server name fields hold more options,
        // read in that order (RFC 2131 section 4.1).
        if let Some(overload) = optional(options.get(OPTION_OVERLOAD), one_byte).ok()? {
            if !(1..=3).contains(&overload) {
                return None;
            }
            if overload & 1 != 0 {
                options.read(&bytes[FILE])?;
            }
            if overload & 2 != 0 {
                options.read(&bytes[SNAME])?;
            }
        }
        let mut chaddr = [0u8; 6];
        chaddr.copy_from_slice(&bytes[28..34]);
        Some(Reply {
            kind: optional(options.get(OPTION_MESSAGE_TYPE), one_byte).ok()??,
            xid: u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
            chaddr,
            yiaddr: Ipv4Addr::new(bytes[16], bytes[17], bytes[18], bytes[19]),
            server: optional(options.get(OPTION_SERVER), address).ok()?,
            netmask: optional(options.get(OPTION_NETMASK), address).ok()?,
            routers: optional(options.get(OPTION_ROUTERS), addresses).ok()?.unwrap_or_default(),
            nameservers: optional(options.get(OPTION_NAMESERVERS), addresses)
                .ok()?
                .unwrap_or_default(),
            lease_seconds: optional(options.get(OPTION_LEASE_TIME), seconds).ok()?,
            renewal_seconds: optional(options.get(OPTION_RENEWAL_TIME), seconds).ok()?,
            rebinding_seconds: optional(options.get(OPTION_REBINDING_TIME), seconds).ok()?,
        })
    }
}

// ---- what the client sends -------------------------------------------------

/// The fields that vary between the messages this client sends. The fields a
/// client always leaves at zero are written as zero by `build`.
struct Outgoing {
    kind: u8,
    xid: u32,
    secs: u16,
    /// Set until a lease is held, because until then an answer sent to the
    /// offered address would not be taken off the card.
    broadcast: bool,
    ciaddr: Ipv4Addr,
    requested: Option<Ipv4Addr>,
    server: Option<Ipv4Addr>,
}

impl Outgoing {
    fn build(&self, mac: [u8; 6]) -> Vec<u8> {
        let mut out = Vec::with_capacity(MIN_MESSAGE_LEN);
        out.push(OP_REQUEST);
        out.push(HARDWARE_ETHERNET);
        out.push(HARDWARE_LEN);
        out.push(0); // hops
        out.extend_from_slice(&self.xid.to_be_bytes());
        out.extend_from_slice(&self.secs.to_be_bytes());
        let flags = if self.broadcast { FLAG_BROADCAST } else { 0 };
        out.extend_from_slice(&flags.to_be_bytes());
        out.extend_from_slice(&self.ciaddr.to_be_bytes());
        out.extend_from_slice(&[0u8; 12]); // yiaddr, siaddr, giaddr
        out.extend_from_slice(&mac);
        out.extend_from_slice(&[0u8; 10]); // the rest of chaddr
        out.extend_from_slice(&[0u8; 64 + 128]); // sname, file
        out.extend_from_slice(&MAGIC_COOKIE);
        out.extend_from_slice(&[OPTION_MESSAGE_TYPE, 1, self.kind]);
        // The card's address again, with its hardware type in front (RFC 2132
        // section 9.14), which is what a server keys the lease on.
        out.extend_from_slice(&[OPTION_CLIENT_ID, 7, HARDWARE_ETHERNET]);
        out.extend_from_slice(&mac);
        if let Some(requested) = self.requested {
            out.extend_from_slice(&[OPTION_REQUESTED_ADDRESS, 4]);
            out.extend_from_slice(&requested.to_be_bytes());
        }
        if let Some(server) = self.server {
            out.extend_from_slice(&[OPTION_SERVER, 4]);
            out.extend_from_slice(&server.to_be_bytes());
        }
        out.extend_from_slice(&[OPTION_PARAMETERS, PARAMETERS.len() as u8]);
        out.extend_from_slice(&PARAMETERS);
        out.push(OPTION_END);
        let padded = out.len().max(MIN_MESSAGE_LEN);
        out.resize(padded, OPTION_PAD);
        out
    }
}

// ---- the client --------------------------------------------------------------

/// One run of messages sharing a transaction id.
#[derive(Clone, Copy)]
struct Exchange {
    xid: u32,
    /// When the exchange began, which the `secs` field counts from.
    began: u64,
    /// When the first REQUEST of this exchange went out. A lease is counted
    /// from the REQUEST that was answered (RFC 2131 section 4.4.1), and
    /// counting from the first one is never later than that.
    first_sent: u64,
    sent: u32,
    /// When to send again, or to give up.
    deadline: u64,
}

impl Exchange {
    fn new(now: u64) -> Exchange {
        Exchange {
            xid: crate::rng::next_u64() as u32,
            began: now,
            first_sent: now,
            sent: 0,
            deadline: now,
        }
    }

    fn secs(&self, now: u64) -> u16 {
        (now.saturating_sub(self.began) / HZ).min(u16::MAX as u64) as u16
    }
}

/// What the server whose offer was taken offered.
#[derive(Clone, Copy)]
struct Offer {
    address: Ipv4Addr,
    server: Ipv4Addr,
}

/// A lease held: the configuration it gives, who gave it, and the ticks at
/// which it has to be renewed, rebound and given up.
#[derive(Clone, Copy)]
pub struct Lease {
    pub config: Config,
    pub server: Ipv4Addr,
    pub seconds: u32,
    pub renew_at: u64,
    pub rebind_at: u64,
    pub expires_at: u64,
}

impl Lease {
    fn new(config: Config, server: Ipv4Addr, reply: &Reply, start: u64) -> Lease {
        let seconds = reply.lease_seconds.unwrap_or(DEFAULT_LEASE_SECONDS).max(MIN_LEASE_SECONDS);
        if seconds == INFINITE {
            return Lease {
                config,
                server,
                seconds,
                renew_at: u64::MAX,
                rebind_at: u64::MAX,
                expires_at: u64::MAX,
            };
        }
        let length = seconds as u64;
        // RFC 2131 section 4.4.5: T1 is half the lease and T2 seven eighths of
        // it unless the server says otherwise. A server's own times are taken
        // when they are in order -- T2 no later than the end, T1 no later than
        // T2 -- and the defaults take the place of any that are not.
        let t2 = reply
            .rebinding_seconds
            .map(u64::from)
            .filter(|t2| *t2 <= length)
            .unwrap_or(length * 7 / 8);
        let t1 = reply
            .renewal_seconds
            .map(u64::from)
            .filter(|t1| *t1 <= t2)
            .unwrap_or((length / 2).min(t2));
        Lease {
            config,
            server,
            seconds,
            renew_at: start + t1 * HZ,
            rebind_at: start + t2 * HZ,
            expires_at: start + length * HZ,
        }
    }
}

/// How long a lease is, as the boot log says it.
struct Length(u32);

impl core::fmt::Display for Length {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        if self.0 == INFINITE {
            write!(f, "forever")
        } else {
            write!(f, "{} s", self.0)
        }
    }
}

/// RFC 2131's client states, figure 5, less INIT-REBOOT and REBOOTING.
#[derive(Clone, Copy)]
enum State {
    /// Nothing held and nothing asked. The first DISCOVER goes at `at`.
    Init { at: u64 },
    /// DISCOVERs out, waiting for an offer.
    Selecting(Exchange),
    /// An offer taken, and a REQUEST out for it.
    Requesting(Exchange, Offer),
    /// A lease held, with nothing to say until T1.
    Bound(Lease),
    /// Past T1: asking the server that granted the lease to extend it.
    Renewing(Exchange, Lease),
    /// Past T2: asking any server at all.
    Rebinding(Exchange, Lease),
}

/// Something `poll` decided should happen, for its caller to do.
pub enum Effect {
    /// Send this message to port 67 at this address.
    Send { to: Ipv4Addr, message: Vec<u8> },
    /// This is now the machine's configuration.
    Configure(Option<Config>),
}

/// The wait after the `sent`th message of a DISCOVER or first REQUEST.
fn backoff(sent: u32) -> u64 {
    let doublings = sent.saturating_sub(1).min(4);
    randomized((FIRST_RETRANSMIT << doublings).min(MAX_RETRANSMIT))
}

/// `delay`, moved by a uniform amount of up to `JITTER` either way.
fn randomized(delay: u64) -> u64 {
    let offset = crate::rng::next_u64() % (2 * JITTER + 1);
    delay - JITTER + offset
}

/// When to send a REQUEST again in RENEWING or REBINDING, where the state that
/// follows takes over at `end`.
fn retransmit_before(now: u64, end: u64) -> u64 {
    let half = end.saturating_sub(now) / 2;
    now.saturating_add(half.max(MIN_RENEW_RETRANSMIT)).min(end)
}

pub struct Client {
    socket: Arc<InetSocket>,
    mac: [u8; 6],
    /// `nameserver=` from the command line, which takes the place of whatever
    /// name servers a lease names.
    nameserver: Option<Ipv4Addr>,
    state: State,
    link_up: bool,
    buffer: Vec<u8>,
}

impl Client {
    /// A client for the card with this hardware address. Its first DISCOVER
    /// goes out on the first `poll` at or after `now`.
    pub fn new(mac: [u8; 6], nameserver: Option<Ipv4Addr>, now: u64) -> Result<Client, Errno> {
        let socket = InetSocket::new(false);
        socket.bind(Endpoint::new(Ipv4Addr::UNSPECIFIED, CLIENT_PORT))?;
        Ok(Client {
            socket,
            mac,
            nameserver,
            state: State::Init { at: now },
            link_up: false,
            buffer: alloc::vec![0u8; ether::MTU],
        })
    }

    pub fn socket(&self) -> &Arc<InetSocket> {
        &self.socket
    }

    /// The lease held, if one is.
    pub fn lease(&self) -> Option<Lease> {
        match self.state {
            State::Bound(lease) | State::Renewing(_, lease) | State::Rebinding(_, lease) => {
                Some(lease)
            }
            State::Init { .. } | State::Selecting(_) | State::Requesting(..) => None,
        }
    }

    /// The tick at which `poll` next has something to do if nothing arrives.
    pub fn next_deadline(&self) -> u64 {
        match self.state {
            State::Init { at } => at,
            State::Selecting(exchange) | State::Requesting(exchange, _) => exchange.deadline,
            State::Bound(lease) => lease.renew_at,
            State::Renewing(exchange, lease) => exchange.deadline.min(lease.rebind_at),
            State::Rebinding(exchange, lease) => exchange.deadline.min(lease.expires_at),
        }
    }

    /// Read what has arrived, look at the clock, and say what should happen.
    pub fn poll(&mut self, now: u64) -> Vec<Effect> {
        let mut effects = Vec::new();

        let up = super::link_up();
        if up && !self.link_up && self.lease().is_none() {
            // Whatever went out while the link was down arrived nowhere, and
            // after a few unanswered DISCOVERs the timer may not fire again for
            // a minute. Start again now.
            self.state = State::Init { at: now };
        }
        self.link_up = up;

        // As many as the socket can hold, so a socket whose read side somebody
        // shut down, which reads as empty for ever, cannot hold this here.
        for _ in 0..64 {
            let Ok(received) = self.socket.receive_into(&mut self.buffer, false) else {
                break;
            };
            if received.from.map(|from| from.port) != Some(SERVER_PORT) {
                continue;
            }
            if let Some(reply) = Reply::parse(&self.buffer[..received.taken]) {
                self.on_reply(&reply, now, &mut effects);
            }
        }

        self.on_timer(now, &mut effects);
        effects
    }

    /// Do what `poll` decides, straight away. For a client nothing else holds,
    /// which is what the self test drives.
    pub fn run(&mut self, now: u64) {
        let effects = self.poll(now);
        apply(&self.socket, effects);
    }

    fn send(&self, effects: &mut Vec<Effect>, to: Ipv4Addr, message: Outgoing) {
        effects.push(Effect::Send { to, message: message.build(self.mac) });
    }

    /// The configuration a server's message describes, with `nameserver=` in
    /// place of its name servers if the command line gave one.
    fn config_from(&self, reply: &Reply) -> Result<Config, &'static str> {
        let netmask = reply.netmask.unwrap_or_else(|| Config::class_netmask(reply.yiaddr));
        // Routers are listed in order of preference (RFC 2132 section 3.5).
        // The first one on the subnet is the gateway; one anywhere else could
        // only be reached through a route this stack has no way to hold.
        let gateway = reply
            .routers
            .iter()
            .copied()
            .find(|router| Config::new(reply.yiaddr, netmask, Some(*router), &[]).is_ok());
        let nameservers = match &self.nameserver {
            Some(server) => core::slice::from_ref(server),
            None => reply.nameservers.as_slice(),
        };
        Config::new(reply.yiaddr, netmask, gateway, nameservers)
    }

    fn on_reply(&mut self, reply: &Reply, now: u64, effects: &mut Vec<Effect>) {
        // Every broadcast answer to every client on the segment arrives here:
        // only one naming this card belongs to this client.
        if reply.chaddr != self.mac {
            return;
        }
        match self.state {
            State::Selecting(exchange) => {
                if reply.xid != exchange.xid || reply.kind != OFFER {
                    return;
                }
                // An offer names its server (RFC 2131 table 3), and the
                // REQUEST that takes it has to name the same one.
                let Some(server) = reply.server else { return };
                if let Err(reason) = self.config_from(reply) {
                    crate::println!(
                        "dhcp: ignoring the offer of {} from {}: {}",
                        reply.yiaddr,
                        server,
                        reason
                    );
                    return;
                }
                // The first acceptable offer is taken. The REQUEST carries the
                // same transaction id, and goes out from `on_timer` straight
                // after this.
                let offer = Offer { address: reply.yiaddr, server };
                let exchange = Exchange { first_sent: now, sent: 0, deadline: now, ..exchange };
                self.state = State::Requesting(exchange, offer);
            }
            State::Requesting(exchange, offer) => {
                if reply.xid != exchange.xid {
                    return;
                }
                // Every server hears the REQUEST, and those it does not name
                // are to stay quiet: an answer from one of them is not about
                // this request.
                if reply.server.is_some_and(|server| server != offer.server) {
                    return;
                }
                match reply.kind {
                    ACK => self.take_lease(reply, offer.server, exchange.first_sent, now, effects),
                    NAK => {
                        crate::println!(
                            "dhcp: {} refused {}; asking again",
                            offer.server,
                            offer.address
                        );
                        self.state = State::Init { at: now + randomized(FIRST_RETRANSMIT) };
                    }
                    _ => {}
                }
            }
            State::Renewing(exchange, lease) | State::Rebinding(exchange, lease) => {
                if reply.xid != exchange.xid {
                    return;
                }
                // A renewal goes to the server that granted the lease, so only
                // its answer counts; a rebinding asks every server.
                let renewing = matches!(self.state, State::Renewing(..));
                if renewing && reply.server.is_some_and(|server| server != lease.server) {
                    return;
                }
                let server = reply.server.unwrap_or(lease.server);
                match reply.kind {
                    ACK => self.take_lease(reply, server, exchange.first_sent, now, effects),
                    NAK => {
                        crate::println!(
                            "dhcp: {} refused to extend the lease on {}; giving the address up",
                            server,
                            lease.config.address()
                        );
                        effects.push(Effect::Configure(None));
                        // Not at once: a server that refuses every time would
                        // otherwise be asked as fast as it can answer.
                        self.state = State::Init { at: now + randomized(FIRST_RETRANSMIT) };
                    }
                    _ => {}
                }
            }
            State::Init { .. } | State::Bound(_) => {}
        }
    }

    fn take_lease(
        &mut self,
        reply: &Reply,
        server: Ipv4Addr,
        start: u64,
        now: u64,
        effects: &mut Vec<Effect>,
    ) {
        let held = self.lease();
        let config = match self.config_from(reply) {
            Ok(config) => config,
            Err(reason) => {
                crate::println!(
                    "dhcp: {} granted {}, which cannot be used: {}",
                    server,
                    reply.yiaddr,
                    reason
                );
                if held.is_some() {
                    effects.push(Effect::Configure(None));
                }
                self.state = State::Init { at: now + randomized(FIRST_RETRANSMIT) };
                return;
            }
        };
        let lease = Lease::new(config, server, reply, start);
        if held.is_some_and(|previous| previous.config == config) {
            crate::println!(
                "dhcp: {} renewed, lease {} from {}",
                config.address(),
                Length(lease.seconds),
                server
            );
        } else {
            crate::println!("dhcp: {}, lease {} from {}", config, Length(lease.seconds), server);
            effects.push(Effect::Configure(Some(config)));
        }
        self.state = State::Bound(lease);
    }

    fn on_timer(&mut self, now: u64, effects: &mut Vec<Effect>) {
        // Each pass either finds nothing due and returns, or moves to another
        // state. A lease passes through at most five on its way back to a
        // DISCOVER, which is how many can come due at once when the clock has
        // not been looked at for a long time.
        for _ in 0..8 {
            match self.state {
                State::Init { at } => {
                    if now < at {
                        return;
                    }
                    self.state = State::Selecting(Exchange::new(now));
                }
                State::Selecting(mut exchange) => {
                    if now < exchange.deadline {
                        return;
                    }
                    if exchange.sent > 0 {
                        let missing = if self.link_up { "answer" } else { "link" };
                        crate::println!("dhcp: no {} yet, still trying", missing);
                    }
                    let message = Outgoing {
                        kind: DISCOVER,
                        xid: exchange.xid,
                        secs: exchange.secs(now),
                        broadcast: true,
                        ciaddr: Ipv4Addr::UNSPECIFIED,
                        requested: None,
                        server: None,
                    };
                    self.send(effects, Ipv4Addr::BROADCAST, message);
                    exchange.sent += 1;
                    exchange.deadline = now + backoff(exchange.sent);
                    self.state = State::Selecting(exchange);
                    return;
                }
                State::Requesting(mut exchange, offer) => {
                    if now < exchange.deadline {
                        return;
                    }
                    if exchange.sent >= REQUEST_ATTEMPTS {
                        crate::println!(
                            "dhcp: {} did not answer the request for {}; starting over",
                            offer.server,
                            offer.address
                        );
                        self.state = State::Init { at: now };
                        continue;
                    }
                    // Broadcast, naming the chosen server, so the servers whose
                    // offers were not taken hear that as well (RFC 2131
                    // section 4.4.1).
                    let message = Outgoing {
                        kind: REQUEST,
                        xid: exchange.xid,
                        secs: exchange.secs(now),
                        broadcast: true,
                        ciaddr: Ipv4Addr::UNSPECIFIED,
                        requested: Some(offer.address),
                        server: Some(offer.server),
                    };
                    self.send(effects, Ipv4Addr::BROADCAST, message);
                    exchange.sent += 1;
                    exchange.deadline = now + backoff(exchange.sent);
                    self.state = State::Requesting(exchange, offer);
                    return;
                }
                State::Bound(lease) => {
                    if now < lease.renew_at {
                        return;
                    }
                    self.state = State::Renewing(Exchange::new(now), lease);
                }
                State::Renewing(mut exchange, lease) => {
                    if now >= lease.rebind_at {
                        self.state = State::Rebinding(Exchange::new(now), lease);
                        continue;
                    }
                    if now < exchange.deadline {
                        return;
                    }
                    // To the server that granted the lease, from the address it
                    // granted. RFC 2131 table 5 has a REQUEST in this state name
                    // neither the address nor the server.
                    let message = Outgoing {
                        kind: REQUEST,
                        xid: exchange.xid,
                        secs: exchange.secs(now),
                        broadcast: false,
                        ciaddr: lease.config.address(),
                        requested: None,
                        server: None,
                    };
                    self.send(effects, lease.server, message);
                    exchange.sent += 1;
                    exchange.deadline = retransmit_before(now, lease.rebind_at);
                    self.state = State::Renewing(exchange, lease);
                    return;
                }
                State::Rebinding(mut exchange, lease) => {
                    if now >= lease.expires_at {
                        crate::println!(
                            "dhcp: the lease on {} ran out; giving the address up",
                            lease.config.address()
                        );
                        effects.push(Effect::Configure(None));
                        self.state = State::Init { at: now };
                        continue;
                    }
                    if now < exchange.deadline {
                        return;
                    }
                    let message = Outgoing {
                        kind: REQUEST,
                        xid: exchange.xid,
                        secs: exchange.secs(now),
                        broadcast: false,
                        ciaddr: lease.config.address(),
                        requested: None,
                        server: None,
                    };
                    self.send(effects, Ipv4Addr::BROADCAST, message);
                    exchange.sent += 1;
                    exchange.deadline = retransmit_before(now, lease.expires_at);
                    self.state = State::Rebinding(exchange, lease);
                    return;
                }
            }
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        super::socket::close(&self.socket);
    }
}

/// Do what `poll` decided, in the order it decided it.
pub fn apply(socket: &InetSocket, effects: Vec<Effect>) {
    for effect in effects {
        match effect {
            Effect::Configure(config) => super::configure(config),
            Effect::Send { to, message } => {
                // A message that cannot go out now -- the link is down -- is
                // lost the way one on a lossy link is, and the retransmission
                // timer covers both.
                let _ = socket.send(&message, Some(Endpoint::new(to, SERVER_PORT)));
            }
        }
    }
}

// ---- the machine's own client ------------------------------------------------

static CLIENT: Spinlock<Option<Client>> = Spinlock::new(None);

/// Start asking for an address. What follows happens in `on_tick`, which the
/// network task calls every tick.
pub fn start(nameserver: Option<Ipv4Addr>) {
    match Client::new(super::mac(), nameserver, crate::trap::ticks()) {
        Ok(client) => *CLIENT.lock() = Some(client),
        Err(err) => crate::println!("dhcp: cannot use port {}: {:?}", CLIENT_PORT, err),
    }
}

pub fn on_tick() {
    let now = crate::trap::ticks();
    // Decided under the lock; done after it is let go.
    let work = CLIENT.lock().as_mut().map(|client| (client.socket.clone(), client.poll(now)));
    if let Some((socket, effects)) = work {
        apply(&socket, effects);
    }
}

pub fn next_deadline() -> u64 {
    CLIENT.lock().as_ref().map_or(u64::MAX, |client| client.next_deadline())
}
