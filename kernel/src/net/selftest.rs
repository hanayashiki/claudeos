//! The protocol stack against a fake card.
//!
//! A card that records what it is asked to send stands in for the real one,
//! and frames are handed to `receive` by hand. Every check compares bytes:
//! the expectations here are written out field by field rather than built with
//! the same code that produced the frame, so a header laid out wrongly fails
//! rather than agreeing with itself.
//!
//! Above that sits a peer with a link in each direction, for the behaviour
//! that only shows when packets go missing. The link is told what to do with
//! each segment before the test runs -- lose this one, hold that one back,
//! deliver the next twice, damage the one after -- so a failure names one
//! packet and happens again on the next run, which is what a real network
//! cannot be asked for.
//!
//! Run with `net=test` on the kernel command line.

use super::ether;
use super::ip::{self, Ipv4Addr};
use super::socket::{self, Endpoint, InetSocket, Protocol};
use super::tcp;
use super::{Config, Interface};
use crate::abi::Errno;
use crate::sync::Spinlock;
use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;

const OUR_MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];
const PEER_MAC: [u8; 6] = [0x52, 0x55, 0x0A, 0x00, 0x02, 0x02];
const OUR_IP: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 15);
const PEER_IP: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 2);
const NAMESERVER_IP: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 3);

/// The addresses the checks run with, all but the ones about having none.
fn test_config() -> Config {
    Config::new(OUR_IP, Ipv4Addr::new(255, 255, 255, 0), Some(PEER_IP), &[NAMESERVER_IP])
        .expect("the addresses the checks run with")
}

/// A card that goes no further than remembering what it was handed.
struct FakeNic {
    sent: Spinlock<Vec<Vec<u8>>>,
    /// Up unless a check takes it down, to see what the DHCP client does
    /// when it comes back.
    link: core::sync::atomic::AtomicBool,
}

impl Interface for FakeNic {
    fn mac(&self) -> [u8; 6] {
        OUR_MAC
    }

    fn transmit(&self, frame: &[u8]) -> Result<(), Errno> {
        self.sent.lock().push(frame.to_vec());
        Ok(())
    }

    fn link_up(&self) -> bool {
        self.link.load(core::sync::atomic::Ordering::Relaxed)
    }
}

impl FakeNic {
    fn take(&self) -> Vec<Vec<u8>> {
        core::mem::take(&mut *self.sent.lock())
    }
}

// ---- reporting ------------------------------------------------------------

struct Report {
    passed: usize,
    failed: usize,
}

impl Report {
    fn check(&mut self, name: &str, holds: bool) {
        if holds {
            self.passed += 1;
            crate::println!("  ok    {}", name);
        } else {
            self.failed += 1;
            crate::println!("  FAIL  {}", name);
        }
    }

    fn bytes(&mut self, name: &str, actual: &[u8], expected: &[u8]) {
        if actual == expected {
            self.passed += 1;
            crate::println!("  ok    {} ({} bytes)", name, actual.len());
            return;
        }
        self.failed += 1;
        crate::println!("  FAIL  {}", name);
        crate::print!("        sent     ");
        dump(actual);
        crate::print!("        expected ");
        dump(expected);
    }

    fn value(&mut self, name: &str, actual: u64, expected: u64) {
        if actual == expected {
            self.passed += 1;
            crate::println!("  ok    {} = {:#x}", name, actual);
        } else {
            self.failed += 1;
            crate::println!(
                "  FAIL  {}: got {:#x}, wanted {:#x}",
                name,
                actual,
                expected
            );
        }
    }
}

fn dump(bytes: &[u8]) {
    for (i, byte) in bytes.iter().enumerate() {
        if i == 96 {
            crate::print!("...");
            break;
        }
        crate::print!("{:02x}", byte);
    }
    crate::println!();
}

// ---- building what the far end would send ---------------------------------

fn frame(ethertype: u16, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&OUR_MAC);
    out.extend_from_slice(&PEER_MAC);
    out.extend_from_slice(&ethertype.to_be_bytes());
    out.extend_from_slice(payload);
    while out.len() < 60 {
        out.push(0);
    }
    out
}

/// An Ethernet frame as this stack should have built it: to the peer, from us,
/// padded to the minimum length.
fn expected_frame(ethertype: u16, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&PEER_MAC);
    out.extend_from_slice(&OUR_MAC);
    out.extend_from_slice(&ethertype.to_be_bytes());
    out.extend_from_slice(payload);
    while out.len() < 60 {
        out.push(0);
    }
    out
}

/// An IPv4 datagram, header written out field by field.
fn datagram(
    identification: u16,
    protocol: u8,
    source: Ipv4Addr,
    destination: Ipv4Addr,
    payload: &[u8],
) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(0x45); // version 4, five words of header
    out.push(0x00); // no differentiated services
    out.extend_from_slice(&((20 + payload.len()) as u16).to_be_bytes());
    out.extend_from_slice(&identification.to_be_bytes());
    out.extend_from_slice(&[0x40, 0x00]); // don't fragment, offset zero
    out.push(64); // time to live
    out.push(protocol);
    out.extend_from_slice(&[0, 0]);
    out.extend_from_slice(&source.to_be_bytes());
    out.extend_from_slice(&destination.to_be_bytes());
    let checksum = ip::checksum(&out);
    out[10..12].copy_from_slice(&checksum.to_be_bytes());
    out.extend_from_slice(payload);
    out
}

#[allow(clippy::too_many_arguments)]
fn segment(
    source_port: u16,
    destination_port: u16,
    sequence: u32,
    acknowledgement: u32,
    flags: u8,
    window: u16,
    options: &[u8],
    payload: &[u8],
    source: Ipv4Addr,
    destination: Ipv4Addr,
) -> Vec<u8> {
    let offset = 20 + options.len();
    let mut out = Vec::new();
    out.extend_from_slice(&source_port.to_be_bytes());
    out.extend_from_slice(&destination_port.to_be_bytes());
    out.extend_from_slice(&sequence.to_be_bytes());
    out.extend_from_slice(&acknowledgement.to_be_bytes());
    out.push(((offset / 4) as u8) << 4);
    out.push(flags);
    out.extend_from_slice(&window.to_be_bytes());
    out.extend_from_slice(&[0, 0]); // checksum
    out.extend_from_slice(&[0, 0]); // urgent pointer
    out.extend_from_slice(options);
    out.extend_from_slice(payload);
    let pseudo = ip::pseudo_sum(source, destination, ip::PROTO_TCP, out.len());
    let checksum = ip::fold(ip::sum(&out, pseudo));
    out[16..18].copy_from_slice(&checksum.to_be_bytes());
    out
}

fn udp_datagram(
    source_port: u16,
    destination_port: u16,
    payload: &[u8],
    source: Ipv4Addr,
    destination: Ipv4Addr,
) -> Vec<u8> {
    let length = 8 + payload.len();
    let mut out = Vec::new();
    out.extend_from_slice(&source_port.to_be_bytes());
    out.extend_from_slice(&destination_port.to_be_bytes());
    out.extend_from_slice(&(length as u16).to_be_bytes());
    out.extend_from_slice(&[0, 0]);
    out.extend_from_slice(payload);
    let pseudo = ip::pseudo_sum(source, destination, ip::PROTO_UDP, length);
    let mut checksum = ip::fold(ip::sum(&out, pseudo));
    if checksum == 0 {
        checksum = 0xFFFF;
    }
    out[6..8].copy_from_slice(&checksum.to_be_bytes());
    out
}

/// The identification field of the datagram inside a frame this stack sent.
/// It counts up, so it is read back rather than predicted.
fn identification_of(frame: &[u8]) -> u16 {
    u16::from_be_bytes([frame[18], frame[19]])
}

fn deliver(ethertype: u16, payload: &[u8]) {
    let bytes = frame(ethertype, payload);
    super::receive(&bytes);
}

fn deliver_ip(protocol: u8, payload: &[u8]) {
    let bytes = datagram(0x4242, protocol, PEER_IP, OUR_IP, payload);
    deliver(ether::ETHERTYPE_IPV4, &bytes);
}

/// The same, for a datagram addressed somewhere other than at this machine.
fn deliver_ip_between(protocol: u8, payload: &[u8], source: Ipv4Addr, destination: Ipv4Addr) {
    let bytes = datagram(0x4242, protocol, source, destination, payload);
    deliver(ether::ETHERTYPE_IPV4, &bytes);
}

// ---- the tests ------------------------------------------------------------

pub fn run() -> bool {
    let nic: &'static FakeNic = Box::leak(Box::new(FakeNic {
        sent: Spinlock::new(Vec::new()),
        link: core::sync::atomic::AtomicBool::new(true),
    }));
    super::attach(nic);
    super::configure(Some(test_config()));
    socket::reset();
    nic.take();

    let mut report = Report { passed: 0, failed: 0 };

    // Page tables and the frame allocator, which nothing above them can ask
    // about. Counted into this summary because the harness reads one line per
    // boot, and a second summary would let a failure here pass unnoticed.
    crate::println!("mm: page tables and frames");
    let mut memory = crate::mm::selftest::Report { passed: 0, failed: 0 };
    crate::mm::selftest::run(&mut memory);
    report.passed += memory.passed;
    report.failed += memory.failed;

    // The generator, which is neither memory nor network but is checked here
    // for the same reason the memory checks are: one summary line per boot.
    crate::println!("random: chacha20 against the published vectors");
    let mut rng = crate::rng::selftest::Report { passed: 0, failed: 0 };
    crate::rng::selftest::run(&mut rng);
    report.passed += rng.passed;
    report.failed += rng.failed;

    crate::println!("net: checksums");
    checksums(&mut report);
    crate::println!("net: address resolution");
    address_resolution(&mut report, nic);
    crate::println!("net: echo");
    echo(&mut report, nic);
    crate::println!("net: connection refused");
    refused(&mut report, nic);
    crate::println!("net: a served connection");
    connection(&mut report, nic);
    crate::println!("net: segments outside the window");
    unacceptable_segments(&mut report, nic);
    crate::println!("net: a link that loses and reorders");
    lossy_link(&mut report, nic);
    crate::println!("net: segments that arrive out of order");
    reassembly(&mut report, nic);
    crate::println!("net: the round trip, measured");
    round_trip_estimate(&mut report, nic);
    crate::println!("net: a segment lost out of the middle of a stream");
    fast_retransmit(&mut report, nic);
    crate::println!("net: a connection the program has closed");
    abandoned_connection(&mut report, nic);
    crate::println!("net: addresses this machine does not have");
    foreign_addresses(&mut report, nic);
    crate::println!("net: initial sequence numbers");
    initial_sequence_numbers(&mut report, nic);
    crate::println!("net: datagrams");
    datagrams(&mut report, nic);
    crate::println!("net: what makes a configuration");
    configurations(&mut report);
    crate::println!("net: a machine with no address");
    no_address(&mut report, nic);
    crate::println!("net: dhcp, a lease taken, renewed, rebound and run out");
    dhcp_exchange(&mut report, nic);
    crate::println!("net: dhcp, refused");
    dhcp_refusal(&mut report, nic);
    crate::println!("net: dhcp, malformed messages");
    dhcp_malformed(&mut report, nic);
    crate::println!("net: dhcp, retransmission and the link coming up");
    dhcp_retransmission(&mut report, nic);
    super::configure(Some(test_config()));
    // The driver for this board's own Ethernet, as far as it can be exercised
    // with no such Ethernet anywhere: nothing emulates it, so this is the only
    // thing that runs against it before it meets a board.
    #[cfg(target_arch = "aarch64")]
    {
        crate::println!("net: the board's own ethernet");
        let mut genet = super::genettest::Report { passed: 0, failed: 0 };
        super::genettest::run(&mut genet);
        report.passed += genet.passed;
        report.failed += genet.failed;

        // The board's WiFi, which nothing emulates either.
        crate::println!("net: the board's own wifi");
        let mut wifi = super::wifi::test::Report { passed: 0, failed: 0 };
        super::wifi::test::run(&mut wifi);
        report.passed += wifi.passed;
        report.failed += wifi.failed;
    }

    socket::reset();
    crate::println!();
    crate::println!("=== {} passed, {} failed ===", report.passed, report.failed);
    report.failed == 0
}

/// The ones' complement sum, against the worked example in RFC 1071 and a
/// header whose checksum is known.
fn checksums(report: &mut Report) {
    let example = [0x00u8, 0x01, 0xf2, 0x03, 0xf4, 0xf5, 0xf6, 0xf7];
    report.value("rfc 1071 example", ip::checksum(&example) as u64, 0x220d);

    let header = [
        0x45u8, 0x00, 0x00, 0x73, 0x00, 0x00, 0x40, 0x00, 0x40, 0x11, 0x00, 0x00, 0xc0, 0xa8,
        0x00, 0x01, 0xc0, 0xa8, 0x00, 0xc7,
    ];
    report.value("ipv4 header checksum", ip::checksum(&header) as u64, 0xb861);

    let mut filled = header;
    filled[10] = 0xb8;
    filled[11] = 0x61;
    report.value("checksum of a complete header", ip::checksum(&filled) as u64, 0);
}

