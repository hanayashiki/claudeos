//! TCP.
//!
//! What is here is the passive open path and the data transfer that follows
//! it: a listening socket answers a connection request, completes the
//! handshake, hands the connection to accept, delivers in-order data,
//! acknowledges what it takes, retransmits what is not acknowledged, and
//! closes in order with the wait state at the end. An active open is here too.
//!
//! What is not here, all of which only matters on a link that loses or
//! reorders packets:
//!
//!   * No reassembly queue. A segment that arrives out of order is dropped and
//!     the acknowledgement repeats what is still wanted, so the sender has to
//!     send everything from that point again.
//!   * Loss recovery is the retransmission timer alone. There is no duplicate
//!     acknowledgement count, no fast retransmit, no fast recovery, and no
//!     selective acknowledgement, so one lost segment costs a whole timeout
//!     and a go-back-N retransmission.
//!   * The retransmission timeout is a fixed starting value doubled on each
//!     loss. Round trip time is never measured, so there is no Karn or
//!     Jacobson estimator behind it.
//!   * Congestion control is slow start and the textbook congestion avoidance
//!     increment, reset to one segment on every timeout. Without duplicate
//!     acknowledgements there is nothing else it could react to.
//!   * No Nagle, no delayed acknowledgement, no window scaling, no timestamps
//!     and so no protection against wrapped sequence numbers.
//!
//! On a quiet link -- which is what QEMU's user mode network is -- none of
//! that shows. On a lossy one it would be slow rather than wrong.

use super::ip::{self, Ipv4Addr};
use super::socket::{self, Endpoint, InetSocket, Protocol};
use crate::abi::Errno;
use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec::Vec;

pub const FIN: u8 = 0x01;
pub const SYN: u8 = 0x02;
pub const RST: u8 = 0x04;
pub const PSH: u8 = 0x08;
pub const ACK: u8 = 0x10;
pub const URG: u8 = 0x20;

pub const HEADER_LEN: usize = 20;
/// The largest payload one segment carries: a 1500 byte link less the IPv4
/// and TCP headers.
pub const MSS: usize = 1460;
/// The smallest maximum segment size a peer is allowed to insist on.
const MIN_MSS: usize = 536;
/// Bytes a connection will hold in each direction.
pub const RECEIVE_WINDOW: usize = 32 * 1024;
pub const SEND_BUFFER: usize = 32 * 1024;

/// Timer values, in timer ticks. The tick is ten milliseconds.
const INITIAL_RTO: u64 = 50;
const MAX_RTO: u64 = 600;
const MAX_RETRIES: u32 = 8;
const SYN_RETRIES: u32 = 5;
/// Twice the maximum segment lifetime. A real stack waits sixty seconds; this
/// waits ten, because nothing here runs long enough for a segment from an old
/// connection to turn up, and a shorter wait keeps the socket table small.
const TIME_WAIT_TICKS: u64 = 1000;
/// How long a connection whose descriptor is gone waits in FIN-WAIT-2 for the
/// other end to finish. Linux bounds the same wait with tcp_fin_timeout, which
/// is sixty seconds; without a bound a peer that acknowledges the finish and
/// then says nothing keeps the record, and everything it holds, until reboot.
const FIN_WAIT_2_TICKS: u64 = 6000;

#[inline]
pub fn seq_lt(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) < 0
}

#[inline]
pub fn seq_le(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) <= 0
}

#[inline]
pub fn seq_gt(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) > 0
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum State {
    Closed,
    Listen,
    SynSent,
    SynReceived,
    Established,
    FinWait1,
    FinWait2,
    CloseWait,
    Closing,
    LastAck,
    TimeWait,
}

impl State {
    /// True once both ends have agreed sequence numbers, so data may flow and
    /// a name may be reported for the far end.
    pub fn is_synchronised(self) -> bool {
        matches!(
            self,
            State::Established
                | State::FinWait1
                | State::FinWait2
                | State::CloseWait
                | State::Closing
                | State::LastAck
                | State::TimeWait
        )
    }

    fn can_output(self) -> bool {
        matches!(
            self,
            State::Established
                | State::CloseWait
                | State::FinWait1
                | State::Closing
                | State::LastAck
        )
    }

    pub fn name(self) -> &'static str {
        match self {
            State::Closed => "CLOSED",
            State::Listen => "LISTEN",
            State::SynSent => "SYN-SENT",
            State::SynReceived => "SYN-RECEIVED",
            State::Established => "ESTABLISHED",
            State::FinWait1 => "FIN-WAIT-1",
            State::FinWait2 => "FIN-WAIT-2",
            State::CloseWait => "CLOSE-WAIT",
            State::Closing => "CLOSING",
            State::LastAck => "LAST-ACK",
            State::TimeWait => "TIME-WAIT",
        }
    }
}

