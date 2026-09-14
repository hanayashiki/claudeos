//! What the socket calls say about names: who sent a datagram, who is at the
//! other end of a connection, and what a socket is bound to.
//!
//! These make the system calls directly. `std::net` receives a datagram with
//! recvfrom and never with recvmsg, which is what a C library's resolver
//! uses: musl's, from 1.2.4, sends its query with sendto, takes the answer
//! with recvmsg, and drops the answer unless the name recvmsg reports compares
//! equal, all sixteen bytes of it, to the name server it asked. The first
//! check below is that comparison. The kernel used to answer recvmsg without
//! writing the name at all, so every answer was dropped and every lookup by
//! name timed out.
//!
//! A check that a kernel gets wrong has to fail rather than wait for ever, so
//! wherever getting a flag wrong would leave a receive waiting, something is
//! queued behind it for that receive to take, and the socket is drained before
//! the next check.

use crate::sys;
use crate::Report;
use std::io::Write;
use std::net::{SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::os::unix::io::AsRawFd;
use std::time::{Duration, Instant};

#[cfg(target_arch = "x86_64")]
mod numbers {
    pub const ACCEPT: u64 = 43;
    pub const RECVFROM: u64 = 45;
    pub const SENDMSG: u64 = 46;
    pub const RECVMSG: u64 = 47;
    pub const GETSOCKNAME: u64 = 51;
    pub const RECVMMSG: u64 = 299;
    pub const SENDMMSG: u64 = 307;
}

#[cfg(target_arch = "aarch64")]
mod numbers {
    pub const ACCEPT: u64 = 202;
    pub const GETSOCKNAME: u64 = 204;
    pub const RECVFROM: u64 = 207;
    pub const SENDMSG: u64 = 211;
    pub const RECVMSG: u64 = 212;
    pub const RECVMMSG: u64 = 243;
    pub const SENDMMSG: u64 = 269;
}

use numbers::*;

const AF_INET: u16 = 2;
const MSG_PEEK: u64 = 0x2;
const MSG_TRUNC: i32 = 0x20;
const MSG_DONTWAIT: u64 = 0x40;
const MSG_WAITFORONE: u64 = 0x10000;
const EAGAIN: i64 = -11;

#[repr(C)]
struct IoVec {
    base: *mut u8,
    len: usize,
}

/// `struct msghdr` as the kernel reads it, with the length of the buffers and
/// of the control data at their full width.
#[repr(C)]
struct MsgHdr {
    name: *mut u8,
    name_len: u32,
    iov: *mut IoVec,
    iov_len: usize,
    control: *mut u8,
    control_len: usize,
    flags: i32,
}

/// `struct mmsghdr`: a message, and the count of bytes it moved.
#[repr(C)]
struct MMsgHdr {
    header: MsgHdr,
    len: u32,
}

/// The sixteen bytes of `struct sockaddr_in` for `address`, padding and all.
fn sockaddr_in(address: SocketAddr) -> [u8; 16] {
    let SocketAddr::V4(address) = address else {
        return [0; 16];
    };
    let mut bytes = [0u8; 16];
    bytes[0..2].copy_from_slice(&AF_INET.to_ne_bytes());
    bytes[2..4].copy_from_slice(&address.port().to_be_bytes());
    bytes[4..8].copy_from_slice(&address.ip().octets());
    bytes
}

/// What one recvmsg returned and left in the header.
struct Receipt {
    result: i64,
    name_len: u32,
    control_len: usize,
    flags: i32,
}

/// Receive one message into `data`, offering `name` with `name_room` bytes of
/// room for the sender, and `control` for control data.
fn recvmsg(
    fd: i32,
    data: &mut [u8],
    name: &mut [u8],
    name_room: u32,
    control: &mut [u8],
    flags: u64,
) -> Receipt {
    let mut buffer = IoVec { base: data.as_mut_ptr(), len: data.len() };
    let mut header = MsgHdr {
        name: name.as_mut_ptr(),
        name_len: name_room,
        iov: &mut buffer,
        iov_len: 1,
        control: control.as_mut_ptr(),
        control_len: control.len(),
        // Something other than what the kernel should leave, so a field it
        // never writes shows.
        flags: -1,
    };
    let result = unsafe {
        sys::syscall(RECVMSG, fd as u64, &mut header as *mut MsgHdr as u64, flags, 0, 0, 0)
    };
    Receipt {
        result,
        name_len: header.name_len,
        control_len: header.control_len,
        flags: header.flags,
    }
}

/// Take whatever is queued on `socket` without waiting. It goes through the
/// descriptor's own non-blocking flag rather than MSG_DONTWAIT, which is one of
/// the things being checked.
fn drain(socket: &UdpSocket) {
    let mut data = [0u8; 256];
    let _ = socket.set_nonblocking(true);
    while socket.recv_from(&mut data).is_ok() {}
    let _ = socket.set_nonblocking(false);
}

/// Send `texts` to `to` from a socket of its own, `after` from now.
fn send_later(to: SocketAddr, texts: &'static [&'static [u8]], after: Duration) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        std::thread::sleep(after);
        let socket = UdpSocket::bind("127.0.0.1:0").expect("bind");
        for text in texts {
            let _ = socket.send_to(text, to);
        }
    })
}

