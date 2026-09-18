//! Broadcom GENET v5, the wired Ethernet inside a BCM2711.
//!
//! The shape is the e1000's -- two rings of descriptors, buffers the device
//! reaches by physical address, an interrupt that only moves frames off the
//! ring and wakes a task -- and four things about it are different.
//!
//! **It is not on a bus that can be enumerated.** It sits at a fixed address
//! inside the chip and the firmware describes it in the device tree. So does
//! it say where the registers are, which interrupt line it raises, and what
//! hardware address the board was built with. That last one matters: the
//! address is per-board and the firmware is the only thing that knows it, so a
//! driver that invents one gets its frames dropped by the first switch that
//! sees two machines claiming the same name.
//!
//! **The descriptors are not in memory.** They are 256 twelve-byte slots
//! inside the device's own register block, three 32-bit registers each: a
//! length-and-status word and a 64-bit buffer address. Nothing about them
//! needs cache maintenance, because reading and writing them is register
//! access. Only the packet buffers are in memory.
//!
//! **The packet buffers do need cache maintenance.** The processor's caches
//! are not coherent with this device. A frame written into a buffer sits in
//! the data cache until something pushes it out, and the device would fetch
//! whatever memory held before; a frame the device writes into a buffer is
//! invisible to a read that a cached line can answer. `arch::clean_data_cache`
//! and `arch::invalidate_data_cache` are the two halves of that, and the
//! driver calls them either side of every transfer.
//!
//! **The link is a separate chip.** A BCM54213PE hangs off a management bus
//! the controller exposes as two registers, and it is what negotiates speed
//! and duplex with the other end. Nothing can be sent until it says the link
//! is up, and what it settled on has to be written back into the controller,
//! which does not find out by itself.
//!
//! Every register offset and bit below is from Linux's driver,
//! `drivers/net/ethernet/broadcom/genet/`, and the naming follows it so the
//! two can be read side by side. Where u-boot's `drivers/net/bcmgenet.c`, a
//! bare-metal driver for this same board, does something differently, the
//! difference is called out at the point it matters.

use crate::abi::Errno;
use crate::arch::{self, TrapFrame};
use crate::mm::frame::alloc_contiguous;
use crate::mm::{phys_to_virt, PAGE_SIZE};
use crate::net::Interface;
use crate::sched::{self, WaitQueue};
use crate::sync::Spinlock;
use crate::task::Task;
use alloc::boxed::Box;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// What the node in the device tree calls itself. The Pi 4's tree names this
/// exact string; `brcm,genet-v5` is the generic one and is not what the
/// firmware writes here.
const COMPATIBLE: &[u8] = b"brcm,bcm2711-genet-v5";

// ---------------------------------------------------------------------------
// Register blocks
//
// Offsets are bytes from the start of the controller's register window, and
// every register is 32 bits. The block bases are `bcmgenet.h`, "Register block
// offsets"; the registers inside UMAC are `unimac.h`, which `bcmgenet.h`
// includes.
// ---------------------------------------------------------------------------

const SYS_REV_CTRL: u32 = 0x0000; // version, and which EPHY is fitted
const SYS_PORT_CTRL: u32 = 0x0004; // which kind of PHY the port is wired to
const SYS_RBUF_FLUSH_CTRL: u32 = 0x0008; // bit 1 holds the MAC in reset

/// The value `SYS_PORT_CTRL` takes for an external gigabit PHY on RGMII, which
/// is what this board has. `bcmgenet.h`, PORT_MODE_EXT_GPHY.
const PORT_MODE_EXT_GPHY: u32 = 3;

const EXT_RGMII_OOB_CTRL: u32 = 0x0080 + 0x0C;
/// Tell the RGMII block the link is up. Without it nothing leaves the port.
const RGMII_LINK: u32 = 1 << 4;
/// Out-of-band status input, which this board does not use.
const OOB_DISABLE: u32 = 1 << 5;
const RGMII_MODE_EN: u32 = 1 << 6;
/// Turn *off* the delay the controller otherwise puts on the transmit clock.
const ID_MODE_DIS: u32 = 1 << 16;

/// The first of the two interrupt controllers inside the block. The default
/// ring's receive and transmit completions both arrive through this one; the
/// second is for the priority rings, which are not used.
const INTRL2_0: u32 = 0x0200;
const INTRL2_CPU_STAT: u32 = 0x00;
const INTRL2_CPU_CLEAR: u32 = 0x08;
const INTRL2_CPU_MASK_STATUS: u32 = 0x0C;
const INTRL2_CPU_MASK_SET: u32 = 0x10;
const INTRL2_CPU_MASK_CLEAR: u32 = 0x14;
const INTRL2_1: u32 = 0x0240;

const IRQ_RBUF_OVERFLOW: u32 = 1 << 9;
/// A receive descriptor's worth of work is done on the default ring.
const IRQ_RXDMA_DONE: u32 = 1 << 13;
const IRQ_TXDMA_DONE: u32 = 1 << 16;

const RBUF_CTRL: u32 = 0x0300;
/// Put a 64-byte status block in front of every received frame. Left off: the
/// only thing in it this driver would read is the length, and the descriptor
/// carries that too.
const RBUF_64B_EN: u32 = 1 << 0;
/// Have the controller put two bytes in front of every received frame, so the
/// IP header inside it lands on a four-byte boundary. Both references set it,
/// and both then skip those two bytes on the way out.
const RBUF_ALIGN_2B: u32 = 1 << 1;
const RBUF_TBUF_SIZE_CTRL: u32 = 0x0300 + 0xB4;

/// The transmit buffer block, at `tbuf_offset` for v5.
const TBUF_CTRL: u32 = 0x0600;
/// Expect a 64-byte status block in front of every frame handed over. Left
/// off, to match the receive side: nothing here offloads a checksum, which is
/// the only thing the block is for.
const TBUF_64B_EN: u32 = 1 << 0;

const UMAC: u32 = 0x0800;
const UMAC_CMD: u32 = UMAC + 0x008;
const UMAC_MAC0: u32 = UMAC + 0x00C;
const UMAC_MAC1: u32 = UMAC + 0x010;
const UMAC_MAX_FRAME_LEN: u32 = UMAC + 0x014;
const UMAC_TX_FLUSH: u32 = UMAC + 0x334;
const UMAC_MIB_CTRL: u32 = UMAC + 0x580;
pub(crate) const UMAC_MDIO_CMD: u32 = UMAC + 0x614;
const UMAC_MDF_CTRL: u32 = UMAC + 0x650;

const CMD_TX_EN: u32 = 1 << 0;
const CMD_RX_EN: u32 = 1 << 1;
const CMD_SPEED_10: u32 = 0;
const CMD_SPEED_100: u32 = 1;
const CMD_SPEED_1000: u32 = 2;
const CMD_SPEED_SHIFT: u32 = 2;
const CMD_SPEED_MASK: u32 = 3;
/// Take every frame, whoever it is addressed to.
const CMD_PROMISC: u32 = 1 << 4;
const CMD_HD_EN: u32 = 1 << 10;
const CMD_SW_RESET: u32 = 1 << 13;
/// Loop the (RG)MII back inside the block. Held together with the reset so the
/// receive clock is steady while the MAC is being set up, which is what
/// u-boot's `bcmgenet_eth_probe` does and what Linux's `reset_umac` leaves to
/// the clock being running already.
const CMD_LCL_LOOP_EN: u32 = 1 << 15;
const CMD_RX_PAUSE_IGNORE: u32 = 1 << 8;
const CMD_TX_PAUSE_IGNORE: u32 = 1 << 28;

const MIB_RESET_RX: u32 = 1 << 0;
const MIB_RESET_RUNT: u32 = 1 << 1;
const MIB_RESET_TX: u32 = 1 << 2;

/// Body 1500, Ethernet header 14, VLAN tag 4, Broadcom tag 6, CRC 4, pad 8.
/// `bcmgenet.h`, ENET_MAX_MTU_SIZE.
const MAX_FRAME_LEN: u32 = 1536;

// ---------------------------------------------------------------------------
// The two DMA engines
//
// Each has 256 descriptors, then a block of seventeen ring register sets, then
// its own control registers. `bcmgenet.c`: GENET_RDMA_REG_OFF and
// GENET_TDMA_REG_OFF skip the descriptors, DMA_RINGS_SIZE skips the rings.
// The per-ring offsets are `genet_dma_ring_regs_v4` and the control offsets
// `bcmgenet_dma_regs_v3plus`, which is the pair GENET v4 and v5 select.
// ---------------------------------------------------------------------------

