//! End-to-end check of the card, behind the `nettest` boot word.
//!
//! It builds one ARP request by hand, broadcasts it, and waits for the reply.
//! Nothing in here uses the protocol stack: the twenty-eight byte payload is
//! written out byte by byte and the reply is picked apart the same way, so
//! what passes or fails is the driver and only the driver. It runs as its own
//! kernel task because `transmit` is defined to be called from task context,
//! and because the reply only arrives once the network task is running.
//!
//! QEMU's user-mode network answers ARP for the gateway itself, so this needs
//! no host configuration:
//!
//!     guest    10.0.2.15
//!     gateway  10.0.2.2
//!
//! Run it with
//!
//!     ./scripts/run.sh --net --initrd build/distro/test-x86_64/initramfs.cpio --append nettest
//!
//! The first request usually goes unanswered, and that is QEMU rather than
//! the card. Writing the receive control register arms a one-second timer in
//! QEMU's model, and for as long as it is pending the model reports the card
//! unable to receive and holds incoming frames in a queue instead of putting
//! them in the ring. Bring-up writes that register, so the first second after
//! boot swallows everything; the reply to the first request is delivered, in
//! full, when the timer fires. The test therefore retries, and then does a
//! second exchange to show what the round trip costs once the card is past
//! that window.

use crate::sched;
use crate::sync::Spinlock;
use crate::task::Task;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

const GUEST_IP: [u8; 4] = [10, 0, 2, 15];
const GATEWAY_IP: [u8; 4] = [10, 0, 2, 2];
const BROADCAST: [u8; 6] = [0xFF; 6];

const ETHERTYPE_ARP: u16 = 0x0806;
const ARP_HTYPE_ETHERNET: u16 = 1;
const ARP_PTYPE_IPV4: u16 = 0x0800;
const ARP_REQUEST: u16 = 1;
const ARP_REPLY: u16 = 2;

/// How long to wait for one reply, in timer ticks at 100 Hz.
const REPLY_TIMEOUT_TICKS: u64 = 50;
/// Requests to send before giving up. Enough to outlast QEMU's one-second
/// window with room to spare.
const ATTEMPTS: u32 = 5;

static ARMED: AtomicBool = AtomicBool::new(false);
/// The hardware address the gateway answered with, and the tick it was seen.
static ANSWER: Spinlock<Option<([u8; 6], u64)>> = Spinlock::new(None);

pub fn armed() -> bool {
    ARMED.load(Ordering::Relaxed)
}

/// One Ethernet frame carrying an ARP request for `target`.
///
///     0..6    destination, all ones: nobody knows who to ask yet
///     6..12   source, this card
///     12..14  0x0806, ARP
///     14..16  hardware type 1, Ethernet
///     16..18  protocol type 0x0800, IPv4
///     18      hardware address length, 6
///     19      protocol address length, 4
///     20..22  operation 1, request
///     22..28  sender hardware address
///     28..32  sender protocol address
///     32..38  target hardware address, zero: that is the question
///     38..42  target protocol address
fn build_request(mac: &[u8; 6], sender: [u8; 4], target: [u8; 4]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(42);
    frame.extend_from_slice(&BROADCAST);
    frame.extend_from_slice(mac);
    frame.extend_from_slice(&ETHERTYPE_ARP.to_be_bytes());
    frame.extend_from_slice(&ARP_HTYPE_ETHERNET.to_be_bytes());
    frame.extend_from_slice(&ARP_PTYPE_IPV4.to_be_bytes());
    frame.push(6);
    frame.push(4);
    frame.extend_from_slice(&ARP_REQUEST.to_be_bytes());
    frame.extend_from_slice(mac);
    frame.extend_from_slice(&sender);
    frame.extend_from_slice(&[0u8; 6]);
    frame.extend_from_slice(&target);
    frame
}