// ---- segments -------------------------------------------------------------

pub struct Segment<'a> {
    pub source_port: u16,
    pub destination_port: u16,
    pub sequence: u32,
    pub acknowledgement: u32,
    pub flags: u8,
    pub window: u16,
    pub mss: Option<u16>,
    pub payload: &'a [u8],
}

impl<'a> Segment<'a> {
    pub fn parse(bytes: &'a [u8]) -> Option<Segment<'a>> {
        if bytes.len() < HEADER_LEN {
            return None;
        }
        let offset = (bytes[12] >> 4) as usize * 4;
        if offset < HEADER_LEN || bytes.len() < offset {
            return None;
        }
        let mut mss = None;
        let mut i = HEADER_LEN;
        while i < offset {
            match bytes[i] {
                0 => break,     // end of option list
                1 => i += 1,    // one byte of padding
                kind => {
                    if i + 1 >= offset {
                        break;
                    }
                    let length = bytes[i + 1] as usize;
                    if length < 2 || i + length > offset {
                        break;
                    }
                    if kind == 2 && length == 4 {
                        mss = Some(u16::from_be_bytes([bytes[i + 2], bytes[i + 3]]));
                    }
                    i += length;
                }
            }
        }
        Some(Segment {
            source_port: u16::from_be_bytes([bytes[0], bytes[1]]),
            destination_port: u16::from_be_bytes([bytes[2], bytes[3]]),
            sequence: u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
            acknowledgement: u32::from_be_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]),
            flags: bytes[13],
            window: u16::from_be_bytes([bytes[14], bytes[15]]),
            mss,
            payload: &bytes[offset..],
        })
    }

    /// How many sequence numbers this segment consumes.
    pub fn length(&self) -> u32 {
        self.payload.len() as u32
            + (self.flags & SYN != 0) as u32
            + (self.flags & FIN != 0) as u32
    }
}

/// Build one segment, checksum included.
#[allow(clippy::too_many_arguments)]
pub fn build(
    source: Endpoint,
    destination: Endpoint,
    sequence: u32,
    acknowledgement: u32,
    flags: u8,
    window: u16,
    mss: Option<u16>,
    payload: &[u8],
) -> Vec<u8> {
    let options = if mss.is_some() { 4 } else { 0 };
    let offset = HEADER_LEN + options;
    let mut segment = Vec::with_capacity(offset + payload.len());
    segment.extend_from_slice(&source.port.to_be_bytes());
    segment.extend_from_slice(&destination.port.to_be_bytes());
    segment.extend_from_slice(&sequence.to_be_bytes());
    segment.extend_from_slice(&acknowledgement.to_be_bytes());
    segment.push(((offset / 4) as u8) << 4);
    segment.push(flags);
    segment.extend_from_slice(&window.to_be_bytes());
    segment.extend_from_slice(&[0, 0]); // checksum
    segment.extend_from_slice(&[0, 0]); // urgent pointer
    if let Some(mss) = mss {
        segment.push(2);
        segment.push(4);
        segment.extend_from_slice(&mss.to_be_bytes());
    }
    segment.extend_from_slice(payload);
    let pseudo = ip::pseudo_sum(
        source.address,
        destination.address,
        ip::PROTO_TCP,
        segment.len(),
    );
    let checksum = ip::fold(ip::sum(&segment, pseudo));
    segment[16..18].copy_from_slice(&checksum.to_be_bytes());
    segment
}

/// A starting sequence number that differs between connections and advances
/// with the clock, so a segment left over from an earlier connection between
/// the same two ports falls outside the new one's window.
fn initial_sequence(local: Endpoint, remote: Endpoint) -> u32 {
    let clock = (crate::time::monotonic_ns() / 4000) as u32;
    let salt = ((local.port as u32) << 16)
        ^ (remote.port as u32)
        ^ remote.address.0.rotate_left(13);
    clock.wrapping_add(salt.wrapping_mul(0x9E37_79B9))
}

// ---- the transmission control block --------------------------------------

pub struct Tcb {
    pub state: State,
    pub local: Endpoint,
    pub remote: Endpoint,
    pub bound: bool,

    pub iss: u32,
    pub send_unacknowledged: u32,
    pub send_next: u32,
    pub send_window: u32,
    window_sequence: u32,
    window_ack: u32,

