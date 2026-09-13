//! Internet sockets: the object a descriptor points at, and the table the
//! arriving packet is matched against.
//!
//! One lock per socket, taken for the whole of an operation on it. The table
//! of sockets has a lock of its own, and the rule that keeps the two apart is
//! that the table lock is never held while a socket lock is taken: a lookup
//! clones the reference out and lets go first. A listening socket may be
//! locked while one of its children is, and never the other way round.

use super::ip::Ipv4Addr;
use super::tcp::{self, Tcb};
use super::udp::UdpState;
use crate::abi::Errno;
use crate::sync::Spinlock;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU16, Ordering};

/// An address and a port: one end of a conversation.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Endpoint {
    pub address: Ipv4Addr,
    pub port: u16,
}

impl Endpoint {
    pub const UNSPECIFIED: Endpoint =
        Endpoint { address: Ipv4Addr::UNSPECIFIED, port: 0 };

    pub const fn new(address: Ipv4Addr, port: u16) -> Endpoint {
        Endpoint { address, port }
    }

    pub fn is_unspecified(&self) -> bool {
        self.address.is_unspecified() && self.port == 0
    }
}

impl core::fmt::Display for Endpoint {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}:{}", self.address, self.port)
    }
}

pub enum Protocol {
    Tcp(Tcb),
    Udp(UdpState),
}

pub struct InetSocket {
    pub stream: bool,
    pub inner: Spinlock<Protocol>,
    /// Set when the last descriptor naming this socket has gone. The socket
    /// itself may outlive that: a closed connection still has to see its own
    /// FIN acknowledged, and then sit in TIME-WAIT.
    pub detached: AtomicBool,
}

impl InetSocket {
    pub fn new(stream: bool) -> Arc<InetSocket> {
        let inner = if stream {
            Protocol::Tcp(Tcb::new())
        } else {
            Protocol::Udp(UdpState::new())
        };
        Arc::new(InetSocket {
            stream,
            inner: Spinlock::new(inner),
            detached: AtomicBool::new(false),
        })
    }

    pub fn is_detached(&self) -> bool {
        self.detached.load(Ordering::Acquire)
    }

    fn with_tcp<T>(&self, f: impl FnOnce(&mut Tcb) -> T) -> Result<T, Errno> {
        match &mut *self.inner.lock() {
            Protocol::Tcp(tcb) => Ok(f(tcb)),
            Protocol::Udp(_) => Err(Errno::EOPNOTSUPP),
        }
    }

    pub fn local_endpoint(&self) -> Endpoint {
        match &*self.inner.lock() {
            Protocol::Tcp(tcb) => tcb.local,
            Protocol::Udp(state) => state.local,
        }
    }

    pub fn peer_endpoint(&self) -> Result<Endpoint, Errno> {
        match &*self.inner.lock() {
            Protocol::Tcp(tcb) => {
                if tcb.state.is_synchronised() {
                    Ok(tcb.remote)
                } else {
                    Err(Errno::ENOTCONN)
                }
            }
            Protocol::Udp(state) => state.remote.ok_or(Errno::ENOTCONN),
        }
    }

    /// A poll would report this readable: there is something to read, or
    /// something to accept, or a read would report an end rather than block.
    pub fn readable(&self) -> bool {
        match &*self.inner.lock() {
            Protocol::Tcp(tcb) => {
                if tcb.state == tcp::State::Listen {
                    // This locks each child while holding the listener, which
                    // is the direction the module comment allows.
                    return tcb.children.iter().any(|child| child.handshake_done());
                }
                !tcb.received.is_empty()
                    || tcb.fin_received
                    || tcb.error.is_some()
                    || tcb.state == tcp::State::Closed
            }
            Protocol::Udp(state) => !state.queue.is_empty(),
        }
    }

    pub fn writable(&self) -> bool {
        match &*self.inner.lock() {
            Protocol::Tcp(tcb) => match tcb.state {
                tcp::State::Established | tcp::State::CloseWait => {
                    tcb.pending.len() < tcp::SEND_BUFFER
                }
                // A socket that can never take data again is reported
                // writable so the write reports the error rather than the
                // poll blocking forever.
                tcp::State::Closed => true,
                _ => tcb.error.is_some(),
            },
            Protocol::Udp(_) => true,
        }
    }

