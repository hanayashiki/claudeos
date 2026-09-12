//! Intel 8254x gigabit Ethernet, the card QEMU calls `e1000`.
//!
//! The card is a pair of rings in memory the kernel allocates and the card
//! reads and writes by itself. Each ring is an array of sixteen-byte
//! descriptors; each descriptor names a buffer by *physical* address, because
//! the card's DMA engine does not go through the CPU's page tables and knows
//! nothing about the direct map. Two register pairs per ring say who owns
//! what: the head is the card's cursor and the tail is the kernel's, and the
//! card works on descriptors from the head up to but not including the tail.
//!
//! Receiving: the kernel hands the card empty buffers by moving the receive
//! tail forward. The card fills a buffer, writes the length and a
//! descriptor-done bit into the descriptor, and raises an interrupt. The
//! kernel copies the bytes out, clears the status byte and moves the tail
//! past the descriptor to lend it back.
//!
//! Transmitting: the kernel copies the frame into a buffer, points a
//! descriptor at it with the end-of-packet and report-status bits set, and
//! moves the transmit tail forward. The card sends the frame and sets the
//! descriptor's done bit.
//!
//! The interrupt handler does not parse anything. It reads the cause register
//! (which is what lowers the card's interrupt line), moves every completed
//! frame into a queue of preallocated buffers, and wakes the network task.
//! The network task is an ordinary scheduled task, so `net::receive` runs
//! with interrupts on and may allocate, take locks, and transmit replies.

use crate::abi::Errno;
use crate::cpu::idt::TrapFrame;
use crate::cpu::pic;
use crate::mm::frame::alloc_contiguous;
use crate::mm::paging::AddressSpace;
use crate::mm::{phys_to_virt, PAGE_SIZE};
use crate::net::Interface;
use crate::pci;
use crate::sched::{self, WaitQueue};
use crate::sync::Spinlock;
use crate::task::Task;
use alloc::boxed::Box;
use alloc::vec::Vec;
use core::sync::atomic::{fence, AtomicBool, AtomicU64, Ordering};

/// What QEMU's `e1000` device presents: Intel 82540EM.
const VENDOR_INTEL: u16 = 0x8086;
const DEVICE_82540EM: u16 = 0x100E;

// ---------------------------------------------------------------------------
// Register block. Offsets are byte offsets from the start of BAR 0, and every
// register is 32 bits wide.
// ---------------------------------------------------------------------------

const CTRL: u32 = 0x0000; // device control
const STATUS: u32 = 0x0008; // device status
const EERD: u32 = 0x0014; // EEPROM read
const ICR: u32 = 0x00C0; // interrupt cause, read to acknowledge
const ITR: u32 = 0x00C4; // interrupt throttling interval
const IMS: u32 = 0x00D0; // interrupt mask set
const IMC: u32 = 0x00D8; // interrupt mask clear
const RCTL: u32 = 0x0100; // receive control
const TCTL: u32 = 0x0400; // transmit control
const TIPG: u32 = 0x0410; // transmit inter-packet gap
const RDBAL: u32 = 0x2800; // receive ring base, low 32 bits
const RDBAH: u32 = 0x2804; // receive ring base, high 32 bits
const RDLEN: u32 = 0x2808; // receive ring length in bytes
const RDH: u32 = 0x2810; // receive head, the card's cursor
const RDT: u32 = 0x2818; // receive tail, the kernel's cursor
const RDTR: u32 = 0x2820; // receive interrupt delay
const RADV: u32 = 0x282C; // receive absolute interrupt delay
const TDBAL: u32 = 0x3800;
const TDBAH: u32 = 0x3804;
const TDLEN: u32 = 0x3808;
const TDH: u32 = 0x3810;
const TDT: u32 = 0x3818;
const TIDV: u32 = 0x3820; // transmit interrupt delay
const TADV: u32 = 0x382C; // transmit absolute interrupt delay
const MTA: u32 = 0x5200; // multicast table array, 128 words
const RAL0: u32 = 0x5400; // receive address 0, low four bytes
const RAH0: u32 = 0x5404; // receive address 0, high two bytes plus flags