/// A request for our address is answered, byte for byte.
fn address_resolution(report: &mut Report, nic: &FakeNic) {
    let mut request = Vec::new();
    request.extend_from_slice(&[0x00, 0x01]); // ethernet
    request.extend_from_slice(&[0x08, 0x00]); // ipv4
    request.extend_from_slice(&[6, 4, 0x00, 0x01]); // lengths, request
    request.extend_from_slice(&PEER_MAC);
    request.extend_from_slice(&PEER_IP.to_be_bytes());
    request.extend_from_slice(&[0u8; 6]);
    request.extend_from_slice(&OUR_IP.to_be_bytes());
    deliver(ether::ETHERTYPE_ARP, &request);

    let sent = nic.take();
    report.check("one reply", sent.len() == 1);
    if sent.len() != 1 {
        return;
    }

    let mut reply = Vec::new();
    reply.extend_from_slice(&[0x00, 0x01]);
    reply.extend_from_slice(&[0x08, 0x00]);
    reply.extend_from_slice(&[6, 4, 0x00, 0x02]); // reply
    reply.extend_from_slice(&OUR_MAC);
    reply.extend_from_slice(&OUR_IP.to_be_bytes());
    reply.extend_from_slice(&PEER_MAC);
    reply.extend_from_slice(&PEER_IP.to_be_bytes());
    report.bytes("arp reply", &sent[0], &expected_frame(ether::ETHERTYPE_ARP, &reply));

    // The request also taught us where the other end is.
    report.check("sender remembered", super::arp::lookup(PEER_IP) == Some(PEER_MAC));

    // And a request for an address that is not ours is ignored.
    let mut elsewhere = request.clone();
    elsewhere[24..28].copy_from_slice(&Ipv4Addr::new(10, 0, 2, 99).to_be_bytes());
    deliver(ether::ETHERTYPE_ARP, &elsewhere);
    report.check("request for another address ignored", nic.take().is_empty());

    // A reply nobody asked for, claiming this machine's own address. Taking
    // it would point our own address at somebody else's card.
    let impostor: [u8; 6] = [0x52, 0x55, 0x0A, 0x00, 0x02, 0x63];
    let mut claim = Vec::new();
    claim.extend_from_slice(&[0x00, 0x01]);
    claim.extend_from_slice(&[0x08, 0x00]);
    claim.extend_from_slice(&[6, 4, 0x00, 0x02]); // reply
    claim.extend_from_slice(&impostor);
    claim.extend_from_slice(&OUR_IP.to_be_bytes());
    claim.extend_from_slice(&PEER_MAC);
    claim.extend_from_slice(&PEER_IP.to_be_bytes());
    deliver(ether::ETHERTYPE_ARP, &claim);
    report.check("a claim on our own address is not believed", super::arp::lookup(OUR_IP).is_none());
    nic.take();
}

/// A ping is answered with an echo reply whose checksum is right.
fn echo(report: &mut Report, nic: &FakeNic) {
    let body = b"abcdefghijklmnopqrstuvwabcdefghi";
    let mut request = Vec::new();
    request.push(8); // echo request
    request.push(0);
    request.extend_from_slice(&[0, 0]); // checksum, filled in below
    request.extend_from_slice(&[0x13, 0x37]); // identifier
    request.extend_from_slice(&[0x00, 0x05]); // sequence
    request.extend_from_slice(body);
    let checksum = ip::checksum(&request);
    request[2..4].copy_from_slice(&checksum.to_be_bytes());
    deliver_ip(ip::PROTO_ICMP, &request);

    let sent = nic.take();
    report.check("one echo reply", sent.len() == 1);
    if sent.len() != 1 {
        return;
    }
    let reply_frame = &sent[0];

    let mut reply = Vec::new();
    reply.push(0); // echo reply
    reply.push(0);
    reply.extend_from_slice(&[0, 0]);
    reply.extend_from_slice(&[0x13, 0x37]);
    reply.extend_from_slice(&[0x00, 0x05]);
    reply.extend_from_slice(body);
    let checksum = ip::checksum(&reply);
    reply[2..4].copy_from_slice(&checksum.to_be_bytes());

    let expected = expected_frame(
        ether::ETHERTYPE_IPV4,
        &datagram(identification_of(reply_frame), ip::PROTO_ICMP, OUR_IP, PEER_IP, &reply),
    );
    report.bytes("echo reply", reply_frame, &expected);

    // Independently of the comparison: the checksums as a receiver would
    // test them, which is that the sum over the whole thing folds to zero.
    let (header, payload) = ip::parse(&reply_frame[14..]).expect("a datagram");
    report.value("ip header checksum verifies", ip::checksum(&reply_frame[14..14 + header.ihl]) as u64, 0);
    report.value("icmp checksum verifies", ip::checksum(payload) as u64, 0);
}

/// A connection request for a port nobody is listening on is refused.
fn refused(report: &mut Report, nic: &FakeNic) {
    let request = segment(
        40000,
        9999,
        0x0102_0304,
        0,
        tcp::SYN,
        64240,
        &[],
        &[],
        PEER_IP,
        OUR_IP,
    );
    deliver_ip(ip::PROTO_TCP, &request);
    let sent = nic.take();
    report.check("one refusal", sent.len() == 1);
    if sent.len() != 1 {
        return;
    }
    let expected = expected_frame(
        ether::ETHERTYPE_IPV4,
        &datagram(
            identification_of(&sent[0]),
            ip::PROTO_TCP,
            OUR_IP,
            PEER_IP,
            &segment(
                9999,
                40000,
                0,
                0x0102_0305,
                tcp::RST | tcp::ACK,
                0,
                &[],
                &[],
                OUR_IP,
                PEER_IP,
            ),
        ),
    );
    report.bytes("reset", &sent[0], &expected);
}

/// The whole of a served connection: the handshake, a request, an answer, and
/// an orderly close.
fn connection(report: &mut Report, nic: &FakeNic) {
    const CLIENT_PORT: u16 = 40001;
    const SERVER_PORT: u16 = 8080;
    const CLIENT_ISS: u32 = 0x1122_3344;
    // What this stack advertises: the whole receive buffer, nothing taken yet.
    const WINDOW: u16 = tcp::RECEIVE_WINDOW as u16;

    let listener = InetSocket::new(true);
    if listener.bind(Endpoint::new(Ipv4Addr::UNSPECIFIED, SERVER_PORT)).is_err() {
        report.check("bind", false);
        return;
    }
    if listener.listen(8).is_err() {
        report.check("listen", false);
        return;
    }
    report.check("listening", listener.is_listening());
    nic.take();

    // ---- the connection request ----
    deliver_ip(
        ip::PROTO_TCP,
        &segment(
            CLIENT_PORT,
            SERVER_PORT,
            CLIENT_ISS,
            0,
            tcp::SYN,
            64240,
            &[2, 4, 0x05, 0xB4], // maximum segment size 1460
            &[],
            PEER_IP,
            OUR_IP,
        ),
    );
    let sent = nic.take();
    report.check("one answer to the request", sent.len() == 1);
    if sent.len() != 1 {
        return;
    }
    let answer = &sent[0];
    let parsed = tcp::Segment::parse(&answer[34..]).expect("a segment");
    let server_iss = parsed.sequence;
    report.value("answer acknowledges the request", parsed.acknowledgement as u64, CLIENT_ISS as u64 + 1);
    report.value("answer is SYN and ACK", parsed.flags as u64, (tcp::SYN | tcp::ACK) as u64);
    report.check("sequence number is not zero", server_iss != 0);
    report.check(
        "sequence number is not the other end's",
        server_iss != CLIENT_ISS && server_iss != CLIENT_ISS + 1,
    );
    let expected = expected_frame(
        ether::ETHERTYPE_IPV4,
        &datagram(
            identification_of(answer),
            ip::PROTO_TCP,
            OUR_IP,
            PEER_IP,
            &segment(
                SERVER_PORT,
                CLIENT_PORT,
                server_iss,
                CLIENT_ISS + 1,
                tcp::SYN | tcp::ACK,
                WINDOW,
                &[2, 4, 0x05, 0xB4],
                &[],
                OUR_IP,
                PEER_IP,
            ),
        ),
    );
    report.bytes("answer to the connection request", answer, &expected);

    // ---- the acknowledgement that completes it ----
    deliver_ip(
        ip::PROTO_TCP,
        &segment(
            CLIENT_PORT,
            SERVER_PORT,
            CLIENT_ISS + 1,
            server_iss + 1,
            tcp::ACK,
            64240,
            &[],
            &[],
            PEER_IP,
            OUR_IP,
        ),
    );
    report.check("handshake sends nothing further", nic.take().is_empty());

    let accepted = match listener.accept_ready() {
        Ok(Some(child)) => child,
        _ => {
            report.check("a connection to accept", false);
            return;
        }
    };
    report.check("a connection to accept", true);
    report.check(
        "accepted connection names the other end",
        accepted.peer_endpoint() == Ok(Endpoint::new(PEER_IP, CLIENT_PORT)),
    );
    report.check(
        "accepted connection names this end",
        accepted.local_endpoint() == Endpoint::new(OUR_IP, SERVER_PORT),
    );
    report.check("listener has nothing left to accept", matches!(listener.accept_ready(), Ok(None)));

    // ---- a request arrives ----
    let request = b"GET / HTTP/1.1\r\nHost: claudeos\r\n\r\n";
    deliver_ip(
        ip::PROTO_TCP,
        &segment(
            CLIENT_PORT,
            SERVER_PORT,
            CLIENT_ISS + 1,
            server_iss + 1,
            tcp::PSH | tcp::ACK,
            64240,
            &[],
            request,
            PEER_IP,
            OUR_IP,
        ),
    );
    let sent = nic.take();
    report.check("one acknowledgement for the request", sent.len() == 1);
    if sent.len() != 1 {
        return;
    }
    let expected = expected_frame(
        ether::ETHERTYPE_IPV4,
        &datagram(
            identification_of(&sent[0]),
            ip::PROTO_TCP,
            OUR_IP,
            PEER_IP,
            &segment(
                SERVER_PORT,
                CLIENT_PORT,
                server_iss + 1,
                CLIENT_ISS + 1 + request.len() as u32,
                tcp::ACK,
                WINDOW - request.len() as u16,
                &[],
                &[],
                OUR_IP,
                PEER_IP,
            ),
        ),
    );
    report.bytes("acknowledgement of the request", &sent[0], &expected);

    let mut buf = [0u8; 256];
    match accepted.receive_into(&mut buf, false) {
        Ok(socket::Received { taken: n, .. }) => {
            report.bytes("the request, as the socket reads it", &buf[..n], request);
        }
        Err(_) => report.check("the request, as the socket reads it", false),
    }

    // ---- the answer goes back ----
    let answer_body = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nhi";
    match accepted.send(answer_body, None) {
        Ok(n) => report.value("the whole answer was taken", n as u64, answer_body.len() as u64),
        Err(_) => report.check("the whole answer was taken", false),
    }
    let sent = nic.take();
    report.check("one segment carries the answer", sent.len() == 1);
    if sent.len() != 1 {
        return;
    }
    let expected = expected_frame(
        ether::ETHERTYPE_IPV4,
        &datagram(
            identification_of(&sent[0]),
            ip::PROTO_TCP,
            OUR_IP,
            PEER_IP,
            &segment(
                SERVER_PORT,
                CLIENT_PORT,
                server_iss + 1,
                CLIENT_ISS + 1 + request.len() as u32,
                tcp::PSH | tcp::ACK,
                // The right edge of the window has not moved since the
                // handshake: the request that was read out of the socket
                // freed 34 bytes, and the edge only moves when it can move
                // by a whole segment.
                WINDOW - request.len() as u16,
                &[],
                answer_body,
                OUR_IP,
                PEER_IP,
            ),
        ),
    );
    report.bytes("the answer on the wire", &sent[0], &expected);

    // The other end takes it.
    let client_next = CLIENT_ISS + 1 + request.len() as u32;
    let server_next = server_iss + 1 + answer_body.len() as u32;
    deliver_ip(
        ip::PROTO_TCP,
        &segment(
            CLIENT_PORT,
            SERVER_PORT,
            client_next,
            server_next,
            tcp::ACK,
            64240,
            &[],
            &[],
            PEER_IP,
            OUR_IP,
        ),
    );
    report.check("an acknowledgement needs no answer", nic.take().is_empty());
    report.check("nothing left unacknowledged", !retransmit_pending(&accepted));

    // ---- the close ----
    socket::close(&accepted);
    let sent = nic.take();
    report.check("the close sends one segment", sent.len() == 1);
    if sent.len() != 1 {
        return;
    }
    let expected = expected_frame(
        ether::ETHERTYPE_IPV4,
        &datagram(
            identification_of(&sent[0]),
            ip::PROTO_TCP,
            OUR_IP,
            PEER_IP,
            &segment(
                SERVER_PORT,
                CLIENT_PORT,
                server_next,
                client_next,
                tcp::FIN | tcp::ACK,
                WINDOW - request.len() as u16,
                &[],
                &[],
                OUR_IP,
                PEER_IP,
            ),
        ),
    );
    report.bytes("the finish", &sent[0], &expected);
    report.check("waiting for the other end", state_of(&accepted) == tcp::State::FinWait1);

    deliver_ip(
        ip::PROTO_TCP,
        &segment(
            CLIENT_PORT,
            SERVER_PORT,
            client_next,
            server_next + 1,
            tcp::ACK,
            64240,
            &[],
            &[],
            PEER_IP,
            OUR_IP,
        ),
    );
    report.check("the finish was acknowledged", state_of(&accepted) == tcp::State::FinWait2);
    report.check("nothing sent in answer", nic.take().is_empty());

    deliver_ip(
        ip::PROTO_TCP,
        &segment(
            CLIENT_PORT,
            SERVER_PORT,
            client_next,
            server_next + 1,
            tcp::FIN | tcp::ACK,
            64240,
            &[],
            &[],
            PEER_IP,
            OUR_IP,
        ),
    );
    let sent = nic.take();
    report.check("the other end's finish is acknowledged", sent.len() == 1);
    if sent.len() != 1 {
        return;
    }
    let expected = expected_frame(
        ether::ETHERTYPE_IPV4,
        &datagram(
            identification_of(&sent[0]),
            ip::PROTO_TCP,
            OUR_IP,
            PEER_IP,
            &segment(
                SERVER_PORT,
                CLIENT_PORT,
                server_next + 1,
                client_next + 1,
                tcp::ACK,
                WINDOW - request.len() as u16 - 1,
                &[],
                &[],
                OUR_IP,
                PEER_IP,
            ),
        ),
    );
    report.bytes("the last acknowledgement", &sent[0], &expected);
    report.check("in the wait state", state_of(&accepted) == tcp::State::TimeWait);

    socket::close(&listener);
    nic.take();
}