/// Descriptors the hardware has, per direction. Not a choice: it is how many
/// slots there are in the register block.
const TOTAL_DESCS: usize = 256;
/// Three 32-bit words to a descriptor, which is `words_per_bd` for v5.
const DESC_SIZE: u32 = 12;
const DESC_LENGTH_STATUS: u32 = 0x00;
const DESC_ADDRESS_LO: u32 = 0x04;
const DESC_ADDRESS_HI: u32 = 0x08;

const RDMA_DESCS: u32 = 0x2000;
const TDMA_DESCS: u32 = 0x4000;
const RDMA_RINGS: u32 = RDMA_DESCS + TOTAL_DESCS as u32 * DESC_SIZE;
const TDMA_RINGS: u32 = TDMA_DESCS + TOTAL_DESCS as u32 * DESC_SIZE;
const RING_STRIDE: u32 = 0x40;
/// The ring every frame goes through. Sixteen priority rings sit below it and
/// none of them is enabled: nothing here sorts traffic into classes.
const DEFAULT_RING: u32 = 16;
const RINGS_SIZE: u32 = RING_STRIDE * (DEFAULT_RING + 1);

pub(crate) const RDMA_RING: u32 = RDMA_RINGS + DEFAULT_RING * RING_STRIDE;
pub(crate) const TDMA_RING: u32 = TDMA_RINGS + DEFAULT_RING * RING_STRIDE;
pub(crate) const RDMA_CTRL: u32 = RDMA_RINGS + RINGS_SIZE;
pub(crate) const TDMA_CTRL: u32 = TDMA_RINGS + RINGS_SIZE;

// Per-ring registers, as offsets within one ring's block. Where the two
// directions give a register different meanings it has two names here, the
// way `enum dma_ring_reg` does.
const RDMA_WRITE_PTR: u32 = 0x00;
const TDMA_READ_PTR: u32 = 0x00;
const RDMA_PROD_INDEX: u32 = 0x08; // the device's cursor
const TDMA_CONS_INDEX: u32 = 0x08; // the device's cursor
const RDMA_CONS_INDEX: u32 = 0x0C; // the kernel's cursor
const TDMA_PROD_INDEX: u32 = 0x0C; // the kernel's cursor
const DMA_RING_BUF_SIZE: u32 = 0x10;
const DMA_START_ADDR: u32 = 0x14;
const DMA_END_ADDR: u32 = 0x1C;
const DMA_MBUF_DONE_THRESH: u32 = 0x24;
const RDMA_XON_XOFF_THRESH: u32 = 0x28;
const TDMA_FLOW_PERIOD: u32 = 0x28;
const RDMA_READ_PTR: u32 = 0x2C;
const TDMA_WRITE_PTR: u32 = 0x2C;

// Control registers of one direction.
const DMA_RING_CFG: u32 = 0x00;
const DMA_CTRL: u32 = 0x04;
const DMA_SCB_BURST_SIZE: u32 = 0x0C;
/// Per-ring interrupt timeout, one register each from ring zero up.
const DMA_RING0_TIMEOUT: u32 = 0x2C;

const DMA_EN: u32 = 1 << 0;
const DMA_RING_BUF_EN_SHIFT: u32 = 1;
/// How much the controller may move in one burst on the internal bus. The
/// BCM2711 takes a smaller value than a generic v5 part, and Linux carries it
/// as `bcm2711_plat_data.dma_max_burst_length`.
const DMA_MAX_BURST_LENGTH: u32 = 0x08;

// Length-and-status word, shared by both directions. `bcmgenet.h`, "Tx/Rx Dma
// Descriptor common bits".
const DMA_BUFLENGTH_SHIFT: u32 = 16;
const DMA_BUFLENGTH_MASK: u32 = 0x0FFF;
/// Where the descriptor count goes in a ring's size register. The same place
/// as a length in a descriptor, and a different register.
const DMA_RING_SIZE_SHIFT: u32 = 16;
const DMA_EOP: u32 = 0x4000;
const DMA_SOP: u32 = 0x2000;
/// Have the controller compute and append the Ethernet CRC.
const DMA_TX_APPEND_CRC: u32 = 0x0040;
const DMA_TX_QTAG_SHIFT: u32 = 7;
/// `qtag_mask` for v5.
const QTAG_MASK: u32 = 0x3F;

// Error bits in a received descriptor's status half.
const DMA_RX_LG: u32 = 0x0010; // too long
const DMA_RX_NO: u32 = 0x0008; // odd number of nibbles
const DMA_RX_RXER: u32 = 0x0004; // the PHY reported an error
const DMA_RX_CRC_ERROR: u32 = 0x0002;
const DMA_RX_OV: u32 = 0x0001; // overflowed
const DMA_RX_ERRORS: u32 = DMA_RX_LG | DMA_RX_NO | DMA_RX_RXER | DMA_RX_CRC_ERROR | DMA_RX_OV;

/// Both indices count frames modulo this, whatever the ring's size is, and
/// the difference between them is how many are outstanding.
const INDEX_MASK: u32 = 0xFFFF;
/// The receive producer index keeps a count of discarded frames in its top
/// half, so it has to be masked before being compared with anything.
const DISCARD_SHIFT: u32 = 16;

/// Bytes per packet buffer. `RX_BUF_LENGTH` in both references.
const BUFFER_SIZE: usize = 2048;
/// The two bytes `RBUF_ALIGN_2B` puts in front of a received frame. The length
/// the descriptor reports includes them.
const RX_BUF_OFFSET: usize = 2;

/// Descriptors this driver uses, of the 256 there are. The ring's size has to
/// divide the index space so that a descriptor's position is the index's low
/// bits, which means a power of two. The receive ring keeps all 256 so that
/// the flow control thresholds below are the ones both references compute.
const RX_DESCS: usize = 256;
const TX_DESCS: usize = 64;

// What the arithmetic above takes for granted. None of it can be found out by
// running the driver, so it is said here where the compiler will check it.
const _: () = assert!(RX_DESCS.is_power_of_two() && RX_DESCS <= TOTAL_DESCS);
const _: () = assert!(TX_DESCS.is_power_of_two() && TX_DESCS <= TOTAL_DESCS);
/// The index space is 65536 wide, so a power of two ring divides it and the
/// two wrap together.
const _: () = assert!(RX_DESCS <= 0x1_0000 && TX_DESCS <= 0x1_0000);
/// Buffers are cut out of whole pages and none may straddle one.
const _: () = assert!(PAGE_SIZE % BUFFER_SIZE == 0);
/// A frame the controller will accept has to fit in a buffer.
const _: () = assert!(MAX_FRAME_LEN as usize <= BUFFER_SIZE);

/// Flow control thresholds for the receive ring, in descriptors: ask the other
/// end to pause when this many are left, resume when this many are free again.
/// `DMA_FC_THRESH_LO` and `DMA_FC_THRESH_HI` in `bcmgenet.h`, where the upper
/// one is the ring's size over sixteen.
const FC_THRESH_LO: u32 = 5;
const FC_THRESH_HI: u32 = (RX_DESCS >> 4) as u32;
const XOFF_THRESHOLD_SHIFT: u32 = 16;

// ---------------------------------------------------------------------------
// Arithmetic the ring needs, kept separate so it can be checked without a
// device to run it against.
// ---------------------------------------------------------------------------

/// Where descriptor `index` of the receive ring is, as an offset into the
/// register window.
pub(crate) const fn rx_desc(index: usize) -> u32 {
    RDMA_DESCS + index as u32 * DESC_SIZE
}

pub(crate) const fn tx_desc(index: usize) -> u32 {
    TDMA_DESCS + index as u32 * DESC_SIZE
}

/// Which descriptor a producer or consumer index refers to. The index counts
/// frames and wraps at 65536; the ring holds `count` descriptors and `count`
/// divides 65536, so the low bits pick the slot and the wrap of one is the
/// wrap of the other.
pub(crate) const fn slot(index: u32, count: usize) -> usize {
    (index as usize) & (count - 1)
}

/// How many descriptors the far cursor is ahead of the near one, across the
/// point where both wrap.
pub(crate) const fn outstanding(producer: u32, consumer: u32) -> u32 {
    (producer.wrapping_sub(consumer)) & INDEX_MASK
}

/// The length-and-status word for a frame of `len` bytes occupying one whole
/// descriptor. `bcmgenet_xmit`: the queue tag is all ones, the controller
/// appends the CRC, and one descriptor is both the start and the end of the
/// packet.
pub(crate) const fn tx_length_status(len: usize) -> u32 {
    ((len as u32) << DMA_BUFLENGTH_SHIFT)
        | (QTAG_MASK << DMA_TX_QTAG_SHIFT)
        | DMA_TX_APPEND_CRC
        | DMA_SOP
        | DMA_EOP
}