// CTRL bits.
const CTRL_FD: u32 = 1 << 0; // full duplex
const CTRL_LRST: u32 = 1 << 3; // link reset; must stay clear on copper
const CTRL_ASDE: u32 = 1 << 5; // auto speed detection
const CTRL_SLU: u32 = 1 << 6; // set link up
const CTRL_ILOS: u32 = 1 << 7; // invert loss of signal; must stay clear
const CTRL_RST: u32 = 1 << 26; // device reset, self-clearing
const CTRL_VME: u32 = 1 << 30; // VLAN tag stripping
const CTRL_PHY_RST: u32 = 1 << 31;

// STATUS bits.
const STATUS_FD: u32 = 1 << 0;
const STATUS_LU: u32 = 1 << 1; // link up
const STATUS_SPEED_SHIFT: u32 = 6;

// EERD: start in bit 0, done in bit 4, word address in bits 8..15, the word
// read back in bits 16..31.
const EERD_START: u32 = 1 << 0;
const EERD_DONE: u32 = 1 << 4;
const EERD_ADDR_SHIFT: u32 = 8;
const EERD_DATA_SHIFT: u32 = 16;

// Interrupt cause and mask bits. Only the receive ones matter here: transmit
// completion is polled, because `transmit` may not sleep anyway.
const INT_TXDW: u32 = 1 << 0; // a transmit descriptor was written back
const INT_LSC: u32 = 1 << 2; // link status changed
const INT_RXDMT0: u32 = 1 << 4; // receive ring is running low on descriptors
const INT_RXO: u32 = 1 << 6; // receive overrun: a frame was dropped
const INT_RXT0: u32 = 1 << 7; // a frame arrived and the receive timer expired

// RCTL bits.
const RCTL_EN: u32 = 1 << 1;
const RCTL_UPE: u32 = 1 << 3; // unicast promiscuous
const RCTL_MPE: u32 = 1 << 4; // multicast promiscuous
const RCTL_LBM_MASK: u32 = 3 << 6; // loopback mode; zero is normal operation
const RCTL_BAM: u32 = 1 << 15; // accept broadcast, which ARP needs
const RCTL_BSIZE_2048: u32 = 0 << 16; // buffer size, with BSEX clear
const RCTL_SECRC: u32 = 1 << 26; // strip the four-byte Ethernet CRC

// TCTL bits.
const TCTL_EN: u32 = 1 << 1;
const TCTL_PSP: u32 = 1 << 3; // pad frames out to the 60-byte minimum
const TCTL_CT_SHIFT: u32 = 4; // collision threshold
const TCTL_COLD_SHIFT: u32 = 12; // collision distance

/// Receive address high: the entry holds a valid address.
const RAH_AV: u32 = 1 << 31;

// ---------------------------------------------------------------------------
// Rings
// ---------------------------------------------------------------------------

/// Descriptors per ring. The ring length register counts bytes and must be a
/// multiple of 128, so this has to be a multiple of eight.
const RX_DESCS: usize = 32;
const TX_DESCS: usize = 32;
/// Bytes per packet buffer, which must match the size RCTL announces.
const BUFFER_SIZE: usize = 2048;

/// Legacy receive descriptor, exactly as the card lays it out.
#[repr(C)]
#[derive(Clone, Copy)]
struct RxDesc {
    /// Physical address of the buffer the card fills.
    addr: u64,
    /// Bytes written, set by the card.
    length: u16,
    checksum: u16,
    status: u8,
    errors: u8,
    special: u16,
}

const RX_STATUS_DD: u8 = 1 << 0; // descriptor done
const RX_STATUS_EOP: u8 = 1 << 1; // end of packet

/// Legacy transmit descriptor.
#[repr(C)]
#[derive(Clone, Copy)]
struct TxDesc {
    addr: u64,
    length: u16,
    /// Checksum offset; unused, the kernel builds whole frames.
    cso: u8,
    cmd: u8,
    status: u8,
    css: u8,
    special: u16,
}

const TX_CMD_EOP: u8 = 1 << 0; // this descriptor ends the frame
const TX_CMD_IFCS: u8 = 1 << 1; // have the card append the Ethernet CRC
const TX_CMD_RS: u8 = 1 << 3; // report status, i.e. set DD when sent
const TX_STATUS_DD: u8 = 1 << 0;

/// How long `transmit` waits for the card before giving up. QEMU completes a
/// transmit inside the register write that moves the tail, so reaching this
/// means the card has stopped answering.
const TX_SPIN_LIMIT: u32 = 10_000_000;

struct RxRing {
    /// Next descriptor the kernel expects the card to have filled.
    next: usize,
}