    pub irs: u32,
    pub receive_next: u32,

    /// Everything from `send_unacknowledged` onwards: what has been sent and
    /// not acknowledged, followed by what has not been sent.
    pub pending: VecDeque<u8>,
    pub received: VecDeque<u8>,

    /// The user has finished writing; a FIN follows the queued data.
    pub fin_queued: bool,
    /// The sequence number our FIN occupies, once it has been sent.
    pub fin_sequence: Option<u32>,
    pub fin_acknowledged: bool,
    pub fin_received: bool,
    pub read_shutdown: bool,
    /// No descriptor names this connection any more. Nothing can read what
    /// arrives and nothing can close it a second time, so the states that
    /// wait on the other end have to give up by themselves.
    pub abandoned: bool,
    pub error: Option<Errno>,

    peer_mss: usize,

    /// Tick at which the unacknowledged data is sent again. Zero is off.
    pub retransmit_at: u64,
    rto: u64,
    retries: u32,
    /// Tick at which a state that waits on a clock rather than on the other
    /// end gives up: TIME-WAIT's two segment lifetimes, and the bound on
    /// FIN-WAIT-2. Zero is off.
    expire_at: u64,

    congestion_window: u32,
    slow_start_threshold: u32,

    pub backlog: usize,
    pub children: Vec<Arc<InetSocket>>,
}

impl Tcb {
    pub fn new() -> Tcb {
        Tcb {
            state: State::Closed,
            local: Endpoint::UNSPECIFIED,
            remote: Endpoint::UNSPECIFIED,
            bound: false,
            iss: 0,
            send_unacknowledged: 0,
            send_next: 0,
            send_window: MSS as u32,
            window_sequence: 0,
            window_ack: 0,
            irs: 0,
            receive_next: 0,
            pending: VecDeque::new(),
            received: VecDeque::new(),
            fin_queued: false,
            fin_sequence: None,
            fin_acknowledged: false,
            fin_received: false,
            read_shutdown: false,
            abandoned: false,
            error: None,
            peer_mss: MSS,
            retransmit_at: 0,
            rto: INITIAL_RTO,
            retries: 0,
            expire_at: 0,
            congestion_window: MSS as u32,
            slow_start_threshold: 64 * 1024,
            backlog: 1,
            children: Vec::new(),
        }
    }

    fn effective_mss(&self) -> usize {
        self.peer_mss.min(MSS)
    }

    /// The address to put in the source field. A socket bound to every
    /// address still has to name one on the wire.
    fn source_address(&self) -> Ipv4Addr {
        if self.local.address.is_unspecified() {
            super::source_for(self.remote.address)
        } else {
            self.local.address
        }
    }

    /// How much more this end is prepared to receive.
    fn advertised_window(&self) -> u16 {
        RECEIVE_WINDOW
            .saturating_sub(self.received.len())
            .min(u16::MAX as usize) as u16
    }

    fn send_segment(&mut self, flags: u8, sequence: u32, payload: &[u8], with_mss: bool) {
        let source = Endpoint::new(self.source_address(), self.local.port);
        let segment = build(
            source,
            self.remote,
            sequence,
            if flags & ACK != 0 { self.receive_next } else { 0 },
            flags,
            self.advertised_window(),
            if with_mss { Some(MSS as u16) } else { None },
            payload,
        );
        let _ = ip::send_from(
            source.address,
            self.remote.address,
            ip::PROTO_TCP,
            &segment,
        );
    }

    fn acknowledge(&mut self) {
        self.send_segment(ACK, self.send_next, &[], false);
    }

    fn send_syn_ack(&mut self) {
        self.send_segment(SYN | ACK, self.iss, &[], true);
        self.arm_retransmit();
    }

    fn arm_retransmit(&mut self) {
        if self.retransmit_at == 0 {
            self.retransmit_at = crate::trap::ticks() + self.rto;
        }
    }

    // ---- opening ---------------------------------------------------------

    /// Start an active open: send the first connection request.
    pub fn open(&mut self, _passive: bool) {
        self.iss = initial_sequence(self.local, self.remote);
        self.send_unacknowledged = self.iss;
        self.send_next = self.iss;
        self.state = State::SynSent;
        self.congestion_window = MSS as u32;
        self.rto = INITIAL_RTO;
        self.retries = 0;
        self.send_segment(SYN, self.iss, &[], true);
        self.send_next = self.iss.wrapping_add(1);
        self.arm_retransmit();
    }

