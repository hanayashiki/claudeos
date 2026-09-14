//! The telnet console: the kernel's one terminal, reached over TCP port 23.
//!
//! It is not a second terminal. What arrives on the connection is fed to
//! `push_byte`, the way the serial port's input is, so line editing, echo and
//! the interrupt key are the line discipline's and behave as they do on the
//! cable. Everything the console prints, kernel messages included, is copied
//! to the connection as it goes to the serial port.
//!
//! The two directions run in different places. Output is copied by whatever
//! printed it, inside the console lock with interrupts masked, so the copy
//! only appends to a bounded queue, `OUTBOX`, and never waits on the network.
//! Accepting, reading, and moving that queue into the TCP connection happen in
//! `on_tick`, which the network task calls every tick and after every frame.
//!
//! There is one connection at most, and `Service` has one slot for it. A
//! connection that arrives while the slot is full is sent a line saying the
//! console is busy and is closed.
//!
//! The protocol is RFC 854's, with RFC 857's echo and RFC 858's suppressed
//! go-ahead. This end offers both at connect, which is what puts a client in
//! character mode with echo left to this end. Everything the client sends
//! about options, subnegotiation included, is parsed and dropped. A CR from
//! the client comes followed by NUL or LF, and the NUL or LF is dropped,
//! because the line discipline takes the CR alone as the end of a line. In
//! the output an IAC byte is doubled, and a CR that is not followed by LF is
//! followed by NUL, which is how RFC 854 says a carriage return on its own is
//! sent.

use crate::abi::Errno;
use crate::net::ip::Ipv4Addr;
use crate::net::socket::{self, Endpoint, InetSocket, Received};
use crate::net::tcp;
use crate::serial::KernelLog;
use crate::sync::Spinlock;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

pub const PORT: u16 = 23;

// RFC 854's commands, and the options RFC 857 and RFC 858 define.
const IAC: u8 = 255;
const DONT: u8 = 254;
const DO: u8 = 253;
const WONT: u8 = 252;
const WILL: u8 = 251;
const SB: u8 = 250;
const NOP: u8 = 241;
const SE: u8 = 240;
const ECHO: u8 = 1;
const SUPPRESS_GO_AHEAD: u8 = 3;

/// Bytes of output held for the connection, on top of what TCP holds.
///
/// Past this and TCP's own send buffer, the client is taken to have stopped
/// reading and is dropped. It has to hold the kernel log a connection is sent
/// when it attaches, which is 16 KiB before line endings and doubled bytes are
/// added and at most three times that after. A client that is reading does
/// not come near it on the board, where everything the console prints also
/// goes out of a serial port at 11.5 KiB a second.
const OUTBOX_SIZE: usize = 64 * 1024;

/// Finished handshakes the listening socket keeps until `accept` takes them.
/// `accept` runs every tick and takes all of them, so this only has to cover
/// what arrives inside one tick.
const BACKLOG: usize = 4;

/// Bytes moved in either direction per call into the socket.
const SCRATCH: usize = 4096;

// ---- output ---------------------------------------------------------------

/// Output on its way to the connection, and whether there is a connection to
/// copy it to.
struct Outbox {
    ring: [u8; OUTBOX_SIZE],
    /// Where the oldest queued byte is, and how many are queued.
    head: usize,
    len: usize,
    stream: Stream,
    /// The last byte queued was a CR, so what comes next decides whether a
    /// NUL goes in between.
    after_cr: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Stream {
    /// No connection. The console's output is not copied.
    Detached,
    /// Copying. A kernel message the log took in at or before `log_sent` was
    /// in the log the connection was sent when it attached, and is not copied
    /// a second time.
    Attached { log_sent: u64 },
    /// Something the console printed did not fit. Nothing more is copied: the
    /// connection has lost output, and the network side drops it the next
    /// time it looks.
    Overflowed,
}

static OUTBOX: Spinlock<Outbox> = Spinlock::new(Outbox::new());

impl Outbox {
    const fn new() -> Outbox {
        Outbox {
            ring: [0; OUTBOX_SIZE],
            head: 0,
            len: 0,
            stream: Stream::Detached,
            after_cr: false,
        }
    }

    /// Queue bytes for the wire as they stand, or queue none of them and
    /// report that there was no room.
    fn put(&mut self, wire: &[u8]) -> bool {
        if OUTBOX_SIZE - self.len < wire.len() {
            return false;
        }
        for &byte in wire {
            self.ring[(self.head + self.len) % OUTBOX_SIZE] = byte;
            self.len += 1;
        }
        true
    }