struct TxRing {
    /// Next descriptor the kernel will use, which is also the tail.
    next: usize,
}

// ---------------------------------------------------------------------------
// The queue between the interrupt and the network task
// ---------------------------------------------------------------------------

const QUEUE_SLOTS: usize = 64;

/// Frames taken off the receive ring, waiting for the network task.
///
/// The slots are allocated once at bring-up and reused, so the interrupt
/// handler never calls the heap allocator: it copies into a slot whose
/// capacity is already large enough, and the task swaps a spare buffer in to
/// take one out. Overrunning the queue drops frames, which is what a network
/// card does anyway when nothing keeps up.
struct RxQueue {
    slots: Vec<Vec<u8>>,
    head: usize,
    count: usize,
    dropped: u64,
}

impl RxQueue {
    const fn new() -> RxQueue {
        RxQueue { slots: Vec::new(), head: 0, count: 0, dropped: 0 }
    }

    fn push(&mut self, frame: &[u8]) {
        if self.slots.is_empty() || self.count == self.slots.len() {
            self.dropped += 1;
            return;
        }
        let index = (self.head + self.count) % self.slots.len();
        let slot = &mut self.slots[index];
        slot.clear();
        slot.extend_from_slice(frame);
        self.count += 1;
    }

    /// Take the oldest frame, leaving `spare` in its place. `spare` must have
    /// room for a whole buffer, which it does because every buffer that ever
    /// goes into a slot came out of one.
    fn pop(&mut self, spare: &mut Vec<u8>) -> bool {
        if self.count == 0 {
            return false;
        }
        core::mem::swap(&mut self.slots[self.head], spare);
        self.head = (self.head + 1) % self.slots.len();
        self.count -= 1;
        true
    }
}

static QUEUE: Spinlock<RxQueue> = Spinlock::new(RxQueue::new());
static RX_WAIT: WaitQueue = WaitQueue::new();
/// Set by the boot-time test so received frames are logged. Off otherwise, so
/// an ordinary boot with a card says nothing per frame.
static TRACE_RX: AtomicBool = AtomicBool::new(false);

pub fn trace_received(on: bool) {
    TRACE_RX.store(on, Ordering::Relaxed);
}

/// Interrupts the card raised that were its own.
static IRQ_COUNT: AtomicU64 = AtomicU64::new(0);
/// Times the card reported it had run out of receive descriptors.
static OVERRUNS: AtomicU64 = AtomicU64::new(0);

pub fn interrupts() -> u64 {
    IRQ_COUNT.load(Ordering::Relaxed)
}

pub fn overruns() -> u64 {
    OVERRUNS.load(Ordering::Relaxed)
}

/// Frames the queue had no room for.
pub fn dropped() -> u64 {
    QUEUE.lock().dropped
}

// ---------------------------------------------------------------------------
// The card
// ---------------------------------------------------------------------------

pub struct E1000 {
    /// Kernel address the register block answers at.
    regs: u64,
    mac: [u8; 6],
    irq: u8,
    /// Physical addresses of the DMA regions. They are allocated once and
    /// never released, so they are held as plain addresses.
    rx_ring: u64,
    tx_ring: u64,
    rx_buffers: u64,
    tx_buffers: u64,
    rx: Spinlock<RxRing>,
    tx: Spinlock<TxRing>,
}

static DEVICE: Spinlock<Option<&'static E1000>> = Spinlock::new(None);

pub fn device() -> Option<&'static E1000> {
    *DEVICE.lock()
}

impl E1000 {
    #[inline]
    fn read(&self, offset: u32) -> u32 {
        unsafe { core::ptr::read_volatile((self.regs + offset as u64) as *const u32) }
    }

    #[inline]
    fn write(&self, offset: u32, value: u32) {
        unsafe { core::ptr::write_volatile((self.regs + offset as u64) as *mut u32, value) }
    }

    #[inline]
    fn rx_desc(&self, index: usize) -> *mut RxDesc {
        (phys_to_virt(self.rx_ring) as *mut RxDesc).wrapping_add(index)
    }

    #[inline]
    fn tx_desc(&self, index: usize) -> *mut TxDesc {
        (phys_to_virt(self.tx_ring) as *mut TxDesc).wrapping_add(index)
    }