    /// Take the state a connection request establishes. The answering segment
    /// is sent separately, once the socket is in the table.
    fn accept_open(&mut self, segment: &Segment) {
        self.irs = segment.sequence;
        self.receive_next = segment.sequence.wrapping_add(1);
        if let Some(mss) = segment.mss {
            self.peer_mss = (mss as usize).clamp(MIN_MSS, MSS);
        }
        self.send_window = segment.window as u32;
        self.window_sequence = segment.sequence;
        self.iss = initial_sequence(self.local, self.remote);
        self.send_unacknowledged = self.iss;
        self.send_next = self.iss.wrapping_add(1);
        self.window_ack = self.iss;
        self.state = State::SynReceived;
        self.congestion_window = MSS as u32;
        self.rto = INITIAL_RTO;
    }

    // ---- user operations -------------------------------------------------

    pub fn write(&mut self, buf: &[u8]) -> Result<usize, Errno> {
        match self.state {
            State::Established | State::CloseWait => {}
            State::SynSent | State::SynReceived => return Err(Errno::EAGAIN),
            State::Closed => {
                return Err(self.error.take().unwrap_or(Errno::ENOTCONN));
            }
            State::Listen => return Err(Errno::ENOTCONN),
            _ => return Err(Errno::EPIPE),
        }
        if self.fin_queued {
            return Err(Errno::EPIPE);
        }
        let room = SEND_BUFFER.saturating_sub(self.pending.len());
        if room == 0 {
            return Err(Errno::EAGAIN);
        }
        let n = room.min(buf.len());
        self.pending.extend(buf[..n].iter().copied());
        self.output();
        Ok(n)
    }

    pub fn read(&mut self, buf: &mut [u8], peek: bool) -> Result<usize, Errno> {
        if !self.received.is_empty() {
            let free_before = RECEIVE_WINDOW.saturating_sub(self.received.len());
            let n = buf.len().min(self.received.len());
            for (i, slot) in buf[..n].iter_mut().enumerate() {
                *slot = self.received[i];
            }
            if !peek {
                self.received.drain(..n);
                // A window that was closed and is now open again has to be
                // advertised, or the other end waits for a segment that only
                // this acknowledgement can carry.
                let free_after = RECEIVE_WINDOW.saturating_sub(self.received.len());
                if free_before < self.effective_mss() && free_after >= self.effective_mss() {
                    if self.state.is_synchronised() {
                        self.acknowledge();
                    }
                }
            }
            return Ok(n);
        }
        if buf.is_empty() {
            return Ok(0);
        }
        if self.fin_received || self.read_shutdown {
            return Ok(0);
        }
        match self.state {
            State::Closed => match self.error.take() {
                Some(err) => Err(err),
                None => Ok(0),
            },
            State::Listen => Err(Errno::ENOTCONN),
            _ => Err(Errno::EAGAIN),
        }
    }

    /// Stop writing: a FIN goes out behind whatever is still queued.
    pub fn shutdown_write(&mut self) {
        if self.fin_queued {
            return;
        }
        match self.state {
            State::Established | State::SynReceived => {
                self.fin_queued = true;
                self.state = State::FinWait1;
            }
            State::CloseWait => {
                self.fin_queued = true;
                self.state = State::LastAck;
            }
            State::SynSent | State::Listen | State::Closed => {
                self.state = State::Closed;
                self.retransmit_at = 0;
                return;
            }
            _ => return,
        }
        self.output();
    }

    /// Tear the connection down now and tell the other end so.
    pub fn abort(&mut self) {
        if self.state.is_synchronised() || self.state == State::SynReceived {
            let sequence = self.send_next;
            self.send_segment(RST | ACK, sequence, &[], false);
        }
        self.state = State::Closed;
        self.retransmit_at = 0;
        self.pending.clear();
        self.fin_queued = false;
    }

    // ---- output ----------------------------------------------------------

