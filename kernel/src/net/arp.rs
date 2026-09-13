//! Address resolution: which hardware address is at a given IPv4 address.
//!
//! The cache answers two questions. Outbound, it turns the next hop into a
//! destination for the Ethernet header, holding the datagram and asking the
//! link while the answer is outstanding. Inbound, it answers requests for our
//! own address, which is how anything on the segment finds us at all.

use super::ether;
use super::ip::Ipv4Addr;
use crate::abi::Errno;
use crate::sync::Spinlock;
use alloc::vec::Vec;

pub const PACKET_LEN: usize = 28;
const HARDWARE_ETHERNET: u16 = 1;
pub const OP_REQUEST: u16 = 1;
pub const OP_REPLY: u16 = 2;

/// How long a resolved entry is believed, in timer ticks.
const ENTRY_LIFETIME: u64 = 100 * 60;
/// How long to wait for a reply before asking again.
const REQUEST_INTERVAL: u64 = 100;
/// How many times to ask before giving up and dropping what was queued.
const MAX_REQUESTS: u32 = 4;
/// Datagrams held per unresolved address.
const MAX_QUEUED: usize = 4;
const MAX_ENTRIES: usize = 64;

struct Entry {
    address: Ipv4Addr,
    mac: Option<[u8; 6]>,
    /// Tick at which a resolved entry stops being believed, or at which an
    /// unresolved one asks again.
    deadline: u64,
    requests: u32,
    /// Datagrams waiting for the answer, without their Ethernet header.
    queued: Vec<Vec<u8>>,
}

static CACHE: Spinlock<Vec<Entry>> = Spinlock::new(Vec::new());

pub fn clear() {
    CACHE.lock().clear();
}

/// The hardware address for `address`, if one is cached and still believed.
pub fn lookup(address: Ipv4Addr) -> Option<[u8; 6]> {
    let cache = CACHE.lock();
    cache
        .iter()
        .find(|entry| entry.address == address)
        .and_then(|entry| entry.mac)
}

/// Record an address pair, and release anything that was waiting for it.
pub fn learn(address: Ipv4Addr, mac: [u8; 6]) {
    if address.is_unspecified() || ether::is_group(&mac) {
        return;
    }
    // Someone else on the segment saying they hold this machine's address is
    // either a conflict or a lie. Believing it would send everything meant
    // for us to their card.
    let ours = super::config().address;
    if !ours.is_unspecified() && address == ours {
        return;
    }
    let now = crate::trap::ticks();
    let mut released: Vec<Vec<u8>> = Vec::new();
    {
        let mut cache = CACHE.lock();
        match cache.iter_mut().find(|entry| entry.address == address) {
            Some(entry) => {
                entry.mac = Some(mac);
                entry.deadline = now + ENTRY_LIFETIME;
                entry.requests = 0;
                released.append(&mut entry.queued);
            }
            None => {
                if cache.len() >= MAX_ENTRIES {
                    // Throw out whatever expires soonest rather than refuse
                    // to learn anything new.
                    if let Some(index) = oldest(&cache) {
                        cache.remove(index);
                    }
                }
                cache.push(Entry {
                    address,
                    mac: Some(mac),
                    deadline: now + ENTRY_LIFETIME,
                    requests: 0,
                    queued: Vec::new(),
                });
            }
        }
    }
    for datagram in released {
        let _ = transmit_to(mac, ether::ETHERTYPE_IPV4, &datagram);
    }
}

fn oldest(cache: &[Entry]) -> Option<usize> {
    let mut best: Option<(usize, u64)> = None;
    for (index, entry) in cache.iter().enumerate() {
        if best.map_or(true, |(_, deadline)| entry.deadline < deadline) {
            best = Some((index, entry.deadline));
        }
    }
    best.map(|(index, _)| index)
}

fn transmit_to(mac: [u8; 6], ethertype: u16, payload: &[u8]) -> Result<(), Errno> {
    let frame = ether::build(mac, super::mac(), ethertype, payload);
    super::transmit(&frame)
}