    /// An empty receive descriptor pointing at descriptor `index`'s buffer.
    fn fresh_rx_desc(&self, index: usize) -> RxDesc {
        RxDesc {
            addr: self.rx_buffers + (index * BUFFER_SIZE) as u64,
            length: 0,
            checksum: 0,
            status: 0,
            errors: 0,
            special: 0,
        }
    }

    /// Read one 16-bit word out of the card's serial EEPROM, where the
    /// factory-programmed hardware address lives in words 0 to 2.
    fn eeprom_word(&self, word: u16) -> Option<u16> {
        self.write(EERD, ((word as u32) << EERD_ADDR_SHIFT) | EERD_START);
        for _ in 0..100_000 {
            let value = self.read(EERD);
            if value & EERD_DONE != 0 {
                return Some((value >> EERD_DATA_SHIFT) as u16);
            }
            core::hint::spin_loop();
        }
        None
    }

    /// The hardware address: out of the EEPROM if there is one, otherwise out
    /// of receive address register zero, which reset loads for us on parts
    /// that have no EEPROM attached.
    fn read_mac(&self) -> [u8; 6] {
        let mut mac = [0u8; 6];
        let words = (self.eeprom_word(0), self.eeprom_word(1), self.eeprom_word(2));
        if let (Some(w0), Some(w1), Some(w2)) = words {
            if w0 != 0 || w1 != 0 || w2 != 0 {
                // Each word holds two bytes of the address, low byte first.
                mac[0..2].copy_from_slice(&w0.to_le_bytes());
                mac[2..4].copy_from_slice(&w1.to_le_bytes());
                mac[4..6].copy_from_slice(&w2.to_le_bytes());
                return mac;
            }
        }
        let low = self.read(RAL0);
        let high = self.read(RAH0);
        mac[0..4].copy_from_slice(&low.to_le_bytes());
        mac[4..6].copy_from_slice(&(high as u16).to_le_bytes());
        mac
    }

    /// Put the card back to a known state. The reset bit clears itself when
    /// the card is done, which takes microseconds; everything is masked off
    /// first so nothing arrives in the middle of it.
    fn reset(&self) {
        self.write(IMC, 0xFFFF_FFFF);
        self.write(RCTL, 0);
        self.write(TCTL, 0);
        self.read(STATUS); // push the writes out before the reset

        self.write(CTRL, self.read(CTRL) | CTRL_RST);
        for _ in 0..1_000_000 {
            if self.read(CTRL) & CTRL_RST == 0 {
                break;
            }
            core::hint::spin_loop();
        }
        // Reset re-enables the mask, and leaves a cause latched.
        self.write(IMC, 0xFFFF_FFFF);
        self.read(ICR);
    }

    /// Point the card at the receive ring and lend it every descriptor but
    /// one. The tail is the kernel's cursor and the card stops when the head
    /// reaches it, so a tail of `RX_DESCS - 1` keeps the ring from looking
    /// empty by looking full.
    fn setup_rx(&self) {
        for index in 0..RX_DESCS {
            unsafe { core::ptr::write_volatile(self.rx_desc(index), self.fresh_rx_desc(index)) };
        }
        fence(Ordering::SeqCst);

        self.write(RDBAL, self.rx_ring as u32);
        self.write(RDBAH, (self.rx_ring >> 32) as u32);
        self.write(RDLEN, (RX_DESCS * core::mem::size_of::<RxDesc>()) as u32);
        self.write(RDH, 0);
        self.write(RDT, (RX_DESCS - 1) as u32);

        // No delay between a frame arriving and the interrupt. The handler is
        // cheap enough that coalescing would only add latency.
        self.write(RDTR, 0);
        self.write(RADV, 0);
        self.write(ITR, 0);
    }

    /// Start receiving.
    ///
    /// Separate from `setup_rx` so it happens after the interrupt handler is
    /// in place: a frame that lands in the ring before then raises nothing,
    /// and sits there until some later frame's interrupt sweeps it up.
    ///
    /// Take frames addressed to us and broadcasts, and have the card strip
    /// the Ethernet CRC so a descriptor's length is the frame's length.
    /// Promiscuous modes stay off; the receive address filter and broadcast
    /// are all the stack needs.
    fn enable_rx(&self) {
        let rctl = RCTL_EN | RCTL_BAM | RCTL_SECRC | RCTL_BSIZE_2048;
        let rctl = rctl & !(RCTL_UPE | RCTL_MPE | RCTL_LBM_MASK);
        self.write(RCTL, rctl);
    }