    /// Bytes a read would return without blocking, for FIONREAD.
    pub fn available(&self) -> usize {
        match &*self.inner.lock() {
            Protocol::Tcp(tcb) => tcb.received.len(),
            Protocol::Udp(state) => state.queue.front().map_or(0, |(_, data)| data.len()),
        }
    }

    /// The connection is over in both directions: nothing more will arrive
    /// and nothing more can be sent.
    pub fn hung_up(&self) -> bool {
        match &*self.inner.lock() {
            Protocol::Tcp(tcb) => {
                tcb.error.is_some()
                    || matches!(
                        tcb.state,
                        tcp::State::Closed | tcp::State::TimeWait | tcp::State::LastAck
                    )
            }
            Protocol::Udp(_) => false,
        }
    }

    pub fn is_listening(&self) -> bool {
        match &*self.inner.lock() {
            Protocol::Tcp(tcb) => tcb.state == tcp::State::Listen,
            Protocol::Udp(_) => false,
        }
    }

    /// True once this socket has completed a handshake it did not start, so
    /// accept may hand it out.
    pub fn handshake_done(&self) -> bool {
        match &*self.inner.lock() {
            Protocol::Tcp(tcb) => tcb.state.is_synchronised(),
            Protocol::Udp(_) => false,
        }
    }

    /// The pending error, taken, for SO_ERROR.
    pub fn take_error(&self) -> Option<Errno> {
        match &mut *self.inner.lock() {
            Protocol::Tcp(tcb) => tcb.error.take(),
            Protocol::Udp(state) => state.error.take(),
        }
    }

    // ---- binding ---------------------------------------------------------

    pub fn bind(self: &Arc<Self>, requested: Endpoint) -> Result<(), Errno> {
        let config = super::config();
        if !requested.address.is_unspecified()
            && requested.address != config.address
            && !requested.address.is_broadcast()
            && !requested.address.is_loopback()
        {
            return Err(Errno::EADDRNOTAVAIL);
        }
        let mut local = requested;
        if local.port == 0 {
            local.port = allocate_port(self.stream)?;
        } else if port_taken(self.stream, local.port, Some(self)) {
            return Err(Errno::EADDRINUSE);
        }
        match &mut *self.inner.lock() {
            Protocol::Tcp(tcb) => {
                if tcb.state != tcp::State::Closed || tcb.bound {
                    return Err(Errno::EINVAL);
                }
                tcb.local = local;
                tcb.bound = true;
            }
            Protocol::Udp(state) => {
                if state.bound {
                    return Err(Errno::EINVAL);
                }
                state.local = local;
                state.bound = true;
            }
        }
        register(self);
        Ok(())
    }

    /// Give the socket a local port if it does not have one yet.
    fn bind_ephemeral(self: &Arc<Self>) -> Result<(), Errno> {
        if self.local_endpoint().port != 0 {
            return Ok(());
        }
        self.bind(Endpoint::UNSPECIFIED)
    }

    // ---- stream sockets --------------------------------------------------

    pub fn listen(self: &Arc<Self>, backlog: usize) -> Result<(), Errno> {
        if !self.stream {
            return Err(Errno::EOPNOTSUPP);
        }
        self.bind_ephemeral()?;
        self.with_tcp(|tcb| {
            match tcb.state {
                tcp::State::Closed | tcp::State::Listen => {}
                _ => return Err(Errno::EINVAL),
            }
            tcb.state = tcp::State::Listen;
            tcb.backlog = backlog.clamp(1, 128);
            Ok(())
        })?
    }

    /// Take one finished connection off the queue, or report that there is
    /// none yet.
    pub fn accept_ready(self: &Arc<Self>) -> Result<Option<Arc<InetSocket>>, Errno> {
        super::poll();
        let children = self.with_tcp(|tcb| {
            if tcb.state != tcp::State::Listen {
                return Err(Errno::EINVAL);
            }
            Ok(tcb.children.clone())
        })??;

        let mut ready: Option<Arc<InetSocket>> = None;
        let mut dead: Vec<Arc<InetSocket>> = Vec::new();
        for child in children {
            let state = child.with_tcp(|tcb| tcb.state)?;
            if state == tcp::State::Closed {
                dead.push(child);
            } else if state.is_synchronised() && ready.is_none() {
                ready = Some(child);
            }
        }
        // A handshake that was reset before anyone accepted it is nobody's to
        // report, so it is simply forgotten.
        if !dead.is_empty() || ready.is_some() {
            self.with_tcp(|tcb| {
                tcb.children.retain(|child| {
                    !dead.iter().any(|other| Arc::ptr_eq(child, other))
                        && !ready.as_ref().is_some_and(|other| Arc::ptr_eq(child, other))
                });
            })?;
        }
        for child in dead {
            unregister(&child);
        }
        Ok(ready)
    }