fn queued_bytes_of(socket: &Arc<InetSocket>) -> usize {
    match &*socket.inner.lock() {
        Protocol::Udp(state) => state.queued_bytes(),
        Protocol::Tcp(_) => 0,
    }
}

fn state_of(socket: &Arc<InetSocket>) -> tcp::State {
    match &*socket.inner.lock() {
        Protocol::Tcp(tcb) => tcb.state,
        Protocol::Udp(_) => tcp::State::Closed,
    }
}

fn retransmit_pending(socket: &Arc<InetSocket>) -> bool {
    match &*socket.inner.lock() {
        Protocol::Tcp(tcb) => tcb.retransmit_at != 0,
        Protocol::Udp(_) => false,
    }
}

/// One connection that has finished its handshake, for the checks that drive
/// segments at a connection already up.
struct Connection {
    listener: Arc<InetSocket>,
    socket: Arc<InetSocket>,
    port: u16,
    /// The next sequence number each end expects from the other.
    client_next: u32,
    server_next: u32,
}

/// Take a connection through its handshake and hand back both ends' numbers.
fn establish(nic: &FakeNic, server_port: u16, client_port: u16, client_iss: u32) -> Option<Connection> {
    let listener = InetSocket::new(true);
    listener.bind(Endpoint::new(Ipv4Addr::UNSPECIFIED, server_port)).ok()?;
    listener.listen(4).ok()?;
    nic.take();

    deliver_ip(
        ip::PROTO_TCP,
        &segment(client_port, server_port, client_iss, 0, tcp::SYN, 64240, &[], &[], PEER_IP, OUR_IP),
    );
    let sent = nic.take();
    if sent.len() != 1 {
        return None;
    }
    let answer = tcp::Segment::parse(&sent[0][34..])?;
    let server_iss = answer.sequence;
    deliver_ip(
        ip::PROTO_TCP,
        &segment(
            client_port,
            server_port,
            client_iss.wrapping_add(1),
            server_iss.wrapping_add(1),
            tcp::ACK,
            64240,
            &[],
            &[],
            PEER_IP,
            OUR_IP,
        ),
    );
    nic.take();
    let socket = listener.accept_ready().ok()??;
    Some(Connection {
        listener,
        socket,
        port: client_port,
        client_next: client_iss.wrapping_add(1),
        server_next: server_iss.wrapping_add(1),
    })
}

// ---- a link that loses, reorders, duplicates and corrupts -----------------

/// What a link does with one segment. A test lists these in the order the
/// segments reach it, so the same segment meets the same fate on every run
/// and a failure names one packet rather than a probability.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Fate {
    Pass,
    /// Lost, with nothing anywhere to say so.
    Lose,
    /// Held back until the segment behind it has gone, so the two arrive the
    /// wrong way round.
    Late,
    /// Delivered, and then delivered again.
    Twice,
    /// A bit of the checksum flipped, which is what corruption of any part of
    /// the segment looks like to the end that receives it.
    Corrupt,
}

/// One direction of a link.
struct Link {
    /// Where the TCP header starts in what this carries: a frame has the
    /// Ethernet and IPv4 headers in front of it, a datagram only the IPv4 one.
    tcp_at: usize,
    fates: Vec<Fate>,
    carried: usize,
    late: Option<Vec<u8>>,
    lost: usize,
}

impl Link {
    fn new(tcp_at: usize) -> Link {
        Link { tcp_at, fates: Vec::new(), carried: 0, late: None, lost: 0 }
    }

    /// What is to happen to the next segments through here. Anything past the
    /// end of the list passes.
    fn script(&mut self, fates: &[Fate]) {
        self.fates = fates.to_vec();
        self.carried = 0;
        self.lost = 0;
    }

    /// Hand one segment to the link. What comes back is what arrives at the
    /// far end, in the order it arrives there.
    fn carry(&mut self, packet: Vec<u8>) -> Vec<Vec<u8>> {
        let fate = self.fates.get(self.carried).copied().unwrap_or(Fate::Pass);
        self.carried += 1;
        let mut out = Vec::new();
        match fate {
            Fate::Pass => out.push(packet),
            Fate::Lose => self.lost += 1,
            Fate::Late => {
                // One held at a time: a test that wants two reordered says
                // Late twice with something between.
                if let Some(earlier) = self.late.replace(packet) {
                    out.push(earlier);
                }
                return out;
            }
            Fate::Twice => {
                out.push(packet.clone());
                out.push(packet);
            }
            Fate::Corrupt => {
                let mut damaged = packet;
                damaged[self.tcp_at + 16] ^= 0x40;
                out.push(damaged);
            }
        }
        if let Some(earlier) = self.late.take() {
            out.push(earlier);
        }
        out
    }
}

/// The other end of a connection, reached over a link in each direction.
///
/// It is a small TCP of its own: it acknowledges what arrives, holds what
/// arrives before the bytes in front of it so that resending one segment
/// finishes the stream, and counts the acknowledgements it has repeated. What
/// the links do to each segment is scripted, so a test names the segment that
/// is lost rather than waiting for a link to lose one.
struct Peer {
    nic: &'static FakeNic,
    listener: Arc<InetSocket>,
    socket: Arc<InetSocket>,
    port: u16,
    server_port: u16,
    /// The peer's own numbers: what it sends next, and what it wants next.
    send_next: u32,
    receive_next: u32,
    window: u16,
    /// The stream as the peer has taken it, and what arrived past a gap.
    stream: Vec<u8>,
    early: Vec<(u32, Vec<u8>)>,
    fin_seen: bool,
    /// The last number the peer acknowledged, and how many times it has since
    /// acknowledged the same one again.
    last_ack: u32,
    repeated_acks: usize,
    to_peer: Link,
    to_stack: Link,
}

impl Peer {
    /// Take a connection through its handshake and keep both ends' numbers.
    fn open(
        nic: &'static FakeNic,
        server_port: u16,
        client_port: u16,
        client_iss: u32,
    ) -> Option<Peer> {
        let connection = establish(nic, server_port, client_port, client_iss)?;
        Some(Peer {
            nic,
            listener: connection.listener,
            socket: connection.socket,
            port: client_port,
            server_port,
            send_next: connection.client_next,
            receive_next: connection.server_next,
            window: 64240,
            stream: Vec::new(),
            early: Vec::new(),
            fin_seen: false,
            last_ack: connection.server_next,
            repeated_acks: 0,
            to_peer: Link::new(34),
            to_stack: Link::new(20),
        })
    }

    /// Send one segment at a sequence number of the caller's choosing, which
    /// is what puts a stream out of order or repeats a segment already sent.
    fn send(&mut self, sequence: u32, flags: u8, payload: &[u8]) {
        let body = segment(
            self.port,
            self.server_port,
            sequence,
            self.receive_next,
            flags,
            self.window,
            &[],
            payload,
            PEER_IP,
            OUR_IP,
        );
        let packet = datagram(0x4242, ip::PROTO_TCP, PEER_IP, OUR_IP, &body);
        for arriving in self.to_stack.carry(packet) {
            deliver(ether::ETHERTYPE_IPV4, &arriving);
        }
    }

    /// Send the next bytes of the peer's own stream.
    fn write(&mut self, payload: &[u8]) {
        let sequence = self.send_next;
        self.send_next = self.send_next.wrapping_add(payload.len() as u32);
        self.send(sequence, tcp::PSH | tcp::ACK, payload);
    }

    fn ack(&mut self) {
        if self.receive_next == self.last_ack {
            self.repeated_acks += 1;
        }
        self.last_ack = self.receive_next;
        let sequence = self.send_next;
        self.send(sequence, tcp::ACK, &[]);
    }

    /// Take one segment the stack sent. True when an acknowledgement is owed
    /// for it.
    fn absorb(&mut self, frame: &[u8]) -> bool {
        let Some((_, datagram)) = ip::parse(&frame[14..]) else { return false };
        let pseudo = ip::pseudo_sum(OUR_IP, PEER_IP, ip::PROTO_TCP, datagram.len());
        if ip::fold(ip::sum(datagram, pseudo)) != 0 {
            // Damaged on the way: nothing here can be believed, so the peer
            // behaves as if it never arrived.
            return false;
        }
        let Some(parsed) = tcp::Segment::parse(datagram) else { return false };
        let mut owed = false;
        if !parsed.payload.is_empty() {
            owed = true;
            self.take(parsed.sequence, parsed.payload);
        }
        if parsed.flags & tcp::FIN != 0 {
            owed = true;
            let finish = parsed.sequence.wrapping_add(parsed.payload.len() as u32);
            if finish == self.receive_next && !self.fin_seen {
                self.fin_seen = true;
                self.receive_next = self.receive_next.wrapping_add(1);
            }
        }
        owed
    }

    /// Put one segment's bytes where they belong in the peer's own stream.
    fn take(&mut self, sequence: u32, payload: &[u8]) {
        let mut sequence = sequence;
        let mut payload = payload;
        if tcp::seq_lt(sequence, self.receive_next) {
            let skip = self.receive_next.wrapping_sub(sequence) as usize;
            if skip >= payload.len() {
                return;
            }
            payload = &payload[skip..];
            sequence = self.receive_next;
        }
        if sequence != self.receive_next {
            self.early.push((sequence, payload.to_vec()));
            return;
        }
        self.stream.extend_from_slice(payload);
        self.receive_next = self.receive_next.wrapping_add(payload.len() as u32);
        // Whatever arrived early and now follows on.
        while let Some(index) = self.early.iter().position(|(start, data)| {
            tcp::seq_le(*start, self.receive_next)
                && tcp::seq_gt(start.wrapping_add(data.len() as u32), self.receive_next)
        }) {
            let (start, data) = self.early.remove(index);
            let skip = self.receive_next.wrapping_sub(start) as usize;
            self.stream.extend_from_slice(&data[skip..]);
            self.receive_next = self.receive_next.wrapping_add((data.len() - skip) as u32);
        }
    }

    /// Let the stack say everything it has to say, answering each segment as
    /// it arrives. Returns how many segments reached the peer.
    fn pump(&mut self) -> usize {
        let mut seen = 0;
        for _ in 0..512 {
            let sent = self.nic.take();
            if sent.is_empty() {
                break;
            }
            for frame in sent {
                for arriving in self.to_peer.carry(frame) {
                    seen += 1;
                    if self.absorb(&arriving) {
                        self.ack();
                    }
                }
            }
        }
        seen
    }

    fn finish(self) {
        socket::close(&self.socket);
        socket::close(&self.listener);
        self.nic.take();
    }
}

/// The number a segment this stack sent acknowledges.
fn ack_number(frame: &[u8]) -> Option<u32> {
    Some(ack_and_window(frame)?.0)
}

/// That, and the window it offers alongside it.
fn ack_and_window(frame: &[u8]) -> Option<(u32, u16)> {
    let (_, datagram) = ip::parse(&frame[14..])?;
    let parsed = tcp::Segment::parse(datagram)?;
    Some((parsed.acknowledgement, parsed.window))
}

/// Write a whole stream out through a socket, letting the peer answer each
/// segment, and moving the clock on whenever nothing else can move. Returns
/// how many times the clock was what moved it.
fn transfer(peer: &mut Peer, body: &[u8]) -> usize {
    let mut offset = 0;
    let mut timeouts = 0;
    for _ in 0..8192 {
        while offset < body.len() {
            match peer.socket.send(&body[offset..], None) {
                Ok(0) | Err(_) => break,
                Ok(n) => offset += n,
            }
        }
        if peer.pump() > 0 {
            continue;
        }
        if offset >= body.len() && peer.stream.len() >= body.len() {
            break;
        }
        // Nothing arrived and nothing can arrive: what is left is the clock.
        let deadline = deadline_of(&peer.socket);
        if deadline == u64::MAX {
            break;
        }
        fire_timer(&peer.socket, deadline);
        timeouts += 1;
    }
    timeouts
}

/// What the link can do that a real one cannot be asked for: deliver a
/// segment twice, damage one, and lose a chosen one out of a long stream.
fn lossy_link(report: &mut Report, nic: &'static FakeNic) {
    const SERVER_PORT: u16 = 8085;

    // ---- a segment delivered twice ----
    socket::reset();
    let Some(mut peer) = Peer::open(nic, SERVER_PORT, 40500, 11_000_000) else {
        report.check("a connection to drive", false);
        return;
    };
    peer.to_stack.script(&[Fate::Twice]);
    peer.write(b"twelve bytes");
    report.check(
        "a segment delivered twice is taken once",
        received_len_of(&peer.socket) == 12,
    );
    let sent = nic.take();
    let acks: Vec<u32> = sent.iter().filter_map(|frame| ack_number(frame)).collect();
    report.check(
        "and each copy is acknowledged in the same place",
        acks.len() == 2 && acks[0] == acks[1],
    );
    peer.finish();

    // ---- a segment damaged on the way ----
    socket::reset();
    let Some(mut peer) = Peer::open(nic, SERVER_PORT, 40501, 12_000_000) else {
        report.check("a connection to drive", false);
        return;
    };
    peer.to_stack.script(&[Fate::Corrupt]);
    let sequence = peer.send_next;
    peer.send(sequence, tcp::PSH | tcp::ACK, b"damaged");
    report.check(
        "a damaged segment is neither taken nor answered",
        received_len_of(&peer.socket) == 0 && nic.take().is_empty(),
    );
    peer.to_stack.script(&[]);
    peer.send(sequence, tcp::PSH | tcp::ACK, b"damaged");
    report.check(
        "and the same segment again is taken",
        received_len_of(&peer.socket) == 7,
    );
    peer.finish();

    // ---- a long transfer over a link that loses segments ----
    socket::reset();
    let Some(mut peer) = Peer::open(nic, SERVER_PORT, 40502, 13_000_000) else {
        report.check("a connection to drive", false);
        return;
    };
    // One in seven of what this stack sends, for as far ahead as the stream
    // reaches, so several separate losses have to be recovered from.
    let fates: Vec<Fate> = (0..96)
        .map(|i| if i % 7 == 6 { Fate::Lose } else { Fate::Pass })
        .collect();
    peer.to_peer.script(&fates);
    let body: Vec<u8> = (0..96 * 1024).map(|i| (i % 251) as u8).collect();
    let timeouts = transfer(&mut peer, &body);
    report.check(
        "a transfer over a link that loses segments arrives whole",
        peer.stream == body,
    );
    report.check("with several segments lost on the way", peer.to_peer.lost >= 9);
    report.check(
        "and fewer waits on the clock than there were losses",
        timeouts < peer.to_peer.lost,
    );
    peer.finish();
    socket::reset();
    nic.take();
}