    fn setup_tx(&self) {
        for index in 0..TX_DESCS {
            let desc = TxDesc {
                addr: self.tx_buffers + (index * BUFFER_SIZE) as u64,
                length: 0,
                cso: 0,
                cmd: 0,
                // Done from the start: nothing is in flight yet, and
                // `transmit` reads this before reusing a descriptor.
                status: TX_STATUS_DD,
                css: 0,
                special: 0,
            };
            unsafe { core::ptr::write_volatile(self.tx_desc(index), desc) };
        }
        fence(Ordering::SeqCst);

        self.write(TDBAL, self.tx_ring as u32);
        self.write(TDBAH, (self.tx_ring >> 32) as u32);
        self.write(TDLEN, (TX_DESCS * core::mem::size_of::<TxDesc>()) as u32);
        self.write(TDH, 0);
        self.write(TDT, 0);
        self.write(TIDV, 0);
        self.write(TADV, 0);

        // Collision threshold 16 and collision distance 64 byte times are
        // what the manual gives for full duplex; on a switched link neither
        // is ever reached. PSP pads short frames out to 60 bytes, which a
        // 42-byte ARP request needs.
        self.write(
            TCTL,
            TCTL_EN | TCTL_PSP | (0x10 << TCTL_CT_SHIFT) | (0x40 << TCTL_COLD_SHIFT),
        );
        // Inter-packet gap: 10 byte times back to back, then 8 and 6 for the
        // two receive-side parts. This is the value the manual gives for
        // copper 1000BASE-T.
        self.write(TIPG, 10 | (8 << 10) | (6 << 20));
    }

    /// Write the hardware address into receive address register zero, so the
    /// card's filter accepts frames addressed to it.
    fn set_receive_address(&self, mac: &[u8; 6]) {
        let low = u32::from_le_bytes([mac[0], mac[1], mac[2], mac[3]]);
        let high = u16::from_le_bytes([mac[4], mac[5]]) as u32;
        self.write(RAL0, low);
        self.write(RAH0, high | RAH_AV);
        // Everything else in the filter is off: no multicast groups joined.
        for entry in 0..128u32 {
            self.write(MTA + entry * 4, 0);
        }
    }

    pub fn link_up(&self) -> bool {
        self.read(STATUS) & STATUS_LU != 0
    }

    /// Link speed in megabits, from the two speed bits in STATUS.
    pub fn link_speed(&self) -> u32 {
        match (self.read(STATUS) >> STATUS_SPEED_SHIFT) & 3 {
            0 => 10,
            1 => 100,
            _ => 1000,
        }
    }

    pub fn full_duplex(&self) -> bool {
        self.read(STATUS) & STATUS_FD != 0
    }

    /// Move every completed frame off the receive ring into the queue.
    ///
    /// Called from the interrupt handler, so it copies and does nothing else:
    /// no parsing, no allocation, no call into the protocol code.
    fn drain_rx(&self) -> usize {
        let mut rx = self.rx.lock();
        let mut taken = 0;
        loop {
            let desc = self.rx_desc(rx.next);
            // The done bit is the last thing the card writes, so read it
            // first: everything else in the descriptor is only meaningful
            // once it is set.
            let status = unsafe { core::ptr::read_volatile(core::ptr::addr_of!((*desc).status)) };
            if status & RX_STATUS_DD == 0 {
                break;
            }
            fence(Ordering::SeqCst);
            let (errors, length) = unsafe {
                (
                    core::ptr::read_volatile(core::ptr::addr_of!((*desc).errors)),
                    core::ptr::read_volatile(core::ptr::addr_of!((*desc).length)) as usize,
                )
            };
            // A frame split across descriptors would need reassembly, but the
            // buffers are larger than the maximum frame, so one descriptor is
            // always one frame.
            let whole = status & RX_STATUS_EOP != 0;
            if whole && errors == 0 && (14..=BUFFER_SIZE).contains(&length) {
                let buffer = phys_to_virt(self.rx_buffers + (rx.next * BUFFER_SIZE) as u64);
                let bytes = unsafe { core::slice::from_raw_parts(buffer as *const u8, length) };
                QUEUE.lock().push(bytes);
                taken += 1;
            }

            // Lend the descriptor back. The whole thing is rewritten, buffer
            // address included, because the write-back does not promise to
            // leave it alone. The tail must not move before the status byte
            // is clear, or the card can refill the descriptor and have its
            // new done bit overwritten by this store.
            unsafe { core::ptr::write_volatile(desc, self.fresh_rx_desc(rx.next)) };
            fence(Ordering::SeqCst);
            self.write(RDT, rx.next as u32);
            rx.next = (rx.next + 1) % RX_DESCS;
        }
        taken
    }