    /// Send whatever the windows allow.
    pub fn output(&mut self) {
        if !self.state.can_output() {
            return;
        }
        let usable = self.send_window.min(self.congestion_window);
        let limit = self.send_unacknowledged.wrapping_add(usable);
        loop {
            let offset = self.send_next.wrapping_sub(self.send_unacknowledged) as usize;
            let available = self.pending.len().saturating_sub(offset);
            if available > 0 {
                if !seq_lt(self.send_next, limit) {
                    break;
                }
                let room = limit.wrapping_sub(self.send_next) as usize;
                let n = available.min(self.effective_mss()).min(room);
                if n == 0 {
                    break;
                }
                let chunk: Vec<u8> =
                    self.pending.iter().skip(offset).take(n).copied().collect();
                let sequence = self.send_next;
                let last = offset + n == self.pending.len();
                let flags = ACK | if last { PSH } else { 0 };
                self.send_segment(flags, sequence, &chunk, false);
                self.send_next = sequence.wrapping_add(n as u32);
                self.arm_retransmit();
            } else if self.fin_queued && self.fin_sequence.is_none() {
                let sequence = self.send_next;
                self.send_segment(FIN | ACK, sequence, &[], false);
                self.fin_sequence = Some(sequence);
                self.send_next = sequence.wrapping_add(1);
                self.arm_retransmit();
                break;
            } else {
                break;
            }
        }
    }

    // ---- input -----------------------------------------------------------

    /// RFC 793 section 3.9's first test: does this segment fall anywhere in
    /// the window this end is waiting for? A segment that does not belongs to
    /// some other conversation, or to nobody at all, and nothing in it may be
    /// acted on -- not the flags, not the acknowledgement, not the data.
    fn acceptable(&self, segment: &Segment) -> bool {
        let window = self.advertised_window() as u32;
        let length = segment.length();
        let first = segment.sequence;
        if length == 0 {
            if window == 0 {
                return first == self.receive_next;
            }
            return seq_le(self.receive_next, first)
                && seq_lt(first, self.receive_next.wrapping_add(window));
        }
        if window == 0 {
            return false;
        }
        let last = first.wrapping_add(length - 1);
        let right = self.receive_next.wrapping_add(window);
        (seq_le(self.receive_next, first) && seq_lt(first, right))
            || (seq_le(self.receive_next, last) && seq_lt(last, right))
    }

    /// What to do with a segment that fell outside the window. Nothing in it
    /// is believed; the answer says where this end actually is, which is what
    /// a peer that is genuinely out of step needs and what someone guessing
    /// at the connection cannot see.
    fn on_unacceptable(&mut self, segment: &Segment) -> bool {
        if segment.flags & RST != 0 {
            // RFC 5961 section 3: a reset this far out is not answered at
            // all. An answer would tell whoever guessed how close they came.
            return false;
        }
        if self.state == State::SynReceived
            && segment.flags & SYN != 0
            && segment.flags & ACK == 0
            && segment.sequence == self.irs
        {
            // The connection request again, because our answer was lost. It
            // sits one before what is wanted next, so the window test turns
            // it away, and it is still the handshake carrying on.
            self.send_syn_ack();
            return false;
        }
        self.acknowledge();
        false
    }