/// The byte count out of a received descriptor's word, including the two
/// alignment bytes in front of the frame.
pub(crate) const fn rx_length(length_status: u32) -> usize {
    ((length_status >> DMA_BUFLENGTH_SHIFT) & DMA_BUFLENGTH_MASK) as usize
}

/// A descriptor's start and end, in 32-bit words, for the registers that bound
/// a ring. `bcmgenet_init_rx_ring`: the end is the last word, not one past it.
pub(crate) const fn ring_start_word(first: usize) -> u32 {
    first as u32 * (DESC_SIZE / 4)
}

pub(crate) const fn ring_end_word(count: usize) -> u32 {
    count as u32 * (DESC_SIZE / 4) - 1
}

// ---------------------------------------------------------------------------
// The management bus and the chip on the other end of it
// ---------------------------------------------------------------------------

/// `micros` microseconds, as a count of architected counter ticks. Rounded up,
/// so a wait shorter than one tick still waits for one.
fn counter_ticks(micros: u64) -> u64 {
    let frequency = arch::counter_frequency();
    if frequency == 0 {
        return 0;
    }
    (micros * frequency + 999_999) / 1_000_000
}

/// Microseconds to wait for one management transfer.
const MDIO_TIMEOUT_US: u32 = 20_000;
/// Microseconds to wait for the PHY to come out of its reset. The standard
/// gives a PHY half a second.
const PHY_RESET_TIMEOUT_US: u32 = 500_000;

const MDIO_START_BUSY: u32 = 1 << 29;
const MDIO_READ_FAIL: u32 = 1 << 28;
const MDIO_RD: u32 = 2 << 26;
const MDIO_WR: u32 = 1 << 26;
const MDIO_PMD_SHIFT: u32 = 21;
const MDIO_REG_SHIFT: u32 = 16;

// The registers every PHY has, from clause 22 of the standard.
const MII_BMCR: u8 = 0x00;
const MII_BMSR: u8 = 0x01;
const MII_PHYSID1: u8 = 0x02;
const MII_PHYSID2: u8 = 0x03;
const MII_ADVERTISE: u8 = 0x04;
const MII_LPA: u8 = 0x05;
const MII_CTRL1000: u8 = 0x09;
const MII_STAT1000: u8 = 0x0A;

const BMCR_RESET: u16 = 1 << 15;
const BMCR_ANENABLE: u16 = 1 << 12;
const BMCR_ANRESTART: u16 = 1 << 9;
const BMSR_LSTATUS: u16 = 1 << 2;
const BMSR_ANEGCOMPLETE: u16 = 1 << 5;

// What is advertised in register 4 and read back from the other end in
// register 5, which put the same abilities in the same bits.
const ADVERTISE_CSMA: u16 = 1 << 0;
const ADVERTISE_10HALF: u16 = 1 << 5;
const ADVERTISE_10FULL: u16 = 1 << 6;
const ADVERTISE_100HALF: u16 = 1 << 7;
const ADVERTISE_100FULL: u16 = 1 << 8;
// The gigabit pair, which live in registers 9 and 10 instead. Their numbering
// starts again from the bottom, so 1 << 8 here is not the same ability as
// 1 << 8 above, and the two sets must not be mixed between registers.
const ADVERTISE_1000HALF: u16 = 1 << 8;
const ADVERTISE_1000FULL: u16 = 1 << 9;

/// Broadcom's shadow-register windows, which is how the delay settings are
/// reached. `include/linux/brcmphy.h`.
const MII_BCM54XX_AUX_CTL: u8 = 0x18;
const MII_BCM54XX_SHD: u8 = 0x1C;
const AUXCTL_SHDWSEL_MISC: u16 = 0x07;
/// Every bit of the window-select field, which is what a read puts in it.
const AUXCTL_SHDWSEL_MASK: u16 = 0x0007;
const AUXCTL_MISC_RGMII_SKEW_EN: u16 = 0x0100;
const AUXCTL_MISC_WREN: u16 = 0x8000;
const AUXCTL_SHDWSEL_READ_SHIFT: u16 = 12;
const SHD_CLK_CTL: u16 = 0x03;
const SHD_CLK_CTL_GTXCLK_EN: u16 = 1 << 9;
const SHD_WRITE: u16 = 0x8000;

/// How the port is wired between the controller and the PHY, which decides
/// which end puts the quarter-cycle delay on which clock. The tree says.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PhyMode {
    /// Neither end delays anything; the board's traces do it.
    Rgmii,
    /// The PHY delays the receive clock, the controller the transmit clock.
    /// This is what a Pi 4's tree says, as `rgmii-rxid`.
    RgmiiRxId,
    /// The PHY delays the transmit clock.
    RgmiiTxId,
    /// The PHY delays both.
    RgmiiId,
}

// ---------------------------------------------------------------------------
// The queue between the interrupt and the network task
// ---------------------------------------------------------------------------

const QUEUE_SLOTS: usize = 64;

/// Frames taken off the receive ring, waiting for the network task. Preallocated
/// at bring-up, the same as the e1000's, so the interrupt handler never reaches
/// the heap allocator.
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
static TRACE_RX: AtomicBool = AtomicBool::new(false);

pub fn trace_received(on: bool) {
    TRACE_RX.store(on, Ordering::Relaxed);
}

static IRQ_COUNT: AtomicU64 = AtomicU64::new(0);
static OVERRUNS: AtomicU64 = AtomicU64::new(0);
/// The highest the receive engine's own count of discarded frames has been
/// seen at. It is cumulative and there is no way to clear it that does not
/// also write the producer index, which belongs to the device.
static DISCARDS: AtomicU64 = AtomicU64::new(0);
/// Frames given up on because the transmit ring stayed full.
static TX_BLOCKED: AtomicU64 = AtomicU64::new(0);

pub fn transmits_dropped() -> u64 {
    TX_BLOCKED.load(Ordering::Relaxed)
}

pub fn interrupts() -> u64 {
    IRQ_COUNT.load(Ordering::Relaxed)
}

/// Times the controller had nowhere to put a frame: once for each overflow it
/// interrupted about, plus the count the receive engine keeps of frames it
/// discarded because no descriptor was free.
pub fn overruns() -> u64 {
    OVERRUNS.load(Ordering::Relaxed) + DISCARDS.load(Ordering::Relaxed)
}

pub fn dropped() -> u64 {
    QUEUE.lock().dropped
}

// ---------------------------------------------------------------------------
// The controller
// ---------------------------------------------------------------------------

struct RxRing {
    /// Frames handed back to the device so far, which is the register the
    /// kernel owns and also says which descriptor comes next.
    consumer: u32,
}

struct TxRing {
    /// Frames handed to the device so far.
    producer: u32,
}

/// What the link settled on, or that it has not.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Link {
    pub up: bool,
    pub speed: u32,
    pub full_duplex: bool,
}

pub struct Genet {
    /// Kernel address the register block answers at.
    regs: u64,
    mac: [u8; 6],
    irq: u8,
    /// Where the PHY answers on the management bus.
    phy: u8,
    mode: PhyMode,
    /// Whether the tree said this device's transfers are coherent with the
    /// caches. On a Pi 4 it does not, and the maintenance below is done
    /// whatever it says: on a device that is coherent the maintenance is
    /// wasted work and not a mistake, while the barrier at the end of it is
    /// still what orders a buffer's bytes against the descriptor that points
    /// at them. Skipping it on the strength of one property would leave that
    /// ordering to a path no board here has ever taken.
    coherent: bool,
    /// Physical addresses of the packet buffers, one per descriptor. They are
    /// allocated once and never released.
    rx_buffers: Vec<u64>,
    tx_buffers: Vec<u64>,
    rx: Spinlock<RxRing>,
    tx: Spinlock<TxRing>,
    link: Spinlock<Link>,
}

static DEVICE: Spinlock<Option<&'static Genet>> = Spinlock::new(None);

pub fn device() -> Option<&'static Genet> {
    *DEVICE.lock()
}

impl Genet {
    #[inline]
    fn read(&self, offset: u32) -> u32 {
        unsafe { core::ptr::read_volatile((self.regs + offset as u64) as *const u32) }
    }

    #[inline]
    fn write(&self, offset: u32, value: u32) {
        unsafe { core::ptr::write_volatile((self.regs + offset as u64) as *mut u32, value) }
    }

    fn modify(&self, offset: u32, clear: u32, set: u32) {
        let value = (self.read(offset) & !clear) | set;
        self.write(offset, value);
    }