    /// Acknowledge the card's interrupt and move its work off the ring.
    fn handle_interrupt(&self) {
        // Reading the cause register is what clears it and drops the card's
        // interrupt line. The line is level triggered and may be shared, so a
        // cause of zero means this interrupt belonged to something else.
        let cause = self.read(ICR);
        if cause == 0 {
            return;
        }
        IRQ_COUNT.fetch_add(1, Ordering::Relaxed);
        if cause & INT_RXO != 0 {
            // The ring filled before the kernel got here and the card dropped
            // frames on its own. Draining below is the whole remedy; the
            // count is there so a stack that loses frames can tell where.
            OVERRUNS.fetch_add(1, Ordering::Relaxed);
        }
        if self.drain_rx() > 0 {
            RX_WAIT.wake_all();
        }
    }
}

impl Interface for E1000 {
    fn mac(&self) -> [u8; 6] {
        self.mac
    }

    fn transmit(&self, frame: &[u8]) -> Result<(), Errno> {
        if frame.len() < 14 || frame.len() > BUFFER_SIZE {
            return Err(Errno::EINVAL);
        }
        let mut tx = self.tx.lock();
        let index = tx.next;
        let desc = self.tx_desc(index);

        // The descriptor at the tail has to be one the card is finished with.
        // Every descriptor starts marked done and `transmit` waits for the
        // done bit before it returns, so this only ever spins if the ring has
        // wrapped onto something still in flight.
        let mut spins = 0u32;
        while unsafe { core::ptr::read_volatile(core::ptr::addr_of!((*desc).status)) }
            & TX_STATUS_DD
            == 0
        {
            spins += 1;
            if spins > TX_SPIN_LIMIT {
                return Err(Errno::EIO);
            }
            core::hint::spin_loop();
        }

        let buffer = phys_to_virt(self.tx_buffers + (index * BUFFER_SIZE) as u64);
        unsafe {
            core::ptr::copy_nonoverlapping(frame.as_ptr(), buffer as *mut u8, frame.len());
            let filled = TxDesc {
                addr: self.tx_buffers + (index * BUFFER_SIZE) as u64,
                length: frame.len() as u16,
                cso: 0,
                // One descriptor is the whole frame; the card appends the
                // CRC and writes the done bit back when it has gone out.
                cmd: TX_CMD_EOP | TX_CMD_IFCS | TX_CMD_RS,
                status: 0,
                css: 0,
                special: 0,
            };
            core::ptr::write_volatile(desc, filled);
        }
        // The descriptor has to be in memory before the card is told to look.
        fence(Ordering::SeqCst);

        tx.next = (index + 1) % TX_DESCS;
        self.write(TDT, tx.next as u32);

        // The contract says this may not sleep, so the wait for the card is a
        // spin. QEMU sends the frame inside the tail write above, so this
        // almost always sees the done bit on its first read.
        let mut spins = 0u32;
        loop {
            let status =
                unsafe { core::ptr::read_volatile(core::ptr::addr_of!((*desc).status)) };
            if status & TX_STATUS_DD != 0 {
                return Ok(());
            }
            spins += 1;
            if spins > TX_SPIN_LIMIT {
                return Err(Errno::EIO);
            }
            core::hint::spin_loop();
        }
    }
}

// ---------------------------------------------------------------------------
// Bring-up
// ---------------------------------------------------------------------------

/// Allocate `bytes` of physically contiguous, zeroed memory for the card to
/// read and write, returning its physical address. The memory is never given
/// back, so no handle is kept.
fn dma_alloc(bytes: usize) -> Option<u64> {
    let pages = (bytes + PAGE_SIZE - 1) / PAGE_SIZE;
    let phys = alloc_contiguous(pages)?;
    unsafe { core::ptr::write_bytes(phys_to_virt(phys) as *mut u8, 0, pages * PAGE_SIZE) };
    Some(phys)
}