    /// Returns true when something a waiter cares about changed.
    pub fn on_segment(&mut self, segment: &Segment) -> bool {
        match self.state {
            State::SynSent => return self.on_syn_sent(segment),
            State::Closed | State::Listen => return false,
            _ => {}
        }

        if !self.acceptable(segment) {
            return self.on_unacceptable(segment);
        }

        if segment.flags & RST != 0 {
            if segment.sequence != self.receive_next {
                // RFC 5961 section 3: inside the window, but not the byte
                // that is wanted next. Say where this end is and wait for a
                // reset that names it, which leaves a blind sender one number
                // to find rather than a whole window of them.
                self.acknowledge();
                return false;
            }
            self.state = State::Closed;
            self.retransmit_at = 0;
            self.pending.clear();
            if self.error.is_none() {
                self.error = Some(Errno::ECONNRESET);
            }
            return true;
        }

        if segment.flags & SYN != 0 {
            // RFC 5961 section 4: a connection request inside an open
            // connection draws the same acknowledgement rather than a reset.
            // A peer that really has restarted answers that with a reset of
            // its own, which does carry the sequence number this end named.
            self.acknowledge();
            return false;
        }

        if segment.flags & ACK == 0 {
            return false;
        }

        let mut woke = false;
        let acknowledgement = segment.acknowledgement;

        if self.state == State::SynReceived {
            if seq_lt(self.send_unacknowledged, acknowledgement)
                && seq_le(acknowledgement, self.send_next)
            {
                self.state = State::Established;
                self.retransmit_at = 0;
                self.retries = 0;
                woke = true;
            } else {
                // An acknowledgement for something never sent.
                self.send_segment(RST, acknowledgement, &[], false);
                return false;
            }
        }

        if seq_gt(acknowledgement, self.send_unacknowledged)
            && seq_le(acknowledgement, self.send_next)
        {
            let mut acked = acknowledgement.wrapping_sub(self.send_unacknowledged) as usize;
            if let Some(fin_sequence) = self.fin_sequence {
                if seq_lt(fin_sequence, acknowledgement) && !self.fin_acknowledged {
                    self.fin_acknowledged = true;
                    acked = acked.saturating_sub(1);
                }
            }
            let drop_count = acked.min(self.pending.len());
            self.pending.drain(..drop_count);
            self.send_unacknowledged = acknowledgement;
            self.retries = 0;
            self.rto = INITIAL_RTO;
            if self.congestion_window < self.slow_start_threshold {
                self.congestion_window = self.congestion_window.saturating_add(MSS as u32);
            } else {
                let increment = (MSS * MSS) as u32 / self.congestion_window.max(1);
                self.congestion_window = self.congestion_window.saturating_add(increment.max(1));
            }
            self.retransmit_at = if self.send_unacknowledged == self.send_next {
                0
            } else {
                crate::trap::ticks() + self.rto
            };
            woke = true;
        }

        // The send window, taken from the most recent segment that carries a
        // newer view of it than the one already held.
        if seq_lt(self.window_sequence, segment.sequence)
            || (self.window_sequence == segment.sequence
                && seq_le(self.window_ack, acknowledgement))
        {
            self.send_window = segment.window as u32;
            self.window_sequence = segment.sequence;
            self.window_ack = acknowledgement;
        }

        // Our own FIN being acknowledged moves the close along.
        if self.fin_acknowledged {
            match self.state {
                State::FinWait1 => {
                    self.enter_fin_wait_2();
                }
                State::Closing => {
                    self.enter_time_wait();
                    woke = true;
                }
                State::LastAck => {
                    self.state = State::Closed;
                    self.retransmit_at = 0;
                    woke = true;
                }
                _ => {}
            }
        }

        // Data. Anything already delivered is trimmed off the front; anything
        // beyond what is wanted next is dropped, because there is no queue to
        // hold it in.
        let mut payload: &[u8] = segment.payload;
        let mut sequence = segment.sequence;
        if seq_lt(sequence, self.receive_next) {
            let skip = self.receive_next.wrapping_sub(sequence) as usize;
            payload = if skip >= payload.len() { &[] } else { &payload[skip..] };
            sequence = self.receive_next;
        }
        let in_order = sequence == self.receive_next;
        let mut need_ack = false;
        if !segment.payload.is_empty() {
            need_ack = true;
            if in_order && !payload.is_empty() {
                if !self.can_receive() {
                    // Nobody will ever read this. Taking it off the sequence
                    // space anyway is what keeps the other end from sending
                    // it again forever.
                    self.receive_next =
                        self.receive_next.wrapping_add(payload.len() as u32);
                } else {
                    let free = RECEIVE_WINDOW.saturating_sub(self.received.len());
                    let n = payload.len().min(free);
                    self.received.extend(payload[..n].iter().copied());
                    self.receive_next = self.receive_next.wrapping_add(n as u32);
                    woke = true;
                }
            }
        }

        // The FIN sits one past the segment's data, and is only taken once
        // every byte before it has been.
        if segment.flags & FIN != 0 {
            let fin_sequence = sequence.wrapping_add(payload.len() as u32);
            if self.receive_next == fin_sequence {
                need_ack = true;
                if !self.fin_received {
                    self.fin_received = true;
                    self.receive_next = self.receive_next.wrapping_add(1);
                    woke = true;
                    match self.state {
                        State::Established => self.state = State::CloseWait,
                        State::FinWait1 => self.state = State::Closing,
                        State::FinWait2 => self.enter_time_wait(),
                        _ => {}
                    }
                }
            } else if self.fin_received && self.receive_next == fin_sequence.wrapping_add(1) {
                need_ack = true;
            }
        }

        if need_ack && self.state != State::Closed {
            self.acknowledge();
        }
        self.output();
        woke
    }

