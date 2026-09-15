//! The socket system calls, for AF_INET.
//!
//! These are the ones the Rust standard library makes for a listening server:
//! socket, setsockopt, bind, listen, accept4, read, write, shutdown, close and
//! getsockname; and the ones a resolver or a datagram server makes: sendto,
//! recvfrom, sendmsg, recvmsg and their batched forms. Descriptors on a
//! connected pair still go to the AF_UNIX code in `file`, which several of
//! these calls hand off to when the descriptor turns out to be one of those.

use super::file;
use crate::abi::*;
use crate::fs::{FileBacking, InetHandle, OpenFile};
use crate::net::ip::Ipv4Addr;
use crate::net::socket::{self as inet, Endpoint, InetSocket, Received};
use crate::sched;
use crate::sync::Spinlock;
use crate::uaccess;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

pub const AF_UNIX: u32 = 1;
pub const AF_INET: u32 = 2;
pub const AF_INET6: u32 = 10;

pub const SOCK_STREAM: u32 = 1;
pub const SOCK_DGRAM: u32 = 2;
const SOCK_NONBLOCK: u32 = 0o4000;
const SOCK_CLOEXEC: u32 = 0o2000000;

const IPPROTO_IP: u32 = 0;
const IPPROTO_TCP: u32 = 6;
const IPPROTO_UDP: u32 = 17;

const SOL_SOCKET: u32 = 1;
const SO_REUSEADDR: u32 = 2;
const SO_TYPE: u32 = 3;
const SO_ERROR: u32 = 4;
const SO_SNDBUF: u32 = 7;
const SO_RCVBUF: u32 = 8;
const SO_ACCEPTCONN: u32 = 30;

const MSG_PEEK: u32 = 0x0002;
const MSG_TRUNC: u32 = 0x0020;
const MSG_DONTWAIT: u32 = 0x0040;
const MSG_ERRQUEUE: u32 = 0x2000;
const MSG_NOSIGNAL: u32 = 0x4000;
const MSG_WAITFORONE: u32 = 0x10000;

const SHUT_RD: u32 = 0;
const SHUT_WR: u32 = 1;
const SHUT_RDWR: u32 = 2;

/// `struct sockaddr_in`: family, port and address, then eight bytes of
/// padding that make it the size of `struct sockaddr`.
const SOCKADDR_IN_LEN: usize = 16;

/// The most one send or receive moves through a kernel buffer. No datagram is
/// larger, and the caller of a stream socket comes back for the rest.
const MAX_TRANSFER: usize = 64 * 1024;

/// The most buffers one message names, and the most messages one batched call
/// handles: Linux's UIO_MAXIOV, which it applies to both.
const MAX_VECTOR: usize = 1024;

/// Where the fields of `struct msghdr` sit. The structure is the same on both
/// 64-bit machines: a pointer and an int for the name, a pointer and a size
/// for the buffers, a pointer and a size for control data, and an int of
/// flags.
const MSGHDR_NAME: u64 = 0;
const MSGHDR_NAMELEN: u64 = 8;
const MSGHDR_IOV: u64 = 16;
const MSGHDR_IOVLEN: u64 = 24;
const MSGHDR_CONTROLLEN: u64 = 40;
const MSGHDR_FLAGS: u64 = 48;

/// `struct mmsghdr`: a `msghdr`, then the count of bytes that message moved,
/// padded to a multiple of eight.
const MMSGHDR_LEN: u64 = 64;
const MMSGHDR_COUNT: u64 = 56;

fn handle_of(fd: i32) -> Result<Arc<InetHandle>, Errno> {
    let file = sched::current().fds.get(fd)?;
    match &file.backing {
        FileBacking::Inet(handle) => Ok(handle.clone()),
        FileBacking::Socket(_) => Err(Errno::EOPNOTSUPP),
        _ => Err(Errno::ENOTSOCK),
    }
}

/// The descriptor and its socket, when it is an internet one.
fn inet_of(fd: i32) -> Result<Option<(Arc<OpenFile>, Arc<InetHandle>)>, Errno> {
    let file = sched::current().fds.get(fd)?;
    match &file.backing {
        FileBacking::Inet(handle) => {
            let handle = handle.clone();
            Ok(Some((file, handle)))
        }
        _ => Ok(None),
    }
}

