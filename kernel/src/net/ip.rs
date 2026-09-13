//! IPv4: addresses, the header, the ones' complement checksum, and one route.
//!
//! Reassembly is left out. A datagram that arrives as a fragment -- either the
//! more-fragments bit is set or the fragment offset is non-zero -- is dropped
//! here rather than held, so nothing above ever sees half a datagram. Nothing
//! this stack sends is larger than the link's maximum transmission unit, so it
//! never fragments either: a payload that would not fit is refused with
//! EMSGSIZE at the protocol that offered it.

use crate::abi::Errno;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU16, Ordering};

/// An IPv4 address, held in host byte order.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Default)]
pub struct Ipv4Addr(pub u32);

impl Ipv4Addr {
    pub const UNSPECIFIED: Ipv4Addr = Ipv4Addr(0);
    pub const BROADCAST: Ipv4Addr = Ipv4Addr(0xFFFF_FFFF);

    pub const fn new(a: u8, b: u8, c: u8, d: u8) -> Ipv4Addr {
        Ipv4Addr(((a as u32) << 24) | ((b as u32) << 16) | ((c as u32) << 8) | d as u32)
    }

    pub fn from_be_bytes(bytes: [u8; 4]) -> Ipv4Addr {
        Ipv4Addr(u32::from_be_bytes(bytes))
    }

    pub fn to_be_bytes(self) -> [u8; 4] {
        self.0.to_be_bytes()
    }

    pub fn is_unspecified(self) -> bool {
        self.0 == 0
    }

    pub fn is_broadcast(self) -> bool {
        self.0 == 0xFFFF_FFFF
    }

    pub fn is_multicast(self) -> bool {
        self.0 >> 28 == 0xE
    }

    pub fn is_loopback(self) -> bool {
        self.0 >> 24 == 127
    }
}

impl core::fmt::Display for Ipv4Addr {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let b = self.to_be_bytes();
        write!(f, "{}.{}.{}.{}", b[0], b[1], b[2], b[3])
    }
}

/// Parse dotted quad notation. Used by the kernel command line.
pub fn parse_address(text: &str) -> Option<Ipv4Addr> {
    let mut octets = [0u8; 4];
    let mut seen = 0;
    for part in text.split('.') {
        if seen == 4 || part.is_empty() || part.len() > 3 {
            return None;
        }
        octets[seen] = part.parse::<u8>().ok()?;
        seen += 1;
    }
    if seen != 4 {
        return None;
    }
    Some(Ipv4Addr::from_be_bytes(octets))
}

pub const PROTO_ICMP: u8 = 1;
pub const PROTO_TCP: u8 = 6;
pub const PROTO_UDP: u8 = 17;

pub const HEADER_LEN: usize = 20;
const DEFAULT_TTL: u8 = 64;
const FLAG_DONT_FRAGMENT: u16 = 0x4000;
const FLAG_MORE_FRAGMENTS: u16 = 0x2000;
const FRAGMENT_OFFSET_MASK: u16 = 0x1FFF;

/// A running sum of sixteen bit big-endian words, with the odd trailing byte
/// treated as the high half of a word.
pub fn sum(bytes: &[u8], start: u32) -> u32 {
    let mut total = start;
    let mut i = 0;
    while i + 1 < bytes.len() {
        total += u16::from_be_bytes([bytes[i], bytes[i + 1]]) as u32;
        i += 2;
    }
    if i < bytes.len() {
        total += (bytes[i] as u32) << 8;
    }
    total
}

/// Fold the carries back in and complement: the value that goes in the header.
pub fn fold(mut total: u32) -> u16 {
    while total >> 16 != 0 {
        total = (total & 0xFFFF) + (total >> 16);
    }
    !(total as u16)
}

pub fn checksum(bytes: &[u8]) -> u16 {
    fold(sum(bytes, 0))
}

/// The part of a TCP or UDP checksum that comes from the IP header.
pub fn pseudo_sum(source: Ipv4Addr, destination: Ipv4Addr, protocol: u8, length: usize) -> u32 {
    let s = source.0;
    let d = destination.0;
    (s >> 16) + (s & 0xFFFF) + (d >> 16) + (d & 0xFFFF) + protocol as u32 + length as u32
}

#[derive(Clone, Copy)]
pub struct Header {
    pub ihl: usize,
    pub total_length: usize,
    pub identification: u16,
    pub flags_and_offset: u16,
    pub ttl: u8,
    pub protocol: u8,
    pub source: Ipv4Addr,
    pub destination: Ipv4Addr,
}

impl Header {
    pub fn is_fragment(&self) -> bool {
        self.flags_and_offset & FLAG_MORE_FRAGMENTS != 0
            || self.flags_and_offset & FRAGMENT_OFFSET_MASK != 0
    }
}

