//! Ethernet II frames.
//!
//! Everything above this sees a payload and a protocol number; everything
//! below sees a byte string with a fourteen byte header on the front.

use alloc::vec::Vec;

pub const HEADER_LEN: usize = 14;
/// The shortest frame a card will put on the wire. Anything shorter is padded
/// with zeroes, which every protocol above ignores because each carries its
/// own length.
pub const MIN_FRAME_LEN: usize = 60;
/// The largest payload an untagged frame carries.
pub const MTU: usize = 1500;

pub const ETHERTYPE_IPV4: u16 = 0x0800;
pub const ETHERTYPE_ARP: u16 = 0x0806;
pub const ETHERTYPE_IPV6: u16 = 0x86DD;

pub const BROADCAST: [u8; 6] = [0xFF; 6];

pub struct Frame<'a> {
    pub destination: [u8; 6],
    pub source: [u8; 6],
    pub ethertype: u16,
    pub payload: &'a [u8],
}

impl<'a> Frame<'a> {
    pub fn parse(bytes: &'a [u8]) -> Option<Frame<'a>> {
        if bytes.len() < HEADER_LEN {
            return None;
        }
        let mut destination = [0u8; 6];
        let mut source = [0u8; 6];
        destination.copy_from_slice(&bytes[0..6]);
        source.copy_from_slice(&bytes[6..12]);
        Some(Frame {
            destination,
            source,
            ethertype: u16::from_be_bytes([bytes[12], bytes[13]]),
            payload: &bytes[HEADER_LEN..],
        })
    }
}

/// One frame, padded to the minimum length.
pub fn build(destination: [u8; 6], source: [u8; 6], ethertype: u16, payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity((HEADER_LEN + payload.len()).max(MIN_FRAME_LEN));
    frame.extend_from_slice(&destination);
    frame.extend_from_slice(&source);
    frame.extend_from_slice(&ethertype.to_be_bytes());
    frame.extend_from_slice(payload);
    while frame.len() < MIN_FRAME_LEN {
        frame.push(0);
    }
    frame
}

/// True when `address` has the group bit set: broadcast and multicast both.
pub fn is_group(address: &[u8; 6]) -> bool {
    address[0] & 1 != 0
}