fn nonblocking(file: &Arc<OpenFile>, flags: u32) -> bool {
    file.flags() & O_NONBLOCK != 0 || flags & MSG_DONTWAIT != 0
}

pub fn read_endpoint(addr: u64, len: u64) -> Result<Endpoint, Errno> {
    if addr == 0 || (len as usize) < SOCKADDR_IN_LEN {
        return Err(Errno::EINVAL);
    }
    let mut bytes = [0u8; SOCKADDR_IN_LEN];
    uaccess::read_bytes(addr, &mut bytes)?;
    let family = u16::from_le_bytes([bytes[0], bytes[1]]) as u32;
    if family != AF_INET {
        return Err(Errno::EAFNOSUPPORT);
    }
    Ok(Endpoint::new(
        Ipv4Addr::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
        u16::from_be_bytes([bytes[2], bytes[3]]),
    ))
}

/// `struct sockaddr_in` as a program reads it, padding included.
///
/// Built whole, so the eight bytes of padding are always written as zero
/// rather than left as whatever the caller's buffer held. Programs compare
/// the whole structure: musl's resolver drops a reply unless its sender
/// compares equal, all sixteen bytes, to the name server it asked.
fn sockaddr_in(endpoint: Endpoint) -> [u8; SOCKADDR_IN_LEN] {
    let mut bytes = [0u8; SOCKADDR_IN_LEN];
    bytes[0..2].copy_from_slice(&(AF_INET as u16).to_le_bytes());
    bytes[2..4].copy_from_slice(&endpoint.port.to_be_bytes());
    bytes[4..8].copy_from_slice(&endpoint.address.to_be_bytes());
    bytes
}

/// Hand a socket name back to a program, as every call that reports one does
/// on Linux.
///
/// `room_addr` points at a length. Going in, it is the room the program
/// offered at `addr`; coming out, it is the length the name has, even when
/// that is more than fitted. Only what fits is copied. An empty name reports a
/// length of zero, which is what a receive on a connected pair or a stream
/// socket says about who sent the bytes.
pub fn put_name(name: &[u8], addr: u64, room_addr: u64) -> Result<(), Errno> {
    let room = uaccess::read_u32(room_addr)? as i32;
    if room < 0 {
        return Err(Errno::EINVAL);
    }
    let n = (room as usize).min(name.len());
    if n > 0 {
        uaccess::write_bytes(addr, &name[..n])?;
    }
    uaccess::write_u32(room_addr, name.len() as u32)
}

/// The name a receive reports for whoever sent what it took.
fn put_sender(from: Option<Endpoint>, addr: u64, room_addr: u64) -> Result<(), Errno> {
    match from {
        Some(endpoint) => put_name(&sockaddr_in(endpoint), addr, room_addr),
        None => put_name(&[], addr, room_addr),
    }
}

fn make_file(socket: Arc<InetSocket>, nonblock: bool) -> Arc<OpenFile> {
    Arc::new(OpenFile {
        backing: FileBacking::Inet(InetHandle::new(socket)),
        offset: Spinlock::new(0),
        flags: Spinlock::new(O_RDWR | if nonblock { O_NONBLOCK } else { 0 }),
        path: alloc::string::String::from("socket:[inet]"),
        listing: Spinlock::new(None),
    })
}

pub fn socket(domain: u32, kind: u32, protocol: u32) -> SysResult {
    if domain != AF_INET {
        // AF_UNIX has no unconnected form here, and there is no IPv6.
        return Err(Errno::EAFNOSUPPORT);
    }
    let stream = match kind & 0xF {
        SOCK_STREAM => true,
        SOCK_DGRAM => false,
        _ => return Err(Errno::EPROTONOSUPPORT),
    };
    let allowed = if stream { IPPROTO_TCP } else { IPPROTO_UDP };
    if protocol != IPPROTO_IP && protocol != allowed {
        return Err(Errno::EPROTONOSUPPORT);
    }
    let file = make_file(InetSocket::new(stream), kind & SOCK_NONBLOCK != 0);
    let fd = sched::current().fds.alloc(file, kind & SOCK_CLOEXEC != 0)?;
    Ok(fd as u64)
}