    /// Spin for `micros` microseconds.
    ///
    /// The timer tick is not running during bring-up -- interrupts are still
    /// off -- but the architected counter is, and it reports its own rate, so
    /// this is a measured wait rather than a guessed number of iterations.
    /// Every use is a delay the reference driver asks for by name, where the
    /// hardware needs a moment and nothing says when it is done.
    fn delay(&self, micros: u64) {
        let ticks = counter_ticks(micros);
        let start = arch::cycle_counter();
        while arch::cycle_counter().wrapping_sub(start) < ticks {
            core::hint::spin_loop();
        }
    }

    // ---- the management bus -------------------------------------------

    /// One read from a PHY register. `mdio-bcm-unimac.c`, `unimac_mdio_read`:
    /// the command goes in, the busy bit is set separately, and the answer is
    /// in the bottom half of the same register when it clears.
    fn mdio_read(&self, phy: u8, reg: u8) -> Option<u16> {
        let command =
            MDIO_RD | ((phy as u32) << MDIO_PMD_SHIFT) | ((reg as u32) << MDIO_REG_SHIFT);
        self.write(UMAC_MDIO_CMD, command);
        self.write(UMAC_MDIO_CMD, command | MDIO_START_BUSY);
        if !self.mdio_wait() {
            return None;
        }
        let value = self.read(UMAC_MDIO_CMD);
        // The controller reports a transfer the PHY never answered. It is not
        // an error for a scan -- it is what an empty address looks like -- so
        // it is passed back as "nothing" rather than as a value of all ones.
        if value & MDIO_READ_FAIL != 0 {
            return None;
        }
        Some(value as u16)
    }

    fn mdio_write(&self, phy: u8, reg: u8, value: u16) -> bool {
        let command = MDIO_WR
            | ((phy as u32) << MDIO_PMD_SHIFT)
            | ((reg as u32) << MDIO_REG_SHIFT)
            | value as u32;
        self.write(UMAC_MDIO_CMD, command);
        self.write(UMAC_MDIO_CMD, command | MDIO_START_BUSY);
        self.mdio_wait()
    }

    /// Wait for a management transfer to finish. One frame on this bus is 64
    /// bit times at 2.5 MHz, so about 26 microseconds; the limit here is far
    /// beyond that and is only there so a bus with nothing driving it cannot
    /// stop the kernel. u-boot waits 20 milliseconds for the same bit.
    fn mdio_wait(&self) -> bool {
        for _ in 0..MDIO_TIMEOUT_US {
            if self.read(UMAC_MDIO_CMD) & MDIO_START_BUSY == 0 {
                return true;
            }
            self.delay(1);
        }
        false
    }

    /// Read one of the windows behind the auxiliary control register.
    /// `bcm54xx_auxctl_read`: selecting a window for reading means writing the
    /// number into both the select field in the low three bits, which for a
    /// read is all ones, and the read-select field further up.
    fn auxctl_read(&self, window: u16) -> Option<u16> {
        self.mdio_write(
            self.phy,
            MII_BCM54XX_AUX_CTL,
            AUXCTL_SHDWSEL_MASK | (window << AUXCTL_SHDWSEL_READ_SHIFT),
        );
        self.mdio_read(self.phy, MII_BCM54XX_AUX_CTL)
    }

    /// `bcm54xx_auxctl_write`: the window number in the low bits and the value
    /// above it. The value has to carry the write-enable bit or the register
    /// keeps what it had, and the caller is the one that puts it there.
    fn auxctl_write(&self, window: u16, value: u16) -> bool {
        self.mdio_write(self.phy, MII_BCM54XX_AUX_CTL, window | value)
    }

    /// `bcm_phy_read_shadow`: the window number goes in the top, the value
    /// comes back in the bottom ten bits.
    fn shadow_read(&self, window: u16) -> Option<u16> {
        self.mdio_write(self.phy, MII_BCM54XX_SHD, (window & 0x1F) << 10);
        Some(self.mdio_read(self.phy, MII_BCM54XX_SHD)? & 0x3FF)
    }

    fn shadow_write(&self, window: u16, value: u16) -> bool {
        self.mdio_write(
            self.phy,
            MII_BCM54XX_SHD,
            SHD_WRITE | ((window & 0x1F) << 10) | (value & 0x3FF),
        )
    }

    /// Put the quarter-cycle delays where the port's wiring says they belong.
    ///
    /// Linux does this in the PHY driver rather than in the Ethernet driver --
    /// `bcm54xx_config_clock_delay` in `drivers/net/phy/broadcom.c` -- because
    /// it has one; here there is nowhere else for it to go. The two bits are
    /// the receive skew, in the misc window of the auxiliary control register,
    /// and the transmit clock delay, in shadow register 3.
    ///
    /// With `rgmii-rxid`, which is what a Pi 4's tree says, the PHY delays the
    /// receive clock and does not delay the transmit clock: the controller
    /// does that end, and `ID_MODE_DIS` below is left clear so that it will.
    ///
    /// Neither register can be read back and checked against anything, and the
    /// part fitted may well come out of reset with these already right --
    /// u-boot never writes them and its network works. Writing them is the
    /// deliberate choice: a strap is a property of the board and not of the
    /// driver. If a write goes to the wrong window the link still comes up and
    /// the symptom is the one below.
    fn set_clock_delays(&self) {
        let rx_delay = matches!(self.mode, PhyMode::RgmiiRxId | PhyMode::RgmiiId);
        let tx_delay = matches!(self.mode, PhyMode::RgmiiTxId | PhyMode::RgmiiId);

        if let Some(misc) = self.auxctl_read(AUXCTL_SHDWSEL_MISC) {
            let misc = misc | AUXCTL_MISC_WREN;
            let misc = if rx_delay {
                misc | AUXCTL_MISC_RGMII_SKEW_EN
            } else {
                misc & !AUXCTL_MISC_RGMII_SKEW_EN
            };
            self.auxctl_write(AUXCTL_SHDWSEL_MISC, misc);
        }
        if let Some(clock) = self.shadow_read(SHD_CLK_CTL) {
            let clock = if tx_delay {
                clock | SHD_CLK_CTL_GTXCLK_EN
            } else {
                clock & !SHD_CLK_CTL_GTXCLK_EN
            };
            self.shadow_write(SHD_CLK_CTL, clock);
        }
    }

    /// Reset the PHY and start it negotiating. Advertise everything a gigabit
    /// copper port can do and let the other end choose.
    fn start_phy(&self) {
        self.mdio_write(self.phy, MII_BMCR, BMCR_RESET);
        // The reset bit clears itself when the PHY is done. Poll it in
        // millisecond steps rather than waiting the whole half second, since
        // it is usually a few milliseconds.
        for _ in 0..PHY_RESET_TIMEOUT_US / 1000 {
            match self.mdio_read(self.phy, MII_BMCR) {
                Some(value) if value & BMCR_RESET == 0 => break,
                _ => {}
            }
            self.delay(1000);
        }
        // A reset puts the delay settings back to their strapped values, so
        // they are written after it rather than before.
        self.set_clock_delays();

        self.mdio_write(
            self.phy,
            MII_ADVERTISE,
            ADVERTISE_CSMA
                | ADVERTISE_10HALF
                | ADVERTISE_10FULL
                | ADVERTISE_100HALF
                | ADVERTISE_100FULL,
        );
        self.mdio_write(self.phy, MII_CTRL1000, ADVERTISE_1000FULL | ADVERTISE_1000HALF);
        self.mdio_write(self.phy, MII_BMCR, BMCR_ANENABLE | BMCR_ANRESTART);
    }