fn fin_received_of(socket: &Arc<InetSocket>) -> bool {
    match &*socket.inner.lock() {
        Protocol::Tcp(tcb) => tcb.fin_received,
        Protocol::Udp(_) => false,
    }
}

/// The acknowledgement this stack would send: where it is in both directions.
fn is_bare_ack(frame: &[u8], port: u16, sequence: u32, acknowledgement: u32) -> bool {
    // Through the IPv4 header's own length: the link pads a short frame, and
    // that padding would read as payload.
    let Some((_, datagram)) = ip::parse(&frame[14..]) else { return false };
    let Some(parsed) = tcp::Segment::parse(datagram) else { return false };
    parsed.destination_port == port
        && parsed.flags == tcp::ACK
        && parsed.sequence == sequence
        && parsed.acknowledgement == acknowledgement
        && parsed.payload.is_empty()
}

/// A segment whose sequence number is nowhere near what this end wants next
/// says nothing about the connection, whatever it is flagged as. Anyone who
/// can send us a packet can guess at these, so acting on one closes a
/// connection for a stranger who never saw a byte of it.
fn unacceptable_segments(report: &mut Report, nic: &FakeNic) {
    const SERVER_PORT: u16 = 8081;

    // ---- a reset at a sequence number nobody is waiting for ----
    socket::reset();
    let Some(connection) = establish(nic, SERVER_PORT, 40100, 2_000_000) else {
        report.check("a connection to drive", false);
        return;
    };
    deliver_ip(
        ip::PROTO_TCP,
        &segment(
            connection.port,
            SERVER_PORT,
            0xDEAD_BEEF,
            connection.server_next,
            tcp::RST | tcp::ACK,
            64240,
            &[],
            &[],
            PEER_IP,
            OUR_IP,
        ),
    );
    report.check(
        "a reset outside the window leaves the connection up",
        state_of(&connection.socket) == tcp::State::Established,
    );
    // RFC 5961: answering would tell a blind sender how close it got.
    report.check("and is not answered", nic.take().is_empty());
    socket::close(&connection.socket);
    socket::close(&connection.listener);
    nic.take();

    // ---- a connection request in the middle of a connection ----
    socket::reset();
    let Some(connection) = establish(nic, SERVER_PORT, 40101, 3_000_000) else {
        report.check("a connection to drive", false);
        return;
    };
    deliver_ip(
        ip::PROTO_TCP,
        &segment(
            connection.port,
            SERVER_PORT,
            0x1234_5678,
            0,
            tcp::SYN,
            64240,
            &[],
            &[],
            PEER_IP,
            OUR_IP,
        ),
    );
    report.check(
        "a stray connection request leaves the connection up",
        state_of(&connection.socket) == tcp::State::Established,
    );
    let sent = nic.take();
    // RFC 5961 section 4: the challenge acknowledgement. The peer that really
    // lost our answer is the only one it helps, and it costs a blind sender
    // the one thing it does not have.
    report.check(
        "and draws an acknowledgement of where we are",
        sent.len() == 1
            && is_bare_ack(&sent[0], connection.port, connection.server_next, connection.client_next),
    );
    socket::close(&connection.socket);
    socket::close(&connection.listener);
    nic.take();

    // ---- a finish at a sequence number long since passed ----
    socket::reset();
    let Some(connection) = establish(nic, SERVER_PORT, 40102, 1_000_000) else {
        report.check("a connection to drive", false);
        return;
    };
    deliver_ip(
        ip::PROTO_TCP,
        &segment(
            connection.port,
            SERVER_PORT,
            7,
            connection.server_next,
            tcp::FIN | tcp::ACK,
            64240,
            &[],
            &[],
            PEER_IP,
            OUR_IP,
        ),
    );
    report.check(
        "a stale finish is not the end of the stream",
        !fin_received_of(&connection.socket)
            && state_of(&connection.socket) == tcp::State::Established,
    );
    let sent = nic.take();
    report.check(
        "and draws an acknowledgement of where we are",
        sent.len() == 1
            && is_bare_ack(&sent[0], connection.port, connection.server_next, connection.client_next),
    );

    // ---- the request again, because our answer was lost ----
    // The one case where a repeated connection request is not an attack: the
    // handshake has to carry on rather than be challenged.
    socket::reset();
    let listener = InetSocket::new(true);
    if listener.bind(Endpoint::new(Ipv4Addr::UNSPECIFIED, SERVER_PORT)).is_err()
        || listener.listen(4).is_err()
    {
        report.check("a listening socket", false);
        return;
    }
    nic.take();
    let request = segment(
        40103,
        SERVER_PORT,
        4_000_000,
        0,
        tcp::SYN,
        64240,
        &[],
        &[],
        PEER_IP,
        OUR_IP,
    );
    deliver_ip(ip::PROTO_TCP, &request);
    let first = nic.take();
    deliver_ip(ip::PROTO_TCP, &request);
    let again = nic.take();
    let repeated = match (first.first(), again.first()) {
        (Some(first), Some(again)) => {
            match (tcp::Segment::parse(&first[34..]), tcp::Segment::parse(&again[34..])) {
                (Some(first), Some(again)) => {
                    first.flags == (tcp::SYN | tcp::ACK)
                        && again.flags == (tcp::SYN | tcp::ACK)
                        && first.sequence == again.sequence
                        && again.acknowledgement == 4_000_001
                }
                _ => false,
            }
        }
        _ => false,
    };
    report.check("a repeated connection request is answered again", repeated);
    socket::close(&listener);
    socket::reset();
    nic.take();
}

fn received_len_of(socket: &Arc<InetSocket>) -> usize {
    match &*socket.inner.lock() {
        Protocol::Tcp(tcb) => tcb.received.len(),
        Protocol::Udp(_) => 0,
    }
}

fn read_shutdown_of(socket: &Arc<InetSocket>) -> bool {
    match &*socket.inner.lock() {
        Protocol::Tcp(tcb) => tcb.read_shutdown,
        Protocol::Udp(_) => false,
    }
}

fn deadline_of(socket: &Arc<InetSocket>) -> u64 {
    match &*socket.inner.lock() {
        Protocol::Tcp(tcb) => tcb.next_deadline(),
        Protocol::Udp(_) => u64::MAX,
    }
}

/// Put the clock where the connection says its next deadline is. Nothing here
/// can wait out a minute, so the timer is driven to the tick it named rather
/// than waited for.
fn fire_timer(socket: &Arc<InetSocket>, now: u64) {
    match &mut *socket.inner.lock() {
        Protocol::Tcp(tcb) => {
            tcb.on_timer(now);
        }
        Protocol::Udp(_) => {}
    }
}

/// A connection closed at this end, acknowledged by the other end, and then
/// left alone. Nothing can ever be read from it and nobody can close it
/// again, so the stack is what has to let go of it.
fn abandoned_connection(report: &mut Report, nic: &FakeNic) {
    const SERVER_PORT: u16 = 8082;
    const CLIENT_PORT: u16 = 40200;

    socket::reset();
    let Some(connection) = establish(nic, SERVER_PORT, CLIENT_PORT, 5_000_000) else {
        report.check("a connection to drive", false);
        return;
    };
    socket::close(&connection.socket);
    nic.take();
    deliver_ip(
        ip::PROTO_TCP,
        &segment(
            CLIENT_PORT,
            SERVER_PORT,
            connection.client_next,
            connection.server_next.wrapping_add(1),
            tcp::ACK,
            64240,
            &[],
            &[],
            PEER_IP,
            OUR_IP,
        ),
    );
    report.check(
        "the finish was acknowledged",
        state_of(&connection.socket) == tcp::State::FinWait2,
    );
    nic.take();

    let deadline = deadline_of(&connection.socket);
    report.check("and the connection does not then wait forever", deadline != u64::MAX);

    // The other end sends data at a socket no descriptor names.
    let payload = [0x5Au8; 1024];
    deliver_ip(
        ip::PROTO_TCP,
        &segment(
            CLIENT_PORT,
            SERVER_PORT,
            connection.client_next,
            connection.server_next.wrapping_add(1),
            tcp::PSH | tcp::ACK,
            64240,
            &[],
            &payload,
            PEER_IP,
            OUR_IP,
        ),
    );
    report.check(
        "data nobody can read is not held",
        received_len_of(&connection.socket) == 0,
    );
    let sent = nic.take();
    report.check(
        "but is acknowledged, so the other end stops sending it",
        sent.len() == 1
            && is_bare_ack(
                &sent[0],
                CLIENT_PORT,
                connection.server_next.wrapping_add(1),
                connection.client_next.wrapping_add(payload.len() as u32),
            ),
    );

    // And then the other end says nothing at all.
    report.check("two sockets in the table", socket::count() == 2);
    fire_timer(&connection.socket, deadline);
    report.check(
        "the deadline closes it",
        state_of(&connection.socket) == tcp::State::Closed,
    );
    super::tcp::on_tick();
    report.check("and it leaves the table", socket::count() == 1);

    // The same connection, finished properly by the other end: the wait state
    // holds nothing and reads as an end.
    socket::reset();
    let Some(connection) = establish(nic, SERVER_PORT, CLIENT_PORT + 1, 6_000_000) else {
        report.check("a connection to drive", false);
        return;
    };
    socket::close(&connection.socket);
    nic.take();
    deliver_ip(
        ip::PROTO_TCP,
        &segment(
            CLIENT_PORT + 1,
            SERVER_PORT,
            connection.client_next,
            connection.server_next.wrapping_add(1),
            tcp::FIN | tcp::ACK,
            64240,
            &[],
            &[],
            PEER_IP,
            OUR_IP,
        ),
    );
    report.check(
        "the other end's finish reaches the wait state",
        state_of(&connection.socket) == tcp::State::TimeWait,
    );
    report.check(
        "which holds nothing and takes nothing more",
        received_len_of(&connection.socket) == 0 && read_shutdown_of(&connection.socket),
    );
    socket::reset();
    nic.take();
}

fn held_runs_of(socket: &Arc<InetSocket>) -> usize {
    match &*socket.inner.lock() {
        Protocol::Tcp(tcb) => tcb.held_runs(),
        Protocol::Udp(_) => 0,
    }
}