    pub fn connect(self: &Arc<Self>, remote: Endpoint) -> Result<(), Errno> {
        if remote.port == 0 || remote.address.is_unspecified() {
            return Err(Errno::EINVAL);
        }
        if !self.stream {
            // A datagram socket's connect only records where to send.
            self.bind_ephemeral()?;
            match &mut *self.inner.lock() {
                Protocol::Udp(state) => state.remote = Some(remote),
                Protocol::Tcp(_) => unreachable!(),
            }
            return Ok(());
        }
        self.bind_ephemeral()?;
        let local = self.local_endpoint();
        self.with_tcp(|tcb| {
            match tcb.state {
                tcp::State::Closed => {}
                tcp::State::SynSent => return Err(Errno::EALREADY),
                tcp::State::Listen => return Err(Errno::EINVAL),
                _ => return Err(Errno::EISCONN),
            }
            tcb.local = Endpoint::new(super::source_for(remote.address), local.port);
            tcb.remote = remote;
            tcb.open(false);
            Ok(())
        })??;
        super::poll();
        Ok(())
    }

    // ---- transfer --------------------------------------------------------

    pub fn send(&self, buf: &[u8], to: Option<Endpoint>) -> Result<usize, Errno> {
        match &mut *self.inner.lock() {
            Protocol::Tcp(tcb) => tcb.write(buf),
            Protocol::Udp(state) => state.send(buf, to),
        }
    }

    /// Returns the bytes taken and, for a datagram socket, where they came
    /// from.
    pub fn receive_into(&self, buf: &mut [u8], peek: bool) -> Result<(usize, Endpoint), Errno> {
        match &mut *self.inner.lock() {
            Protocol::Tcp(tcb) => {
                let remote = tcb.remote;
                tcb.read(buf, peek).map(|n| (n, remote))
            }
            Protocol::Udp(state) => state.receive(buf, peek),
        }
    }

    /// True while an active open is still waiting for an answer. Asked from
    /// inside a sleep, so it takes nothing: the error belongs to whoever
    /// wakes up and calls `connection_progress`.
    pub fn connecting(&self) -> bool {
        match &*self.inner.lock() {
            Protocol::Tcp(tcb) => {
                matches!(tcb.state, tcp::State::SynSent | tcp::State::SynReceived)
            }
            Protocol::Udp(_) => false,
        }
    }

    /// Where an active open has got to: not yet, connected, or failed.
    pub fn connection_progress(&self) -> Result<bool, Errno> {
        match &mut *self.inner.lock() {
            Protocol::Tcp(tcb) => match tcb.state {
                tcp::State::SynSent | tcp::State::SynReceived => Ok(false),
                tcp::State::Closed => {
                    Err(tcb.error.take().unwrap_or(Errno::ECONNREFUSED))
                }
                tcp::State::Listen => Err(Errno::EINVAL),
                _ => Ok(true),
            },
            Protocol::Udp(state) => Ok(state.remote.is_some()),
        }
    }

    /// Read, sleeping until there is something to read if the descriptor
    /// allows it.
    pub fn read_blocking(
        self: &Arc<Self>,
        buf: &mut [u8],
        nonblock: bool,
        peek: bool,
    ) -> Result<(usize, Endpoint), Errno> {
        super::poll();
        loop {
            match self.receive_into(buf, peek) {
                Err(Errno::EAGAIN) => {}
                other => {
                    // Taking data may have reopened the window, which sends
                    // an acknowledgement the other end is waiting for.
                    super::poll();
                    return other;
                }
            }
            if nonblock {
                return Err(Errno::EAGAIN);
            }
            wait_for(|| self.readable())?;
        }
    }

