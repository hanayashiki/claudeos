//! Internet sockets from userland, through the Rust standard library.
//!
//!   inet            the socket system calls, over the loopback address
//!   inet serve [port] [count]
//!                   an HTTP server, for talking to from outside the machine
//!
//! Nothing here knows it is running on claudeos: this is `std::net` making the
//! same system calls it would make on Linux.
//!
//! The memory, program loader and rename checks ride along here rather than
//! in a suite of their own: the harness boots the machine once per suite and
//! reads one summary line out of each, and these are things only a real process
//! can ask about.

mod loader;
mod memory;
mod rename;
mod sys;
mod threads;

use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::time::Duration;

struct Report {
    passed: usize,
    failed: usize,
}

impl Report {
    fn check(&mut self, name: &str, condition: bool, detail: String) {
        if condition {
            self.passed += 1;
            println!("PASS  {}", name);
        } else {
            self.failed += 1;
            println!("FAIL  {}: {}", name, detail);
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(|s| s.as_str()) {
        Some("serve") => {
            let port: u16 = args.get(2).and_then(|p| p.parse().ok()).unwrap_or(8080);
            let count: usize = args.get(3).and_then(|c| c.parse().ok()).unwrap_or(usize::MAX);
            serve(port, count);
        }
        // Started by the memory checks, in a process of its own.
        Some("leave-a-thread") => memory::leave_a_thread_and_exec(),
        _ => test(),
    }
}

// ---- the self test --------------------------------------------------------

fn test() {
    let mut report = Report { passed: 0, failed: 0 };

    println!("-- a listening socket --");
    listener_names(&mut report);
    println!("-- a connection, end to end --");
    round_trip(&mut report);
    println!("-- a stream longer than one segment --");
    bulk(&mut report);
    println!("-- refusals and errors --");
    refusals(&mut report);
    println!("-- datagrams --");
    datagrams(&mut report);
    println!("-- an HTTP exchange --");
    http(&mut report);
    println!("-- memory management --");
    memory::run(&mut report);
    println!("-- what threads share --");
    threads::run(&mut report);
    println!("-- the program loader --");
    loader::run(&mut report);
    println!("-- renaming --");
    rename::run(&mut report);

    println!();
    println!("=== {} passed, {} failed ===", report.passed, report.failed);
    std::process::exit(if report.failed == 0 { 0 } else { 1 });
}

/// bind, getsockname and listen.
fn listener_names(report: &mut Report) {
    let listener = match TcpListener::bind("127.0.0.1:0") {
        Ok(listener) => listener,
        Err(err) => {
            report.check("bind to an unused port", false, format!("{}", err));
            return;
        }
    };
    let local = listener.local_addr();
    report.check("bind to an unused port", true, String::new());
    match local {
        Ok(SocketAddr::V4(address)) => {
            report.check(
                "the port that was chosen is reported back",
                address.port() != 0,
                format!("{}", address),
            );
            report.check(
                "the address that was asked for is reported back",
                address.ip().octets() == [127, 0, 0, 1],
                format!("{}", address),
            );
        }
        other => report.check("getsockname", false, format!("{:?}", other)),
    }

    // The second bind to a port in use has to be refused.
    let port = listener.local_addr().unwrap().port();
    let again = TcpListener::bind(("127.0.0.1", port));
    report.check(
        "a port in use cannot be bound twice",
        again.is_err(),
        format!("{:?}", again.map(|l| l.local_addr())),
    );
}

/// A whole connection: connect, accept, both directions, and an orderly close.
fn round_trip(report: &mut Report) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("local_addr");

    let client = std::thread::spawn(move || -> std::io::Result<Vec<u8>> {
        let mut stream = TcpStream::connect(address)?;
        stream.write_all(b"question")?;
        stream.shutdown(Shutdown::Write)?;
        let mut answer = Vec::new();
        stream.read_to_end(&mut answer)?;
        Ok(answer)
    });

    let accepted = listener.accept();
    let (mut stream, peer) = match accepted {
        Ok(pair) => pair,
        Err(err) => {
            report.check("accept a connection", false, format!("{}", err));
            return;
        }
    };
    report.check("accept a connection", true, String::new());
    report.check(
        "the connection names the other end",
        peer.ip().to_string() == "127.0.0.1" && peer.port() != 0,
        format!("{}", peer),
    );
    report.check(
        "and names this end",
        stream.local_addr().map(|a| a.port()).ok() == Some(address.port()),
        format!("{:?}", stream.local_addr()),
    );

    let mut question = Vec::new();
    let read = stream.read_to_end(&mut question);
    report.check(
        "the whole question arrived",
        read.is_ok() && question == b"question",
        format!("{:?}", String::from_utf8_lossy(&question)),
    );
    let written = stream.write_all(b"answer");
    report.check("the answer was written", written.is_ok(), format!("{:?}", written));
    drop(stream);

    match client.join() {
        Ok(Ok(answer)) => report.check(
            "the answer arrived at the other end",
            answer == b"answer",
            format!("{:?}", String::from_utf8_lossy(&answer)),
        ),
        other => report.check("the answer arrived at the other end", false, format!("{:?}", other.is_ok())),
    }
}

/// More than fits in one segment, and more than fits in the send buffer, so
/// the sender has to wait for the other end to take some of it.
fn bulk(report: &mut Report) {
    const TOTAL: usize = 200 * 1024;
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("local_addr");

    let sender = std::thread::spawn(move || -> std::io::Result<()> {
        let mut stream = TcpStream::connect(address)?;
        let block: Vec<u8> = (0..1024u32).map(|i| (i % 251) as u8).collect();
        for _ in 0..TOTAL / 1024 {
            stream.write_all(&block)?;
        }
        stream.shutdown(Shutdown::Write)?;
        Ok(())
    });

    let (mut stream, _) = listener.accept().expect("accept");
    let mut received = Vec::new();
    let read = stream.read_to_end(&mut received);
    report.check(
        "every byte arrived",
        read.is_ok() && received.len() == TOTAL,
        format!("{:?} bytes of {}", read.map(|_| received.len()), TOTAL),
    );
    let intact = received
        .chunks(1024)
        .all(|chunk| chunk.iter().enumerate().all(|(i, b)| *b == (i % 251) as u8));
    report.check("and in the right order", intact, String::new());
    report.check("the sender finished", sender.join().is_ok(), String::new());
}

/// A connection nobody is listening for, and a read from a socket the other
/// end has finished with.
fn refusals(report: &mut Report) {
    // A port that was listened on and then closed is the surest way to find
    // one with nothing behind it.
    let port = {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        listener.local_addr().expect("local_addr").port()
    };
    let refused = TcpStream::connect(("127.0.0.1", port));
    report.check(
        "connecting to a closed port is refused",
        refused.is_err(),
        format!("{:?}", refused.map(|s| s.peer_addr())),
    );

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("local_addr");
    let client = std::thread::spawn(move || {
        let stream = TcpStream::connect(address).expect("connect");
        drop(stream);
    });
    let (mut stream, _) = listener.accept().expect("accept");
    let mut buf = [0u8; 16];
    let n = stream.read(&mut buf);
    report.check(
        "a closed connection reads as an end, not an error",
        matches!(n, Ok(0)) || n.is_err(),
        format!("{:?}", n),
    );
    let _ = client.join();

    listener.set_nonblocking(true).expect("set_nonblocking");
    let nothing = listener.accept();
    report.check(
        "accept on an idle socket does not block when told not to",
        nothing.is_err(),
        format!("{:?}", nothing.map(|(_, a)| a)),
    );
}

fn datagrams(report: &mut Report) {
    let one = match UdpSocket::bind("127.0.0.1:0") {
        Ok(socket) => socket,
        Err(err) => {
            report.check("bind a datagram socket", false, format!("{}", err));
            return;
        }
    };
    let two = UdpSocket::bind("127.0.0.1:0").expect("bind");
    report.check("bind a datagram socket", true, String::new());
    let two_address = two.local_addr().expect("local_addr");
    let one_address = one.local_addr().expect("local_addr");

    let sent = one.send_to(b"a datagram", two_address);
    report.check("send", matches!(sent, Ok(10)), format!("{:?}", sent));

    let mut buf = [0u8; 64];
    match two.recv_from(&mut buf) {
        Ok((n, from)) => {
            report.check(
                "the datagram arrived whole",
                &buf[..n] == b"a datagram",
                format!("{:?}", String::from_utf8_lossy(&buf[..n])),
            );
            report.check(
                "and says where it came from",
                from.port() == one_address.port(),
                format!("{} wanted {}", from, one_address),
            );
        }
        Err(err) => report.check("the datagram arrived whole", false, format!("{}", err)),
    }
}

/// The thing this is all for: a request and a response over one connection.
fn http(report: &mut Report) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("local_addr");