/// Split a datagram into its header and its payload, refusing anything whose
/// lengths or checksum do not agree.
pub fn parse(bytes: &[u8]) -> Option<(Header, &[u8])> {
    if bytes.len() < HEADER_LEN {
        return None;
    }
    if bytes[0] >> 4 != 4 {
        return None;
    }
    let ihl = (bytes[0] & 0x0F) as usize * 4;
    if ihl < HEADER_LEN || bytes.len() < ihl {
        return None;
    }
    if checksum(&bytes[..ihl]) != 0 {
        return None;
    }
    let total_length = u16::from_be_bytes([bytes[2], bytes[3]]) as usize;
    // A frame padded up to the minimum length is longer than its datagram;
    // one truncated by the link is shorter, and cannot be trusted.
    if total_length < ihl || total_length > bytes.len() {
        return None;
    }
    let header = Header {
        ihl,
        total_length,
        identification: u16::from_be_bytes([bytes[4], bytes[5]]),
        flags_and_offset: u16::from_be_bytes([bytes[6], bytes[7]]),
        ttl: bytes[8],
        protocol: bytes[9],
        source: Ipv4Addr::from_be_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]),
        destination: Ipv4Addr::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]),
    };
    Some((header, &bytes[ihl..total_length]))
}

static NEXT_ID: AtomicU16 = AtomicU16::new(1);

/// Build a datagram with no options and the don't-fragment bit set.
pub fn build(source: Ipv4Addr, destination: Ipv4Addr, protocol: u8, payload: &[u8]) -> Vec<u8> {
    let total = HEADER_LEN + payload.len();
    let mut out = Vec::with_capacity(total);
    out.push(0x45);
    out.push(0);
    out.extend_from_slice(&(total as u16).to_be_bytes());
    out.extend_from_slice(&NEXT_ID.fetch_add(1, Ordering::Relaxed).to_be_bytes());
    out.extend_from_slice(&FLAG_DONT_FRAGMENT.to_be_bytes());
    out.push(DEFAULT_TTL);
    out.push(protocol);
    out.extend_from_slice(&[0, 0]); // checksum, filled in below
    out.extend_from_slice(&source.to_be_bytes());
    out.extend_from_slice(&destination.to_be_bytes());
    let header_checksum = checksum(&out[..HEADER_LEN]);
    out[10..12].copy_from_slice(&header_checksum.to_be_bytes());
    out.extend_from_slice(payload);
    out
}

/// Send one datagram, choosing the next hop and resolving its hardware
/// address on the way.
pub fn send(destination: Ipv4Addr, protocol: u8, payload: &[u8]) -> Result<(), Errno> {
    send_from(super::config().address, destination, protocol, payload)
}

pub fn send_from(
    source: Ipv4Addr,
    destination: Ipv4Addr,
    protocol: u8,
    payload: &[u8],
) -> Result<(), Errno> {
    if HEADER_LEN + payload.len() > super::ether::MTU {
        // No fragmentation: the caller has to offer something that fits.
        return Err(Errno::EMSGSIZE);
    }
    let datagram = build(source, destination, protocol, payload);
    // Anything addressed to this machine is handed back to this machine.
    if destination.is_loopback() || destination == super::config().address {
        super::loop_back(datagram);
        return Ok(());
    }
    super::arp::send_datagram(next_hop(destination), datagram)
}

/// The default route: anything off the local subnet goes to the gateway.
pub fn next_hop(destination: Ipv4Addr) -> Ipv4Addr {
    let config = super::config();
    if destination.is_broadcast() || destination.is_multicast() {
        return destination;
    }
    if config.on_link(destination) {
        destination
    } else {
        config.gateway
    }
}

/// Where a datagram came from: off a card, or from this machine talking to
/// itself. The two have different addresses to answer to.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    Card,
    Loopback,
}

/// One received datagram, already stripped of its Ethernet header.
pub fn receive(bytes: &[u8], source_mac: [u8; 6], origin: Origin) {
    let Some((header, payload)) = parse(bytes) else { return };
    let config = super::config();
    let for_us = match origin {
        Origin::Card => {
            // A datagram off a card that claims this machine's own address is
            // either a loop or a forgery, and 127/8 names whoever is asking,
            // so it never crosses a link in either field.
            if !config.address.is_unspecified() && header.source == config.address {
                return;
            }
            if header.source.is_loopback() || header.destination.is_loopback() {
                return;
            }
            header.destination == config.address
                || header.destination.is_broadcast()
                || header.destination == config.broadcast()
                // Before the address is configured, take whatever turns up:
                // the alternative is to drop the very packets that would
                // configure it.
                || config.address.is_unspecified()
        }
        // Built here for here; nothing else reaches the queue it came from.
        Origin::Loopback => {
            header.destination.is_loopback() || header.destination == config.address
        }
    };
    if !for_us {
        return;
    }
    // A neighbour that talks to us has just proved which hardware address it
    // is at, which saves a request the next time we answer it.
    if origin == Origin::Card
        && config.on_link(header.source)
        && !header.source.is_unspecified()
    {
        super::arp::learn(header.source, source_mac);
    }
    if header.is_fragment() {
        return;
    }
    match header.protocol {
        PROTO_ICMP => super::icmp::receive(header.source, header.destination, payload),
        PROTO_UDP => super::udp::receive(header.source, header.destination, payload),
        PROTO_TCP => super::tcp::receive(header.source, header.destination, payload),
        _ => {}
    }
}