    fn on_syn_sent(&mut self, segment: &Segment) -> bool {
        if segment.flags & ACK != 0
            && !(seq_lt(self.iss, segment.acknowledgement)
                && seq_le(segment.acknowledgement, self.send_next))
        {
            if segment.flags & RST == 0 {
                self.send_segment(RST, segment.acknowledgement, &[], false);
            }
            return false;
        }
        if segment.flags & RST != 0 {
            self.state = State::Closed;
            self.retransmit_at = 0;
            self.error = Some(Errno::ECONNREFUSED);
            return true;
        }
        if segment.flags & SYN == 0 {
            return false;
        }
        self.irs = segment.sequence;
        self.receive_next = segment.sequence.wrapping_add(1);
        if let Some(mss) = segment.mss {
            self.peer_mss = (mss as usize).clamp(MIN_MSS, MSS);
        }
        self.send_window = segment.window as u32;
        self.window_sequence = segment.sequence;
        if segment.flags & ACK != 0 {
            self.send_unacknowledged = segment.acknowledgement;
            self.window_ack = segment.acknowledgement;
            self.state = State::Established;
            self.retransmit_at = 0;
            self.retries = 0;
            self.acknowledge();
            self.output();
            return true;
        }
        // Both ends opened at once.
        self.state = State::SynReceived;
        self.send_syn_ack();
        false
    }

    fn enter_time_wait(&mut self) {
        self.state = State::TimeWait;
        self.expire_at = crate::trap::ticks() + TIME_WAIT_TICKS;
        self.retransmit_at = 0;
        self.pending.clear();
        self.read_shutdown = true;
        if self.abandoned {
            // Both ends have finished and no descriptor is left, so what
            // arrived and was never read is held for nobody.
            self.received.clear();
        }
    }

    fn enter_fin_wait_2(&mut self) {
        self.state = State::FinWait2;
        if self.abandoned {
            self.expire_at = crate::trap::ticks() + FIN_WAIT_2_TICKS;
        }
    }

    /// The last descriptor naming this connection has gone.
    pub fn abandon(&mut self) {
        self.abandoned = true;
        if self.state == State::FinWait2 && self.expire_at == 0 {
            self.expire_at = crate::trap::ticks() + FIN_WAIT_2_TICKS;
        }
    }

    /// Whether anything arriving now could still be read. A connection with
    /// no descriptor has nobody to read it, one whose read side is shut has
    /// been told there is nothing more, and in any other state the other end
    /// has already finished sending.
    fn can_receive(&self) -> bool {
        !self.read_shutdown
            && !self.abandoned
            && matches!(
                self.state,
                State::Established | State::FinWait1 | State::FinWait2
            )
    }

    // ---- timers ----------------------------------------------------------

    pub fn next_deadline(&self) -> u64 {
        let mut earliest = u64::MAX;
        if self.expire_at != 0 {
            earliest = self.expire_at;
        }
        if self.retransmit_at != 0 {
            earliest = earliest.min(self.retransmit_at);
        }
        earliest
    }

    pub fn on_timer(&mut self, now: u64) -> bool {
        if self.expire_at != 0 {
            // Waiting on a clock rather than on the other end: there is
            // nothing to send, and reaching the deadline is the end of it.
            if now >= self.expire_at {
                self.state = State::Closed;
                self.expire_at = 0;
                self.retransmit_at = 0;
                self.pending.clear();
                return true;
            }
            return false;
        }
        if self.retransmit_at == 0 || now < self.retransmit_at {
            return false;
        }
        self.retries += 1;
        let limit = match self.state {
            State::SynSent | State::SynReceived => SYN_RETRIES,
            _ => MAX_RETRIES,
        };
        if self.retries > limit {
            let reason = if self.state == State::SynSent {
                Errno::ETIMEDOUT
            } else {
                Errno::ETIMEDOUT
            };
            self.abort();
            self.error = Some(reason);
            return true;
        }
        self.rto = (self.rto * 2).min(MAX_RTO);
        self.retransmit_at = now + self.rto;
        let flight = self.send_next.wrapping_sub(self.send_unacknowledged);
        self.slow_start_threshold = (flight / 2).max(2 * MSS as u32);
        self.congestion_window = MSS as u32;

        match self.state {
            State::SynSent => self.send_segment(SYN, self.iss, &[], true),
            State::SynReceived => self.send_segment(SYN | ACK, self.iss, &[], true),
            _ => {
                // Go back to the first byte the other end has not confirmed.
                self.send_next = self.send_unacknowledged;
                self.fin_sequence = None;
                if self.send_window == 0 && !self.pending.is_empty() {
                    // No room at the other end. One byte keeps the exchange
                    // going until a window update arrives.
                    let byte = [self.pending[0]];
                    let sequence = self.send_next;
                    self.send_segment(ACK, sequence, &byte, false);
                    self.send_next = sequence.wrapping_add(1);
                } else {
                    self.output();
                }
            }
        }
        false
    }
}

// ---- demultiplexing -------------------------------------------------------