    let server = std::thread::spawn(move || {
        if let Ok((stream, _)) = listener.accept() {
            answer(stream);
        }
    });

    let mut stream = TcpStream::connect(address).expect("connect");
    stream
        .write_all(b"GET /hello HTTP/1.1\r\nHost: claudeos\r\nConnection: close\r\n\r\n")
        .expect("write");
    let mut response = String::new();
    let read = stream.read_to_string(&mut response);
    report.check("a response came back", read.is_ok(), format!("{:?}", read));
    report.check(
        "with a status line",
        response.starts_with("HTTP/1.1 200 OK\r\n"),
        response.lines().next().unwrap_or("").to_string(),
    );
    report.check(
        "and the body the server wrote",
        response.ends_with("hello from claudeos\n"),
        format!("{} bytes", response.len()),
    );
    let _ = server.join();
}

// ---- the server ----------------------------------------------------------

fn answer(mut stream: TcpStream) {
    let mut request = Vec::new();
    let mut buf = [0u8; 1024];
    // Read until the end of the headers rather than to end of file: a client
    // that is waiting for an answer has not closed its end.
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                request.extend_from_slice(&buf[..n]);
                if request.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            Err(_) => return,
        }
    }
    let body = "hello from claudeos\n";
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
    let _ = stream.shutdown(Shutdown::Write);
    // Give the other end a moment to take it before the socket goes away.
    std::thread::sleep(Duration::from_millis(20));
}

fn serve(port: u16, count: usize) {
    let listener = match TcpListener::bind(("0.0.0.0", port)) {
        Ok(listener) => listener,
        Err(err) => {
            println!("inet: cannot listen on port {}: {}", port, err);
            std::process::exit(1);
        }
    };
    println!("inet: listening on {}", listener.local_addr().expect("local_addr"));
    let mut served = 0;
    while served < count {
        match listener.accept() {
            Ok((stream, peer)) => {
                println!("inet: connection from {}", peer);
                answer(stream);
                served += 1;
            }
            Err(err) => {
                println!("inet: accept failed: {}", err);
                break;
            }
        }
    }
    println!("inet: served {} connections", served);
}
