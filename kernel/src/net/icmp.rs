//! ICMP, far enough to answer a ping.
//!
//! Only the echo request is acted on. Destination unreachable and the rest are
//! parsed no further than deciding to ignore them, because nothing here keeps
//! the state that would let it act on one.

use super::ip::{self, Ipv4Addr};
use alloc::vec::Vec;

pub const TYPE_ECHO_REPLY: u8 = 0;
pub const TYPE_DESTINATION_UNREACHABLE: u8 = 3;
pub const TYPE_ECHO_REQUEST: u8 = 8;

pub const HEADER_LEN: usize = 8;

/// Build one ICMP message: type, code, four bytes of per-type header, then the
/// body. The checksum covers the whole thing.
pub fn build(kind: u8, code: u8, rest: [u8; 4], body: &[u8]) -> Vec<u8> {
    let mut message = Vec::with_capacity(HEADER_LEN + body.len());
    message.push(kind);
    message.push(code);
    message.extend_from_slice(&[0, 0]);
    message.extend_from_slice(&rest);
    message.extend_from_slice(body);
    let checksum = ip::checksum(&message);
    message[2..4].copy_from_slice(&checksum.to_be_bytes());
    message
}

pub fn receive(source: Ipv4Addr, destination: Ipv4Addr, message: &[u8]) {
    if message.len() < HEADER_LEN {
        return;
    }
    if ip::checksum(message) != 0 {
        return;
    }
    if message[0] != TYPE_ECHO_REQUEST || message[1] != 0 {
        return;
    }
    // The identifier and sequence number go back unchanged; so does the body,
    // which is how the sender measures the round trip.
    let mut rest = [0u8; 4];
    rest.copy_from_slice(&message[4..8]);
    let reply = build(TYPE_ECHO_REPLY, 0, rest, &message[HEADER_LEN..]);
    // Answer from the address that was asked, so a reply to a broadcast ping
    // still comes from this machine's own address.
    let from = if destination == super::config().address {
        destination
    } else {
        super::config().address
    };
    let _ = ip::send_from(from, source, ip::PROTO_ICMP, &reply);
}