    /// Queue one byte the terminal printed, encoded for the wire.
    fn put_text(&mut self, byte: u8) -> bool {
        let mut wire = [0u8; 3];
        let mut n = 0;
        if self.after_cr && byte != b'\n' {
            // The NUL that makes the CR before it a carriage return alone.
            n += 1;
        }
        wire[n] = byte;
        n += 1;
        if byte == IAC {
            wire[n] = IAC;
            n += 1;
        }
        if !self.put(&wire[..n]) {
            return false;
        }
        self.after_cr = byte == b'\r';
        true
    }

    /// Queue a telnet command. A CR still waiting to learn whether it is on
    /// its own is given its NUL first, so a command never comes between the
    /// two.
    fn put_command(&mut self, command: &[u8]) -> bool {
        if self.after_cr {
            if !self.put(&[0]) {
                return false;
            }
            self.after_cr = false;
        }
        self.put(command)
    }

    fn copy(&mut self, bytes: &[u8], logged_at: Option<u64>) {
        let Stream::Attached { log_sent } = self.stream else { return };
        if logged_at.is_some_and(|position| position <= log_sent) {
            return;
        }
        for &byte in bytes {
            if !self.put_text(byte) {
                self.stream = Stream::Overflowed;
                return;
            }
        }
    }

    /// Send a command on the connection that is attached, if one is.
    fn command(&mut self, command: &[u8]) {
        if let Stream::Attached { .. } = self.stream {
            if !self.put_command(command) {
                self.stream = Stream::Overflowed;
            }
        }
    }

    /// Start a new connection's stream: the two offers that set the client's
    /// mode, the kernel log as it stands, and from then on whatever the
    /// console prints.
    ///
    /// A `&KernelLog` can only be had by holding the log's lock, and holding
    /// it across this is what makes it one step as far as `serial::Logged` is
    /// concerned, which takes the same lock to put a message in the log before
    /// printing it. A message is either in the log sent here, or printed after
    /// `stream` says to copy it with a position past `log_sent`: never
    /// neither, and never both.
    ///
    /// This copies up to the log's 16 KiB with interrupts masked, which is a
    /// small fraction of a tick on either machine, and happens once a
    /// connection.
    fn attach(&mut self, log: &KernelLog) {
        self.head = 0;
        self.len = 0;
        self.after_cr = false;
        let mut room = self.put_command(&[IAC, WILL, ECHO])
            && self.put_command(&[IAC, WILL, SUPPRESS_GO_AHEAD]);
        for &byte in log.bytes() {
            // The log holds a line feed where the console printed CR LF.
            if byte == b'\n' {
                room = room && self.put_text(b'\r');
            }
            room = room && self.put_text(byte);
        }
        self.stream = if room {
            Stream::Attached { log_sent: log.pushed() }
        } else {
            Stream::Overflowed
        };
    }

    fn detach(&mut self) {
        self.stream = Stream::Detached;
        self.head = 0;
        self.len = 0;
        self.after_cr = false;
    }

    /// Copy out the oldest queued bytes without taking them off the queue.
    fn peek(&self, out: &mut [u8]) -> usize {
        let n = out.len().min(self.len);
        let first = n.min(OUTBOX_SIZE - self.head);
        out[..first].copy_from_slice(&self.ring[self.head..self.head + first]);
        out[first..n].copy_from_slice(&self.ring[..n - first]);
        n
    }