pub fn bind(fd: i32, addr: u64, len: u64) -> SysResult {
    let handle = handle_of(fd)?;
    let endpoint = read_endpoint(addr, len)?;
    handle.socket.bind(endpoint)?;
    Ok(0)
}

pub fn listen(fd: i32, backlog: i32) -> SysResult {
    let handle = handle_of(fd)?;
    handle.socket.listen(backlog.max(1) as usize)?;
    Ok(0)
}

pub fn accept4(fd: i32, addr: u64, addrlen: u64, flags: u32) -> SysResult {
    let file = sched::current().fds.get(fd)?;
    let handle = match &file.backing {
        FileBacking::Inet(handle) => handle.clone(),
        FileBacking::Socket(_) => return Err(Errno::EOPNOTSUPP),
        _ => return Err(Errno::ENOTSOCK),
    };
    let nonblock = file.flags() & O_NONBLOCK != 0;

    let child = loop {
        if let Some(child) = handle.socket.accept_ready()? {
            break child;
        }
        if nonblock {
            return Err(Errno::EAGAIN);
        }
        inet::wait_for(|| handle.socket.readable())?;
    };

    let peer = child.peer_endpoint().unwrap_or(Endpoint::UNSPECIFIED);
    // The file is made first because dropping it is what resets the
    // connection. The name is written before the descriptor is installed, so a
    // name that cannot be written fails the call without leaving behind a
    // descriptor the program was never told about, which is Linux's order.
    let file = make_file(child, flags & SOCK_NONBLOCK != 0);
    if addr != 0 {
        put_name(&sockaddr_in(peer), addr, addrlen)?;
    }
    let fd = sched::current().fds.alloc(file, flags & SOCK_CLOEXEC != 0)?;
    Ok(fd as u64)
}

pub fn connect(fd: i32, addr: u64, len: u64) -> SysResult {
    let file = sched::current().fds.get(fd)?;
    let handle = match &file.backing {
        FileBacking::Inet(handle) => handle.clone(),
        FileBacking::Socket(_) => return Err(Errno::EISCONN),
        _ => return Err(Errno::ENOTSOCK),
    };
    let nonblock = file.flags() & O_NONBLOCK != 0;
    crate::net::poll();

    // A second connect on a socket already opening reports how far it got
    // rather than starting again.
    let already = handle.socket.connecting();
    if !already {
        let endpoint = read_endpoint(addr, len)?;
        handle.socket.connect(endpoint)?;
    }
    if !handle.socket.stream {
        return Ok(0);
    }
    loop {
        if handle.socket.connection_progress()? {
            return Ok(0);
        }
        if nonblock {
            return Err(if already { Errno::EALREADY } else { Errno::EINPROGRESS });
        }
        inet::wait_for(|| !handle.socket.connecting())?;
    }
}

pub fn getsockname(fd: i32, addr: u64, addrlen: u64) -> SysResult {
    match inet_of(fd)? {
        Some((_, handle)) => {
            put_name(&sockaddr_in(handle.socket.local_endpoint()), addr, addrlen)?;
            Ok(0)
        }
        None => file::getsockname(fd, addr, addrlen),
    }
}

pub fn getpeername(fd: i32, addr: u64, addrlen: u64) -> SysResult {
    match inet_of(fd)? {
        Some((_, handle)) => {
            let peer = handle.socket.peer_endpoint()?;
            put_name(&sockaddr_in(peer), addr, addrlen)?;
            Ok(0)
        }
        None => file::getsockname(fd, addr, addrlen),
    }
}

pub fn setsockopt(fd: i32, _level: u32, _option: u32, _value: u64, _len: u64) -> SysResult {
    // Every option this stack understands is already how it behaves:
    // addresses are reusable, there is no Nagle to turn off, and the buffer
    // sizes are fixed. Refusing them would stop programs that set them as a
    // matter of course, so they are accepted and have no effect.
    sched::current().fds.get(fd)?;
    Ok(0)
}