fn format_mac(mac: &[u8; 6]) -> alloc::string::String {
    use core::fmt::Write;
    let mut out = alloc::string::String::new();
    for (i, byte) in mac.iter().enumerate() {
        let _ = write!(out, "{}{:02x}", if i == 0 { "" } else { ":" }, byte);
    }
    out
}

/// Find the card, map it, bring it up and register it with the stack.
///
/// Returns false when there is no card, which is the ordinary case on a
/// machine booted without one and is not an error.
///
/// This has to run before the first user address space is created: the
/// register mapping goes into the kernel half of the page tables, and a new
/// address space copies the kernel half as it stands at the moment it is
/// made.
pub fn probe() -> bool {
    let Some(dev) = pci::find(VENDOR_INTEL, DEVICE_82540EM) else {
        return false;
    };
    crate::println!(
        "pci: {:04x}:{:04x} at {:02x}:{:02x}.{} class {:02x}:{:02x} irq {}",
        dev.vendor,
        dev.id,
        dev.bus,
        dev.device,
        dev.function,
        dev.class,
        dev.subclass,
        dev.irq_line
    );

    // Without a routed line nothing would ever tell the kernel a frame had
    // arrived, and the vector number below would be nonsense. 0xFF is what
    // configuration space holds when the firmware routed nothing.
    if dev.irq_line == 0 || dev.irq_line > 15 {
        crate::println!("e1000: interrupt line {} is not routable; giving up", dev.irq_line);
        return false;
    }

    let Some(pci::Bar::Memory { addr, size, .. }) = dev.bar(0) else {
        crate::println!("e1000: base address register 0 is not memory; giving up");
        return false;
    };
    if addr == 0 || size == 0 {
        crate::println!("e1000: base address register 0 is unassigned; giving up");
        return false;
    }

    // Let the card decode its registers and start transfers of its own.
    dev.enable(pci::CMD_MEMORY_SPACE | pci::CMD_BUS_MASTER);

    let Some(regs) = pci::map_device(addr, size) else {
        crate::println!("e1000: no room to map {} KiB of registers", size / 1024);
        return false;
    };
    crate::println!(
        "e1000: registers at {:#x} ({} KiB) mapped uncached at {:#x}",
        addr,
        size / 1024,
        regs
    );

    let rings = match dma_alloc(PAGE_SIZE) {
        Some(phys) => phys,
        None => {
            crate::println!("e1000: cannot allocate descriptor rings");
            return false;
        }
    };
    let Some(rx_buffers) = dma_alloc(RX_DESCS * BUFFER_SIZE) else {
        crate::println!("e1000: cannot allocate receive buffers");
        return false;
    };
    let Some(tx_buffers) = dma_alloc(TX_DESCS * BUFFER_SIZE) else {
        crate::println!("e1000: cannot allocate transmit buffers");
        return false;
    };

    // Both rings fit in one page with room to spare. A ring's base must be
    // sixteen-byte aligned and its length a multiple of 128 bytes; a page
    // boundary and a half-page offset satisfy both.
    let rx_ring = rings;
    let tx_ring = rings + 2048;

    let card = Box::leak(Box::new(E1000 {
        regs,
        mac: [0; 6],
        irq: dev.irq_line,
        rx_ring,
        tx_ring,
        rx_buffers,
        tx_buffers,
        rx: Spinlock::new(RxRing { next: 0 }),
        tx: Spinlock::new(TxRing { next: 0 }),
    }));

    card.reset();
    let mac = card.read_mac();
    card.mac = mac;
    // Everything past bring-up shares the card, so give up the exclusive
    // reference the leak handed back; a shared one can be copied into the
    // interrupt handler's slot and into the stack.
    let card: &'static E1000 = card;

    card.set_receive_address(&mac);
    card.setup_rx();
    card.setup_tx();

    // Link up, let the PHY negotiate speed, and keep the bits that would
    // break a copper link clear.
    let ctrl = card.read(CTRL);
    let ctrl = (ctrl | CTRL_SLU | CTRL_ASDE | CTRL_FD)
        & !(CTRL_LRST | CTRL_PHY_RST | CTRL_ILOS | CTRL_VME);
    card.write(CTRL, ctrl);

    // Preallocate the queue the interrupt handler copies into, so it never
    // reaches the heap allocator.
    {
        let mut queue = QUEUE.lock();
        queue.slots = Vec::with_capacity(QUEUE_SLOTS);
        for _ in 0..QUEUE_SLOTS {
            queue.slots.push(Vec::with_capacity(BUFFER_SIZE));
        }
    }

    *DEVICE.lock() = Some(card);

    // Only now is it safe for the card to interrupt: the ring, the queue and
    // the handler's view of the device are all in place.
    crate::cpu::idt::register(pic::PIC1_OFFSET + dev.irq_line, interrupt);
    card.write(IMS, INT_RXT0 | INT_RXDMT0 | INT_RXO | INT_LSC);
    pic::unmask(dev.irq_line);
    card.enable_rx();

    // The link comes up in microseconds on an emulated card; wait a little so
    // the first frame out is not sent into a link that is still down.
    for _ in 0..1_000_000 {
        if card.link_up() {
            break;
        }
        core::hint::spin_loop();
    }

    crate::println!(
        "e1000: {} link {} {} Mb/s {}",
        format_mac(&mac),
        if card.link_up() { "up" } else { "down" },
        card.link_speed(),
        if card.full_duplex() { "full duplex" } else { "half duplex" }
    );

    crate::net::attach(card);
    true
}

