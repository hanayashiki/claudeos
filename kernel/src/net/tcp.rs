//! TCP.
//!
//! What is here is the passive open path and the data transfer that follows
//! it: a listening socket answers a connection request, completes the
//! handshake, hands the connection to accept, delivers in-order data,
//! acknowledges what it takes, retransmits what is not acknowledged, and
//! closes in order with the wait state at the end. An active open is here too.
//!
//! A segment that arrives before the bytes in front of it is held until they
//! come, so one packet taking a different route costs nothing. What is held
//! is bounded three ways: by the receive window, which every held byte lies
//! inside; by a count of separate runs, so a gap in front of every byte is
//! not a gap in front of every byte's allocation; and by a count every
//! connection shares, so a thousand connections each holding one segment hold
//! no more between them than one does at full stretch. A gap that never fills
//! is given up on after thirty seconds and what was held behind it released.
//!
//! The retransmission timeout comes from RFC 6298's estimator: one segment at
//! a time is timed from the moment it goes out to the acknowledgement that
//! covers it, and the smoothed average and variation of those samples are
//! what the timeout is built from. Karn's rule decides what may be sampled --
//! a segment sent twice is not, because its acknowledgement does not say
//! which copy it answers -- and a timeout doubles the timeout and leaves it
//! doubled until a segment that went out once is acknowledged.
//!
//! A segment lost out of the middle of a stream is not waited out. Three
//! acknowledgements naming the same byte mean the segments behind that one
//! arrived, so it alone is sent again and the ones behind it are not. RFC
//! 6582's partial acknowledgement handling covers a second loss inside the
//! same window without leaving recovery. The congestion response goes with
//! it: the threshold halves and the window comes down to it, because
//! retransmitting quickly and sending just as much as before is worse on a
//! congested link than doing neither.
//!
//! What is not here:
//!
//!   * No selective acknowledgement. A lost segment is found by the
//!     acknowledgements repeating, one loss per round trip, rather than by
//!     the other end naming what it has; on a link that loses several
//!     segments out of one window that is slower, and it is still correct.
//!   * No window scaling, so the window is bounded at 64 KiB, which bounds
//!     throughput on a link whose delay and bandwidth multiply out past that.
//!   * No timestamps, and so no protection against wrapped sequence numbers,
//!     and one round trip sample in flight at a time rather than one per
//!     segment.
//!   * No Nagle and no delayed acknowledgement: every segment is sent as soon
//!     as there is a window for it and answered as soon as it arrives.

use super::ip::{self, Ipv4Addr};
use super::socket::{self, Endpoint, InetSocket, Protocol};
use crate::abi::Errno;
use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};

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
///
/// RFC 6298 2.1: one second, until a round trip has actually been measured.
const INITIAL_RTO: u64 = 100;
/// RFC 6298 2.4 puts the floor at one second. Linux uses a fifth of that and
/// so does this: what the floor is for is keeping the timeout clear of the
/// clock's own granularity, not waiting out a second on a link whose round
/// trip is measured in microseconds.
const MIN_RTO: u64 = 20;
const MAX_RTO: u64 = 600;
const MAX_RETRIES: u32 = 8;
/// RFC 5681: acknowledgements naming the same byte that mean the segment for
/// it was lost rather than that two arrived in the wrong order. Fewer would
/// fire on ordinary reordering, which costs a retransmission that was never
/// needed; more waits longer than the segments behind the gap take to arrive.
const DUPLICATE_ACK_THRESHOLD: u32 = 3;
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
/// How long a gap in the stream is waited on. The other end retransmits what
/// is missing long before this; what the bound is for is the other end that
/// leaves a gap and then says nothing, which would otherwise hold what came
/// after it until the machine rebooted.
const REASSEMBLY_TICKS: u64 = 3000;

/// Separate runs of bytes one connection will hold past a gap.
///
/// What one connection holds is already bounded by the receive window, since
/// every byte held lies inside it. The number of runs is not: a peer that
/// sends every other byte leaves a gap in front of each one, and sixteen
/// thousand allocations for thirty-two kilobytes is not a trade worth making.
/// Past this the run furthest ahead is dropped, which costs whoever sent it a
/// retransmission.
const MAX_HELD_RUNS: usize = 16;