pub fn getsockopt(fd: i32, level: u32, option: u32, value: u64, len: u64) -> SysResult {
    let Some((_, handle)) = inet_of(fd)? else {
        return file::getsockopt(fd, value, len);
    };
    let answer: u32 = match (level, option) {
        (SOL_SOCKET, SO_ERROR) => handle.socket.take_error().map_or(0, |err| err as u32),
        (SOL_SOCKET, SO_TYPE) => {
            if handle.socket.stream {
                SOCK_STREAM
            } else {
                SOCK_DGRAM
            }
        }
        (SOL_SOCKET, SO_ACCEPTCONN) => handle.socket.is_listening() as u32,
        (SOL_SOCKET, SO_RCVBUF) => crate::net::tcp::RECEIVE_WINDOW as u32,
        (SOL_SOCKET, SO_SNDBUF) => crate::net::tcp::SEND_BUFFER as u32,
        (SOL_SOCKET, SO_REUSEADDR) => 1,
        _ => 0,
    };
    if value != 0 {
        uaccess::write_u32(value, answer)?;
    }
    if len != 0 {
        uaccess::write_u32(len, 4)?;
    }
    Ok(0)
}

pub fn shutdown(fd: i32, how: u32) -> SysResult {
    match inet_of(fd)? {
        Some((_, handle)) => {
            handle
                .socket
                .shutdown(how == SHUT_RD || how == SHUT_RDWR, how == SHUT_WR || how == SHUT_RDWR)?;
            // The finish it just queued goes out now.
            crate::net::poll();
            crate::sched::io_ready();
            Ok(0)
        }
        None => file::shutdown(fd, how),
    }
}

// ---- sending and receiving ------------------------------------------------

/// Send once on an internet socket. sendto and sendmsg both send through
/// this, so the flags they honour are the same flags.
fn send(
    file: &Arc<OpenFile>,
    handle: &InetHandle,
    bytes: &[u8],
    flags: u32,
    destination: Option<Endpoint>,
) -> SysResult {
    let n = handle.socket.write_blocking(
        bytes,
        nonblocking(file, flags),
        destination,
        flags & MSG_NOSIGNAL == 0,
    )?;
    Ok(n as u64)
}

/// Receive once from an internet socket into the buffers a program named.
///
/// recvfrom, recvmsg and recvmmsg all receive through this, and what it hands
/// back carries the sender. recvmsg used to read through readv, which has no
/// way to say who sent what it read: it never wrote the name, so musl's
/// resolver found its own bind address where the name server's should have
/// been and dropped every answer, while recvfrom on the same socket reported
/// the sender correctly.
fn receive(
    file: &Arc<OpenFile>,
    handle: &InetHandle,
    buffers: &[IoVec],
    flags: u32,
) -> Result<Received, Errno> {
    // This stack queues no errors, so the error queue is always empty. Reading
    // ordinary data in its place would hand the program a datagram as if it
    // were an error report, and the datagram would be gone.
    if flags & MSG_ERRQUEUE != 0 {
        return Err(Errno::EAGAIN);
    }
    let mut room = 0usize;
    for buffer in buffers {
        let take = (buffer.len as usize).min(MAX_TRANSFER - room);
        // Checked before anything is taken, so a bad pointer fails the call
        // instead of discarding the datagram.
        uaccess::validate(buffer.base, take as u64, true)?;
        room += take;
    }
    let mut bytes = vec![0u8; room];
    let received =
        handle.socket.read_blocking(&mut bytes, nonblocking(file, flags), flags & MSG_PEEK != 0)?;
    let mut copied = 0;
    for buffer in buffers {
        if copied == received.taken {
            break;
        }
        let n = (buffer.len as usize).min(received.taken - copied);
        uaccess::write_bytes(buffer.base, &bytes[copied..copied + n])?;
        copied += n;
    }
    Ok(received)
}

/// What a receive returns: the bytes taken, or, when MSG_TRUNC is passed on a
/// datagram socket, the length the datagram had, which is how a program finds
/// out how large a buffer it needed.
fn reported_length(handle: &InetHandle, received: &Received, flags: u32) -> u64 {
    if flags & MSG_TRUNC != 0 && !handle.socket.stream {
        received.length as u64
    } else {
        received.taken as u64
    }
}