    /// What the PHY says the link is now. Reading the status register twice is
    /// the standard's own instruction: the loss-of-link bit latches, so the
    /// first read reports whether it has ever been down since the last read
    /// and the second reports the state now.
    fn read_link(&self) -> Link {
        let down = Link { up: false, speed: 0, full_duplex: false };
        let Some(_) = self.mdio_read(self.phy, MII_BMSR) else { return down };
        let Some(status) = self.mdio_read(self.phy, MII_BMSR) else { return down };
        if status & BMSR_LSTATUS == 0 {
            return down;
        }
        if status & BMSR_ANEGCOMPLETE == 0 {
            // The link is up electrically but the two ends have not agreed on
            // anything yet, and the speed registers do not mean anything until
            // they have.
            return down;
        }

        // What both ends can do is what each advertised, and the fastest of
        // those wins. This is the resolution `genphy_read_status` performs.
        let gigabit = match (
            self.mdio_read(self.phy, MII_CTRL1000),
            self.mdio_read(self.phy, MII_STAT1000),
        ) {
            (Some(ours), Some(theirs)) => {
                // The far end's gigabit abilities arrive shifted two places
                // from where ours sit, which is how the register is laid out.
                let common = ours & (theirs >> 2);
                if common & ADVERTISE_1000FULL != 0 {
                    Some(true)
                } else if common & ADVERTISE_1000HALF != 0 {
                    Some(false)
                } else {
                    None
                }
            }
            _ => None,
        };
        if let Some(full_duplex) = gigabit {
            return Link { up: true, speed: 1000, full_duplex };
        }

        let ours = self.mdio_read(self.phy, MII_ADVERTISE).unwrap_or(0);
        let theirs = self.mdio_read(self.phy, MII_LPA).unwrap_or(0);
        let common = ours & theirs;
        if common & ADVERTISE_100FULL != 0 {
            Link { up: true, speed: 100, full_duplex: true }
        } else if common & ADVERTISE_100HALF != 0 {
            Link { up: true, speed: 100, full_duplex: false }
        } else if common & ADVERTISE_10FULL != 0 {
            Link { up: true, speed: 10, full_duplex: true }
        } else if common & ADVERTISE_10HALF != 0 {
            Link { up: true, speed: 10, full_duplex: false }
        } else {
            down
        }
    }

    /// Tell the controller what the link turned out to be, and let frames
    /// through. `bcmgenet_mac_config` in `bcmmii.c`.
    ///
    /// The speed in the command register is not only a bookkeeping field: it
    /// is what selects the transmit clock the RGMII block drives, 125 MHz for
    /// a gigabit link and 25 MHz for a hundred megabit one. Getting it wrong
    /// does not slow the link down, it corrupts every frame sent.
    fn apply_link(&self, link: Link) {
        if !link.up {
            self.modify(EXT_RGMII_OOB_CTRL, RGMII_LINK, 0);
            self.modify(UMAC_CMD, CMD_TX_EN | CMD_RX_EN, 0);
            return;
        }

        self.modify(EXT_RGMII_OOB_CTRL, 0, RGMII_LINK);

        let speed = match link.speed {
            1000 => CMD_SPEED_1000,
            100 => CMD_SPEED_100,
            _ => CMD_SPEED_10,
        };
        // Nothing here negotiates pause frames or would know what to do with
        // one, so both directions ignore them whatever the duplex is. Linux
        // decides this from what the two ends advertised.
        let mut set = (speed << CMD_SPEED_SHIFT) | CMD_RX_PAUSE_IGNORE | CMD_TX_PAUSE_IGNORE;
        if !link.full_duplex {
            set |= CMD_HD_EN;
        }
        // Promiscuous, because the stack decides what is addressed to it and
        // the controller's own destination filter is left switched off. See
        // `set_receive_filter`.
        set |= CMD_PROMISC | CMD_TX_EN | CMD_RX_EN;

        let clear = (CMD_SPEED_MASK << CMD_SPEED_SHIFT)
            | CMD_HD_EN
            | CMD_RX_PAUSE_IGNORE
            | CMD_TX_PAUSE_IGNORE
            | CMD_SW_RESET
            | CMD_LCL_LOOP_EN;
        self.modify(UMAC_CMD, clear, set);
    }

    pub fn link(&self) -> Link {
        *self.link.lock()
    }

    /// Look at the PHY and, if the link has changed, tell the controller.
    /// Called from the network task, which is ordinary task context: a
    /// management transfer takes tens of microseconds and has no business in
    /// an interrupt handler.
    pub fn poll_link(&self) {
        let now = self.read_link();
        let mut held = self.link.lock();
        if now == *held {
            return;
        }
        *held = now;
        drop(held);
        self.apply_link(now);
        if now.up {
            crate::println!(
                "genet: link up, {} Mb/s {}",
                now.speed,
                if now.full_duplex { "full duplex" } else { "half duplex" }
            );
        } else {
            crate::println!("genet: link down");
        }
    }

    // ---- bringing the controller up ------------------------------------

    /// The version in the top byte of the revision register, translated the
    /// way both references translate it: the part reports one more than the
    /// generation it belongs to from v4 on.
    fn version(&self) -> u32 {
        let major = (self.read(SYS_REV_CTRL) >> 24) & 0x0F;
        match major {
            6 => 5,
            5 => 4,
            0 => 1,
            other => other,
        }
    }

    /// Hold the MAC in reset and quiet the receive buffer, so that the
    /// management bus can be used while nothing is moving.
    /// u-boot's `bcmgenet_eth_probe`.
    fn hold_in_reset(&self) {
        self.write(SYS_RBUF_FLUSH_CTRL, 0);
        self.delay(10);
        self.write(UMAC_CMD, 0);
        self.write(UMAC_CMD, CMD_SW_RESET | CMD_LCL_LOOP_EN);
        self.delay(2);
    }

    /// Take the MAC through its reset and leave it configured but stopped.
    /// u-boot's `bcmgenet_umac_reset`, which is Linux's `bcmgenet_umac_reset`
    /// followed by the part of `init_umac` that is not about checksum offload
    /// or the status blocks.
    fn reset(&self) {
        // Bit 1 of the flush register holds the MAC in reset; raising and
        // lowering it is what takes it out.
        let flush = self.read(SYS_RBUF_FLUSH_CTRL);
        self.write(SYS_RBUF_FLUSH_CTRL, flush | (1 << 1));
        self.delay(10);
        self.write(SYS_RBUF_FLUSH_CTRL, flush & !(1 << 1));
        self.delay(10);
        self.write(SYS_RBUF_FLUSH_CTRL, 0);
        self.delay(10);

        self.write(UMAC_CMD, 0);
        self.write(UMAC_CMD, CMD_SW_RESET | CMD_LCL_LOOP_EN);
        self.delay(2);
        self.write(UMAC_CMD, 0);

        self.write(UMAC_MIB_CTRL, MIB_RESET_RX | MIB_RESET_TX | MIB_RESET_RUNT);
        self.write(UMAC_MIB_CTRL, 0);

        self.write(UMAC_MAX_FRAME_LEN, MAX_FRAME_LEN);

        // Two bytes in front of every received frame, and no status blocks in
        // either direction. Linux turns the status blocks on here because it
        // offloads checksums; u-boot does not and neither does this. Both
        // reference drivers leave the enables to whatever reset put there,
        // because each only ever sets bits; these two are cleared outright, so
        // that a reset value of one cannot turn a frame into sixty-four bytes
        // of status followed by a truncated frame.
        //
        // If the two-byte alignment is not what it is taken for, every
        // received frame is offset by two: the destination address reads as
        // the last four bytes of a real one, the stack's own filter throws all
        // of them away, and the interface looks dead in one direction while
        // transmitting perfectly.
        self.modify(RBUF_CTRL, RBUF_64B_EN, RBUF_ALIGN_2B);
        self.modify(TBUF_CTRL, TBUF_64B_EN, 0);
        self.write(RBUF_TBUF_SIZE_CTRL, 1);

        self.mask_interrupts();
    }

    fn mask_interrupts(&self) {
        self.write(INTRL2_0 + INTRL2_CPU_MASK_SET, 0xFFFF_FFFF);
        self.write(INTRL2_0 + INTRL2_CPU_CLEAR, 0xFFFF_FFFF);
        self.write(INTRL2_1 + INTRL2_CPU_MASK_SET, 0xFFFF_FFFF);
        self.write(INTRL2_1 + INTRL2_CPU_CLEAR, 0xFFFF_FFFF);
    }

    /// Write the hardware address where the controller keeps it: the first
    /// four bytes in one register and the last two in the next, each in the
    /// order they go on the wire, which puts the first byte in the top of the
    /// word. `bcmgenet_set_hw_addr`.
    fn set_hw_addr(&self, mac: &[u8; 6]) {
        self.write(UMAC_MAC0, u32::from_be_bytes([mac[0], mac[1], mac[2], mac[3]]));
        self.write(UMAC_MAC1, u16::from_be_bytes([mac[4], mac[5]]) as u32);
    }

    /// Leave the controller's destination-address filter switched off.
    ///
    /// This is the branch of `bcmgenet_set_rx_mode` that a promiscuous
    /// interface takes: no filter entries enabled and `CMD_PROMISC` set in the
    /// command register. The stack already discards frames that are not
    /// addressed to this machine, so the filter would only save interrupts,
    /// and a filter programmed wrongly drops every frame with nothing to show
    /// for it. u-boot never touches these registers either.
    fn set_receive_filter(&self) {
        self.write(UMAC_MDF_CTRL, 0);
        self.modify(UMAC_CMD, 0, CMD_PROMISC);
    }