/// Answer a segment that belongs to no socket. RFC 793's rule: a segment with
/// an acknowledgement is answered from that number, one without is answered
/// with an acknowledgement of everything it carried.
fn send_reset(local: Endpoint, remote: Endpoint, segment: &Segment) {
    let (sequence, acknowledgement, flags) = if segment.flags & ACK != 0 {
        (segment.acknowledgement, 0, RST)
    } else {
        (0, segment.sequence.wrapping_add(segment.length()), RST | ACK)
    };
    let source = Endpoint::new(
        if local.address.is_unspecified() {
            super::source_for(remote.address)
        } else {
            local.address
        },
        local.port,
    );
    let reply = build(source, remote, sequence, acknowledgement, flags, 0, None, &[]);
    let _ = ip::send_from(source.address, remote.address, ip::PROTO_TCP, &reply);
}

/// A connection request for a listening socket: make the child that will
/// become the accepted connection and answer.
fn accept_connection(
    listener: &Arc<InetSocket>,
    local: Endpoint,
    remote: Endpoint,
    segment: &Segment,
) {
    let room = match &*listener.inner.lock() {
        Protocol::Tcp(tcb) => tcb.children.len() < tcb.backlog,
        Protocol::Udp(_) => false,
    };
    if !room {
        // The queue is full. Dropping the request rather than refusing it
        // lets the other end try again when the server has caught up.
        return;
    }

    let child = InetSocket::new(true);
    match &mut *child.inner.lock() {
        Protocol::Tcp(tcb) => {
            tcb.local = local;
            tcb.remote = remote;
            tcb.bound = true;
            tcb.accept_open(segment);
        }
        Protocol::Udp(_) => return,
    }
    // In the table before the answer goes out, so the acknowledgement that
    // completes the handshake finds it.
    socket::register(&child);
    match &mut *listener.inner.lock() {
        Protocol::Tcp(tcb) => tcb.children.push(child.clone()),
        Protocol::Udp(_) => {}
    }
    match &mut *child.inner.lock() {
        Protocol::Tcp(tcb) => tcb.send_syn_ack(),
        Protocol::Udp(_) => {}
    };
}

pub fn receive(source: Ipv4Addr, destination: Ipv4Addr, bytes: &[u8]) {
    let Some(segment) = Segment::parse(bytes) else { return };
    let pseudo = ip::pseudo_sum(source, destination, ip::PROTO_TCP, bytes.len());
    if ip::fold(ip::sum(bytes, pseudo)) != 0 {
        return;
    }
    let local = Endpoint::new(destination, segment.destination_port);
    let remote = Endpoint::new(source, segment.source_port);

    if let Some(socket) = socket::lookup_stream(local, remote) {
        let woke = match &mut *socket.inner.lock() {
            Protocol::Tcp(tcb) => tcb.on_segment(&segment),
            Protocol::Udp(_) => false,
        };
        if woke {
            crate::sched::io_ready();
        }
        return;
    }

    if segment.flags & SYN != 0 && segment.flags & (ACK | RST) == 0 {
        if let Some(listener) = socket::lookup_listener(local) {
            accept_connection(&listener, local, remote, &segment);
            return;
        }
    }
    if segment.flags & RST == 0 {
        send_reset(local, remote, &segment);
    }
}

// ---- timers across every connection --------------------------------------

pub fn next_deadline() -> u64 {
    // A machine whose network nobody is using pays one lock for this, and no
    // copy of the table, on every tick.
    if socket::is_empty() {
        return u64::MAX;
    }
    let mut earliest = u64::MAX;
    for socket in socket::all() {
        if !socket.stream {
            continue;
        }
        if let Protocol::Tcp(tcb) = &*socket.inner.lock() {
            earliest = earliest.min(tcb.next_deadline());
        }
    }
    earliest
}

pub fn on_tick() {
    if socket::is_empty() {
        return;
    }
    let now = crate::trap::ticks();
    let mut woke = false;
    let mut finished: Vec<Arc<InetSocket>> = Vec::new();
    for socket in socket::all() {
        if !socket.stream {
            continue;
        }
        let done = {
            let mut inner = socket.inner.lock();
            match &mut *inner {
                Protocol::Tcp(tcb) => {
                    if tcb.on_timer(now) {
                        woke = true;
                    }
                    tcb.state == State::Closed
                }
                Protocol::Udp(_) => false,
            }
        };
        // A socket whose descriptor is gone and whose protocol has nothing
        // left to do leaves the table.
        if done && socket.is_detached() {
            finished.push(socket);
        }
    }
    for socket in finished {
        socket::unregister(&socket);
    }
    if woke {
        crate::sched::io_ready();
    }
}