/// Bytes held past a gap across every connection at once.
///
/// A window's worth each is bounded per connection and unbounded in total:
/// somebody who opens a thousand connections and leaves a gap in each holds
/// as much as they care to. Past this a segment that arrives early is dropped
/// rather than held, which costs its sender a retransmission and costs this
/// machine nothing.
pub const REASSEMBLY_LIMIT: usize = 128 * 1024;
static HELD_BYTES: AtomicUsize = AtomicUsize::new(0);

/// Bytes every connection together is holding past a gap.
pub fn held_bytes() -> usize {
    HELD_BYTES.load(Ordering::Relaxed)
}

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

/// A secret drawn once per boot and mixed into every initial sequence number.
///
/// It comes from the kernel's own generator, which is a xorshift seeded from
/// the cycle counter. That is the best source here, and it is worth being
/// plain about what it buys: someone off the machine cannot work the secret
/// out from the sequence numbers it produces, and it is not a cryptographic
/// hash and is not claimed to be one. Anyone who can read kernel memory, or
/// who can watch this machine's start-up timing closely enough to guess the
/// seed, has the secret and everything that follows from it.
static ISN_SECRET: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

fn isn_secret() -> u64 {
    use core::sync::atomic::Ordering;
    let held = ISN_SECRET.load(Ordering::Relaxed);
    if held != 0 {
        return held;
    }
    // Zero is what says it has not been drawn yet, so it is not a value the
    // secret may take.
    let drawn = crate::fs::dev::random_u64() | 1;
    match ISN_SECRET.compare_exchange(0, drawn, Ordering::Relaxed, Ordering::Relaxed) {
        Ok(_) => drawn,
        Err(other) => other,
    }
}