    /// Point the port at an external gigabit PHY and set the RGMII block up
    /// for it. `bcmgenet_mii_config`.
    fn set_port_mode(&self) {
        self.write(SYS_PORT_CTRL, PORT_MODE_EXT_GPHY);

        // The out-of-band status input is not wired on this board, and the
        // RGMII block has to be switched on or the port does nothing at all.
        //
        // `ID_MODE_DIS` turns off the delay the controller puts on the
        // transmit clock. Linux clears it for `rgmii-rxid` -- the controller
        // delays, the PHY does not -- and sets it only for plain `rgmii`,
        // where the board's traces are meant to have done it. u-boot's driver
        // sets it for `rgmii-rxid` as well, which is the opposite, and the two
        // cannot both be right. Linux's reading is the one taken here, because
        // it is what every Pi 4 running Linux does.
        //
        // If it is the wrong one the link still comes up and negotiates 1000
        // Mb/s, and frames are then corrupt in one direction or both: receive
        // counters climbing with CRC errors, nothing getting through, and a
        // link forced to 100 Mb/s working perfectly, because the delay is
        // bypassed below a gigabit.
        let id_mode_dis = match self.mode {
            PhyMode::Rgmii => ID_MODE_DIS,
            _ => 0,
        };
        self.modify(EXT_RGMII_OOB_CTRL, OOB_DISABLE | ID_MODE_DIS, RGMII_MODE_EN | id_mode_dis);
    }

    /// Stop both engines and empty the transmit path.
    /// `bcmgenet_dma_disable`.
    fn disable_dma(&self) {
        self.modify(TDMA_CTRL + DMA_CTRL, DMA_EN, 0);
        self.modify(RDMA_CTRL + DMA_CTRL, DMA_EN, 0);
        self.write(UMAC_TX_FLUSH, 1);
        self.delay(10);
        self.write(UMAC_TX_FLUSH, 0);
    }

    fn enable_dma(&self) {
        let enable = (1 << (DEFAULT_RING + DMA_RING_BUF_EN_SHIFT)) | DMA_EN;
        self.modify(RDMA_CTRL + DMA_CTRL, 0, enable);
        self.modify(TDMA_CTRL + DMA_CTRL, 0, enable);
    }

    /// Point every receive descriptor at its buffer and set the ring's bounds.
    ///
    /// The producer index belongs to the device and a write to it does not
    /// necessarily take, so rather than assuming it is zero the kernel's
    /// consumer index is set to whatever the device's producer index actually
    /// reads back as, and the descriptor cursor is derived from that. u-boot
    /// does this and says outright that the register cannot be initialised;
    /// Linux writes zero to both and does not check. Doing both costs one read
    /// and is right either way.
    fn setup_rx(&self) {
        self.write(RDMA_CTRL + DMA_SCB_BURST_SIZE, DMA_MAX_BURST_LENGTH);

        for index in 0..RX_DESCS {
            let buffer = self.rx_buffers[index];
            // The buffer is about to be written by the device. Anything the
            // kernel has in cache for it -- it was just zeroed, so there is --
            // has to go out and be dropped, or a write-back later would land
            // on top of a received frame.
            arch::flush_data_cache(phys_to_virt(buffer), BUFFER_SIZE);
            let desc = rx_desc(index);
            self.write(desc + DESC_ADDRESS_LO, buffer as u32);
            self.write(desc + DESC_ADDRESS_HI, (buffer >> 32) as u32);
        }

        self.write(RDMA_RING + DMA_START_ADDR, ring_start_word(0));
        self.write(RDMA_RING + RDMA_READ_PTR, ring_start_word(0));
        self.write(RDMA_RING + RDMA_WRITE_PTR, ring_start_word(0));
        self.write(RDMA_RING + DMA_END_ADDR, ring_end_word(RX_DESCS));

        self.write(RDMA_RING + RDMA_PROD_INDEX, 0);
        self.write(RDMA_RING + RDMA_CONS_INDEX, 0);
        let producer = self.read(RDMA_RING + RDMA_PROD_INDEX) & INDEX_MASK;
        self.write(RDMA_RING + RDMA_CONS_INDEX, producer);
        self.rx.lock().consumer = producer;
        let first = slot(producer, RX_DESCS);
        self.write(RDMA_RING + RDMA_READ_PTR, ring_start_word(first));
        self.write(RDMA_RING + RDMA_WRITE_PTR, ring_start_word(first));

        self.write(
            RDMA_RING + DMA_RING_BUF_SIZE,
            ((RX_DESCS as u32) << DMA_RING_SIZE_SHIFT) | BUFFER_SIZE as u32,
        );
        self.write(
            RDMA_RING + RDMA_XON_XOFF_THRESH,
            (FC_THRESH_LO << XOFF_THRESHOLD_SHIFT) | FC_THRESH_HI,
        );
        // One descriptor's worth of work is enough to interrupt for, and no
        // timeout on top of it. `bcmgenet_set_rx_coalesce` with one frame and
        // no microseconds.
        self.write(RDMA_RING + DMA_MBUF_DONE_THRESH, 1);
        self.write(RDMA_CTRL + DMA_RING0_TIMEOUT + DEFAULT_RING * 4, 0);

        self.write(RDMA_CTRL + DMA_RING_CFG, 1 << DEFAULT_RING);
    }

    fn setup_tx(&self) {
        self.write(TDMA_CTRL + DMA_SCB_BURST_SIZE, DMA_MAX_BURST_LENGTH);

        for index in 0..TX_DESCS {
            let buffer = self.tx_buffers[index];
            let desc = tx_desc(index);
            self.write(desc + DESC_ADDRESS_LO, buffer as u32);
            self.write(desc + DESC_ADDRESS_HI, (buffer >> 32) as u32);
            self.write(desc + DESC_LENGTH_STATUS, 0);
        }

        self.write(TDMA_RING + DMA_START_ADDR, ring_start_word(0));
        self.write(TDMA_RING + TDMA_READ_PTR, ring_start_word(0));
        self.write(TDMA_RING + TDMA_WRITE_PTR, ring_start_word(0));
        self.write(TDMA_RING + DMA_END_ADDR, ring_end_word(TX_DESCS));

        // The consumer index is the device's here, so it is the one read back.
        self.write(TDMA_RING + TDMA_PROD_INDEX, 0);
        self.write(TDMA_RING + TDMA_CONS_INDEX, 0);
        let consumer = self.read(TDMA_RING + TDMA_CONS_INDEX) & INDEX_MASK;
        self.write(TDMA_RING + TDMA_PROD_INDEX, consumer);
        self.tx.lock().producer = consumer;
        let first = slot(consumer, TX_DESCS);
        self.write(TDMA_RING + TDMA_READ_PTR, ring_start_word(first));
        self.write(TDMA_RING + TDMA_WRITE_PTR, ring_start_word(first));

        self.write(TDMA_RING + DMA_MBUF_DONE_THRESH, 1);
        self.write(TDMA_RING + TDMA_FLOW_PERIOD, 0);
        self.write(
            TDMA_RING + DMA_RING_BUF_SIZE,
            ((TX_DESCS as u32) << DMA_RING_SIZE_SHIFT) | BUFFER_SIZE as u32,
        );

        self.write(TDMA_CTRL + DMA_RING_CFG, 1 << DEFAULT_RING);
    }

    // ---- moving frames -------------------------------------------------