pub fn sendto(fd: i32, buf: u64, len: usize, flags: u32, addr: u64, addrlen: u64) -> SysResult {
    let Some((file, handle)) = inet_of(fd)? else {
        return file::sendto(fd, buf, len, addr);
    };
    let destination = if addr != 0 && addrlen != 0 {
        Some(read_endpoint(addr, addrlen)?)
    } else {
        None
    };
    let mut bytes = vec![0u8; len.min(MAX_TRANSFER)];
    uaccess::read_bytes(buf, &mut bytes)?;
    send(&file, &handle, &bytes, flags, destination)
}

pub fn recvfrom(
    fd: i32,
    buf: u64,
    len: usize,
    flags: u32,
    addr: u64,
    addrlen: u64,
) -> SysResult {
    let Some((file, handle)) = inet_of(fd)? else {
        return file::recvfrom(fd, buf, len, addr, addrlen);
    };
    let buffers = [IoVec { base: buf, len: len as u64 }];
    let received = receive(&file, &handle, &buffers, flags)?;
    if addr != 0 {
        put_sender(received.from, addr, addrlen)?;
    }
    Ok(reported_length(&handle, &received, flags))
}

/// The fields of a `struct msghdr` the kernel reads.
struct MessageHeader {
    name: u64,
    name_len: i32,
    iov: u64,
    iov_len: u64,
    control_len: u64,
}

impl MessageHeader {
    fn read(msg: u64) -> Result<MessageHeader, Errno> {
        let header = MessageHeader {
            name: uaccess::read_u64(msg + MSGHDR_NAME)?,
            name_len: uaccess::read_u32(msg + MSGHDR_NAMELEN)? as i32,
            iov: uaccess::read_u64(msg + MSGHDR_IOV)?,
            iov_len: uaccess::read_u64(msg + MSGHDR_IOVLEN)?,
            control_len: uaccess::read_u64(msg + MSGHDR_CONTROLLEN)?,
        };
        if header.name != 0 && header.name_len < 0 {
            return Err(Errno::EINVAL);
        }
        if header.iov_len > MAX_VECTOR as u64 {
            return Err(Errno::EMSGSIZE);
        }
        Ok(header)
    }

    fn buffers(&self) -> Result<Vec<IoVec>, Errno> {
        uaccess::read_iovecs(self.iov, self.iov_len as usize)
    }

    fn has_name(&self) -> bool {
        self.name != 0 && self.name_len != 0
    }
}

pub fn sendmsg(fd: i32, msg: u64, flags: u32) -> SysResult {
    let header = MessageHeader::read(msg)?;
    // Control data carries descriptors and per-packet options, and nothing
    // here would act on either. Refusing it keeps a program from believing an
    // option took effect.
    if header.control_len != 0 {
        return Err(Errno::EOPNOTSUPP);
    }
    let Some((file, handle)) = inet_of(fd)? else {
        // A connected pair has nowhere else to send to, as with sendto.
        if header.has_name() {
            return Err(Errno::EISCONN);
        }
        return file::writev(fd, header.iov, header.iov_len as usize);
    };
    let destination = if header.has_name() {
        Some(read_endpoint(header.name, header.name_len as u64)?)
    } else {
        None
    };
    // One message is one datagram, however many buffers it is spread over.
    let mut bytes = Vec::new();
    for buffer in header.buffers()? {
        let take = (buffer.len as usize).min(MAX_TRANSFER - bytes.len());
        let start = bytes.len();
        bytes.resize(start + take, 0);
        uaccess::read_bytes(buffer.base, &mut bytes[start..])?;
    }
    send(&file, &handle, &bytes, flags, destination)
}