/// A segment that arrives before the bytes in front of it is held until they
/// come, and what is held is bounded in every direction someone could push on
/// it.
fn reassembly(report: &mut Report, nic: &'static FakeNic) {
    const SERVER_PORT: u16 = 8086;
    let first_half = b"1234567890";
    let second_half = b"abcdefghij";

    // ---- two segments the wrong way round ----
    socket::reset();
    let Some(mut peer) = Peer::open(nic, SERVER_PORT, 40600, 14_000_000) else {
        report.check("a connection to drive", false);
        return;
    };
    let first = peer.send_next;
    let second = first.wrapping_add(first_half.len() as u32);
    peer.send(second, tcp::PSH | tcp::ACK, second_half);
    report.check(
        "a segment that arrives early is not delivered yet",
        received_len_of(&peer.socket) == 0,
    );
    let sent = nic.take();
    report.check(
        "but is held rather than thrown away",
        tcp::held_bytes() == second_half.len(),
    );
    report.check(
        "and the answer still names the byte that is missing",
        sent.len() == 1 && ack_number(&sent[0]) == Some(first),
    );

    peer.send(first, tcp::PSH | tcp::ACK, first_half);
    let sent = nic.take();
    report.check(
        "filling the gap acknowledges both segments at once",
        sent.len() == 1 && ack_number(&sent[0]) == Some(second + second_half.len() as u32),
    );
    report.check("and nothing is held any more", tcp::held_bytes() == 0);
    let mut buf = [0u8; 64];
    match peer.socket.receive_into(&mut buf, false) {
        Ok(socket::Received { taken: n, .. }) => report.bytes(
            "the stream reads in order",
            &buf[..n],
            b"1234567890abcdefghij",
        ),
        Err(_) => report.check("the stream reads in order", false),
    }
    report.check(
        "and the other end was never asked for anything twice",
        peer.to_stack.carried == 2,
    );
    peer.finish();

    // ---- a finish that arrives before the last of the data ----
    socket::reset();
    let Some(mut peer) = Peer::open(nic, SERVER_PORT, 40601, 15_000_000) else {
        report.check("a connection to drive", false);
        return;
    };
    let first = peer.send_next;
    let second = first.wrapping_add(first_half.len() as u32);
    peer.send(second, tcp::FIN | tcp::PSH | tcp::ACK, second_half);
    report.check(
        "a finish behind a gap is not the end of the stream yet",
        !fin_received_of(&peer.socket)
            && state_of(&peer.socket) == tcp::State::Established,
    );
    nic.take();
    peer.send(first, tcp::PSH | tcp::ACK, first_half);
    report.check(
        "and is taken when the gap fills",
        fin_received_of(&peer.socket)
            && state_of(&peer.socket) == tcp::State::CloseWait,
    );
    let sent = nic.take();
    report.check(
        "acknowledged past the last byte and the finish",
        sent.len() == 1
            && ack_number(&sent[0]) == Some(second + second_half.len() as u32 + 1),
    );
    peer.finish();

    // ---- a stream with one segment held back on the way ----
    socket::reset();
    let Some(mut peer) = Peer::open(nic, SERVER_PORT, 40604, 18_000_000) else {
        report.check("a connection to drive", false);
        return;
    };
    peer.to_stack.script(&[Fate::Pass, Fate::Pass, Fate::Late, Fate::Pass, Fate::Pass]);
    let block: Vec<u8> = (0..2000u32).map(|i| (i % 251) as u8).collect();
    for chunk in block.chunks(400) {
        peer.write(chunk);
    }
    let mut arrived = Vec::new();
    let mut buf = [0u8; 2048];
    while let Ok(socket::Received { taken: n, .. }) = peer.socket.receive_into(&mut buf, false) {
        if n == 0 {
            break;
        }
        arrived.extend_from_slice(&buf[..n]);
    }
    report.check(
        "a stream with one segment delivered late still reads in order",
        arrived == block,
    );
    report.check(
        "and costs the other end nothing to send again",
        peer.to_stack.carried == 5,
    );
    peer.finish();

    // ---- a gap that never fills ----
    socket::reset();
    let Some(mut peer) = Peer::open(nic, SERVER_PORT, 40602, 16_000_000) else {
        report.check("a connection to drive", false);
        return;
    };
    let payload = [0x77u8; 1400];
    peer.send(peer.send_next.wrapping_add(10), tcp::PSH | tcp::ACK, &payload);
    nic.take();
    report.check("what arrived early is held", tcp::held_bytes() == payload.len());
    let deadline = deadline_of(&peer.socket);
    report.check("and there is a deadline to give it up at", deadline != u64::MAX);
    fire_timer(&peer.socket, deadline);
    report.check("which lets it go", tcp::held_bytes() == 0);
    report.check(
        "and leaves the connection up",
        state_of(&peer.socket) == tcp::State::Established,
    );
    peer.finish();

    // ---- one connection, a gap in front of every byte ----
    socket::reset();
    let Some(mut peer) = Peer::open(nic, SERVER_PORT, 40603, 17_000_000) else {
        report.check("a connection to drive", false);
        return;
    };
    let base = peer.send_next;
    for i in 1..64u32 {
        peer.send(base.wrapping_add(i * 2), tcp::PSH | tcp::ACK, b"x");
    }
    nic.take();
    report.check(
        "a gap in front of every byte holds only so many runs",
        held_runs_of(&peer.socket) <= 16 && tcp::held_bytes() <= 16,
    );
    peer.finish();

    // ---- one segment held on each of many connections ----
    socket::reset();
    let listener = InetSocket::new(true);
    if listener.bind(Endpoint::new(Ipv4Addr::UNSPECIFIED, SERVER_PORT)).is_err()
        || listener.listen(4).is_err()
    {
        report.check("a listening socket", false);
        return;
    }
    nic.take();
    const CONNECTIONS: u32 = 128;
    let mut sockets: Vec<Arc<InetSocket>> = Vec::new();
    let early = [0x5Au8; tcp::MSS];
    for i in 0..CONNECTIONS {
        let client_port = 41000 + i as u16;
        let client_iss = 20_000_000 + i * 100_000;
        deliver_ip(
            ip::PROTO_TCP,
            &segment(
                client_port, SERVER_PORT, client_iss, 0, tcp::SYN, 64240, &[], &[], PEER_IP,
                OUR_IP,
            ),
        );
        let sent = nic.take();
        let Some(answer) = sent.first().and_then(|frame| tcp::Segment::parse(&frame[34..]))
        else {
            break;
        };
        let server_next = answer.sequence.wrapping_add(1);
        deliver_ip(
            ip::PROTO_TCP,
            &segment(
                client_port,
                SERVER_PORT,
                client_iss.wrapping_add(1),
                server_next,
                tcp::ACK,
                64240,
                &[],
                &[],
                PEER_IP,
                OUR_IP,
            ),
        );
        nic.take();
        let Ok(Some(socket)) = listener.accept_ready() else { break };
        // A segment one whole segment past what this connection wants next,
        // and nothing to fill the gap it leaves.
        deliver_ip(
            ip::PROTO_TCP,
            &segment(
                client_port,
                SERVER_PORT,
                client_iss.wrapping_add(1 + tcp::MSS as u32),
                server_next,
                tcp::PSH | tcp::ACK,
                64240,
                &[],
                &early,
                PEER_IP,
                OUR_IP,
            ),
        );
        nic.take();
        sockets.push(socket);
    }
    report.check("a connection each for all of them", sockets.len() == CONNECTIONS as usize);
    report.check(
        "what every connection together holds is bounded",
        tcp::held_bytes() <= tcp::REASSEMBLY_LIMIT
            && tcp::held_bytes() + tcp::MSS > tcp::REASSEMBLY_LIMIT,
    );
    socket::reset();
    drop(sockets);
    report.check("and letting them go gives all of it back", tcp::held_bytes() == 0);
    socket::close(&listener);
    socket::reset();
    nic.take();
}

fn rto_of(socket: &Arc<InetSocket>) -> u64 {
    match &*socket.inner.lock() {
        Protocol::Tcp(tcb) => tcb.retransmit_timeout(),
        Protocol::Udp(_) => 0,
    }
}

fn srtt_of(socket: &Arc<InetSocket>) -> u32 {
    match &*socket.inner.lock() {
        Protocol::Tcp(tcb) => tcb.smoothed_round_trip(),
        Protocol::Udp(_) => 0,
    }
}

/// The retransmission timeout comes from a measured round trip, and a segment
/// that was sent twice is not measured.
fn round_trip_estimate(report: &mut Report, nic: &'static FakeNic) {
    const SERVER_PORT: u16 = 8087;

    socket::reset();
    let Some(mut peer) = Peer::open(nic, SERVER_PORT, 40700, 19_000_000) else {
        report.check("a connection to drive", false);
        return;
    };
    // The handshake is itself a round trip: the answer to the connection
    // request was timed and the acknowledgement that completed it measured.
    report.check("the handshake measures a round trip", srtt_of(&peer.socket) != 0);
    let measured = rto_of(&peer.socket);
    report.check(
        "and the timeout comes from that rather than from the opening guess",
        (20..100).contains(&measured),
    );

    // ---- a segment lost, and the one sent in its place ----
    peer.to_peer.script(&[Fate::Lose]);
    let _ = peer.socket.send(b"lost on the way", None);
    peer.pump();
    report.check("a segment the link ate reaches nobody", peer.stream.is_empty());

    let deadline = deadline_of(&peer.socket);
    fire_timer(&peer.socket, deadline);
    let backed_off = rto_of(&peer.socket);
    report.check("the timeout doubles when one runs out", backed_off == measured * 2);
    peer.pump();
    report.check("what was sent again does arrive", peer.stream == b"lost on the way");
    // Karn: the acknowledgement does not say which copy it answers, so the
    // time to it is either the round trip or the round trip plus a timeout.
    report.check(
        "but says nothing about the round trip, so the timeout stays doubled",
        rto_of(&peer.socket) == backed_off,
    );

    // ---- and one that went out once ----
    let _ = peer.socket.send(b"sent once", None);
    peer.pump();
    report.check(
        "a segment sent once brings the timeout back down",
        rto_of(&peer.socket) < backed_off,
    );
    peer.finish();
    socket::reset();
    nic.take();
}

fn fast_retransmits_of(socket: &Arc<InetSocket>) -> u32 {
    match &*socket.inner.lock() {
        Protocol::Tcp(tcb) => tcb.fast_retransmits,
        Protocol::Udp(_) => 0,
    }
}

fn timeouts_of(socket: &Arc<InetSocket>) -> u32 {
    match &*socket.inner.lock() {
        Protocol::Tcp(tcb) => tcb.timeouts,
        Protocol::Udp(_) => 0,
    }
}

fn slow_start_threshold_of(socket: &Arc<InetSocket>) -> u32 {
    match &*socket.inner.lock() {
        Protocol::Tcp(tcb) => tcb.slow_start_threshold(),
        Protocol::Udp(_) => 0,
    }
}

/// A segment lost out of the middle of a stream is sent again on the third
/// acknowledgement that says the same thing, without waiting out a timeout.
fn fast_retransmit(report: &mut Report, nic: &'static FakeNic) {
    const SERVER_PORT: u16 = 8088;

    socket::reset();
    let Some(mut peer) = Peer::open(nic, SERVER_PORT, 40800, 21_000_000) else {
        report.check("a connection to drive", false);
        return;
    };
    // Far enough into the stream that several segments are in flight behind
    // the one that goes missing, which is what produces the repeated
    // acknowledgements at all.
    let mut fates = [Fate::Pass; 9];
    fates[8] = Fate::Lose;
    peer.to_peer.script(&fates);
    let opened = slow_start_threshold_of(&peer.socket);
    let body: Vec<u8> = (0..32 * 1024).map(|i| (i % 251) as u8).collect();
    let waits = transfer(&mut peer, &body);

    report.check("the stream arrives whole", peer.stream == body);
    report.check(
        "after the other end said the same thing three times or more",
        peer.repeated_acks >= 3,
    );
    report.check(
        "the missing segment was sent again on that",
        fast_retransmits_of(&peer.socket) >= 1,
    );
    report.check(
        "and the clock was never waited out",
        waits == 0 && timeouts_of(&peer.socket) == 0,
    );
    // A machine that retransmits quickly and sends just as much as before is
    // worse on a congested link than one that does neither.
    report.check(
        "and the point it stops opening the window at came down with it",
        slow_start_threshold_of(&peer.socket) < opened,
    );
    peer.finish();

    // ---- what the other end tests before it believes a repeat ----
    //
    // The other end counts an acknowledgement as a repeat only if the window
    // it offers has not changed; one that has changed is a window update. A
    // window that moved every time the program read a byte, in between the
    // repeats, left the other end waiting out its own timeout for every
    // segment it lost.
    socket::reset();
    let Some(mut peer) = Peer::open(nic, SERVER_PORT, 40801, 22_000_000) else {
        report.check("a connection to drive", false);
        return;
    };
    let block = [0x33u8; 1460];
    for _ in 0..3 {
        peer.write(&block);
    }
    nic.take();
    let hole = peer.send_next;
    peer.send_next = peer.send_next.wrapping_add(block.len() as u32);
    let mut repeats: Vec<(u32, u16)> = Vec::new();
    for i in 0..4u32 {
        peer.send(
            hole.wrapping_add((i + 1) * block.len() as u32),
            tcp::PSH | tcp::ACK,
            &block,
        );
        // The program takes some of what arrived before the gap, which is
        // what used to move the window between one repeat and the next.
        let mut buf = [0u8; 1024];
        let _ = peer.socket.receive_into(&mut buf, false);
        for frame in nic.take() {
            if let Some(pair) = ack_and_window(&frame) {
                repeats.push(pair);
            }
        }
    }
    report.check(
        "an acknowledgement repeated while a gap waits names the same byte",
        repeats.len() >= 4 && repeats.iter().all(|(ack, _)| *ack == hole),
    );
    report.check(
        "and offers the same window, which is what makes it count as a repeat",
        repeats.windows(2).all(|pair| pair[0].1 == pair[1].1),
    );
    peer.finish();
    socket::reset();
    nic.take();
}

/// An echo request, ready to be addressed anywhere.
fn echo_request() -> Vec<u8> {
    let mut request = Vec::new();
    request.push(8);
    request.push(0);
    request.extend_from_slice(&[0, 0]);
    request.extend_from_slice(&[0x13, 0x37]);
    request.extend_from_slice(&[0x00, 0x09]);
    request.extend_from_slice(b"abcdefghijklmnopqrstuvwabcdefghi");
    let checksum = ip::checksum(&request);
    request[2..4].copy_from_slice(&checksum.to_be_bytes());
    request
}