    /// Move every completed frame off the receive ring into the queue.
    ///
    /// Called from the interrupt handler, so it copies and does nothing else.
    fn drain_rx(&self) -> usize {
        let mut rx = self.rx.lock();
        // The engine keeps a count of frames it had nowhere to put in the top
        // half of the same register the producer index is in, so the index has
        // to be masked out of it before it is compared with anything.
        let cursor = self.read(RDMA_RING + RDMA_PROD_INDEX);
        let producer = cursor & INDEX_MASK;
        DISCARDS.fetch_max(((cursor >> DISCARD_SHIFT) & INDEX_MASK) as u64, Ordering::Relaxed);
        let mut taken = 0;
        let mut ready = outstanding(producer, rx.consumer);
        // A ring cannot have more outstanding than it has descriptors; more
        // than that means the two cursors have lost each other, and walking
        // the difference would read descriptors the device is still writing.
        if ready as usize > RX_DESCS {
            ready = RX_DESCS as u32;
        }
        for _ in 0..ready {
            let index = slot(rx.consumer, RX_DESCS);
            let desc = rx_desc(index);
            // The descriptor is inside the device, so this is a register read
            // and there is nothing cached about it to worry over.
            let length_status = self.read(desc + DESC_LENGTH_STATUS);
            // The count includes the two alignment bytes in front of the
            // frame and excludes the Ethernet CRC, because `CMD_CRC_FWD` is
            // never set and the controller therefore strips it. Linux trims
            // four bytes here when it has asked for the CRC to be kept.
            let length = rx_length(length_status);
            let flags = length_status & 0xFFFF;

            let whole = flags & DMA_SOP != 0 && flags & DMA_EOP != 0;
            let sane = length >= RX_BUF_OFFSET + 14 && length <= BUFFER_SIZE;
            if whole && sane && flags & DMA_RX_ERRORS == 0 {
                let buffer = self.rx_buffers[index];
                // What the device wrote is in memory and not in the cache.
                // Everything cached over the buffer has to go before the
                // bytes are read, or the read is answered from a line fetched
                // before the transfer.
                arch::invalidate_data_cache(phys_to_virt(buffer), BUFFER_SIZE);
                let start = phys_to_virt(buffer) + RX_BUF_OFFSET as u64;
                let frame = unsafe {
                    core::slice::from_raw_parts(start as *const u8, length - RX_BUF_OFFSET)
                };
                QUEUE.lock().push(frame);
                taken += 1;
            }

            // Lend the descriptor back. The buffer address has not moved, and
            // the device does not rewrite it, so only the cursor advances.
            rx.consumer = rx.consumer.wrapping_add(1) & INDEX_MASK;
            self.write(RDMA_RING + RDMA_CONS_INDEX, rx.consumer);
        }
        taken
    }

    /// Acknowledge the controller's interrupt and move its work off the ring.
    /// `bcmgenet_isr0`: what is pending and not masked is what this interrupt
    /// is about, and writing it back to the clear register lowers the line.
    fn handle_interrupt(&self) {
        let pending = self.read(INTRL2_0 + INTRL2_CPU_STAT);
        let masked = self.read(INTRL2_0 + INTRL2_CPU_MASK_STATUS);
        let status = pending & !masked;
        if status == 0 {
            return;
        }
        self.write(INTRL2_0 + INTRL2_CPU_CLEAR, status);
        IRQ_COUNT.fetch_add(1, Ordering::Relaxed);

        if status & IRQ_RBUF_OVERFLOW != 0 {
            OVERRUNS.fetch_add(1, Ordering::Relaxed);
        }
        if status & IRQ_RXDMA_DONE != 0 && self.drain_rx() > 0 {
            RX_WAIT.wake_all();
        }
    }
}

impl Interface for Genet {
    fn mac(&self) -> [u8; 6] {
        self.mac
    }

    /// What the network task last read from the phy, which it asks every
    /// half second.
    fn link_up(&self) -> bool {
        self.link.lock().up
    }

    fn transmit(&self, frame: &[u8]) -> Result<(), Errno> {
        if frame.len() < 14 || frame.len() > BUFFER_SIZE {
            return Err(Errno::EINVAL);
        }
        if !self.link.lock().up {
            // Nothing can leave a port whose other end is not there, and the
            // descriptor would sit in the ring until it was.
            return Err(Errno::ENETDOWN);
        }

        let mut tx = self.tx.lock();
        // Wait for a descriptor to come free. The contract says `transmit` may
        // not sleep, so this is a spin, and the lock above has interrupts off
        // for the whole of it -- which is why the wait is short and the frame
        // is dropped rather than waited out. A ring of sixty-four only fills
        // if the stack has handed over sixty-four frames faster than the wire
        // takes them, and a dropped frame is what a full card does anyway.
        let started = arch::cycle_counter();
        let limit = counter_ticks(TX_WAIT_US);
        loop {
            let consumer = self.read(TDMA_RING + TDMA_CONS_INDEX) & INDEX_MASK;
            if (outstanding(tx.producer, consumer) as usize) < TX_DESCS {
                break;
            }
            if arch::cycle_counter().wrapping_sub(started) > limit {
                TX_BLOCKED.fetch_add(1, Ordering::Relaxed);
                return Err(Errno::EAGAIN);
            }
            core::hint::spin_loop();
        }

        let index = slot(tx.producer, TX_DESCS);
        let buffer = self.tx_buffers[index];
        let virt = phys_to_virt(buffer);
        unsafe {
            core::ptr::copy_nonoverlapping(frame.as_ptr(), virt as *mut u8, frame.len());
        }
        // The bytes have to be in memory before the device is told to fetch
        // them. The barrier at the end of this is also what orders the copy
        // above against the register writes below.
        arch::clean_data_cache(virt, frame.len());

        let desc = tx_desc(index);
        self.write(desc + DESC_ADDRESS_LO, buffer as u32);
        self.write(desc + DESC_ADDRESS_HI, (buffer >> 32) as u32);
        self.write(desc + DESC_LENGTH_STATUS, tx_length_status(frame.len()));

        tx.producer = tx.producer.wrapping_add(1) & INDEX_MASK;
        self.write(TDMA_RING + TDMA_PROD_INDEX, tx.producer);
        Ok(())
    }
}

/// How long `transmit` waits for a descriptor before dropping the frame. Two
/// milliseconds is long enough for a gigabit link to empty a full ring twice
/// over and short enough not to hold the timer off if it never does.
const TX_WAIT_US: u64 = 2_000;

// ---------------------------------------------------------------------------
// Bring-up
// ---------------------------------------------------------------------------

/// Take the hardware address out of the node the firmware wrote it into.
///
/// Three spellings, in the order `fwnode_get_mac_address` tries them. A Pi's
/// firmware writes `local-mac-address` into the Ethernet node as it hands the
/// tree over, and it is the only place the board's real address exists: it is
/// derived from the serial number burned into the chip, and there is nothing
/// in the controller to read it back from.
fn mac_from(node: &arch::fdt::Node) -> Option<[u8; 6]> {
    for name in [b"mac-address".as_slice(), b"local-mac-address".as_slice(), b"address".as_slice()]
    {
        if let Some(value) = node.property(name) {
            if value.len() == 6 {
                let mut mac = [0u8; 6];
                mac.copy_from_slice(value);
                // All zeroes is a placeholder rather than an address, and a
                // multicast bit in the first byte is not a station address at
                // all.
                if mac != [0; 6] && mac[0] & 1 == 0 {
                    return Some(mac);
                }
            }
        }
    }
    None
}

fn phy_mode_from(node: &arch::fdt::Node) -> Option<PhyMode> {
    let value = node.property(b"phy-mode").or_else(|| node.property(b"phy-connection-type"))?;
    let value = match value.iter().position(|&b| b == 0) {
        Some(at) => &value[..at],
        None => value,
    };
    match value {
        b"rgmii" => Some(PhyMode::Rgmii),
        b"rgmii-rxid" => Some(PhyMode::RgmiiRxId),
        b"rgmii-txid" => Some(PhyMode::RgmiiTxId),
        b"rgmii-id" => Some(PhyMode::RgmiiId),
        _ => None,
    }
}

/// Where the PHY answers on the management bus, from the node the Ethernet
/// node points at. A Pi 4 says address one. There is no fallback scan: an
/// address guessed wrong talks to nothing, and a tree that does not say is a
/// tree this driver has not been written for.
fn phy_address_from(node: &arch::fdt::Node) -> Option<u8> {
    let handle = node.cell(b"phy-handle")?;
    let phy = arch::fdt::find_phandle(handle)?;
    let address = phy.bus_address(0)?;
    if address > 31 {
        return None;
    }
    Some(address as u8)
}

/// Allocate `count` packet buffers and give back their physical addresses.
///
/// One page holds two buffers exactly, so this asks for pages one at a time
/// rather than for one run of contiguous memory: a buffer only has to be
/// contiguous in itself, and each is named to the device separately.
pub(crate) fn allocate_buffers(count: usize) -> Option<Vec<u64>> {
    let per_page = PAGE_SIZE / BUFFER_SIZE;
    let mut buffers = Vec::with_capacity(count);
    let mut left = count;
    while left > 0 {
        let page = alloc_contiguous(1)?;
        unsafe { core::ptr::write_bytes(phys_to_virt(page) as *mut u8, 0, PAGE_SIZE) };
        for slot in 0..per_page.min(left) {
            buffers.push(page + (slot * BUFFER_SIZE) as u64);
        }
        left = left.saturating_sub(per_page);
    }
    Some(buffers)
}

fn format_mac(mac: &[u8; 6]) -> alloc::string::String {
    use core::fmt::Write;
    let mut out = alloc::string::String::new();
    for (i, byte) in mac.iter().enumerate() {
        let _ = write!(out, "{}{:02x}", if i == 0 { "" } else { ":" }, byte);
    }
    out
}