pub fn run(report: &mut Report) {
    let one = UdpSocket::bind("127.0.0.1:0").expect("bind");
    let two = UdpSocket::bind("127.0.0.1:0").expect("bind");
    let one_address = one.local_addr().expect("local_addr");
    let two_address = two.local_addr().expect("local_addr");
    let one_name = sockaddr_in(one_address);
    let two_name = sockaddr_in(two_address);
    let fd = two.as_raw_fd();
    let mut data = [0u8; 256];

    // musl binds its socket to {AF_INET, port 0, 0.0.0.0} and then receives
    // into that same structure, offering its size; a name that is not written
    // leaves the bind address there.
    one.send_to(b"answer", two_address).expect("send_to");
    let mut name = [0u8; 16];
    name[0..2].copy_from_slice(&AF_INET.to_ne_bytes());
    let got = recvmsg(fd, &mut data, &mut name, 16, &mut [], 0);
    report.check(
        "recvmsg names the sender, byte for byte as a resolver compares it",
        got.result == 6 && got.name_len == 16 && name == one_name,
        format!(
            "returned {}, name {:02x?} of length {}, sent from {:02x?}",
            got.result, name, got.name_len, one_name
        ),
    );

    one.send_to(b"padding", two_address).expect("send_to");
    let mut name = [0xAAu8; 32];
    let mut control = [0xAAu8; 32];
    let got = recvmsg(fd, &mut data, &mut name, 32, &mut control, 0);
    report.check(
        "given more room, it writes sixteen bytes with the padding zeroed",
        got.result == 7 && got.name_len == 16 && name[..16] == one_name && name[16..] == [0xAA; 16],
        format!("returned {}, name {:02x?} of length {}", got.result, name, got.name_len),
    );
    report.check(
        "and reports no control data and no flags",
        got.control_len == 0 && got.flags == 0,
        format!("control length {}, flags {:#x}", got.control_len, got.flags),
    );

    // A datagram larger than the buffer: what fits is taken, the rest is
    // discarded, and the flags say so.
    one.send_to(&[7u8; 100], two_address).expect("send_to");
    one.send_to(b"next", two_address).expect("send_to");
    let mut small = [0u8; 10];
    let mut name = [0u8; 16];
    let cut = recvmsg(fd, &mut small, &mut name, 16, &mut [], 0);
    let next = recvmsg(fd, &mut data, &mut name, 16, &mut [], 0);
    report.check(
        "a datagram cut short is flagged MSG_TRUNC, and the rest of it is gone",
        cut.result == 10 && cut.flags & MSG_TRUNC != 0 && next.result == 4 && &data[..4] == b"next",
        format!(
            "the short receive returned {} with flags {:#x}; the next returned {}",
            cut.result, cut.flags, next.result
        ),
    );
    drain(&two);

    one.send_to(&[7u8; 100], two_address).expect("send_to");
    let whole = recvmsg(fd, &mut small, &mut name, 16, &mut [], MSG_TRUNC as u64);
    report.check(
        "MSG_TRUNC asks for the length the datagram had",
        whole.result == 100,
        format!("returned {}", whole.result),
    );
    drain(&two);

    // A second datagram behind the first, for the receive after the peek to
    // take if the peek took the first.
    one.send_to(b"peek", two_address).expect("send_to");
    one.send_to(b"then", two_address).expect("send_to");
    let mut peeked_name = [0u8; 16];
    let peeked = recvmsg(fd, &mut data, &mut peeked_name, 16, &mut [], MSG_PEEK);
    let peeked_text = data[..4].to_vec();
    let taken = recvmsg(fd, &mut data, &mut name, 16, &mut [], 0);
    let taken_text = data[..4].to_vec();
    report.check(
        "a peek names the sender and leaves the datagram to be taken",
        peeked.result == 4
            && peeked_name == one_name
            && peeked_text == b"peek"
            && taken.result == 4
            && taken_text == b"peek",
        format!(
            "the peek returned {} with {:?}, the receive after it {} with {:?}",
            peeked.result,
            String::from_utf8_lossy(&peeked_text),
            taken.result,
            String::from_utf8_lossy(&taken_text)
        ),
    );
    drain(&two);

    // Nothing is queued. A receive that ignored MSG_DONTWAIT would wait for
    // the datagram sent later, and take it.
    let late = send_later(two_address, &[b"late"], Duration::from_millis(1500));
    let started = Instant::now();
    let empty = recvmsg(fd, &mut data, &mut name, 16, &mut [], MSG_DONTWAIT);
    let waited = started.elapsed();
    report.check(
        "MSG_DONTWAIT on an empty socket answers EAGAIN at once",
        empty.result == EAGAIN && waited < Duration::from_millis(1000),
        format!("returned {} after {:?}", empty.result, waited),
    );
    let _ = late.join();
    drain(&two);

    // One message spread over two buffers, sent to the name it carries.
    let mut first = *b"ab";
    let mut second = *b"cd";
    let mut buffers = [
        IoVec { base: first.as_mut_ptr(), len: 2 },
        IoVec { base: second.as_mut_ptr(), len: 2 },
    ];
    let mut to = two_name;
    let header = MsgHdr {
        name: to.as_mut_ptr(),
        name_len: 16,
        iov: buffers.as_mut_ptr(),
        iov_len: 2,
        control: std::ptr::null_mut(),
        control_len: 0,
        flags: 0,
    };
    let sent = unsafe {
        sys::syscall(SENDMSG, one.as_raw_fd() as u64, &header as *const MsgHdr as u64, 0, 0, 0, 0)
    };
    let arrived = if sent == 4 { two.recv_from(&mut data).ok() } else { None };
    report.check(
        "sendmsg sends to the name it is given, as one datagram",
        sent == 4 && arrived == Some((4, one_address)) && &data[..4] == b"abcd",
        format!("sendmsg returned {}, and what arrived was {:?}", sent, arrived),
    );
    drain(&two);

    recvmmsg_and_sendmmsg(report, &one, &two, one_name, two_name);
    getsockname_room(report, &two, two_name);
    names_on_a_connection(report);
}

