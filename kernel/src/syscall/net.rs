//! The socket system calls, for AF_INET.
//!
//! These are the ones the Rust standard library makes for a listening server:
//! socket, setsockopt, bind, listen, accept4, read, write, shutdown, close and
//! getsockname. Descriptors on a connected pair still go to the AF_UNIX code
//! in `file`, which several of these calls hand off to when the descriptor
//! turns out to be one of those.

use super::file;
use crate::abi::*;
use crate::fs::{FileBacking, InetHandle, OpenFile};
use crate::net::ip::Ipv4Addr;
use crate::net::socket::{self as inet, Endpoint, InetSocket};
use crate::sched;
use crate::sync::Spinlock;
use crate::uaccess;
use alloc::sync::Arc;
use alloc::vec;

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
const MSG_DONTWAIT: u32 = 0x0040;
const MSG_NOSIGNAL: u32 = 0x4000;

const SHUT_RD: u32 = 0;
const SHUT_WR: u32 = 1;
const SHUT_RDWR: u32 = 2;

/// `struct sockaddr_in`: family, port and address, then eight bytes of
/// padding that make it the size of `struct sockaddr`.
const SOCKADDR_IN_LEN: usize = 16;

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

/// Write an address out, truncating to the room the caller offered but
/// reporting the length it would have taken, which is what Linux does.
pub fn write_endpoint(addr: u64, addrlen: u64, endpoint: Endpoint) -> Result<(), Errno> {
    if addrlen == 0 && addr == 0 {
        return Ok(());
    }
    let capacity = if addrlen != 0 {
        uaccess::read_u32(addrlen)? as usize
    } else {
        SOCKADDR_IN_LEN
    };
    if addr != 0 {
        let mut bytes = [0u8; SOCKADDR_IN_LEN];
        bytes[0..2].copy_from_slice(&(AF_INET as u16).to_le_bytes());
        bytes[2..4].copy_from_slice(&endpoint.port.to_be_bytes());
        bytes[4..8].copy_from_slice(&endpoint.address.to_be_bytes());
        let n = capacity.min(SOCKADDR_IN_LEN);
        uaccess::write_bytes(addr, &bytes[..n])?;
    }
    if addrlen != 0 {
        uaccess::write_u32(addrlen, SOCKADDR_IN_LEN as u32)?;
    }
    Ok(())
}

fn make_file(socket: Arc<InetSocket>, nonblock: bool) -> Arc<OpenFile> {
    Arc::new(OpenFile {
        backing: FileBacking::Inet(InetHandle::new(socket)),
        offset: Spinlock::new(0),
        flags: Spinlock::new(O_RDWR | if nonblock { O_NONBLOCK } else { 0 }),
        path: alloc::string::String::from("socket:[inet]"),
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
    let file = make_file(child, flags & SOCK_NONBLOCK != 0);
    let fd = sched::current().fds.alloc(file, flags & SOCK_CLOEXEC != 0)?;
    write_endpoint(addr, addrlen, peer)?;
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
            let mut local = handle.socket.local_endpoint();
            // A socket bound to every address still reports one that can be
            // connected to once it is listening on a real interface.
            if local.address.is_unspecified() {
                local.address = Ipv4Addr::UNSPECIFIED;
            }
            write_endpoint(addr, addrlen, local)?;
            Ok(0)
        }
        None => file::getsockname(fd, addr, addrlen),
    }
}

pub fn getpeername(fd: i32, addr: u64, addrlen: u64) -> SysResult {
    match inet_of(fd)? {
        Some((_, handle)) => {
            let peer = handle.socket.peer_endpoint()?;
            write_endpoint(addr, addrlen, peer)?;
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

pub fn sendto(fd: i32, buf: u64, len: usize, flags: u32, addr: u64, addrlen: u64) -> SysResult {
    let Some((file, handle)) = inet_of(fd)? else {
        return file::sendto(fd, buf, len, addr);
    };
    let destination = if addr != 0 && addrlen != 0 {
        Some(read_endpoint(addr, addrlen)?)
    } else {
        None
    };
    let len = len.min(64 * 1024);
    let mut bytes = vec![0u8; len];
    uaccess::read_bytes(buf, &mut bytes)?;
    let n = handle.socket.write_blocking(
        &bytes,
        nonblocking(&file, flags),
        destination,
        flags & MSG_NOSIGNAL == 0,
    )?;
    Ok(n as u64)
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
    let len = len.min(64 * 1024);
    uaccess::validate(buf, len as u64, true)?;
    let mut bytes = vec![0u8; len];
    let (n, from) = handle.socket.read_blocking(
        &mut bytes,
        nonblocking(&file, flags),
        flags & MSG_PEEK != 0,
    )?;
    uaccess::write_bytes(buf, &bytes[..n])?;
    if addr != 0 {
        write_endpoint(addr, addrlen, from)?;
    }
    Ok(n as u64)
}