    /// Write, sleeping until the socket will take something. A short count is
    /// what a stream socket reports when the send buffer fills, and callers
    /// come back for the rest.
    pub fn write_blocking(
        self: &Arc<Self>,
        buf: &[u8],
        nonblock: bool,
        to: Option<Endpoint>,
        raise_sigpipe: bool,
    ) -> Result<usize, Errno> {
        if buf.is_empty() {
            return Ok(0);
        }
        super::poll();
        loop {
            match self.send(buf, to) {
                Ok(n) => {
                    super::poll();
                    return Ok(n);
                }
                Err(Errno::EAGAIN) => {}
                Err(Errno::EPIPE) => {
                    // Writing to a connection the other end has finished with
                    // raises SIGPIPE, exactly as it does on a pipe.
                    if raise_sigpipe {
                        crate::sched::raise_on_current(crate::abi::SIGPIPE);
                    }
                    return Err(Errno::EPIPE);
                }
                Err(err) => return Err(err),
            }
            if nonblock {
                return Err(Errno::EAGAIN);
            }
            wait_for(|| self.writable())?;
        }
    }

    pub fn shutdown(&self, read: bool, write: bool) -> Result<(), Errno> {
        match &mut *self.inner.lock() {
            Protocol::Tcp(tcb) => {
                if !tcb.state.is_synchronised() && tcb.state != tcp::State::SynSent {
                    return Err(Errno::ENOTCONN);
                }
                if read {
                    tcb.read_shutdown = true;
                }
                if write {
                    tcb.shutdown_write();
                }
                Ok(())
            }
            Protocol::Udp(state) => {
                if read {
                    state.queue.clear();
                }
                if write {
                    state.write_shutdown = true;
                }
                Ok(())
            }
        }
    }
}

// ---- the table of sockets ------------------------------------------------

static SOCKETS: Spinlock<Vec<Arc<InetSocket>>> = Spinlock::new(Vec::new());

pub fn register(socket: &Arc<InetSocket>) {
    let mut table = SOCKETS.lock();
    if !table.iter().any(|other| Arc::ptr_eq(other, socket)) {
        table.push(socket.clone());
    }
}

pub fn unregister(socket: &Arc<InetSocket>) {
    SOCKETS.lock().retain(|other| !Arc::ptr_eq(other, socket));
}

/// A snapshot of the table, for anything that has to lock the sockets it
/// finds.
pub fn all() -> Vec<Arc<InetSocket>> {
    SOCKETS.lock().clone()
}

pub fn count() -> usize {
    SOCKETS.lock().len()
}

/// Nothing is using the network. Asked before `all`, which copies the table.
pub fn is_empty() -> bool {
    SOCKETS.lock().is_empty()
}

/// Throw everything away. Only the self test does this.
pub fn reset() {
    SOCKETS.lock().clear();
}

fn matches_local(bound: Endpoint, local: Endpoint) -> bool {
    bound.port == local.port
        && (bound.address.is_unspecified() || bound.address == local.address)
}

/// The socket an arriving segment belongs to: the exact conversation first,
/// and only then something listening on the port.
pub fn lookup_stream(local: Endpoint, remote: Endpoint) -> Option<Arc<InetSocket>> {
    let table = all();
    for socket in table.iter() {
        if !socket.stream {
            continue;
        }
        let hit = match &*socket.inner.lock() {
            Protocol::Tcp(tcb) => {
                tcb.state != tcp::State::Listen
                    && tcb.state != tcp::State::Closed
                    && tcb.remote == remote
                    && matches_local(tcb.local, local)
            }
            Protocol::Udp(_) => false,
        };
        if hit {
            return Some(socket.clone());
        }
    }
    None
}

pub fn lookup_listener(local: Endpoint) -> Option<Arc<InetSocket>> {
    let table = all();
    let mut best: Option<Arc<InetSocket>> = None;
    for socket in table.iter() {
        if !socket.stream {
            continue;
        }
        let exact = match &*socket.inner.lock() {
            Protocol::Tcp(tcb) => {
                if tcb.state != tcp::State::Listen || !matches_local(tcb.local, local) {
                    continue;
                }
                tcb.local.address == local.address
            }
            Protocol::Udp(_) => continue,
        };
        // A socket bound to this address beats one bound to all of them.
        if exact {
            return Some(socket.clone());
        }
        if best.is_none() {
            best = Some(socket.clone());
        }
    }
    best
}