pub fn recvmsg(fd: i32, msg: u64, flags: u32) -> SysResult {
    let header = MessageHeader::read(msg)?;
    let (count, from, truncated) = match inet_of(fd)? {
        Some((file, handle)) => {
            let received = receive(&file, &handle, &header.buffers()?, flags)?;
            let truncated = received.length > received.taken;
            (reported_length(&handle, &received, flags), received.from, truncated)
        }
        // A connected pair reads the way readv does and has no name for the
        // other end.
        None => (file::readv(fd, header.iov, header.iov_len as usize)?, None, false),
    };
    if header.name != 0 {
        // The room the program offered is the length field itself, so the
        // name's length goes back into that field.
        put_sender(from, header.name, msg + MSGHDR_NAMELEN)?;
    }
    // No control data is ever produced. The length is a size, eight bytes,
    // and all of it is written.
    uaccess::write_u64(msg + MSGHDR_CONTROLLEN, 0)?;
    uaccess::write_u32(msg + MSGHDR_FLAGS, if truncated { MSG_TRUNC } else { 0 })?;
    Ok(count)
}

/// The number of bytes the buffers of the message at `msg` add up to.
fn message_length(msg: u64) -> Result<u64, Errno> {
    let header = MessageHeader::read(msg)?;
    Ok(header.buffers()?.iter().fold(0u64, |total, buffer| total.saturating_add(buffer.len)))
}

/// `sendmmsg`: sendmsg for each message in turn.
///
/// Stops at the first message that fails, or that did not go out whole, and
/// reports how many went; an error is reported only when none did. That is
/// Linux's rule, and the counts it leaves in each message tell the program
/// where to carry on.
pub fn sendmmsg(fd: i32, vector: u64, count: u32, flags: u32) -> SysResult {
    let count = (count as usize).min(MAX_VECTOR);
    let mut done = 0usize;
    while done < count {
        let entry = vector + done as u64 * MMSGHDR_LEN;
        let sent = match sendmsg(fd, entry, flags) {
            Ok(sent) => sent,
            Err(err) if done == 0 => return Err(err),
            Err(_) => break,
        };
        uaccess::write_u32(entry + MMSGHDR_COUNT, sent as u32)?;
        done += 1;
        if sent < message_length(entry)? {
            break;
        }
    }
    Ok(done as u64)
}

/// `recvmmsg`: recvmsg for each message in turn, under the rules Linux gives
/// the batch.
///
/// An error is reported only when no message was received; after the first it
/// ends the batch. MSG_WAITFORONE waits for the first message and no other.
/// The timeout is looked at only after each message, so it does not bound the
/// wait for the first one. That is Linux's behaviour, which its manual page
/// records as a bug, and programs written against it pass MSG_DONTWAIT or
/// poll first.
pub fn recvmmsg(fd: i32, vector: u64, count: u32, flags: u32, timeout: u64) -> SysResult {
    let deadline = if timeout == 0 {
        None
    } else {
        let spec: Timespec = uaccess::read_struct(timeout)?;
        if spec.tv_sec < 0 || spec.tv_nsec < 0 || spec.tv_nsec >= 1_000_000_000 {
            return Err(Errno::EINVAL);
        }
        let nanos = (spec.tv_sec as u64)
            .saturating_mul(1_000_000_000)
            .saturating_add(spec.tv_nsec as u64);
        Some(crate::time::monotonic_ns().saturating_add(nanos))
    };
    let count = (count as usize).min(MAX_VECTOR);
    let mut flags = flags;
    let mut done = 0usize;
    while done < count {
        let entry = vector + done as u64 * MMSGHDR_LEN;
        let taken = match recvmsg(fd, entry, flags & !MSG_WAITFORONE) {
            Ok(taken) => taken,
            Err(err) if done == 0 => return Err(err),
            Err(_) => break,
        };
        uaccess::write_u32(entry + MMSGHDR_COUNT, taken as u32)?;
        done += 1;
        if flags & MSG_WAITFORONE != 0 {
            flags |= MSG_DONTWAIT;
        }
        if deadline.is_some_and(|deadline| crate::time::monotonic_ns() >= deadline) {
            break;
        }
    }
    if let Some(deadline) = deadline {
        let left = deadline.saturating_sub(crate::time::monotonic_ns());
        let spec = Timespec {
            tv_sec: (left / 1_000_000_000) as i64,
            tv_nsec: (left % 1_000_000_000) as i64,
        };
        uaccess::write_struct(timeout, &spec)?;
    }
    Ok(done as u64)
}