/// Look at one received frame for the reply this test is waiting for.
///
/// Called by the network task on every frame, before the frame is handed to
/// the stack. It does nothing at all unless the test was armed at boot.
pub fn observe(frame: &[u8]) {
    if !armed() || frame.len() < 42 {
        return;
    }
    if u16::from_be_bytes([frame[12], frame[13]]) != ETHERTYPE_ARP {
        return;
    }
    let arp = &frame[14..42];
    if u16::from_be_bytes([arp[0], arp[1]]) != ARP_HTYPE_ETHERNET
        || u16::from_be_bytes([arp[2], arp[3]]) != ARP_PTYPE_IPV4
        || u16::from_be_bytes([arp[6], arp[7]]) != ARP_REPLY
    {
        return;
    }
    // A reply's sender protocol address is the address that was asked about,
    // so this says the gateway answered and not somebody else.
    if arp[14..18] != GATEWAY_IP {
        return;
    }
    let mut mac = [0u8; 6];
    mac.copy_from_slice(&arp[8..14]);
    *ANSWER.lock() = Some((mac, crate::trap::ticks()));
}

fn format_mac(mac: &[u8; 6]) -> alloc::string::String {
    use core::fmt::Write;
    let mut out = alloc::string::String::new();
    for (i, byte) in mac.iter().enumerate() {
        let _ = write!(out, "{}{:02x}", if i == 0 { "" } else { ":" }, byte);
    }
    out
}

fn format_ip(ip: &[u8; 4]) -> alloc::string::String {
    use core::fmt::Write;
    let mut out = alloc::string::String::new();
    let _ = write!(out, "{}.{}.{}.{}", ip[0], ip[1], ip[2], ip[3]);
    out
}

/// Arm the test and start the task that runs it.
pub fn start() {
    ARMED.store(true, Ordering::Relaxed);
    super::trace_received(true);

    let Some(mut task) = Task::new("arptest", None) else {
        crate::println!("[nettest] cannot create the test task");
        return;
    };
    task.prepare_kernel_frame(run as extern "C" fn() -> ! as usize as u64);
    sched::register(task);
}

/// Send one request and wait up to `REPLY_TIMEOUT_TICKS` for the reply.
fn exchange(request: &[u8]) -> Option<([u8; 6], u64)> {
    let sent = crate::trap::ticks();
    crate::println!("[nettest] t={} request, {} bytes", sent, request.len());
    if let Err(err) = crate::net::transmit(request) {
        crate::println!("[nettest] transmit failed: {:?}", err);
        return None;
    }
    let deadline = sent + REPLY_TIMEOUT_TICKS;
    while crate::trap::ticks() < deadline {
        if let Some(answer) = *ANSWER.lock() {
            return Some(answer);
        }
        sched::sleep_ticks(1);
    }
    None
}

extern "C" fn run() -> ! {
    let Some(nic) = crate::net::interface() else {
        crate::println!("[nettest] no interface attached");
        crate::arch::power_off();
    };
    let mac = nic.mac();
    let request = build_request(&mac, GUEST_IP, GATEWAY_IP);

    crate::println!(
        "[nettest] {} is {}, asking who has {}",
        format_mac(&mac),
        format_ip(&GUEST_IP),
        format_ip(&GATEWAY_IP)
    );

    let mut first = None;
    for _ in 0..ATTEMPTS {
        first = exchange(&request);
        if first.is_some() {
            break;
        }
    }

    let Some((gateway, _)) = first else {
        crate::println!("[nettest] no arp reply from {}", format_ip(&GATEWAY_IP));
        crate::println!("=== 0 passed, 1 failed ===");
        report_counters();
        crate::arch::power_off();
    };
    crate::println!("[nettest] {} is at {}", format_ip(&GATEWAY_IP), format_mac(&gateway));

    // Now that the card is past QEMU's window, ask again and time it. This is
    // what a receive actually costs: the card's interrupt, the copy off the
    // ring, and one wake-up of the network task.
    *ANSWER.lock() = None;
    let sent = crate::trap::ticks();
    match exchange(&request) {
        Some((again, at)) if again == gateway => {
            crate::println!("[nettest] second reply after {} ticks", at - sent);
            crate::println!("=== 2 passed, 0 failed ===");
        }
        Some((again, _)) => {
            crate::println!("[nettest] second reply came from {}", format_mac(&again));
            crate::println!("=== 1 passed, 1 failed ===");
        }
        None => {
            crate::println!("[nettest] no second reply");
            crate::println!("=== 1 passed, 1 failed ===");
        }
    }
    report_counters();
    crate::arch::power_off();
}

fn report_counters() {
    let (interrupts, overruns, dropped) = super::counters();
    crate::println!(
        "[nettest] {} card interrupts, {} ring overruns, {} frames dropped by the queue",
        interrupts,
        overruns,
        dropped
    );
}