/// Put one IPv4 datagram on the wire, resolving `next_hop` first if need be.
///
/// An unresolved address is asked about and the datagram held. TCP would
/// retransmit anyway, but a datagram protocol has nothing that would, and the
/// first packet of a connection is the one that matters most.
pub fn send_datagram(next_hop: Ipv4Addr, datagram: Vec<u8>) -> Result<(), Errno> {
    if next_hop.is_broadcast() {
        return transmit_to(ether::BROADCAST, ether::ETHERTYPE_IPV4, &datagram);
    }
    if next_hop.is_multicast() {
        let bytes = next_hop.to_be_bytes();
        let mac = [0x01, 0x00, 0x5E, bytes[1] & 0x7F, bytes[2], bytes[3]];
        return transmit_to(mac, ether::ETHERTYPE_IPV4, &datagram);
    }
    if let Some(mac) = lookup(next_hop) {
        return transmit_to(mac, ether::ETHERTYPE_IPV4, &datagram);
    }
    let now = crate::trap::ticks();
    {
        let mut cache = CACHE.lock();
        match cache.iter_mut().find(|entry| entry.address == next_hop) {
            Some(entry) => {
                if entry.queued.len() < MAX_QUEUED {
                    entry.queued.push(datagram);
                }
            }
            None => {
                if cache.len() >= MAX_ENTRIES {
                    if let Some(index) = oldest(&cache) {
                        cache.remove(index);
                    }
                }
                cache.push(Entry {
                    address: next_hop,
                    mac: None,
                    deadline: now + REQUEST_INTERVAL,
                    requests: 1,
                    queued: alloc::vec![datagram],
                });
            }
        }
    }
    request(next_hop)
}

/// Build one ARP packet.
pub fn build(
    operation: u16,
    sender_mac: [u8; 6],
    sender_ip: Ipv4Addr,
    target_mac: [u8; 6],
    target_ip: Ipv4Addr,
) -> [u8; PACKET_LEN] {
    let mut packet = [0u8; PACKET_LEN];
    packet[0..2].copy_from_slice(&HARDWARE_ETHERNET.to_be_bytes());
    packet[2..4].copy_from_slice(&ether::ETHERTYPE_IPV4.to_be_bytes());
    packet[4] = 6;
    packet[5] = 4;
    packet[6..8].copy_from_slice(&operation.to_be_bytes());
    packet[8..14].copy_from_slice(&sender_mac);
    packet[14..18].copy_from_slice(&sender_ip.to_be_bytes());
    packet[18..24].copy_from_slice(&target_mac);
    packet[24..28].copy_from_slice(&target_ip.to_be_bytes());
    packet
}

/// Ask the segment who holds `address`.
pub fn request(address: Ipv4Addr) -> Result<(), Errno> {
    let packet = build(
        OP_REQUEST,
        super::mac(),
        super::config().address,
        [0u8; 6],
        address,
    );
    transmit_to(ether::BROADCAST, ether::ETHERTYPE_ARP, &packet)
}

/// One received ARP packet.
pub fn receive(bytes: &[u8]) {
    if bytes.len() < PACKET_LEN {
        return;
    }
    if u16::from_be_bytes([bytes[0], bytes[1]]) != HARDWARE_ETHERNET {
        return;
    }
    if u16::from_be_bytes([bytes[2], bytes[3]]) != ether::ETHERTYPE_IPV4 {
        return;
    }
    if bytes[4] != 6 || bytes[5] != 4 {
        return;
    }
    let operation = u16::from_be_bytes([bytes[6], bytes[7]]);
    let mut sender_mac = [0u8; 6];
    sender_mac.copy_from_slice(&bytes[8..14]);
    let sender_ip = Ipv4Addr::from_be_bytes([bytes[14], bytes[15], bytes[16], bytes[17]]);
    let target_ip = Ipv4Addr::from_be_bytes([bytes[24], bytes[25], bytes[26], bytes[27]]);

    // Both a request and a reply carry the sender's pair, and both are worth
    // remembering: a request is usually followed by traffic we have to answer.
    learn(sender_ip, sender_mac);

    let ours = super::config().address;
    if operation == OP_REQUEST && target_ip == ours && !ours.is_unspecified() {
        let reply = build(OP_REPLY, super::mac(), ours, sender_mac, sender_ip);
        let _ = transmit_to(sender_mac, ether::ETHERTYPE_ARP, &reply);
    }
}

/// The next tick at which `expire` has something to do.
pub fn next_deadline() -> u64 {
    let cache = CACHE.lock();
    cache.iter().map(|entry| entry.deadline).min().unwrap_or(u64::MAX)
}

/// Drop entries that have aged out, and ask again about the ones still
/// unanswered.
pub fn expire() {
    let now = crate::trap::ticks();
    let mut ask: Vec<Ipv4Addr> = Vec::new();
    {
        let mut cache = CACHE.lock();
        cache.retain(|entry| {
            if now < entry.deadline {
                return true;
            }
            // A resolved entry that has aged out is simply forgotten; the next
            // datagram through it asks again.
            entry.mac.is_none() && entry.requests < MAX_REQUESTS
        });
        for entry in cache.iter_mut() {
            if now >= entry.deadline && entry.mac.is_none() {
                entry.requests += 1;
                entry.deadline = now + REQUEST_INTERVAL;
                ask.push(entry.address);
            }
        }
    }
    for address in ask {
        let _ = request(address);
    }
}