/// A vector of `count` messages, each with an eight-byte buffer and room for
/// a name, all of it owned here so the pointers stay good.
struct Batch {
    data: Vec<[u8; 8]>,
    names: Vec<[u8; 16]>,
    buffers: Vec<IoVec>,
    messages: Vec<MMsgHdr>,
}

impl Batch {
    fn new(count: usize) -> Batch {
        let mut batch = Batch {
            data: vec![[0u8; 8]; count],
            names: vec![[0u8; 16]; count],
            buffers: Vec::new(),
            messages: Vec::new(),
        };
        for i in 0..count {
            batch.buffers.push(IoVec { base: batch.data[i].as_mut_ptr(), len: 8 });
        }
        for i in 0..count {
            batch.messages.push(MMsgHdr {
                header: MsgHdr {
                    name: batch.names[i].as_mut_ptr(),
                    name_len: 16,
                    iov: batch.buffers.as_mut_ptr().wrapping_add(i),
                    iov_len: 1,
                    control: std::ptr::null_mut(),
                    control_len: 0,
                    flags: -1,
                },
                len: 0,
            });
        }
        batch
    }
}

fn recvmmsg_and_sendmmsg(
    report: &mut Report,
    one: &UdpSocket,
    two: &UdpSocket,
    one_name: [u8; 16],
    two_name: [u8; 16],
) {
    let two_address = two.local_addr().expect("local_addr");
    for text in [b"m0", b"m1", b"m2"] {
        one.send_to(text, two_address).expect("send_to");
    }
    let mut batch = Batch::new(4);
    let count = unsafe {
        sys::syscall(
            RECVMMSG,
            two.as_raw_fd() as u64,
            batch.messages.as_mut_ptr() as u64,
            4,
            MSG_DONTWAIT,
            0,
            0,
        )
    };
    let right = count == 3
        && (0..3).all(|i| {
            batch.messages[i].len == 2
                && batch.data[i][..2] == [b'm', b'0' + i as u8]
                && batch.messages[i].header.name_len == 16
                && batch.names[i] == one_name
        });
    report.check(
        "recvmmsg takes every queued datagram, each with its sender",
        right,
        format!(
            "returned {}; lengths {:?}; names {:02x?}",
            count,
            batch.messages.iter().map(|m| m.len).collect::<Vec<_>>(),
            batch.names
        ),
    );
    drain(two);

    // Two now and two later: a batch that waited to fill instead of for the
    // first datagram takes all four, and fails here rather than waiting.
    one.send_to(b"w0", two_address).expect("send_to");
    one.send_to(b"w1", two_address).expect("send_to");
    let late = send_later(two_address, &[b"w2", b"w3"], Duration::from_millis(1500));
    let mut batch = Batch::new(4);
    let count = unsafe {
        sys::syscall(
            RECVMMSG,
            two.as_raw_fd() as u64,
            batch.messages.as_mut_ptr() as u64,
            4,
            MSG_WAITFORONE,
            0,
            0,
        )
    };
    report.check(
        "MSG_WAITFORONE waits for the first datagram and not for the rest",
        count == 2 && batch.data[0][..2] == *b"w0" && batch.data[1][..2] == *b"w1",
        format!("returned {}", count),
    );
    let _ = late.join();
    drain(two);

    let mut outgoing = Batch::new(2);
    outgoing.data[0][..2].copy_from_slice(b"s0");
    outgoing.data[1][..2].copy_from_slice(b"s1");
    for i in 0..2 {
        outgoing.names[i] = two_name;
        outgoing.buffers[i].len = 2;
        outgoing.messages[i].header.flags = 0;
    }
    let sent = unsafe {
        sys::syscall(
            SENDMMSG,
            one.as_raw_fd() as u64,
            outgoing.messages.as_mut_ptr() as u64,
            2,
            0,
            0,
            0,
        )
    };
    let mut arrived = Vec::new();
    let mut data = [0u8; 16];
    for _ in 0..sent.max(0) {
        if let Ok((n, _)) = two.recv_from(&mut data) {
            arrived.push(data[..n].to_vec());
        }
    }
    report.check(
        "sendmmsg sends each message and records what each one sent",
        sent == 2
            && outgoing.messages[0].len == 2
            && outgoing.messages[1].len == 2
            && arrived == [b"s0".to_vec(), b"s1".to_vec()],
        format!(
            "returned {}; lengths {} and {}; arrived {:?}",
            sent, outgoing.messages[0].len, outgoing.messages[1].len, arrived
        ),
    );
    drain(two);
}

