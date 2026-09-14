//! UDP: ports over IPv4, and one queue of whole datagrams per socket.

use super::ip::{self, Ipv4Addr};
use super::socket::Endpoint;
use crate::abi::Errno;
use alloc::collections::VecDeque;
use alloc::vec::Vec;

pub const HEADER_LEN: usize = 8;
/// Datagrams a socket will hold for a reader that has not turned up. Past
/// this the oldest is dropped, which is what a datagram protocol promises.
const QUEUE_LIMIT: usize = 64;
const QUEUE_BYTES: usize = 64 * 1024;

pub struct UdpState {
    pub local: Endpoint,
    /// Set by connect: where an address-less send goes, and the only source a
    /// receive will accept.
    pub remote: Option<Endpoint>,
    pub bound: bool,
    pub write_shutdown: bool,
    pub read_shutdown: bool,
    pub error: Option<Errno>,
    pub queue: VecDeque<(Endpoint, Vec<u8>)>,
    queued_bytes: usize,
}

impl UdpState {
    pub fn new() -> UdpState {
        UdpState {
            local: Endpoint::UNSPECIFIED,
            remote: None,
            bound: false,
            write_shutdown: false,
            read_shutdown: false,
            error: None,
            queue: VecDeque::new(),
            queued_bytes: 0,
        }
    }

    pub fn queued_bytes(&self) -> usize {
        self.queued_bytes
    }

    /// Nothing will be read from this socket again: what is queued is thrown
    /// away, and the room it took with it.
    pub fn shutdown_read(&mut self) {
        self.queue.clear();
        self.queued_bytes = 0;
        self.read_shutdown = true;
    }

    pub fn enqueue(&mut self, from: Endpoint, data: &[u8]) {
        while self.queue.len() >= QUEUE_LIMIT || self.queued_bytes + data.len() > QUEUE_BYTES {
            match self.queue.pop_front() {
                Some((_, dropped)) => self.queued_bytes -= dropped.len(),
                None => break,
            }
        }
        self.queued_bytes += data.len();
        self.queue.push_back((from, data.to_vec()));
    }

    /// One whole datagram, truncated to the buffer offered. What does not fit
    /// is discarded, which is what a datagram socket does.
    pub fn receive(&mut self, buf: &mut [u8], peek: bool) -> Result<(usize, Endpoint), Errno> {
        let Some((from, data)) = self.queue.front() else {
            if self.read_shutdown {
                // Linux reports the end here rather than waiting for a
                // datagram that nobody would be able to read.
                return Ok((0, Endpoint::UNSPECIFIED));
            }
            return Err(Errno::EAGAIN);
        };
        let n = buf.len().min(data.len());
        buf[..n].copy_from_slice(&data[..n]);
        let from = *from;
        if !peek {
            let (_, data) = self.queue.pop_front().expect("checked above");
            self.queued_bytes -= data.len();
        }
        Ok((n, from))
    }

    pub fn send(&mut self, buf: &[u8], to: Option<Endpoint>) -> Result<usize, Errno> {
        if self.write_shutdown {
            return Err(Errno::EPIPE);
        }
        let destination = match to.or(self.remote) {
            Some(destination) => destination,
            None => return Err(Errno::EDESTADDRREQ),
        };
        if destination.port == 0 {
            return Err(Errno::EINVAL);
        }
        if HEADER_LEN + buf.len() + ip::HEADER_LEN > super::ether::MTU {
            return Err(Errno::EMSGSIZE);
        }
        if self.local.port == 0 {
            return Err(Errno::EINVAL);
        }
        let source_address = if self.local.address.is_unspecified() {
            super::source_for(destination.address)?
        } else {
            self.local.address
        };
        let source = Endpoint::new(source_address, self.local.port);
        let datagram = build(source, destination, buf);
        ip::send_from(source_address, destination.address, ip::PROTO_UDP, &datagram)?;
        Ok(buf.len())
    }
}

/// Build one UDP datagram, checksum included.
///
/// IPv4 lets the checksum be left at zero; it is computed here anyway, because
/// a wrong one is the kind of fault that is invisible until something else
/// refuses the packet.
pub fn build(source: Endpoint, destination: Endpoint, payload: &[u8]) -> Vec<u8> {
    let length = HEADER_LEN + payload.len();
    let mut datagram = Vec::with_capacity(length);
    datagram.extend_from_slice(&source.port.to_be_bytes());
    datagram.extend_from_slice(&destination.port.to_be_bytes());
    datagram.extend_from_slice(&(length as u16).to_be_bytes());
    datagram.extend_from_slice(&[0, 0]);
    datagram.extend_from_slice(payload);
    let pseudo = ip::pseudo_sum(source.address, destination.address, ip::PROTO_UDP, length);
    let checksum = ip::fold(ip::sum(&datagram, pseudo));
    // Zero means "not computed", so a checksum that comes out zero is sent as
    // the other representation of zero in ones' complement.
    let checksum = if checksum == 0 { 0xFFFF } else { checksum };
    datagram[6..8].copy_from_slice(&checksum.to_be_bytes());
    datagram
}

pub fn receive(source: Ipv4Addr, destination: Ipv4Addr, datagram: &[u8]) {
    if datagram.len() < HEADER_LEN {
        return;
    }
    let length = u16::from_be_bytes([datagram[4], datagram[5]]) as usize;
    if length < HEADER_LEN || length > datagram.len() {
        return;
    }
    let datagram = &datagram[..length];
    let carried = u16::from_be_bytes([datagram[6], datagram[7]]);
    if carried != 0 {
        let pseudo = ip::pseudo_sum(source, destination, ip::PROTO_UDP, length);
        if ip::fold(ip::sum(datagram, pseudo)) != 0 {
            return;
        }
    }
    let from = Endpoint::new(source, u16::from_be_bytes([datagram[0], datagram[1]]));
    let to = Endpoint::new(destination, u16::from_be_bytes([datagram[2], datagram[3]]));

    let Some(socket) = super::socket::lookup_datagram(to, from) else {
        // Nothing is listening. A port unreachable message belongs here;
        // this stack does not send one.
        return;
    };
    match &mut *socket.inner.lock() {
        super::socket::Protocol::Udp(state) => state.enqueue(from, &datagram[HEADER_LEN..]),
        super::socket::Protocol::Tcp(_) => return,
    }
    crate::sched::io_ready();
}