/// The card's interrupt.
///
/// It reads the cause register, moves whatever arrived off the ring, and
/// wakes the network task. Everything the protocols do happens in that task,
/// with interrupts on.
fn interrupt(frame: &mut TrapFrame) {
    let irq = (frame.vector - pic::PIC1_OFFSET as u64) as u8;
    if let Some(card) = device() {
        card.handle_interrupt();
    }
    // PCI interrupt lines are shared, and on this machine they can land on a
    // line the console also uses. Pass it on rather than swallowing it.
    match irq {
        1 => crate::console::keyboard_irq(),
        3 | 4 => crate::console::serial_irq(),
        _ => {}
    }
    pic::end_of_interrupt(irq);
}

// ---------------------------------------------------------------------------
// The network task
// ---------------------------------------------------------------------------

/// Start the kernel task that runs the protocol half.
///
/// It never enters user mode, so it has no user address space of its own and
/// runs on the kernel's. It must be created after the init process, because
/// the scheduler hands out process ids in order and a great deal of the
/// system assumes pid 1 is init.
pub fn start_task() {
    let space = AddressSpace::current();
    let Some(mut task) = Task::new("netd", space) else {
        crate::println!("e1000: cannot create the network task");
        return;
    };
    task.prepare_kernel_frame(net_task as extern "C" fn() -> ! as usize as u64);
    let pid = sched::register(task);
    crate::println!("e1000: network task is pid {}", pid);
}

/// Drain received frames into the stack, and give the stack a tick.
///
/// The wait is on both the queue and the next timer tick, so the task wakes
/// either when a frame arrives or when there is a tick's worth of timeouts to
/// run, and sleeps in between.
extern "C" fn net_task() -> ! {
    let mut scratch: Vec<u8> = Vec::with_capacity(BUFFER_SIZE);
    let mut last_tick = crate::trap::ticks();
    loop {
        RX_WAIT.wait_until_or_at(last_tick + 1, || QUEUE.lock().count > 0);

        loop {
            if !QUEUE.lock().pop(&mut scratch) {
                break;
            }
            if TRACE_RX.load(Ordering::Relaxed) {
                log_frame(&scratch);
            }
            super::arptest::observe(&scratch);
            // No driver lock is held here: the stack may allocate, block on
            // its own locks, and call back into `transmit`.
            crate::net::receive(&scratch);
        }

        let now = crate::trap::ticks();
        if now != last_tick {
            // One call per tick, but bounded: a task held off the CPU for a
            // second should not then run a hundred rounds of timers back to
            // back.
            let missed = (now - last_tick).min(8);
            for _ in 0..missed {
                crate::net::tick();
            }
            last_tick = now;
        }
    }
}

/// One line per received frame, for the boot-time test.
fn log_frame(frame: &[u8]) {
    let mut dst = [0u8; 6];
    let mut src = [0u8; 6];
    dst.copy_from_slice(&frame[0..6]);
    src.copy_from_slice(&frame[6..12]);
    let ethertype = u16::from_be_bytes([frame[12], frame[13]]);
    crate::println!(
        "e1000: [t={}] rx {} bytes {} -> {} type {:#06x}",
        crate::trap::ticks(),
        frame.len(),
        format_mac(&src),
        format_mac(&dst),
        ethertype
    );
}