    fn consume(&mut self, n: usize) {
        let n = n.min(self.len);
        self.head = (self.head + n) % OUTBOX_SIZE;
        self.len -= n;
    }
}

/// The console printed `bytes`. `logged_at` is where the kernel log took them
/// in, for a kernel message.
///
/// Called from inside the console lock, with interrupts masked. It takes one
/// more lock and queues at most three bytes for each one given, and does not
/// wait for anything.
pub fn copy(bytes: &[u8], logged_at: Option<u64>) {
    OUTBOX.lock().copy(bytes, logged_at);
}

/// Take the queue's lock back. For the panic path, beside the console's and
/// the log's, since a panic inside a print can be reached with it held.
pub unsafe fn force_release() {
    OUTBOX.force_unlock();
}

// ---- input ----------------------------------------------------------------

/// Where the client's byte stream stands.
#[derive(Clone, Copy)]
enum Input {
    Data,
    /// After a CR, whose NUL or LF is dropped.
    Cr,
    /// After IAC.
    Command,
    /// After WILL, WONT, DO or DONT: the option code comes next.
    Negotiation,
    /// Inside SB, until IAC SE.
    Subnegotiation,
    /// After an IAC inside SB.
    SubnegotiationCommand,
}

impl Input {
    /// Take one byte from the client, and hand it back if it is one for the
    /// terminal.
    ///
    /// Nothing is answered. A request this end does not answer leaves the
    /// option off at the client, which is where every option starts, and the
    /// two this end does want it offers itself.
    fn feed(&mut self, byte: u8) -> Option<u8> {
        match *self {
            Input::Data => match byte {
                IAC => {
                    *self = Input::Command;
                    None
                }
                b'\r' => {
                    *self = Input::Cr;
                    Some(byte)
                }
                _ => Some(byte),
            },
            Input::Cr => {
                *self = Input::Data;
                if byte == 0 || byte == b'\n' {
                    None
                } else {
                    self.feed(byte)
                }
            }
            Input::Command => match byte {
                // A doubled IAC is one 0xFF of data.
                IAC => {
                    *self = Input::Data;
                    Some(IAC)
                }
                WILL | WONT | DO | DONT => {
                    *self = Input::Negotiation;
                    None
                }
                SB => {
                    *self = Input::Subnegotiation;
                    None
                }
                _ => {
                    *self = Input::Data;
                    None
                }
            },
            Input::Negotiation => {
                *self = Input::Data;
                None
            }
            Input::Subnegotiation => {
                if byte == IAC {
                    *self = Input::SubnegotiationCommand;
                }
                None
            }
            Input::SubnegotiationCommand => {
                // Only SE ends it; a doubled IAC is the subnegotiation's own
                // data.
                *self = if byte == SE { Input::Data } else { Input::Subnegotiation };
                None
            }
        }
    }
}

// ---- the service ----------------------------------------------------------

/// Set at boot when there is a card and the command line did not turn the
/// telnet console off, and cleared if the port cannot be had.
static ENABLED: AtomicBool = AtomicBool::new(false);

/// What the network side keeps. `None` until the network has a configuration.
static SERVICE: Spinlock<Option<Service>> = Spinlock::new(None);

struct Service {
    listener: Arc<InetSocket>,
    /// The one connection. `accept` either puts a connection here or turns it
    /// away; there is nowhere for a second one to be kept.
    connection: Option<Connection>,
    /// Bytes pass through here in both directions, so a tick allocates
    /// nothing.
    scratch: Vec<u8>,
}

struct Connection {
    socket: Arc<InetSocket>,
    peer: Endpoint,
    input: Input,
}

/// Why a connection ended.
enum End {
    /// The client closed it.
    Closed,
    /// TCP gave up on it: the client reset it, or stopped answering.
    Failed(Errno),
    /// The console printed more than the queue and TCP could hold for it.
    Behind,
}

/// Turn the telnet console on. Nothing listens until the network has a
/// configuration.
pub fn enable() {
    ENABLED.store(true, Ordering::Relaxed);
}

/// Accept, hand what the client typed to the terminal, and send what the
/// console printed. Called from `net::tick`.
pub fn on_tick() {
    if !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    // The service is taken out of its lock for the call rather than held
    // through it: the call types into the terminal and prints, both of which
    // write to the serial port, and a lock held across that would mask
    // interrupts for as long as the port takes. The guard is what keeps a
    // second caller from finding the lock empty and listening a second time.
    static RUNNING: AtomicBool = AtomicBool::new(false);
    if RUNNING.swap(true, Ordering::Acquire) {
        return;
    }
    // Two statements, because a guard made inside a `let` lives to the end of
    // that statement, and `open` prints.
    let held = SERVICE.lock().take();
    let mut service = held.or_else(open);
    if let Some(service) = service.as_mut() {
        service.accept();
        service.serve();
    }
    *SERVICE.lock() = service;
    RUNNING.store(false, Ordering::Release);
}

/// Listen, once there is an address to be reached at.
fn open() -> Option<Service> {
    let config = crate::net::config()?;
    let listener = InetSocket::new(true);
    let listening = listener
        .bind(Endpoint::new(Ipv4Addr::UNSPECIFIED, PORT))
        .and_then(|()| listener.listen(BACKLOG));
    if let Err(err) = listening {
        // A program bound the port before the address came. Trying again
        // every tick would print this every tick.
        ENABLED.store(false, Ordering::Relaxed);
        socket::close(&listener);
        crate::println!(
            "telnet: cannot listen on port {}: {:?}; the console is not served over the network",
            PORT,
            err
        );
        return None;
    }
    crate::println!("telnet: the console is on {} port {}", config.address(), PORT);
    Some(Service { listener, connection: None, scratch: alloc::vec![0; SCRATCH] })
}

impl Service {
    /// Take every connection that has finished its handshake. The first one
    /// fills the slot; one that arrives while the slot is full is turned away.
    fn accept(&mut self) {
        while let Ok(Some(socket)) = self.listener.accept_ready() {
            let peer = socket.peer_endpoint().unwrap_or(Endpoint::UNSPECIFIED);
            match &self.connection {
                Some(held) => {
                    refuse(&socket, held.peer);
                    held.probe();
                    crate::println!(
                        "telnet: turned {} away; the console is in use from {}",
                        peer,
                        held.peer
                    );
                }
                None => {
                    {
                        let log = crate::serial::LOG.lock();
                        OUTBOX.lock().attach(&log);
                    }
                    self.connection = Some(Connection { socket, peer, input: Input::Data });
                    // Printed after the copy has started, so it is the first
                    // thing the connection is sent after the log, and the
                    // serial port says who attached as well.
                    crate::println!("telnet: console attached from {}", peer);
                }
            }
        }
    }