/// A frame off the card addressed to somewhere this machine is not, or from
/// somewhere it is. Each of these is one frame from whoever can reach the
/// card, and each of them used to be answered.
fn foreign_addresses(report: &mut Report, nic: &FakeNic) {
    const SERVER_PORT: u16 = 8083;
    const CLIENT_PORT: u16 = 40300;
    let loopback = Ipv4Addr::new(127, 0, 0, 1);
    let subnet_broadcast = Ipv4Addr::new(10, 0, 2, 255);

    // ---- a socket bound to the loopback address is not on the network ----
    socket::reset();
    let listener = InetSocket::new(true);
    if listener.bind(Endpoint::new(loopback, SERVER_PORT)).is_err() || listener.listen(4).is_err() {
        report.check("a socket bound to the loopback address", false);
        return;
    }
    nic.take();
    deliver_ip_between(
        ip::PROTO_TCP,
        &segment(
            CLIENT_PORT, SERVER_PORT, 7_000_000, 0, tcp::SYN, 64240, &[], &[], PEER_IP, loopback,
        ),
        PEER_IP,
        loopback,
    );
    report.check(
        "a connection request to the loopback address is not answered",
        nic.take().is_empty(),
    );
    report.check("and reaches no socket", socket::count() == 1);
    socket::reset();

    // ---- the broadcast addresses ----
    let listener = InetSocket::new(true);
    if listener.bind(Endpoint::new(Ipv4Addr::UNSPECIFIED, SERVER_PORT)).is_err()
        || listener.listen(4).is_err()
    {
        report.check("a listening socket", false);
        return;
    }
    nic.take();
    for (name, destination) in [
        ("the all-ones broadcast", Ipv4Addr::BROADCAST),
        ("the subnet broadcast", subnet_broadcast),
    ] {
        deliver_ip_between(
            ip::PROTO_TCP,
            &segment(
                CLIENT_PORT, SERVER_PORT, 7_100_000, 0, tcp::SYN, 64240, &[], &[], PEER_IP,
                destination,
            ),
            PEER_IP,
            destination,
        );
        // RFC 1122: a segment addressed to a broadcast is discarded.
        report.check(name, nic.take().is_empty() && socket::count() == 1);

        // And a closed port there draws nothing either, which is what would
        // otherwise send a reset from an address this machine does not have.
        deliver_ip_between(
            ip::PROTO_TCP,
            &segment(
                CLIENT_PORT, 9999, 7_200_000, 0, tcp::SYN, 64240, &[], &[], PEER_IP, destination,
            ),
            PEER_IP,
            destination,
        );
        report.check("a closed port on it is not refused either", nic.take().is_empty());

        // Nor is an echo request: answering one makes this machine a way of
        // pointing traffic at somebody else. Linux ignores these by default.
        deliver_ip_between(ip::PROTO_ICMP, &echo_request(), PEER_IP, destination);
        report.check("and an echo request to it draws no reply", nic.take().is_empty());
    }

    // ---- a frame claiming to come from us ----
    deliver_ip_between(
        ip::PROTO_TCP,
        &segment(
            CLIENT_PORT, SERVER_PORT, 7_300_000, 0, tcp::SYN, 64240, &[], &[], OUR_IP, OUR_IP,
        ),
        OUR_IP,
        OUR_IP,
    );
    report.check(
        "a frame off the card claiming our own address is dropped",
        nic.take().is_empty() && socket::count() == 1,
    );
    socket::close(&listener);
    socket::reset();
    nic.take();

    // ---- what a broadcast is still for ----
    let socket = InetSocket::new(false);
    if socket.bind(Endpoint::new(Ipv4Addr::UNSPECIFIED, 7779)).is_err() {
        report.check("a datagram socket", false);
        return;
    }
    nic.take();
    deliver_ip_between(
        ip::PROTO_UDP,
        &udp_datagram(5555, 7779, b"to everyone", PEER_IP, subnet_broadcast),
        PEER_IP,
        subnet_broadcast,
    );
    let mut buf = [0u8; 64];
    match socket.receive_into(&mut buf, false) {
        Ok(socket::Received { taken: n, .. }) => report.bytes("a broadcast datagram still arrives", &buf[..n], b"to everyone"),
        Err(_) => report.check("a broadcast datagram still arrives", false),
    }
    socket::close(&socket);
    socket::reset();
    nic.take();
}

/// The sequence number this stack picks for a connection request from
/// `client_port`, taken off the wire.
fn answered_sequence(nic: &FakeNic, server_port: u16, client_port: u16) -> Option<u32> {
    deliver_ip(
        ip::PROTO_TCP,
        &segment(
            client_port, server_port, 9_000_000, 0, tcp::SYN, 64240, &[], &[], PEER_IP, OUR_IP,
        ),
    );
    let sent = nic.take();
    if sent.len() != 1 {
        return None;
    }
    let (_, datagram) = ip::parse(&sent[0][14..])?;
    Some(tcp::Segment::parse(datagram)?.sequence)
}

/// An initial sequence number must not be something the other end can work
/// out. It used to be the uptime plus a published function of the two ports
/// and the peer's address, so one connection gave away the clock and every
/// other connection's number followed from it -- which is the whole of what a
/// blind attacker is otherwise missing.
fn initial_sequence_numbers(report: &mut Report, nic: &FakeNic) {
    const SERVER_PORT: u16 = 8084;

    /// The published half of it: what an attacker who has one number and the
    /// port numbers computes for any other connection.
    fn salt(local_port: u16, remote_port: u16, remote: Ipv4Addr) -> u32 {
        let salt =
            ((local_port as u32) << 16) ^ (remote_port as u32) ^ remote.0.rotate_left(13);
        salt.wrapping_mul(0x9E37_79B9)
    }

    socket::reset();
    let listener = InetSocket::new(true);
    if listener.bind(Endpoint::new(Ipv4Addr::UNSPECIFIED, SERVER_PORT)).is_err()
        || listener.listen(8).is_err()
    {
        report.check("a listening socket", false);
        return;
    }
    nic.take();

    const PORTS: [u16; 4] = [40400, 40401, 40402, 40403];
    let mut numbers = [0u32; PORTS.len()];
    for (slot, port) in numbers.iter_mut().zip(PORTS) {
        match answered_sequence(nic, SERVER_PORT, port) {
            Some(sequence) => *slot = sequence,
            None => {
                report.check("four connection requests answered", false);
                socket::reset();
                nic.take();
                return;
            }
        }
    }

    // The first number and the published function give the clock; the rest
    // follow from it. A count of ticks is 4 microseconds, and these four
    // requests are one after another, so a prediction that is right to within
    // a quarter of a second is a prediction. Two of the three landing inside
    // that by chance is a one in a billion event, so the check tolerates one.
    let clock = numbers[0].wrapping_sub(salt(SERVER_PORT, PORTS[0], PEER_IP));
    let mut predicted = 0;
    for (number, port) in numbers.iter().zip(PORTS).skip(1) {
        let guess = clock.wrapping_add(salt(SERVER_PORT, port, PEER_IP));
        if (number.wrapping_sub(guess) as i32).unsigned_abs() < 1 << 16 {
            predicted += 1;
        }
    }
    report.check(
        "one connection's initial sequence number does not give away another's",
        predicted <= 1,
    );
    socket::close(&listener);
    socket::reset();
    nic.take();
}

/// A datagram socket takes what arrives and sends what it is given.
fn datagrams(report: &mut Report, nic: &FakeNic) {
    const LOCAL_PORT: u16 = 7777;
    const REMOTE_PORT: u16 = 5555;

    let socket = InetSocket::new(false);
    if socket.bind(Endpoint::new(Ipv4Addr::UNSPECIFIED, LOCAL_PORT)).is_err() {
        report.check("bind a datagram socket", false);
        return;
    }
    nic.take();

    deliver_ip(
        ip::PROTO_UDP,
        &udp_datagram(REMOTE_PORT, LOCAL_PORT, b"ping", PEER_IP, OUR_IP),
    );
    let mut buf = [0u8; 64];
    match socket.receive_into(&mut buf, false) {
        Ok(socket::Received { taken: n, from, .. }) => {
            report.bytes("the datagram that arrived", &buf[..n], b"ping");
            report.check(
                "from the right place",
                from == Some(Endpoint::new(PEER_IP, REMOTE_PORT)),
            );
        }
        Err(_) => report.check("the datagram that arrived", false),
    }
    report.check("nothing sent in answer", nic.take().is_empty());

    match socket.send(b"pong", Some(Endpoint::new(PEER_IP, REMOTE_PORT))) {
        Ok(n) => report.value("all four bytes sent", n as u64, 4),
        Err(_) => report.check("all four bytes sent", false),
    }
    let sent = nic.take();
    report.check("one datagram on the wire", sent.len() == 1);
    if sent.len() != 1 {
        return;
    }
    let expected = expected_frame(
        ether::ETHERTYPE_IPV4,
        &datagram(
            identification_of(&sent[0]),
            ip::PROTO_UDP,
            OUR_IP,
            PEER_IP,
            &udp_datagram(LOCAL_PORT, REMOTE_PORT, b"pong", OUR_IP, PEER_IP),
        ),
    );
    report.bytes("the datagram sent", &sent[0], &expected);

    // A datagram for a port nothing is bound to is dropped in silence.
    deliver_ip(
        ip::PROTO_UDP,
        &udp_datagram(REMOTE_PORT, 7778, b"nobody", PEER_IP, OUR_IP),
    );
    report.check("a datagram for nobody is dropped", nic.take().is_empty());

    // Shutting the read side down throws away what is queued, and the count
    // of queued bytes has to go with it or the socket believes it is holding
    // datagrams that are not there.
    for text in [b"first datagram".as_slice(), b"second datagram".as_slice()] {
        deliver_ip(
            ip::PROTO_UDP,
            &udp_datagram(REMOTE_PORT, LOCAL_PORT, text, PEER_IP, OUR_IP),
        );
    }
    let held = queued_bytes_of(&socket);
    if socket.shutdown(true, false).is_err() {
        report.check("the read side can be shut down", false);
        return;
    }
    report.check("what was queued is accounted for", held == 29);
    report.check("and shutting the read side down gives it back", queued_bytes_of(&socket) == 0);
    match socket.receive_into(&mut buf, false) {
        Ok(socket::Received { taken: n, .. }) => report.check("a receive afterwards reports the end", n == 0),
        Err(_) => report.check("a receive afterwards reports the end", false),
    }
    // What arrives next is the only thing held, rather than the last of a
    // queue the socket thinks is still full.
    deliver_ip(
        ip::PROTO_UDP,
        &udp_datagram(REMOTE_PORT, LOCAL_PORT, b"later", PEER_IP, OUR_IP),
    );
    report.check("and the room is there for what comes next", queued_bytes_of(&socket) == 5);

    socket::close(&socket);
}

// ---- having no address, and asking for one --------------------------------

const NETMASK: Ipv4Addr = Ipv4Addr::new(255, 255, 255, 0);
/// The name server the DHCP checks' server hands out. It is not the one the
/// other checks run with, so /etc/resolv.conf shows which of them wrote it.
const LEASE_DNS: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 53);
/// The client never reads the clock itself; these checks say what time it is,
/// starting from here.
const START: u64 = 1_000_000;
const HZ: u64 = crate::arch::TICK_HZ as u64;

const DISCOVER: u8 = 1;
const OFFER: u8 = 2;
const REQUEST: u8 = 3;
const ACK: u8 = 5;
const NAK: u8 = 6;
/// A lease time option of one day.
const A_DAY: [u8; 6] = [51, 4, 0x00, 0x01, 0x51, 0x80];
/// Where the DHCP message starts in a frame: after the Ethernet, IPv4 and UDP
/// headers.
const DHCP_AT: usize = 14 + 20 + 8;

/// A frame from the peer to everyone.
fn broadcast_frame(ethertype: u16, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&[0xFF; 6]);
    out.extend_from_slice(&PEER_MAC);
    out.extend_from_slice(&ethertype.to_be_bytes());
    out.extend_from_slice(payload);
    while out.len() < 60 {
        out.push(0);
    }
    out
}

/// A frame as this stack should have broadcast it.
fn expected_broadcast(ethertype: u16, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&[0xFF; 6]);
    out.extend_from_slice(&OUR_MAC);
    out.extend_from_slice(&ethertype.to_be_bytes());
    out.extend_from_slice(payload);
    while out.len() < 60 {
        out.push(0);
    }
    out
}

/// An ARP packet from the peer, about the peer, to `target`.
fn arp_from_peer(operation: u16, target_mac: [u8; 6], target: Ipv4Addr) -> Vec<u8> {
    let mut packet = Vec::new();
    packet.extend_from_slice(&[0x00, 0x01, 0x08, 0x00, 6, 4]); // ethernet, ipv4, lengths
    packet.extend_from_slice(&operation.to_be_bytes());
    packet.extend_from_slice(&PEER_MAC);
    packet.extend_from_slice(&PEER_IP.to_be_bytes());
    packet.extend_from_slice(&target_mac);
    packet.extend_from_slice(&target.to_be_bytes());
    packet
}

/// A file's contents, or nothing if there is no such file.
fn read_file(path: &str) -> Vec<u8> {
    let Ok(node) = crate::fs::lookup(path) else { return Vec::new() };
    let mut buf = [0u8; 512];
    let length = node.read_at(crate::fs::Offset::new(0), &mut buf).unwrap_or(0);
    buf[..length].to_vec()
}

/// What `Config::new` takes and what it refuses.
fn configurations(report: &mut Report) {
    report.check(
        "an address, a netmask, a gateway and a name server make a configuration",
        Config::new(OUR_IP, NETMASK, Some(PEER_IP), &[NAMESERVER_IP]).is_ok(),
    );
    for (name, address) in [
        ("0.0.0.0 is not an address a host holds", Ipv4Addr::UNSPECIFIED),
        ("nor is the all-ones broadcast", Ipv4Addr::BROADCAST),
        ("nor a loopback address", Ipv4Addr::new(127, 0, 0, 1)),
        ("nor a multicast address", Ipv4Addr::new(224, 0, 0, 1)),
        ("nor the subnet's own broadcast address", Ipv4Addr::new(10, 0, 2, 255)),
        ("nor the subnet's network address", Ipv4Addr::new(10, 0, 2, 0)),
    ] {
        report.check(name, Config::new(address, NETMASK, None, &[]).is_err());
    }
    report.check(
        "a netmask with a hole in it is refused",
        Config::new(OUR_IP, Ipv4Addr::new(255, 0, 255, 0), None, &[]).is_err(),
    );
    report.check(
        "and so is a netmask of nothing",
        Config::new(OUR_IP, Ipv4Addr::UNSPECIFIED, None, &[]).is_err(),
    );
    report.check(
        "a gateway off the subnet is refused",
        Config::new(OUR_IP, NETMASK, Some(Ipv4Addr::new(10, 0, 3, 1)), &[]).is_err(),
    );
    report.check(
        "and so is a gateway that is this machine",
        Config::new(OUR_IP, NETMASK, Some(OUR_IP), &[]).is_err(),
    );
    let one = Ipv4Addr::new(1, 1, 1, 1);
    let eight = Ipv4Addr::new(8, 8, 8, 8);
    let servers = [Ipv4Addr::UNSPECIFIED, NAMESERVER_IP, one, eight, Ipv4Addr::new(9, 9, 9, 9)];
    report.check(
        "zero is left out of the name servers, and so is any past the third",
        Config::new(OUR_IP, NETMASK, None, &servers)
            .is_ok_and(|config| config.nameservers() == [NAMESERVER_IP, one, eight]),
    );
    report.check(
        "a netmask left out is the one the address's class implies",
        Config::class_netmask(Ipv4Addr::new(10, 1, 2, 3)) == Ipv4Addr::new(255, 0, 0, 0)
            && Config::class_netmask(Ipv4Addr::new(172, 16, 0, 1)) == Ipv4Addr::new(255, 255, 0, 0)
            && Config::class_netmask(Ipv4Addr::new(192, 168, 86, 57)) == NETMASK,
    );
}