/// splitmix64's finalizer: every input bit reaches the whole word.
fn mix(mut x: u64) -> u64 {
    x ^= x >> 30;
    x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x ^= x >> 27;
    x = x.wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

/// A starting sequence number that differs between connections and advances
/// with the clock, so a segment left over from an earlier connection between
/// the same two ports falls outside the new one's window.
///
/// RFC 6528: the clock, plus a hash of the connection's own addresses and
/// ports with a secret this machine keeps to itself. The clock is what keeps
/// the old segment out; the secret is what stops one connection's number,
/// which the other end sees, from giving away every other connection's.
fn initial_sequence(local: Endpoint, remote: Endpoint) -> u32 {
    let clock = (crate::time::monotonic_ns() / 4000) as u32;
    let addresses = ((local.address.0 as u64) << 32) | remote.address.0 as u64;
    let ports = ((local.port as u64) << 16) | remote.port as u64;
    let hash = mix(mix(isn_secret() ^ addresses) ^ ports);
    clock.wrapping_add(hash as u32)
}

// ---- the transmission control block --------------------------------------

/// A run of bytes that arrived before the bytes in front of it.
struct Held {
    sequence: u32,
    data: Vec<u8>,
}

impl Held {
    fn end(&self) -> u32 {
        self.sequence.wrapping_add(self.data.len() as u32)
    }
}

pub struct Tcb {
    pub state: State,
    pub local: Endpoint,
    pub remote: Endpoint,
    pub bound: bool,

    pub iss: u32,
    pub send_unacknowledged: u32,
    pub send_next: u32,
    /// The highest sequence number this end has ever sent. `send_next` goes
    /// back to the first unacknowledged byte when the timer retransmits, so
    /// it is not what says whether an acknowledgement names something that
    /// was sent: an acknowledgement for what went out before the timer moved
    /// the pointer back is perfectly ordinary, and rejecting it deadlocks the
    /// connection until it times out altogether.
    pub send_high: u32,
    pub send_window: u32,
    window_sequence: u32,
    window_ack: u32,

    pub irs: u32,
    pub receive_next: u32,
    /// The right edge of the window this end has offered: the first sequence
    /// number it has not promised room for.
    receive_high: u32,
    /// Runs of bytes that arrived before the bytes in front of them, in order
    /// and with the ones that touch joined. Acknowledging only what is
    /// contiguous is what tells the other end which segment to send again;
    /// holding the rest is what keeps that one segment from costing every
    /// segment behind it as well.
    out_of_order: Vec<Held>,
    /// Bytes in `out_of_order`, which is also this connection's share of the
    /// count every connection shares.
    held: usize,
    /// The other end's finish, when it arrived past a gap. It is one sequence
    /// number rather than a run, and it is taken when the gap fills.
    held_fin: Option<u32>,
    /// Tick at which what is held is let go because the gap never filled.
    /// Zero is off.
    reassembly_expire_at: u64,

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
    /// RFC 6298's estimator, in microseconds: the smoothed round trip time
    /// and how much it varies. Zero says nothing has been measured yet, and
    /// the timeout is still the opening guess.
    srtt: u32,
    rttvar: u32,
    /// The segment being timed: the sequence number just past it, and the
    /// clock when it went out.
    ///
    /// Karn's rule is what the `None` is for. An acknowledgement of a segment
    /// that was sent twice does not say which copy it answers, so the time
    /// from either one to it is not a round trip: it is either the real one
    /// or the real one plus a whole timeout, and there is no way to tell.
    /// Every retransmission clears this, so the next sample comes from a
    /// segment that went out once.
    timed: Option<u32>,
    timed_at: u64,
    retries: u32,
    /// Tick at which a state that waits on a clock rather than on the other
    /// end gives up: TIME-WAIT's two segment lifetimes, and the bound on
    /// FIN-WAIT-2. Zero is off.
    expire_at: u64,

    congestion_window: u32,
    slow_start_threshold: u32,
    /// Acknowledgements that named the byte the one before named.
    duplicate_acks: u32,
    /// While recovering from a loss the duplicate acknowledgements found:
    /// the highest sequence number that had been sent when it was found.
    /// RFC 6582's `recover`. Recovery ends when everything up to it has been
    /// acknowledged, and a gap found before then belongs to the same loss
    /// rather than to a new one.
    recover: Option<u32>,
    /// Segments sent again because three acknowledgements said the same
    /// thing, and segments sent again because the clock ran out. Counted so
    /// the suite can say which of the two recovered a loss.
    pub fast_retransmits: u32,
    pub timeouts: u32,

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
            send_high: 0,
            send_window: MSS as u32,
            window_sequence: 0,
            window_ack: 0,
            irs: 0,
            receive_next: 0,
            receive_high: 0,
            out_of_order: Vec::new(),
            held: 0,
            held_fin: None,
            reassembly_expire_at: 0,
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
            srtt: 0,
            rttvar: 0,
            timed: None,
            timed_at: 0,
            retries: 0,
            expire_at: 0,
            congestion_window: MSS as u32,
            slow_start_threshold: 64 * 1024,
            duplicate_acks: 0,
            recover: None,
            fast_retransmits: 0,
            timeouts: 0,
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

    /// How much more this end has said it is prepared to receive: the right
    /// edge it last offered, less what it has taken since.
    fn advertised_window(&self) -> u16 {
        if seq_le(self.receive_high, self.receive_next) {
            return 0;
        }
        self.receive_high
            .wrapping_sub(self.receive_next)
            .min(u16::MAX as u32) as u16
    }

    /// Move the right edge of the window forward.
    ///
    /// It never moves left, and it only moves right by a whole segment or
    /// more, which is RFC 1122 4.2.3.3's rule against offering the other end
    /// room a byte at a time.
    ///
    /// `advanced` says whether the byte wanted next has moved since the last
    /// acknowledgement went out. When it has not -- which is every
    /// acknowledgement repeated while a gap is waiting to be filled -- the
    /// edge is left alone even if there is room to move it, so every one of
    /// those acknowledgements carries the same window. The other end tests
    /// exactly that: an acknowledgement whose window has changed is a window
    /// update rather than a repeat, and does not count towards the three that
    /// make it send the missing segment again. A window that moved with the
    /// reader draining the buffer, in between the repeats, left the other end
    /// waiting out its own timeout for every lost segment.
    ///
    /// A window that has closed is the exception: it has to be able to open
    /// again whether the byte wanted next moved or not, or the other end
    /// waits for room that is already there.
    fn open_window(&mut self, advanced: bool) {
        if seq_lt(self.receive_high, self.receive_next) {
            self.receive_high = self.receive_next;
        }
        let free = RECEIVE_WINDOW.saturating_sub(self.received.len()) as u32;
        let wanted = self.receive_next.wrapping_add(free);
        let step = (MSS as u32).min((RECEIVE_WINDOW / 2) as u32);
        if !seq_gt(wanted, self.receive_high)
            || wanted.wrapping_sub(self.receive_high) < step
        {
            return;
        }
        if advanced || self.advertised_window() < step as u16 {
            self.receive_high = wanted;
        }
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

    /// Note that everything up to `sequence` has now been put on the wire.
    fn sent_through(&mut self, sequence: u32) {
        self.send_next = sequence;
        if seq_lt(self.send_high, sequence) {
            self.send_high = sequence;
        }
    }

    /// The timeout the estimate gives. RFC 6298 2.2: the smoothed average
    /// plus four times its variation, never less than the clock the timer
    /// runs on can measure, and bounded at both ends.
    fn estimated_rto(&self) -> u64 {
        let granularity = 1_000_000 / crate::arch::TICK_HZ as u32;
        let slack = self.rttvar.saturating_mul(4).max(granularity);
        let microseconds = self.srtt.saturating_add(slack) as u64;
        crate::time::ns_to_ticks(microseconds * 1000).clamp(MIN_RTO, MAX_RTO)
    }

    /// One round trip, measured. RFC 6298 2.2 and 2.3: the first sample is
    /// the average outright and half of it the variation; every one after
    /// moves each of them a fraction of the way.
    fn measure(&mut self, microseconds: u32) {
        let sample = microseconds.max(1);
        if self.srtt == 0 {
            self.srtt = sample;
            self.rttvar = sample / 2;
        } else {
            let difference = self.srtt.abs_diff(sample) as u64;
            self.rttvar = ((self.rttvar as u64 * 3 + difference) / 4) as u32;
            self.srtt = ((self.srtt as u64 * 7 + sample as u64) / 8) as u32;
        }
        self.rto = self.estimated_rto();
    }

    /// Time the segment that ends at `sequence`, if nothing is being timed.
    fn time_segment(&mut self, sequence: u32) {
        if self.timed.is_none() {
            self.timed = Some(sequence);
            self.timed_at = crate::time::monotonic_ns();
        }
    }

    /// What the timeout stands at, and what the estimate behind it is. The
    /// suite asks; nothing else does.
    pub fn retransmit_timeout(&self) -> u64 {
        self.rto
    }

    pub fn smoothed_round_trip(&self) -> u32 {
        self.srtt
    }

    pub fn slow_start_threshold(&self) -> u32 {
        self.slow_start_threshold
    }

    fn arm_retransmit(&mut self) {
        if self.retransmit_at == 0 {
            self.retransmit_at = crate::trap::ticks() + self.rto;
        }
    }

    // ---- opening ---------------------------------------------------------

    /// Start an active open: send the first connection request.
    pub fn open(&mut self, _passive: bool) {
        self.receive_high = self.receive_next.wrapping_add(RECEIVE_WINDOW as u32);
        self.iss = initial_sequence(self.local, self.remote);
        self.send_unacknowledged = self.iss;
        self.send_next = self.iss;
        self.send_high = self.iss;
        self.state = State::SynSent;
        self.congestion_window = MSS as u32;
        self.rto = INITIAL_RTO;
        self.retries = 0;
        self.send_segment(SYN, self.iss, &[], true);
        self.sent_through(self.iss.wrapping_add(1));
        self.time_segment(self.iss.wrapping_add(1));
        self.arm_retransmit();
    }

    /// Take the state a connection request establishes. The answering segment
    /// is sent separately, once the socket is in the table.
    fn accept_open(&mut self, segment: &Segment) {
        self.irs = segment.sequence;
        self.receive_next = segment.sequence.wrapping_add(1);
        self.receive_high = self.receive_next.wrapping_add(RECEIVE_WINDOW as u32);
        if let Some(mss) = segment.mss {
            self.peer_mss = (mss as usize).clamp(MIN_MSS, MSS);
        }
        self.send_window = segment.window as u32;
        self.window_sequence = segment.sequence;
        self.iss = initial_sequence(self.local, self.remote);
        self.send_unacknowledged = self.iss;
        self.send_high = self.iss;
        self.sent_through(self.iss.wrapping_add(1));
        self.time_segment(self.iss.wrapping_add(1));
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
            let n = buf.len().min(self.received.len());
            for (i, slot) in buf[..n].iter_mut().enumerate() {
                *slot = self.received[i];
            }
            if !peek {
                self.received.drain(..n);
                // A window that was closed and is now open again has to be
                // advertised, or the other end waits for a segment that only
                // this acknowledgement can carry.
                let offered_before = self.advertised_window();
                self.open_window(false);
                if offered_before < self.advertised_window()
                    && self.state.is_synchronised()
                {
                    self.acknowledge();
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
        self.drop_out_of_order();
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
                let end = sequence.wrapping_add(n as u32);
                // Only a segment going out for the first time is worth
                // timing: Karn again, and the retransmission path comes
                // through here too.
                let first_time = seq_lt(self.send_high, end);
                self.send_segment(flags, sequence, &chunk, false);
                self.sent_through(end);
                if first_time {
                    self.time_segment(end);
                }
                self.arm_retransmit();
            } else if self.fin_queued && self.fin_sequence.is_none() {
                let sequence = self.send_next;
                self.send_segment(FIN | ACK, sequence, &[], false);
                self.fin_sequence = Some(sequence);
                self.sent_through(sequence.wrapping_add(1));
                self.arm_retransmit();
                break;
            } else {
                break;
            }
        }
    }

    /// Send the first segment of what is still unacknowledged again,
    /// without moving the point new data is sent from. This is the one
    /// segment the duplicate acknowledgements say is missing; everything
    /// behind it arrived, so sending that too would be sending it twice.
    fn retransmit_head(&mut self) {
        let n = self.pending.len().min(self.effective_mss());
        if n == 0 {
            // Nothing but a finish is outstanding.
            match self.fin_sequence {
                Some(sequence) if !self.fin_acknowledged => {
                    self.send_segment(FIN | ACK, sequence, &[], false);
                }
                _ => return,
            }
        } else {
            let chunk: Vec<u8> = self.pending.iter().take(n).copied().collect();
            let flags = ACK | if n == self.pending.len() { PSH } else { 0 };
            let sequence = self.send_unacknowledged;
            self.send_segment(flags, sequence, &chunk, false);
        }
        // Karn: nothing sent twice is timed. RFC 6298 5.5: the timer starts
        // over from the retransmission.
        self.timed = None;
        self.retransmit_at = crate::trap::ticks() + self.rto;
    }

    /// RFC 5681: three acknowledgements naming the same byte mean the segment
    /// that byte is in went missing and the segments behind it arrived, which
    /// is enough to send that one again now rather than when the clock says
    /// so.
    ///
    /// The congestion response goes with it rather than being left out.
    /// Duplicate acknowledgements mean segments were dropped somewhere, and a
    /// machine that answers that by retransmitting quickly and sending just
    /// as much as before is worse on a congested link than one that does
    /// neither.
    fn enter_recovery(&mut self) {
        let flight = self.send_high.wrapping_sub(self.send_unacknowledged);
        self.slow_start_threshold = (flight / 2).max(2 * MSS as u32);
        self.recover = Some(self.send_high);
        self.fast_retransmits += 1;
        self.retransmit_head();
        // Three segments have left the network since the missing one, which
        // is what the three added here stand for.
        self.congestion_window = self
            .slow_start_threshold
            .saturating_add(DUPLICATE_ACK_THRESHOLD * MSS as u32);
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
            self.timed = None;
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
            self.drop_out_of_order();
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
        let wanted_before = self.receive_next;
        let acknowledgement = segment.acknowledgement;
        // RFC 5681's duplicate acknowledgement: it carries nothing, changes
        // nothing, names the byte already named, and there is data
        // outstanding for it to be about. Tested before the window this
        // segment carries is taken, because an unchanged window is one of the
        // things that makes it a duplicate.
        let duplicate = segment.payload.is_empty()
            && segment.flags & (SYN | FIN) == 0
            && acknowledgement == self.send_unacknowledged
            && seq_lt(self.send_unacknowledged, self.send_high)
            && segment.window as u32 == self.send_window;

        if self.state == State::SynReceived {
            if seq_lt(self.send_unacknowledged, acknowledgement)
                && seq_le(acknowledgement, self.send_high)
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
            && seq_le(acknowledgement, self.send_high)
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
            if let Some(timed) = self.timed {
                if seq_le(timed, acknowledgement) {
                    let elapsed =
                        crate::time::monotonic_ns().saturating_sub(self.timed_at);
                    self.timed = None;
                    self.measure((elapsed / 1000).min(u32::MAX as u64) as u32);
                }
            }
            if seq_lt(self.send_next, self.send_unacknowledged) {
                // The timer moved the pointer back over bytes that turned out
                // to have arrived. There is nothing there left to send again.
                self.send_next = self.send_unacknowledged;
            }
            self.retries = 0;
            self.duplicate_acks = 0;
            self.retransmit_at = if self.send_unacknowledged == self.send_next {
                0
            } else {
                crate::trap::ticks() + self.rto
            };
            match self.recover {
                Some(recover) if seq_lt(acknowledgement, recover) => {
                    // Only part of what was outstanding when the loss was
                    // found: a second segment behind the first was lost too,
                    // and this acknowledgement says which one. RFC 6582.
                    self.congestion_window = self
                        .congestion_window
                        .saturating_sub(acked as u32)
                        .saturating_add(MSS as u32)
                        .max(MSS as u32);
                    self.fast_retransmits += 1;
                    self.retransmit_head();
                }
                Some(_) => {
                    // Everything that was outstanding then has arrived: the
                    // loss is behind us and the window comes back down to
                    // what the loss set it to.
                    self.recover = None;
                    self.congestion_window = self.slow_start_threshold;
                }
                None if self.congestion_window < self.slow_start_threshold => {
                    self.congestion_window = self.congestion_window.saturating_add(MSS as u32);
                }
                None => {
                    let increment = (MSS * MSS) as u32 / self.congestion_window.max(1);
                    self.congestion_window = self.congestion_window.saturating_add(increment.max(1));
                }
            }
            woke = true;
        }

        if duplicate {
            self.duplicate_acks += 1;
            if self.duplicate_acks == DUPLICATE_ACK_THRESHOLD && self.recover.is_none() {
                self.enter_recovery();
            } else if self.duplicate_acks > DUPLICATE_ACK_THRESHOLD {
                // Each one past the third says one more segment has left the
                // network, so there is room for one more to go in.
                self.congestion_window = self.congestion_window.saturating_add(MSS as u32);
            }
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
        // that arrived before the bytes in front of it is held until they
        // come, and until then the acknowledgement keeps naming the gap.
        let mut payload: &[u8] = segment.payload;
        let mut sequence = segment.sequence;
        if seq_lt(sequence, self.receive_next) {
            let skip = self.receive_next.wrapping_sub(sequence) as usize;
            payload = if skip >= payload.len() { &[] } else { &payload[skip..] };
            sequence = self.receive_next;
        }
        let mut need_ack = false;
        if !segment.payload.is_empty() {
            need_ack = true;
            if payload.is_empty() {
                // Every byte of it had already arrived.
            } else if !self.can_receive() {
                // Nobody will ever read this. Taking it off the sequence
                // space anyway is what keeps the other end from sending it
                // again forever, and only what is contiguous can be taken
                // off it.
                if sequence == self.receive_next {
                    self.receive_next =
                        self.receive_next.wrapping_add(payload.len() as u32);
                }
            } else if sequence == self.receive_next {
                let free = RECEIVE_WINDOW.saturating_sub(self.received.len());
                let n = payload.len().min(free);
                self.received.extend(payload[..n].iter().copied());
                self.receive_next = self.receive_next.wrapping_add(n as u32);
                self.deliver_held();
                woke = true;
            } else {
                self.hold(sequence, payload);
            }
        }

        // The FIN sits one past the segment's data, and is only taken once
        // every byte in front of it has been -- which may be now, or may be
        // when the gap in front of it fills.
        if segment.flags & FIN != 0 {
            need_ack = true;
            let finish = sequence.wrapping_add(payload.len() as u32);
            if finish == self.receive_next {
                self.held_fin = Some(finish);
            } else if seq_gt(finish, self.receive_next) && self.can_receive() {
                self.held_fin = Some(finish);
                self.arm_reassembly();
            }
        }
        if self.held_fin == Some(self.receive_next) && self.take_fin() {
            woke = true;
        }

        if need_ack && self.state != State::Closed {
            self.open_window(self.receive_next != wanted_before);
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
        self.receive_high = self.receive_next.wrapping_add(RECEIVE_WINDOW as u32);
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
        // Nothing more will arrive to fill a gap, so nothing held past one
        // can ever be delivered.
        self.drop_out_of_order();
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
        // Nobody is left to read what is held, or what filling the gap would
        // deliver.
        self.drop_out_of_order();
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

    // ---- what arrived before the bytes in front of it --------------------

    /// How many separate runs are held. The suite asks; nothing else does.
    pub fn held_runs(&self) -> usize {
        self.out_of_order.len()
    }

    /// Account for a change in what this connection holds, in the count every
    /// connection shares.
    fn account(&mut self) {
        let held: usize = self.out_of_order.iter().map(|run| run.data.len()).sum();
        if held > self.held {
            HELD_BYTES.fetch_add(held - self.held, Ordering::Relaxed);
        } else {
            HELD_BYTES.fetch_sub(self.held - held, Ordering::Relaxed);
        }
        self.held = held;
    }

    /// Let go of everything held past a gap.
    pub fn drop_out_of_order(&mut self) {
        self.out_of_order.clear();
        self.held_fin = None;
        self.reassembly_expire_at = 0;
        self.account();
    }

    fn arm_reassembly(&mut self) {
        self.reassembly_expire_at = crate::trap::ticks() + REASSEMBLY_TICKS;
    }

    /// Keep a run that arrived before the bytes in front of it.
    fn hold(&mut self, sequence: u32, payload: &[u8]) {
        // Everything held lies inside the window this end advertised, and
        // that window is what is left of the receive buffer, so what one
        // connection holds needs no bound of its own: the two together never
        // exceed the buffer.
        let right = self.receive_next.wrapping_add(self.advertised_window() as u32);
        if !seq_lt(sequence, right) {
            return;
        }
        let room = right.wrapping_sub(sequence) as usize;
        let payload = &payload[..payload.len().min(room)];
        if payload.is_empty() {
            return;
        }
        if held_bytes() + payload.len() > REASSEMBLY_LIMIT {
            return;
        }
        self.out_of_order.push(Held { sequence, data: payload.to_vec() });
        self.coalesce();
        if self.out_of_order.len() > MAX_HELD_RUNS {
            // The run furthest ahead goes: it is the one with the most
            // missing in front of it, so it is the one whose sender has the
            // furthest to go before any of it can be delivered.
            self.out_of_order.pop();
        }
        self.account();
        self.arm_reassembly();
    }

    /// Put the runs in order and join the ones that touch or overlap.
    fn coalesce(&mut self) {
        let base = self.receive_next;
        let mut runs = core::mem::take(&mut self.out_of_order);
        runs.sort_unstable_by_key(|run| run.sequence.wrapping_sub(base));
        let mut joined: Vec<Held> = Vec::with_capacity(runs.len());
        for run in runs {
            match joined.last_mut() {
                Some(last) if seq_le(run.sequence, last.end()) => {
                    let overlap = last.end().wrapping_sub(run.sequence) as usize;
                    if overlap < run.data.len() {
                        last.data.extend_from_slice(&run.data[overlap..]);
                    }
                }
                _ => joined.push(run),
            }
        }
        self.out_of_order = joined;
    }

    /// Move everything that now follows on into what the reader sees.
    fn deliver_held(&mut self) {
        while let Some(run) = self.out_of_order.first() {
            if seq_gt(run.sequence, self.receive_next) {
                break;
            }
            let run = self.out_of_order.remove(0);
            let skip = self.receive_next.wrapping_sub(run.sequence) as usize;
            if skip >= run.data.len() {
                continue;
            }
            let free = RECEIVE_WINDOW.saturating_sub(self.received.len());
            let take = (run.data.len() - skip).min(free);
            self.received.extend(run.data[skip..skip + take].iter().copied());
            self.receive_next = self.receive_next.wrapping_add(take as u32);
            if skip + take < run.data.len() {
                // No room for the rest of this run. The window is what bounds
                // what was held, so this does not arise; leaving the stream
                // whole rather than assuming so costs one insert.
                let sequence = self.receive_next;
                let rest = run.data[skip + take..].to_vec();
                self.out_of_order.insert(0, Held { sequence, data: rest });
                break;
            }
        }
        self.account();
        if self.out_of_order.is_empty() && self.held_fin.is_none() {
            self.reassembly_expire_at = 0;
        } else {
            // Something moved, so the wait for the rest starts again.
            self.arm_reassembly();
        }
    }

    /// Take the other end's finish, once every byte in front of it has been
    /// taken.
    fn take_fin(&mut self) -> bool {
        self.held_fin = None;
        if self.out_of_order.is_empty() {
            self.reassembly_expire_at = 0;
        }
        if self.fin_received {
            return false;
        }
        self.fin_received = true;
        self.receive_next = self.receive_next.wrapping_add(1);
        match self.state {
            State::Established => self.state = State::CloseWait,
            State::FinWait1 => self.state = State::Closing,
            State::FinWait2 => self.enter_time_wait(),
            _ => {}
        }
        true
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
        if self.reassembly_expire_at != 0 {
            earliest = earliest.min(self.reassembly_expire_at);
        }
        earliest
    }

    pub fn on_timer(&mut self, now: u64) -> bool {
        if self.reassembly_expire_at != 0 && now >= self.reassembly_expire_at {
            // The gap never filled. What is held behind it is of no use to
            // anybody until it does, and the end that left it has stopped
            // trying.
            self.drop_out_of_order();
        }
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
        // RFC 6298 5.5: the timeout doubles, and it stays doubled until a
        // segment that went out once is acknowledged.
        self.rto = (self.rto * 2).min(MAX_RTO);
        self.timed = None;
        self.retransmit_at = now + self.rto;
        let flight = self.send_high.wrapping_sub(self.send_unacknowledged);
        self.slow_start_threshold = (flight / 2).max(2 * MSS as u32);
        self.congestion_window = MSS as u32;
        // Whatever the duplicate acknowledgements were saying, the clock
        // running out says more: start again from one segment.
        self.recover = None;
        self.duplicate_acks = 0;
        self.timeouts += 1;

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
                    self.sent_through(sequence.wrapping_add(1));
                } else {
                    self.output();
                }
            }
        }
        false
    }
}

impl Drop for Tcb {
    fn drop(&mut self) {
        // The count every connection shares is what bounds them all together,
        // so a connection that goes away while holding something has to give
        // its share back.
        HELD_BYTES.fetch_sub(self.held, Ordering::Relaxed);
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
    // RFC 1122: a segment addressed to a broadcast or a multicast address is
    // discarded. Answering one would name an address this machine does not
    // have as the source, and open a connection with every host that answered.
    if destination.is_broadcast()
        || destination.is_multicast()
        || destination == super::config().broadcast()
    {
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