    fn serve(&mut self) {
        let Some(mut connection) = self.connection.take() else { return };
        match connection.exchange(&mut self.scratch) {
            Ok(()) => self.connection = Some(connection),
            Err(end) => connection.end(end),
        }
    }
}

impl Connection {
    /// Hand what the client sent to the terminal, and move what the console
    /// printed into the connection.
    fn exchange(&mut self, scratch: &mut [u8]) -> Result<(), End> {
        loop {
            match self.socket.receive_into(scratch, false) {
                Ok(Received { taken: 0, .. }) => return Err(End::Closed),
                Ok(Received { taken, .. }) => {
                    for &byte in &scratch[..taken] {
                        if let Some(byte) = self.input.feed(byte) {
                            super::push_byte(byte);
                        }
                    }
                }
                Err(Errno::EAGAIN) => break,
                Err(err) => return Err(End::Failed(err)),
            }
        }
        // After the input, whose echo is output too, and before the queue is
        // moved, which would make room and hide that something did not fit.
        if OUTBOX.lock().stream == Stream::Overflowed {
            return Err(End::Behind);
        }
        loop {
            let n = OUTBOX.lock().peek(scratch);
            if n == 0 {
                return Ok(());
            }
            let taken = match self.socket.send(&scratch[..n], None) {
                Ok(taken) => taken,
                Err(Errno::EAGAIN) => 0,
                Err(err) => return Err(End::Failed(err)),
            };
            // Only `on_tick` takes bytes off the queue or empties it, so what
            // was peeked is still at the front.
            OUTBOX.lock().consume(taken);
            if taken < n {
                return Ok(());
            }
        }
    }

    /// Ask the client for an acknowledgement.
    ///
    /// A client that went away without closing -- a laptop that slept, a
    /// network that went down -- otherwise keeps the slot until the console
    /// next prints something. With a byte outstanding, TCP gives up on it once
    /// its retransmissions run out, which is about thirty seconds on a local
    /// network, and the slot is free again.
    fn probe(&self) {
        OUTBOX.lock().command(&[IAC, NOP]);
    }

    fn end(self, end: End) {
        // Detached before anything is printed about it, so the line saying so
        // is not copied to the connection it is about.
        OUTBOX.lock().detach();
        if let End::Behind = end {
            // Reset rather than finished: a finish would follow output that
            // was never sent, and the client would take the connection for one
            // that ended where it was meant to.
            self.socket.abort();
        }
        socket::close(&self.socket);
        match end {
            End::Closed => crate::println!("telnet: console detached from {}", self.peer),
            End::Failed(err) => crate::println!("telnet: lost {}: {:?}", self.peer, err),
            End::Behind => crate::println!(
                "telnet: dropped {}, which fell more than {} KiB behind the console",
                self.peer,
                (OUTBOX_SIZE + tcp::SEND_BUFFER) / 1024
            ),
        }
    }
}

/// Tell a connection the console is taken, and close it.
fn refuse(socket: &Arc<InetSocket>, holder: Endpoint) {
    let line = alloc::format!("telnet console busy: in use from {}\r\n", holder);
    let _ = socket.send(line.as_bytes(), None);
    // What the client has sent already -- its option negotiation, as a rule --
    // is read and thrown away first. A socket closed with unread data is reset
    // rather than finished, and a reset can make the client's end discard the
    // line above before anything reads it.
    let mut discard = [0u8; 256];
    while let Ok(Received { taken, .. }) = socket.receive_into(&mut discard, false) {
        if taken == 0 {
            break;
        }
    }
    socket::close(socket);
}