pub fn lookup_datagram(local: Endpoint, remote: Endpoint) -> Option<Arc<InetSocket>> {
    let table = all();
    let mut best: Option<Arc<InetSocket>> = None;
    for socket in table.iter() {
        if socket.stream {
            continue;
        }
        let score = match &*socket.inner.lock() {
            Protocol::Udp(state) => {
                if !state.bound || !matches_local(state.local, local) {
                    continue;
                }
                match state.remote {
                    Some(connected) if connected != remote => continue,
                    Some(_) => 2,
                    None => 1,
                }
            }
            Protocol::Tcp(_) => continue,
        };
        if score == 2 {
            return Some(socket.clone());
        }
        if best.is_none() {
            best = Some(socket.clone());
        }
    }
    best
}

const EPHEMERAL_FIRST: u16 = 49152;
const EPHEMERAL_LAST: u16 = 65535;
static NEXT_EPHEMERAL: AtomicU16 = AtomicU16::new(EPHEMERAL_FIRST);

fn port_taken(stream: bool, port: u16, except: Option<&Arc<InetSocket>>) -> bool {
    let table = all();
    table.iter().any(|socket| {
        if socket.stream != stream {
            return false;
        }
        if except.is_some_and(|other| Arc::ptr_eq(socket, other)) {
            return false;
        }
        // A connection on its way out does not hold the port against a new
        // listener; this is what SO_REUSEADDR buys on Linux, and every server
        // that restarts sets it.
        if socket.is_detached() {
            return false;
        }
        match &*socket.inner.lock() {
            Protocol::Tcp(tcb) => tcb.bound && tcb.local.port == port,
            Protocol::Udp(state) => state.bound && state.local.port == port,
        }
    })
}

fn allocate_port(stream: bool) -> Result<u16, Errno> {
    for _ in 0..(EPHEMERAL_LAST - EPHEMERAL_FIRST) as u32 {
        let port = NEXT_EPHEMERAL.fetch_add(1, Ordering::Relaxed);
        let port = if port < EPHEMERAL_FIRST {
            NEXT_EPHEMERAL.store(EPHEMERAL_FIRST + 1, Ordering::Relaxed);
            EPHEMERAL_FIRST
        } else {
            port
        };
        if !port_taken(stream, port, None) {
            return Ok(port);
        }
    }
    Err(Errno::EADDRINUSE)
}

/// The descriptor is gone. Start whatever the protocol still owes the other
/// end, and let the table hold the socket until that is finished.
pub fn close(socket: &Arc<InetSocket>) {
    socket.detached.store(true, Ordering::Release);
    if !socket.stream {
        unregister(socket);
        crate::sched::io_ready();
        return;
    }
    let children = {
        let mut inner = socket.inner.lock();
        match &mut *inner {
            Protocol::Tcp(tcb) => {
                let children = core::mem::take(&mut tcb.children);
                // Said before anything else, because it is what bounds the
                // states that go on waiting after this returns.
                tcb.abandon();
                if tcb.state == tcp::State::Listen {
                    tcb.state = tcp::State::Closed;
                } else if !tcb.received.is_empty() {
                    // Data arrived that nobody will ever read. The other end
                    // is told so rather than being left to time out.
                    tcb.abort();
                } else {
                    tcb.shutdown_write();
                }
                children
            }
            Protocol::Udp(_) => Vec::new(),
        }
    };
    // A connection that was never accepted is reset: there is no descriptor
    // that could ever read what it has to say.
    for child in children {
        child.detached.store(true, Ordering::Release);
        let _ = child.with_tcp(|tcb| tcb.abort());
        unregister(&child);
    }
    let finished = socket
        .with_tcp(|tcb| tcb.state == tcp::State::Closed)
        .unwrap_or(true);
    if finished {
        unregister(socket);
    }
    // The finish this just sent goes to the other end now if that end is on
    // this machine.
    super::poll();
    crate::sched::io_ready();
}

/// Block until `ready` holds, servicing the stack's own timers while waiting.
///
/// Waiting on the shared readiness queue is what poll, select and epoll
/// already use, so anything that makes a socket ready wakes all of them the
/// same way.
pub fn wait_for(mut ready: impl FnMut() -> bool) -> Result<(), Errno> {
    loop {
        if ready() {
            return Ok(());
        }
        if crate::sched::has_pending_signal() {
            return Err(Errno::EINTR);
        }
        let deadline = super::next_deadline();
        crate::sched::IO_READY.wait_until_or_at(deadline, || {
            ready() || crate::sched::has_pending_signal()
        });
        super::tick();
    }
}