/// A name longer than the room offered is cut to the room, and the length
/// that comes back is the whole length.
fn getsockname_room(report: &mut Report, socket: &UdpSocket, name: [u8; 16]) {
    let mut bound = [0xAAu8; 16];
    let mut room: u32 = 4;
    let rc = unsafe {
        sys::syscall(
            GETSOCKNAME,
            socket.as_raw_fd() as u64,
            bound.as_mut_ptr() as u64,
            &mut room as *mut u32 as u64,
            0,
            0,
            0,
        )
    };
    report.check(
        "getsockname copies only the room offered and reports the whole length",
        rc == 0 && room == 16 && bound[..4] == name[..4] && bound[4..] == [0xAA; 12],
        format!("returned {}, length {}, bytes {:02x?}", rc, room, bound),
    );
}

/// accept names the peer; a receive on the connection names nobody, because a
/// stream's bytes come from the other end of the connection.
fn names_on_a_connection(report: &mut Report) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("local_addr");
    let client = std::thread::spawn(move || -> std::io::Result<(TcpStream, SocketAddr)> {
        let mut stream = TcpStream::connect(address)?;
        stream.write_all(b"x")?;
        let local = stream.local_addr()?;
        Ok((stream, local))
    });
    let mut peer = [0xAAu8; 32];
    let mut room: u32 = 32;
    let accepted = unsafe {
        sys::syscall(
            ACCEPT,
            listener.as_raw_fd() as u64,
            peer.as_mut_ptr() as u64,
            &mut room as *mut u32 as u64,
            0,
            0,
            0,
        )
    };
    let client = client.join();
    let (stream, local) = match (accepted, client) {
        (fd, Ok(Ok(pair))) if fd >= 0 => pair,
        (fd, other) => {
            report.check(
                "accept names the peer in sixteen bytes with the padding zeroed",
                false,
                format!("accept returned {}; the client {:?}", fd, other.map(|r| r.map(|(_, a)| a))),
            );
            return;
        }
    };
    report.check(
        "accept names the peer in sixteen bytes with the padding zeroed",
        room == 16 && peer[..16] == sockaddr_in(local) && peer[16..] == [0xAA; 16],
        format!("length {}, bytes {:02x?}, the client is {}", room, peer, local),
    );

    let mut byte = [0u8; 8];
    let mut from = [0xAAu8; 16];
    let mut from_room: u32 = 16;
    let n = unsafe {
        sys::syscall(
            RECVFROM,
            accepted as u64,
            byte.as_mut_ptr() as u64,
            byte.len() as u64,
            0,
            from.as_mut_ptr() as u64,
            &mut from_room as *mut u32 as u64,
        )
    };
    report.check(
        "recvfrom on a connection names nobody",
        n == 1 && from_room == 0 && from == [0xAA; 16],
        format!("returned {}, length {}, bytes {:02x?}", n, from_room, from),
    );
    sys::close(accepted as i32);
    drop(stream);
}