/// Find the controller, bring it up and register it with the stack.
///
/// Returns false when there is nothing to bring up, which is what happens on
/// every emulated run: QEMU's Pi 4 has no Ethernet, and when it is handed a
/// real board's device tree it takes the node out rather than pretending.
/// Every way of failing here leaves the kernel booting normally without a
/// network.
///
/// Like the e1000's, this runs before the first user address space exists.
pub fn probe() -> bool {
    let Some(node) = arch::fdt::find_compatible(COMPATIBLE) else {
        if arch::fdt::blob().is_none() {
            crate::println!("genet: no device tree, so nothing says where the controller is");
        } else {
            crate::println!("genet: no \"brcm,bcm2711-genet-v5\" node; this board has no genet");
        }
        return false;
    };
    if !node.enabled() {
        crate::println!("genet: the device tree marks the controller disabled; leaving it alone");
        return false;
    }

    let Some((base, size)) = node.reg(0) else {
        crate::println!("genet: the node has no address the processor can reach; giving up");
        return false;
    };
    // The direct map's last gigabyte is the one with device attributes, which
    // is where every peripheral on this chip lives. Anything outside it would
    // need a mapping of its own, and something that claims to be this
    // controller and is not there is worth refusing rather than poking.
    if base < arch::DEVICE_PHYS_BASE || base + size > arch::HHDM_LIMIT {
        crate::println!(
            "genet: registers at {:#x}..{:#x} are outside the device window; giving up",
            base,
            base + size
        );
        return false;
    }
    let Some(irq) = node.interrupt(0) else {
        crate::println!("genet: the node names no interrupt; giving up");
        return false;
    };
    let Some(mac) = mac_from(&node) else {
        // Inventing one would put a duplicate address on the network, so this
        // is a refusal rather than a fallback.
        crate::println!(
            "genet: the device tree carries no hardware address for this board; giving up"
        );
        return false;
    };
    let Some(mode) = phy_mode_from(&node) else {
        crate::println!("genet: the node's phy-mode is not one this driver knows; giving up");
        return false;
    };
    let Some(phy) = phy_address_from(&node) else {
        crate::println!("genet: the node does not say where the phy is; giving up");
        return false;
    };
    let coherent = node.dma_coherent();

    crate::println!(
        "genet: registers at {:#x} ({} KiB), irq {}, {}, phy at {}",
        base,
        size / 1024,
        irq,
        format_mac(&mac),
        phy
    );

    let Some(rx_buffers) = allocate_buffers(RX_DESCS) else {
        crate::println!("genet: cannot allocate receive buffers");
        return false;
    };
    let Some(tx_buffers) = allocate_buffers(TX_DESCS) else {
        crate::println!("genet: cannot allocate transmit buffers");
        return false;
    };

    let card: &'static Genet = Box::leak(Box::new(Genet {
        regs: phys_to_virt(base),
        mac,
        irq,
        phy,
        mode,
        coherent,
        rx_buffers,
        tx_buffers,
        rx: Spinlock::new(RxRing { consumer: 0 }),
        tx: Spinlock::new(TxRing { producer: 0 }),
        link: Spinlock::new(Link { up: false, speed: 0, full_duplex: false }),
    }));

    // Nothing below here means anything if this is not the part the tree said
    // it was. A read that answers zero is what an address with nothing behind
    // it looks like, which is what an emulator gives.
    let version = card.version();
    if version != 5 {
        crate::println!("genet: the part at {:#x} reports version {}, not 5; giving up", base, version);
        return false;
    }
    crate::println!(
        "genet: v5, transfers are {} with the caches",
        if coherent { "coherent" } else { "not coherent" }
    );

    card.set_port_mode();
    card.hold_in_reset();

    // The management bus works while the MAC is held in reset, and this is
    // where the reference drivers use it: the PHY has half a second of reset
    // and a second or two of negotiation ahead of it, and starting it now
    // means both happen while the rings are being built.
    let id = match (card.mdio_read(phy, MII_PHYSID1), card.mdio_read(phy, MII_PHYSID2)) {
        (Some(high), Some(low)) => ((high as u32) << 16) | low as u32,
        _ => 0,
    };
    if id == 0 || id == 0xFFFF_FFFF {
        crate::println!("genet: nothing answers at phy address {}; giving up", phy);
        return false;
    }
    // The part fitted to a Pi 4 is a BCM54213PE, which identifies itself as
    // 0x600d84a2 with the bottom four bits holding the revision.
    crate::println!("genet: phy identifies as {:#010x}", id);
    card.start_phy();

    card.reset();
    card.set_hw_addr(&mac);
    card.set_receive_filter();

    card.disable_dma();
    card.setup_rx();
    card.setup_tx();
    card.enable_dma();

    {
        let mut queue = QUEUE.lock();
        queue.slots = Vec::with_capacity(QUEUE_SLOTS);
        for _ in 0..QUEUE_SLOTS {
            queue.slots.push(Vec::with_capacity(BUFFER_SIZE));
        }
    }

    *DEVICE.lock() = Some(card);

    // Only now may the controller interrupt: the ring, the queue and the
    // handler's view of the device are all in place.
    //
    // Transmit completions are not asked for, because `transmit` reads the
    // consumer index itself. That takes the index to be a counter the engine
    // keeps whether or not anyone is listening, which is how both reference
    // drivers read it -- u-boot polls exactly this register with no interrupt
    // handler at all. If it were only updated on an acknowledged interrupt
    // instead, the first sixty-four frames would go out and every one after
    // that would be refused.
    //
    // The line number is the one the tree gives plus thirty-two, because the
    // tree numbers shared interrupts from the start of their own group. If
    // that were wrong nothing would ever be received: frames would pile up in
    // the ring until the engine stopped, while transmitting carried on.
    arch::register_irq_handler(card.irq, interrupt);
    card.write(INTRL2_0 + INTRL2_CPU_MASK_CLEAR, IRQ_RXDMA_DONE | IRQ_RBUF_OVERFLOW);
    arch::unmask_irq(card.irq);

    // The link is not waited for. Negotiation takes a second or two and this
    // runs before there is a process to run, so it would be a second or two of
    // a blank console; the network task looks at the phy every half second and
    // says when it settles.
    crate::println!("genet: up, waiting for the link to negotiate");
    crate::net::attach(card);
    true
}

/// The controller's interrupt.
fn interrupt(frame: &mut TrapFrame) {
    let Some(irq) = arch::vector_irq(arch::trap_vector(frame)) else { return };
    if let Some(card) = device() {
        card.handle_interrupt();
    }
    arch::end_of_interrupt(irq);
}

// ---------------------------------------------------------------------------
// The network task
// ---------------------------------------------------------------------------

/// Start the kernel task that runs the protocol half. After the init process,
/// because process ids are handed out in order and a great deal of the system
/// takes pid 1 to be init.
pub fn start_task() {
    let Some(mut task) = Task::new("netd", None) else {
        crate::println!("genet: cannot create the network task");
        return;
    };
    task.prepare_kernel_frame(net_task as extern "C" fn() -> ! as usize as u64);
    let pid = sched::register(task);
    crate::println!("genet: network task is pid {}", pid);
}

/// Ticks between looks at the phy. The link changes when a cable is moved, so
/// half a second is as often as it is worth asking, and each ask is a handful
/// of transfers on a 2.5 MHz bus.
const LINK_POLL_TICKS: u64 = 50;

extern "C" fn net_task() -> ! {
    let mut scratch: Vec<u8> = Vec::with_capacity(BUFFER_SIZE);
    let mut last_tick = crate::trap::ticks();
    let mut last_link = last_tick;
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
            crate::net::receive(&scratch);
        }

        let now = crate::trap::ticks();
        if now != last_tick {
            let missed = (now - last_tick).min(8);
            for _ in 0..missed {
                crate::net::tick();
            }
            last_tick = now;
        }
        if now.wrapping_sub(last_link) >= LINK_POLL_TICKS {
            last_link = now;
            if let Some(card) = device() {
                card.poll_link();
            }
        }
    }
}

fn log_frame(frame: &[u8]) {
    let mut dst = [0u8; 6];
    let mut src = [0u8; 6];
    dst.copy_from_slice(&frame[0..6]);
    src.copy_from_slice(&frame[6..12]);
    let ethertype = u16::from_be_bytes([frame[12], frame[13]]);
    crate::println!(
        "genet: [t={}] rx {} bytes {} -> {} type {:#06x}",
        crate::trap::ticks(),
        frame.len(),
        format_mac(&src),
        format_mac(&dst),
        ethertype
    );
}