/// With no configuration, a socket reaches nothing but the broadcast address
/// and nothing but a broadcast reaches it. A socket still holding an address
/// the machine no longer has sends nothing from it.
fn no_address(report: &mut Report, nic: &FakeNic) {
    const PORT: u16 = 7790;
    let subnet_broadcast = Ipv4Addr::new(10, 0, 2, 255);
    socket::reset();
    super::configure(None);
    nic.take();

    let socket = InetSocket::new(false);
    if socket.bind(Endpoint::new(Ipv4Addr::UNSPECIFIED, PORT)).is_err() {
        report.check("a datagram socket", false);
        return;
    }
    report.check(
        "a datagram to a host has nowhere to go",
        matches!(
            socket.send(b"hello", Some(Endpoint::new(PEER_IP, 5555))),
            Err(Errno::ENETUNREACH)
        ),
    );
    report.check(
        "nor has a connection",
        matches!(InetSocket::new(true).connect(Endpoint::new(PEER_IP, 80)), Err(Errno::ENETUNREACH)),
    );
    report.check(
        "nor a connection to the broadcast address",
        matches!(
            InetSocket::new(true).connect(Endpoint::new(Ipv4Addr::BROADCAST, 80)),
            Err(Errno::ENETUNREACH)
        ),
    );
    report.check("and none of them put anything on the wire", nic.take().is_empty());
    report.check(
        "the address the machine used to have cannot be bound",
        matches!(
            InetSocket::new(false).bind(Endpoint::new(OUR_IP, PORT + 1)),
            Err(Errno::EADDRNOTAVAIL)
        ),
    );

    match socket.send(b"anyone?", Some(Endpoint::new(Ipv4Addr::BROADCAST, 5555))) {
        Ok(n) => report.value("a broadcast can still be sent", n as u64, 7),
        Err(_) => report.check("a broadcast can still be sent", false),
    }
    let sent = nic.take();
    report.check("as one frame", sent.len() == 1);
    if let [frame] = sent.as_slice() {
        let expected = expected_broadcast(
            ether::ETHERTYPE_IPV4,
            &datagram(
                identification_of(frame),
                ip::PROTO_UDP,
                Ipv4Addr::UNSPECIFIED,
                Ipv4Addr::BROADCAST,
                &udp_datagram(PORT, 5555, b"anyone?", Ipv4Addr::UNSPECIFIED, Ipv4Addr::BROADCAST),
            ),
        );
        report.bytes("from 0.0.0.0, to everyone", frame, &expected);
    }

    deliver_ip(ip::PROTO_UDP, &udp_datagram(5555, PORT, b"to an address", PEER_IP, OUR_IP));
    report.check(
        "a datagram sent to an address is not taken by a machine with none",
        queued_bytes_of(&socket) == 0,
    );
    deliver_ip_between(
        ip::PROTO_UDP,
        &udp_datagram(5555, PORT, b"to a subnet", PEER_IP, subnet_broadcast),
        PEER_IP,
        subnet_broadcast,
    );
    report.check(
        "nor one sent to a subnet it is not known to be on",
        queued_bytes_of(&socket) == 0,
    );
    deliver_ip_between(
        ip::PROTO_UDP,
        &udp_datagram(5555, PORT, b"to everyone", PEER_IP, Ipv4Addr::BROADCAST),
        PEER_IP,
        Ipv4Addr::BROADCAST,
    );
    report.check("but one sent to everyone is", queued_bytes_of(&socket) == 11);

    deliver(ether::ETHERTYPE_ARP, &arp_from_peer(super::arp::OP_REQUEST, [0; 6], OUR_IP));
    deliver_ip(ip::PROTO_ICMP, &echo_request());
    report.check(
        "the address it used to have is answered for by neither ARP nor ping",
        nic.take().is_empty(),
    );
    socket::close(&socket);

    // ---- an address taken away from under a socket ----
    super::configure(Some(test_config()));
    let bound = InetSocket::new(false);
    if bound.bind(Endpoint::new(OUR_IP, PORT + 2)).is_err() {
        report.check("a socket bound to the address", false);
        return;
    }
    let other = Config::new(Ipv4Addr::new(10, 0, 2, 16), NETMASK, Some(PEER_IP), &[])
        .expect("another address on the subnet");
    super::configure(Some(other));
    nic.take();
    report.check(
        "a socket bound to an address the machine no longer has cannot send from it",
        matches!(
            bound.send(b"stale", Some(Endpoint::new(PEER_IP, 5555))),
            Err(Errno::EADDRNOTAVAIL)
        ) && nic.take().is_empty(),
    );
    socket::close(&bound);
    socket::reset();
    super::configure(Some(test_config()));
    nic.take();
}

/// A message as a DHCP server sends one, fields written out one by one.
fn bootp_reply(xid: u32, chaddr: [u8; 6], yiaddr: Ipv4Addr, options: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(2); // a reply
    out.push(1); // ethernet
    out.push(6); // hardware address length
    out.push(0); // hops
    out.extend_from_slice(&xid.to_be_bytes());
    out.extend_from_slice(&[0, 0]); // secs
    out.extend_from_slice(&[0x80, 0x00]); // flags: broadcast, as the request asked
    out.extend_from_slice(&[0; 4]); // ciaddr
    out.extend_from_slice(&yiaddr.to_be_bytes());
    out.extend_from_slice(&[0; 4]); // siaddr
    out.extend_from_slice(&[0; 4]); // giaddr
    out.extend_from_slice(&chaddr);
    out.extend_from_slice(&[0; 10]); // the rest of chaddr
    out.extend_from_slice(&[0; 64]); // sname
    out.extend_from_slice(&[0; 128]); // file
    out.extend_from_slice(&[99, 130, 83, 99]); // magic cookie
    out.extend_from_slice(options);
    out
}

/// A server's options: the message type, the server, the netmask, the router
/// and the name server, then `extra`, then the end.
fn server_options(kind: u8, extra: &[u8]) -> Vec<u8> {
    let mut options = alloc::vec![53, 1, kind]; // message type
    options.extend_from_slice(&[54, 4, 10, 0, 2, 2]); // server identifier
    options.extend_from_slice(&[1, 4, 255, 255, 255, 0]); // netmask
    options.extend_from_slice(&[3, 4, 10, 0, 2, 2]); // router
    options.extend_from_slice(&[6, 4, 10, 0, 2, 53]); // name server
    options.extend_from_slice(extra);
    options.push(255);
    options
}

/// A message as this client should have sent it, padded to 300 bytes.
fn bootp_request(xid: u32, secs: u16, flags: u16, ciaddr: Ipv4Addr, options: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(1); // a request
    out.push(1); // ethernet
    out.push(6); // hardware address length
    out.push(0); // hops
    out.extend_from_slice(&xid.to_be_bytes());
    out.extend_from_slice(&secs.to_be_bytes());
    out.extend_from_slice(&flags.to_be_bytes());
    out.extend_from_slice(&ciaddr.to_be_bytes());
    out.extend_from_slice(&[0; 12]); // yiaddr, siaddr, giaddr
    out.extend_from_slice(&OUR_MAC);
    out.extend_from_slice(&[0; 10]); // the rest of chaddr
    out.extend_from_slice(&[0; 192]); // sname and file
    out.extend_from_slice(&[99, 130, 83, 99]); // magic cookie
    out.extend_from_slice(options);
    while out.len() < 300 {
        out.push(0);
    }
    out
}

/// The options this client should send.
fn client_options(kind: u8, requested: Option<Ipv4Addr>, server: Option<Ipv4Addr>) -> Vec<u8> {
    let mut options = alloc::vec![53, 1, kind]; // message type
    options.extend_from_slice(&[61, 7, 1]); // client identifier: ethernet, then the card
    options.extend_from_slice(&OUR_MAC);
    if let Some(requested) = requested {
        options.extend_from_slice(&[50, 4]); // requested address
        options.extend_from_slice(&requested.to_be_bytes());
    }
    if let Some(server) = server {
        options.extend_from_slice(&[54, 4]); // server identifier
        options.extend_from_slice(&server.to_be_bytes());
    }
    // Asking for the netmask, router, name servers, lease time, T1 and T2.
    options.extend_from_slice(&[55, 6, 1, 3, 6, 51, 58, 59]);
    options.push(255);
    options
}

/// The transaction id of a message this client sent. It is drawn at random, so
/// it is read back rather than predicted.
fn xid_of(frame: &[u8]) -> u32 {
    match frame.get(DHCP_AT + 4..DHCP_AT + 8) {
        Some(&[a, b, c, d]) => u32::from_be_bytes([a, b, c, d]),
        _ => 0,
    }
}

/// The message type of a message this client sent, which is its first option.
fn kind_of(frame: &[u8]) -> u8 {
    frame.get(DHCP_AT + 242).copied().unwrap_or(0)
}

/// A server's message to port 68, broadcast.
fn deliver_dhcp_broadcast(message: &[u8]) {
    let packet = datagram(
        0x4242,
        ip::PROTO_UDP,
        PEER_IP,
        Ipv4Addr::BROADCAST,
        &udp_datagram(67, 68, message, PEER_IP, Ipv4Addr::BROADCAST),
    );
    super::receive(&broadcast_frame(ether::ETHERTYPE_IPV4, &packet));
}

/// A server's message to port 68 at this machine's address, which is how a
/// renewal is answered.
fn deliver_dhcp_to_us(message: &[u8]) {
    deliver_ip(ip::PROTO_UDP, &udp_datagram(67, 68, message, PEER_IP, OUR_IP));
}

/// What the checks' DHCP server hands out.
fn leased_config() -> Config {
    Config::new(OUR_IP, NETMASK, Some(PEER_IP), &[LEASE_DNS]).expect("the lease the checks hand out")
}

fn new_client(report: &mut Report, nameserver: Option<Ipv4Addr>) -> Option<super::dhcp::Client> {
    socket::reset();
    super::configure(None);
    match super::dhcp::Client::new(OUR_MAC, nameserver, START) {
        Ok(client) => Some(client),
        Err(_) => {
            report.check("a client on port 68", false);
            None
        }
    }
}

/// Take a client through DISCOVER, OFFER, REQUEST and a day's ACK. The
/// transaction id, or nothing if a step did not happen.
fn lease_up(client: &mut super::dhcp::Client, nic: &FakeNic) -> Option<u32> {
    client.run(START);
    let xid = xid_of(nic.take().first()?);
    deliver_dhcp_broadcast(&bootp_reply(xid, OUR_MAC, OUR_IP, &server_options(OFFER, &A_DAY)));
    client.run(START + 1);
    if nic.take().len() != 1 {
        return None;
    }
    deliver_dhcp_broadcast(&bootp_reply(xid, OUR_MAC, OUR_IP, &server_options(ACK, &A_DAY)));
    client.run(START + 2);
    client.lease().map(|_| xid)
}

/// One lease from the first DISCOVER to its end: taken, renewed with the
/// server that granted it, rebound with anyone, and given up.
fn dhcp_exchange(report: &mut Report, nic: &FakeNic) {
    nic.take();
    let Some(mut client) = new_client(report, None) else { return };

    // ---- the discover ----
    client.run(START);
    let sent = nic.take();
    report.check("one discover", sent.len() == 1);
    let [discover] = sent.as_slice() else { return };
    let xid = xid_of(discover);
    let expected = expected_broadcast(
        ether::ETHERTYPE_IPV4,
        &datagram(
            identification_of(discover),
            ip::PROTO_UDP,
            Ipv4Addr::UNSPECIFIED,
            Ipv4Addr::BROADCAST,
            &udp_datagram(
                68,
                67,
                &bootp_request(xid, 0, 0x8000, Ipv4Addr::UNSPECIFIED, &client_options(DISCOVER, None, None)),
                Ipv4Addr::UNSPECIFIED,
                Ipv4Addr::BROADCAST,
            ),
        ),
    );
    report.bytes("the discover: from 0.0.0.0 to everyone, asking to be answered the same way", discover, &expected);
    let wait = client.next_deadline() - START;
    report.check("and the next is four seconds away, give or take one", (3 * HZ..=5 * HZ).contains(&wait));

    // ---- answers that are not this client's ----
    let offer = server_options(OFFER, &A_DAY);
    deliver_dhcp_broadcast(&bootp_reply(xid ^ 1, OUR_MAC, OUR_IP, &offer));
    deliver_dhcp_broadcast(&bootp_reply(xid, PEER_MAC, OUR_IP, &offer));
    client.run(START + 1);
    report.check(
        "an offer in another exchange, or for another card, is not taken",
        nic.take().is_empty(),
    );

    // ---- the offer, and the request for it ----
    deliver_dhcp_broadcast(&bootp_reply(xid, OUR_MAC, OUR_IP, &offer));
    client.run(START + 2);
    let sent = nic.take();
    report.check("one request for the offer", sent.len() == 1);
    let [request] = sent.as_slice() else { return };
    let expected = expected_broadcast(
        ether::ETHERTYPE_IPV4,
        &datagram(
            identification_of(request),
            ip::PROTO_UDP,
            Ipv4Addr::UNSPECIFIED,
            Ipv4Addr::BROADCAST,
            &udp_datagram(
                68,
                67,
                &bootp_request(
                    xid,
                    0,
                    0x8000,
                    Ipv4Addr::UNSPECIFIED,
                    &client_options(REQUEST, Some(OUR_IP), Some(PEER_IP)),
                ),
                Ipv4Addr::UNSPECIFIED,
                Ipv4Addr::BROADCAST,
            ),
        ),
    );
    report.bytes("the request: in the offer's exchange, naming the address and the server", request, &expected);

    // ---- the acknowledgement ----
    deliver_dhcp_broadcast(&bootp_reply(xid, OUR_MAC, OUR_IP, &server_options(ACK, &A_DAY)));
    client.run(START + 3);
    report.check("the acknowledgement needs no answer", nic.take().is_empty());
    report.check(
        "and the lease it grants is the machine's configuration",
        super::config() == Some(leased_config()),
    );
    let requested_at = START + 2;
    report.check(
        "for a day from the request, renewed at half of it and rebound at seven eighths",
        client.lease().is_some_and(|lease| {
            lease.renew_at == requested_at + 43_200 * HZ
                && lease.rebind_at == requested_at + 75_600 * HZ
                && lease.expires_at == requested_at + 86_400 * HZ
        }),
    );
    let resolv = read_file("/etc/resolv.conf");
    let servers: Vec<&str> = core::str::from_utf8(&resolv)
        .unwrap_or("")
        .lines()
        .filter(|line| line.starts_with("nameserver"))
        .collect();
    report.check("and /etc/resolv.conf names the lease's name server", servers == ["nameserver 10.0.2.53"]);

    // ---- T1: renewing with the server that granted it ----
    let t1 = requested_at + 43_200 * HZ;
    client.run(t1 - 1);
    report.check("nothing is said before T1", nic.take().is_empty());
    client.run(t1);
    let sent = nic.take();
    // The configuration changed, which forgot every hardware address, so the
    // renewal to the server waits on the server's.
    report.check(
        "at T1 the server's hardware address is asked for",
        matches!(sent.as_slice(), [frame] if u16::from_be_bytes([frame[12], frame[13]]) == ether::ETHERTYPE_ARP),
    );
    deliver(ether::ETHERTYPE_ARP, &arp_from_peer(super::arp::OP_REPLY, OUR_MAC, OUR_IP));
    let sent = nic.take();
    report.check("and the renewal goes when that is answered", sent.len() == 1);
    let [renewal] = sent.as_slice() else { return };
    let renew_xid = xid_of(renewal);
    let expected = expected_frame(
        ether::ETHERTYPE_IPV4,
        &datagram(
            identification_of(renewal),
            ip::PROTO_UDP,
            OUR_IP,
            PEER_IP,
            &udp_datagram(
                68,
                67,
                &bootp_request(renew_xid, 0, 0, OUR_IP, &client_options(REQUEST, None, None)),
                OUR_IP,
                PEER_IP,
            ),
        ),
    );
    report.bytes("the renewal: from the leased address to the server alone, naming neither", renewal, &expected);
    report.check("in an exchange of its own", renew_xid != xid);

    // 1200 seconds, T1 at 600 and T2 at 900.
    let shorter = [51, 4, 0, 0, 0x04, 0xB0, 58, 4, 0, 0, 0x02, 0x58, 59, 4, 0, 0, 0x03, 0x84];
    deliver_dhcp_to_us(&bootp_reply(renew_xid, OUR_MAC, OUR_IP, &server_options(ACK, &shorter)));
    client.run(t1 + 1);
    report.check(
        "the renewal's answer needs no answer and leaves the configuration as it was",
        nic.take().is_empty() && super::config() == Some(leased_config()),
    );
    report.check(
        "and the lease runs from the renewal now, by the server's own T1 and T2",
        client.lease().is_some_and(|lease| {
            lease.renew_at == t1 + 600 * HZ
                && lease.rebind_at == t1 + 900 * HZ
                && lease.expires_at == t1 + 1200 * HZ
        }),
    );

    // ---- T2: rebinding with anyone ----
    client.run(t1 + 600 * HZ);
    report.check(
        "at the next T1 the renewal goes straight out, the server's hardware address being known",
        nic.take().len() == 1,
    );
    client.run(t1 + 900 * HZ);
    let sent = nic.take();
    report.check("at T2, still unanswered, one request to everyone", sent.len() == 1);
    let [rebinding] = sent.as_slice() else { return };
    let rebind_xid = xid_of(rebinding);
    let expected = expected_broadcast(
        ether::ETHERTYPE_IPV4,
        &datagram(
            identification_of(rebinding),
            ip::PROTO_UDP,
            OUR_IP,
            Ipv4Addr::BROADCAST,
            &udp_datagram(
                68,
                67,
                &bootp_request(rebind_xid, 0, 0, OUR_IP, &client_options(REQUEST, None, None)),
                OUR_IP,
                Ipv4Addr::BROADCAST,
            ),
        ),
    );
    report.bytes("the rebinding request: from the leased address, to every server", rebinding, &expected);

    // ---- the end of the lease ----
    let end = t1 + 1200 * HZ;
    client.run(end - 1);
    report.check("the address is held to the last tick of the lease", super::config().is_some());
    nic.take();
    client.run(end);
    let sent = nic.take();
    report.check(
        "and given up when it runs out",
        super::config().is_none() && client.lease().is_none(),
    );
    report.check(
        "after which a discover goes out at once, from 0.0.0.0",
        matches!(sent.as_slice(), [frame] if kind_of(frame) == DISCOVER && frame[26..30] == [0, 0, 0, 0]),
    );
    drop(client);
    socket::reset();
    nic.take();
}

/// A NAK. In answer to the first request it takes nothing and the client asks
/// again after a wait; in answer to a renewal it takes the address away.
fn dhcp_refusal(report: &mut Report, nic: &FakeNic) {
    let refusal = [53, 1, NAK, 54, 4, 10, 0, 2, 2, 255];
    nic.take();
    let Some(mut client) = new_client(report, None) else { return };
    client.run(START);
    let Some(xid) = nic.take().first().map(|frame| xid_of(frame)) else {
        report.check("a discover", false);
        return;
    };
    deliver_dhcp_broadcast(&bootp_reply(xid, OUR_MAC, OUR_IP, &server_options(OFFER, &A_DAY)));
    client.run(START + 1);
    nic.take();
    deliver_dhcp_broadcast(&bootp_reply(xid, OUR_MAC, Ipv4Addr::UNSPECIFIED, &refusal));
    client.run(START + 2);
    report.check(
        "a refused request takes no address",
        super::config().is_none() && client.lease().is_none(),
    );
    report.check("and sends nothing straight away", nic.take().is_empty());
    let wait = client.next_deadline() - (START + 2);
    report.check("the next discover waits four seconds, give or take one", (3 * HZ..=5 * HZ).contains(&wait));
    client.run(client.next_deadline());
    report.check(
        "and then goes, in a new exchange",
        matches!(nic.take().as_slice(), [frame] if kind_of(frame) == DISCOVER && xid_of(frame) != xid),
    );
    drop(client);

    // ---- a renewal refused ----
    nic.take();
    let Some(mut client) = new_client(report, None) else { return };
    let leased = lease_up(&mut client, nic).is_some();
    report.check("a lease to renew", leased && super::config() == Some(leased_config()));
    let Some(t1) = client.lease().map(|lease| lease.renew_at) else { return };
    client.run(t1);
    deliver(ether::ETHERTYPE_ARP, &arp_from_peer(super::arp::OP_REPLY, OUR_MAC, OUR_IP));
    let renew_xid = nic.take().last().map(|frame| xid_of(frame)).unwrap_or(0);
    deliver_dhcp_to_us(&bootp_reply(renew_xid, OUR_MAC, Ipv4Addr::UNSPECIFIED, &refusal));
    client.run(t1 + 1);
    report.check(
        "a refused renewal takes the address away",
        super::config().is_none() && client.lease().is_none(),
    );
    drop(client);
    socket::reset();
    nic.take();
}

/// Messages wrong in the ways a length can be wrong. None of them is taken,
/// none stops a good one after them from being taken, and no cut and no
/// length byte panics the parser, which with overflow checks on would take
/// the kernel down with it.
fn dhcp_malformed(report: &mut Report, nic: &FakeNic) {
    let named = Ipv4Addr::new(1, 1, 1, 1);
    nic.take();
    let Some(mut client) = new_client(report, Some(named)) else { return };
    client.run(START);
    let Some(xid) = nic.take().first().map(|frame| xid_of(frame)) else {
        report.check("a discover", false);
        return;
    };

    let server: [u8; 6] = [54, 4, 10, 0, 2, 2];
    let offering: [u8; 3] = [53, 1, OFFER];
    let reply = |options: &[&[u8]]| bootp_reply(xid, OUR_MAC, OUR_IP, &options.concat());
    let mut file_overrun = reply(&[&offering, &server, &[52, 1, 1, 255]]);
    file_overrun[108] = 6;
    file_overrun[109] = 250;
    let mut cut_short = reply(&[&offering, &server, &A_DAY, &[255]]);
    cut_short.truncate(238);
    let cases: [(&str, Vec<u8>); 8] = [
        (
            "an option running past the end of the message is not an offer",
            reply(&[&offering, &server, &[1, 200, 255, 255, 255, 0]]),
        ),
        (
            "nor is a netmask three bytes long",
            reply(&[&offering, &server, &[1, 3, 255, 255, 255], &A_DAY, &[255]]),
        ),
        ("nor a message type two bytes long", reply(&[&[53, 2, OFFER, OFFER], &server, &[255]])),
        (
            "nor a router list with a byte left over",
            reply(&[&offering, &server, &[3, 5, 10, 0, 2, 2, 0], &[255]]),
        ),
        (
            "nor an overload option naming fields that do not exist",
            reply(&[&offering, &server, &[52, 1, 4], &[255]]),
        ),
        ("nor an option running past the end of the file field", file_overrun),
        ("nor an offer naming no server", reply(&[&offering, &A_DAY, &[255]])),
        ("nor a message cut off inside the magic cookie", cut_short),
    ];
    for (name, message) in cases {
        deliver_dhcp_broadcast(&message);
        client.run(START + 1);
        report.check(name, nic.take().is_empty());
    }

    // The parser alone, against every way a good message can be cut short and
    // every value each of its length bytes could hold.
    let good = bootp_reply(xid, OUR_MAC, OUR_IP, &server_options(ACK, &A_DAY));
    let parses = super::dhcp::Reply::parse(&good).is_some();
    for cut in 0..good.len() {
        let _ = super::dhcp::Reply::parse(&good[..cut]);
    }
    let mut at = 240;
    while at + 1 < good.len() && good[at] != 255 {
        for length in 0..=255u8 {
            let mut changed = good.clone();
            changed[at + 1] = length;
            let _ = super::dhcp::Reply::parse(&changed);
        }
        at += 2 + good[at + 1] as usize;
    }
    report.check("a good message parses, and no cut and no length byte panics the parser", parses);

    // Option 52 moving options into the file field, and the name server
    // option split in two, one half in each place (RFC 3396).
    let mut carried = reply(&[&[53, 1, ACK], &server, &[52, 1, 1], &[6, 4, 10, 0, 2, 53], &[255]]);
    carried[108..120].copy_from_slice(&[1, 4, 255, 255, 255, 0, 6, 4, 1, 1, 1, 1]);
    carried[120] = 255;
    report.check(
        "options in the file field are read, and an option split in two is joined",
        super::dhcp::Reply::parse(&carried).is_some_and(|parsed| {
            parsed.netmask == Some(NETMASK) && parsed.nameservers == [LEASE_DNS, named]
        }),
    );

    deliver_dhcp_broadcast(&bootp_reply(xid, OUR_MAC, OUR_IP, &server_options(OFFER, &A_DAY)));
    client.run(START + 2);
    report.check(
        "the good offer after all of them is still taken",
        matches!(nic.take().as_slice(), [frame] if kind_of(frame) == REQUEST),
    );
    deliver_dhcp_broadcast(&bootp_reply(xid, OUR_MAC, OUR_IP, &server_options(ACK, &A_DAY)));
    client.run(START + 3);
    report.check(
        "and nameserver= takes the place of the name server the lease names",
        super::config().is_some_and(|config| config.nameservers() == [named]),
    );
    drop(client);
    socket::reset();
    nic.take();
}

/// The DISCOVER goes again after 4, 8, 16, 32 and then every 64 seconds, each
/// moved by up to a second, all in one exchange; and a link that comes up
/// starts the asking again at once.
fn dhcp_retransmission(report: &mut Report, nic: &FakeNic) {
    nic.take();
    let Some(mut client) = new_client(report, None) else { return };
    client.run(START);
    let Some(xid) = nic.take().first().map(|frame| xid_of(frame)) else {
        report.check("a discover", false);
        return;
    };
    let mut now = START;
    let mut on_time = true;
    let mut one_exchange = true;
    let mut secs_counted = true;
    for seconds in [4u64, 8, 16, 32, 64, 64] {
        let deadline = client.next_deadline();
        on_time &= ((seconds - 1) * HZ..=(seconds + 1) * HZ).contains(&(deadline - now));
        client.run(deadline);
        match nic.take().as_slice() {
            [frame] => {
                one_exchange &= xid_of(frame) == xid && kind_of(frame) == DISCOVER;
                let secs = u16::from_be_bytes([frame[DHCP_AT + 8], frame[DHCP_AT + 9]]) as u64;
                secs_counted &= secs == (deadline - START) / HZ;
            }
            _ => one_exchange = false,
        }
        now = deadline;
    }
    report.check(
        "a discover goes again after 4, 8, 16, 32, 64 and 64 seconds, each give or take one",
        on_time,
    );
    report.check("all of them in the one exchange", one_exchange);
    report.check("with secs counting from the first", secs_counted);

    nic.link.store(false, core::sync::atomic::Ordering::Relaxed);
    client.run(now + 1);
    nic.link.store(true, core::sync::atomic::Ordering::Relaxed);
    client.run(now + 2);
    report.check(
        "a link that comes up starts the asking again at once, in a new exchange",
        matches!(nic.take().as_slice(), [frame] if kind_of(frame) == DISCOVER && xid_of(frame) != xid),
    );
    drop(client);
    socket::reset();
    nic.take();
}
