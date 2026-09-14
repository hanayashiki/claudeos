//! What can be checked about the GENET driver with no GENET present.
//!
//! The controller is on a Raspberry Pi 4 and nothing emulates it, so the
//! driver's first run will be on hardware nobody is watching. Everything that
//! does not need the device is therefore checked here instead: the device tree
//! walk and the three things taken out of it, the arithmetic that turns a ring
//! index into a descriptor, the packing of a descriptor's length-and-status
//! word, and the cache maintenance the buffers depend on.
//!
//! The tree walked is built here rather than taken from the machine, because
//! the machine this runs on has no Ethernet node to walk. It is the shape of a
//! Pi 4's: a bus node between the root and the controller whose `ranges` moves
//! the controller's address, cell counts that differ from level to level, and
//! the PHY two levels further down on a bus that is not memory at all. The
//! numbers asserted below are the real board's -- 0xfd580000, interrupt 157 of
//! the shared group, a PHY at management address one -- so a walk that gets
//! them right gets the real tree right.
//!
//! Run with `net=test` on the kernel command line, alongside the protocol
//! checks.

use crate::arch;
use crate::mm::frame::alloc_contiguous;
use crate::mm::{phys_to_virt, PAGE_SIZE};
use alloc::vec::Vec;

pub struct Report {
    pub passed: usize,
    pub failed: usize,
}

impl Report {
    pub(crate) fn check(&mut self, name: &str, holds: bool) {
        if holds {
            self.passed += 1;
            crate::println!("  ok    {}", name);
        } else {
            self.failed += 1;
            crate::println!("  FAIL  {}", name);
        }
    }

    pub(crate) fn value(&mut self, name: &str, actual: u64, expected: u64) {
        if actual == expected {
            self.passed += 1;
            crate::println!("  ok    {} = {:#x}", name, actual);
        } else {
            self.failed += 1;
            crate::println!("  FAIL  {}: got {:#x}, wanted {:#x}", name, actual, expected);
        }
    }
}

// ---------------------------------------------------------------------------
// Building a flattened device tree to walk
// ---------------------------------------------------------------------------

const MAGIC: u32 = 0xD00D_FEED;
const BEGIN_NODE: u32 = 1;
const END_NODE: u32 = 2;
const PROP: u32 = 3;
const END: u32 = 9;
/// The version of the format this writes. 17 is what every tool has emitted
/// for many years and what the reader expects.
const VERSION: u32 = 17;
const LAST_COMPATIBLE_VERSION: u32 = 16;

/// Assembles the two blocks a blob is made of: a stream of tokens, and a table
/// of property names the tokens point into by offset.
///
/// A blob written and then read by the same understanding of the format would
/// agree with itself whatever that understanding was, so what this writes was
/// compared against what `dtc` compiles from the equivalent source text. The
/// two are byte for byte the same, which makes the tree below a real one and
/// not a private encoding that only this kernel can read.
pub(crate) struct Builder {
    structure: Vec<u8>,
    strings: Vec<u8>,
}

impl Builder {
    pub(crate) fn new() -> Builder {
        Builder { structure: Vec::new(), strings: Vec::new() }
    }

    pub(crate) fn be32(&mut self, value: u32) {
        self.structure.extend_from_slice(&value.to_be_bytes());
    }

    /// Pad the token stream out to the next four-byte boundary, which every
    /// token has to start on.
    pub(crate) fn pad(&mut self) {
        while self.structure.len() % 4 != 0 {
            self.structure.push(0);
        }
    }

    /// Where `name` sits in the string table, adding it if it is not there
    /// already.
    pub(crate) fn intern(&mut self, name: &str) -> u32 {
        let wanted = name.as_bytes();
        let mut at = 0usize;
        while at < self.strings.len() {
            let end = at + self.strings[at..].iter().position(|&b| b == 0).unwrap_or(0);
            if &self.strings[at..end] == wanted {
                return at as u32;
            }
            at = end + 1;
        }
        let offset = self.strings.len() as u32;
        self.strings.extend_from_slice(wanted);
        self.strings.push(0);
        offset
    }

    pub(crate) fn begin_node(&mut self, name: &str) {
        self.be32(BEGIN_NODE);
        self.structure.extend_from_slice(name.as_bytes());
        self.structure.push(0);
        self.pad();
    }

    pub(crate) fn end_node(&mut self) {
        self.be32(END_NODE);
    }

    pub(crate) fn prop(&mut self, name: &str, value: &[u8]) {
        let offset = self.intern(name);
        self.be32(PROP);
        self.be32(value.len() as u32);
        self.be32(offset);
        self.structure.extend_from_slice(value);
        self.pad();
    }

    pub(crate) fn prop_cells(&mut self, name: &str, cells: &[u32]) {
        let mut bytes = Vec::with_capacity(cells.len() * 4);
        for cell in cells {
            bytes.extend_from_slice(&cell.to_be_bytes());
        }
        self.prop(name, &bytes);
    }

    pub(crate) fn prop_u32(&mut self, name: &str, value: u32) {
        self.prop_cells(name, &[value]);
    }

    /// A property whose value is text. The terminator is part of the value.
    pub(crate) fn prop_str(&mut self, name: &str, value: &str) {
        let mut bytes = Vec::with_capacity(value.len() + 1);
        bytes.extend_from_slice(value.as_bytes());
        bytes.push(0);
        self.prop(name, &bytes);
    }

    /// Wrap the two blocks in a header and an empty reserved-memory list.
    pub(crate) fn finish(mut self) -> Vec<u8> {
        self.be32(END);
        let header = 40usize;
        // The reserved-memory list is eight-byte aligned and ends with an
        // entry of two zeroes.
        let reserve = (header + 7) & !7;
        let struct_at = reserve + 16;
        let strings_at = struct_at + self.structure.len();
        let total = strings_at + self.strings.len();

        let mut blob = Vec::with_capacity(total);
        blob.extend_from_slice(&MAGIC.to_be_bytes());
        blob.extend_from_slice(&(total as u32).to_be_bytes());
        blob.extend_from_slice(&(struct_at as u32).to_be_bytes());
        blob.extend_from_slice(&(strings_at as u32).to_be_bytes());
        blob.extend_from_slice(&(reserve as u32).to_be_bytes());
        blob.extend_from_slice(&VERSION.to_be_bytes());
        blob.extend_from_slice(&LAST_COMPATIBLE_VERSION.to_be_bytes());
        blob.extend_from_slice(&0u32.to_be_bytes()); // boot cpu
        blob.extend_from_slice(&(self.strings.len() as u32).to_be_bytes());
        blob.extend_from_slice(&(self.structure.len() as u32).to_be_bytes());
        while blob.len() < struct_at {
            blob.push(0);
        }
        blob.extend_from_slice(&self.structure);
        blob.extend_from_slice(&self.strings);
        blob
    }
}

/// The handle the Ethernet node points at the PHY with. Any number will do;
/// this is the one a Pi 4's own tree happens to use.
const PHY_HANDLE: u32 = 0x40;
const SAMPLE_MAC: [u8; 6] = [0xdc, 0xa6, 0x32, 0x01, 0x02, 0x03];

/// A tree shaped like a Pi 4's, as far as this driver looks into it.
///
/// `status` is what the caller varies: QEMU writes "disabled" into this node
/// when it is handed a real board's tree, because it emulates no such device,
/// and the driver has to notice.
fn sample_tree(status: &str, with_ethernet: bool) -> Vec<u8> {
    let mut tree = Builder::new();
    tree.begin_node("");
    tree.prop_u32("#address-cells", 2);
    // One cell for a size at the root and two below it, so a walk that takes
    // the cell counts from the wrong level reads the wrong words.
    tree.prop_u32("#size-cells", 1);

    tree.begin_node("scb");
    tree.prop_u32("#address-cells", 2);
    tree.prop_u32("#size-cells", 2);
    // The four windows a Pi 4's bus node declares. The first is the one that
    // matters: everything from 0x7c000000 for 56 MiB appears to the processor
    // 0x80000000 higher up.
    tree.prop_cells(
        "ranges",
        &[
            0x0, 0x7c00_0000, 0x0, 0xfc00_0000, 0x0, 0x0380_0000, //
            0x0, 0x4000_0000, 0x0, 0xff80_0000, 0x0, 0x0080_0000, //
            0x6, 0x0, 0x6, 0x0, 0x0, 0x4000_0000, //
            0x0, 0x0, 0x0, 0x0, 0x0, 0xfc00_0000,
        ],
    );

    if with_ethernet {
        tree.begin_node("ethernet@7d580000");
        tree.prop_str("compatible", "brcm,bcm2711-genet-v5");
        tree.prop_cells("reg", &[0x0, 0x7d58_0000, 0x0, 0x0001_0000]);
        tree.prop_u32("#address-cells", 1);
        tree.prop_u32("#size-cells", 1);
        // Two entries, each of a kind, a number and a trigger. Zero is the
        // shared group, whose numbering starts at 32.
        tree.prop_cells("interrupts", &[0, 157, 4, 0, 158, 4]);
        tree.prop_str("status", status);
        tree.prop_u32("phy-handle", PHY_HANDLE);
        tree.prop_str("phy-mode", "rgmii-rxid");
        tree.prop("local-mac-address", &SAMPLE_MAC);

        tree.begin_node("mdio@e14");
        tree.prop_cells("reg", &[0xe14, 0x8]);
        tree.prop_u32("#address-cells", 1);
        // No sizes on this bus, and no `ranges`: a station on it has a number
        // and not an address, and there is nothing to translate it into.
        tree.prop_u32("#size-cells", 0);

        tree.begin_node("ethernet-phy@1");
        tree.prop_u32("reg", 1);
        tree.prop_u32("phandle", PHY_HANDLE);
        tree.end_node();

        tree.end_node();
        tree.end_node();
    }

    tree.end_node();
    tree.end_node();
    tree.finish()
}

/// Put a blob somewhere the reader can reach it.
///
/// The reader takes a physical address and walks upwards from it, so the blob
/// has to be contiguous in physical memory; a heap allocation is contiguous in
/// virtual memory and need not be. One page is more than any of these needs.
pub(crate) fn place(blob: &[u8]) -> Option<u64> {
    if blob.len() > PAGE_SIZE {
        return None;
    }
    let page = alloc_contiguous(1)?;
    unsafe {
        core::ptr::write_bytes(phys_to_virt(page) as *mut u8, 0, PAGE_SIZE);
        core::ptr::copy_nonoverlapping(blob.as_ptr(), phys_to_virt(page) as *mut u8, blob.len());
    }
    Some(page)
}

// ---------------------------------------------------------------------------
// The checks
// ---------------------------------------------------------------------------

pub fn run(report: &mut Report) {
    device_tree(report);
    real_device_tree(report);
    registers(report);
    rings(report);
    descriptors(report);
    cache(report);
    buffers(report);
}

/// Everything the driver takes out of the tree, against a tree shaped like the
/// board's.
fn device_tree(report: &mut Report) {
    let blob = sample_tree("okay", true);
    let Some(phys) = place(&blob) else {
        report.check("a tree to walk", false);
        return;
    };

    let Some(node) = arch::fdt::find_compatible_in(phys, b"brcm,bcm2711-genet-v5") else {
        report.check("the ethernet node is found", false);
        return;
    };
    report.check("the ethernet node is found", true);

    // The whole point of the walk: the number in the node is 0x7d580000 and
    // the bus above it puts the controller 0x80000000 higher. Reading the
    // untranslated number would leave the driver writing to ordinary memory a
    // gigabyte lower down.
    match node.reg(0) {
        Some((base, size)) => {
            report.value("the registers, translated", base, 0xFD58_0000);
            report.value("how much of them", size, 0x1_0000);
        }
        None => report.check("the registers, translated", false),
    }

    report.check("the node is enabled", node.enabled());
    // Shared interrupts are numbered from 32, so 157 in the tree is line 189.
    report.value("the first interrupt line", node.interrupt(0).unwrap_or(0) as u64, 189);
    report.value("the second interrupt line", node.interrupt(1).unwrap_or(0) as u64, 190);
    report.check("there is no third", node.interrupt(2).is_none());
    report.check("transfers are not coherent", !node.dma_coherent());

    match node.property(b"local-mac-address") {
        Some(mac) => report.check("the hardware address", mac == SAMPLE_MAC),
        None => report.check("the hardware address", false),
    }
    match node.property(b"phy-mode") {
        Some(mode) => report.check("how the port is wired", mode == b"rgmii-rxid\0"),
        None => report.check("how the port is wired", false),
    }
    report.check("a property the node does not have", node.property(b"nonesuch").is_none());
    // A property of a child node must not be visible on the parent, or the
    // walk has run past the end of the node's own properties.
    report.check("no property from inside a child", node.property(b"phandle").is_none());

    // The phy is two levels down, on a bus that is not memory.
    match node.cell(b"phy-handle").and_then(|h| arch::fdt::find_phandle_in(phys, h)) {
        Some(phy) => {
            report.value("where the phy answers", phy.bus_address(0).unwrap_or(99), 1);
            report.check(
                "a management address is not a processor address",
                phy.reg(0).is_none(),
            );
        }
        None => {
            report.check("where the phy answers", false);
            report.check("a management address is not a processor address", false);
        }
    }

    // The two ways there is nothing to drive.
    let disabled = sample_tree("disabled", true);
    match place(&disabled).and_then(|p| arch::fdt::find_compatible_in(p, b"brcm,bcm2711-genet-v5"))
    {
        Some(node) => report.check("a disabled node says so", !node.enabled()),
        None => report.check("a disabled node says so", false),
    }
    let without = sample_tree("okay", false);
    match place(&without) {
        Some(p) => report.check(
            "a tree without the node finds nothing",
            arch::fdt::find_compatible_in(p, b"brcm,bcm2711-genet-v5").is_none(),
        ),
        None => report.check("a tree without the node finds nothing", false),
    }
    report.check(
        "so does a blob that is not one",
        arch::fdt::find_compatible_in(0, b"brcm,bcm2711-genet-v5").is_none(),
    );
}

/// The same walk over the tree the machine was actually booted with, against
/// something in it this kernel already knows the answer for.
///
/// The tree built above is a small one and the walk over it could be right for
/// small trees and wrong for the sixty kilobytes of nodes a firmware hands
/// over. The console's registers and interrupt are in that tree, and the
/// kernel knows both from the chip rather than from the tree, so the two can
/// be held up against each other. The console is on a different bus from the
/// Ethernet and is translated through a different set of windows, which makes
/// it a second case rather than the same one twice.
///
/// Nothing runs here when the machine was booted without a tree, which is what
/// QEMU's Pi 4 does.
fn real_device_tree(report: &mut Report) {
    if arch::fdt::blob().is_none() {
        crate::println!("  --    no device tree on this machine, so nothing to walk");
        return;
    }
    let Some(node) = arch::fdt::find_compatible(b"arm,pl011") else {
        report.check("the console is in the real tree", false);
        return;
    };
    report.check("the console is in the real tree", true);
    match node.reg(0) {
        Some((base, _)) => {
            report.value("and at the address the console driver uses", base, arch::CONSOLE_PHYS)
        }
        None => report.check("and at the address the console driver uses", false),
    }
    report.value(
        "and on the line the interrupt table names",
        node.interrupt(0).unwrap_or(0) as u64,
        arch::SERIAL_IRQ as u64,
    );
}

/// The register map, which is arithmetic on top of the block offsets rather
/// than a table of constants, so it is worth pinning to the numbers the
/// reference driver computes.
fn registers(report: &mut Report) {
    use super::genet::{
        rx_desc, tx_desc, RDMA_CTRL, RDMA_RING, TDMA_CTRL, TDMA_RING, UMAC_MDIO_CMD,
    };
    // 0x2000 plus 256 descriptors of twelve bytes each, then sixteen unused
    // rings of 0x40 before the one this driver uses.
    report.value("receive ring registers", RDMA_RING as u64, 0x3000);
    report.value("transmit ring registers", TDMA_RING as u64, 0x5000);
    // The seventeen ring blocks, then the engine's own registers.
    report.value("receive engine control", RDMA_CTRL as u64, 0x3040);
    report.value("transmit engine control", TDMA_CTRL as u64, 0x5040);
    report.value("the management bus", UMAC_MDIO_CMD as u64, 0xE14);
    // Everything touched has to be inside the window the tree gives, which is
    // 64 KiB.
    report.check("all inside the register window", TDMA_CTRL + 0x90 < 0x1_0000);

    report.value("descriptor zero", rx_desc(0) as u64, 0x2000);
    report.value("the last receive descriptor", rx_desc(255) as u64, 0x2000 + 255 * 12);
    report.value("the last transmit descriptor", tx_desc(255) as u64, 0x4000 + 255 * 12);
    report.check("descriptors stop short of the rings", rx_desc(256) == 0x2C00);
}

/// The two cursors, which count frames modulo 65536 while the ring holds a
/// power of two descriptors.
fn rings(report: &mut Report) {
    use super::genet::{outstanding, ring_end_word, ring_start_word, slot};

    report.value("index 0 is descriptor 0", slot(0, 256) as u64, 0);
    report.value("index 255 is descriptor 255", slot(255, 256) as u64, 255);
    report.value("index 256 is descriptor 0 again", slot(256, 256) as u64, 0);
    report.value("a short ring wraps sooner", slot(70, 64) as u64, 6);
    // The index wraps at 65536 and the ring at its own size; the two have to
    // agree at the moment the index wraps, which they do because the size
    // divides 65536.
    report.value("the last index before the wrap", slot(65535, 64) as u64, 63);
    report.value("and the first after it", slot(0, 64) as u64, 0);

    report.value("nothing outstanding", outstanding(7, 7) as u64, 0);
    report.value("three outstanding", outstanding(10, 7) as u64, 3);
    // The producer has gone past 65535 and the consumer has not. Subtracting
    // without the wrap would say 65533 frames are waiting and walk the ring
    // 65533 times.
    report.value("three outstanding across the wrap", outstanding(2, 65535) as u64, 3);
    report.value("a full ring of 64", outstanding(64, 0) as u64, 64);

    // The ring's bounds are in 32-bit words and the end is the last word, not
    // one past it: 256 descriptors of three words is 768 words, so 767.
    report.value("the ring starts at word", ring_start_word(0) as u64, 0);
    report.value("descriptor 16 starts at word", ring_start_word(16) as u64, 48);
    report.value("a 256 descriptor ring ends at word", ring_end_word(256) as u64, 767);
    report.value("a 64 descriptor ring ends at word", ring_end_word(64) as u64, 191);
}

/// The one word the driver writes into a descriptor and the one it reads back.
fn descriptors(report: &mut Report) {
    use super::genet::{rx_length, tx_length_status};

    // A 60 byte frame. The length sits in the top half, so 0x003c in the top
    // and, in the bottom, the queue tag 0x3f shifted up seven to 0x1f80, the
    // append-a-CRC bit 0x0040, and the start and end of packet bits 0x2000 and
    // 0x4000: 0x7fc0 together.
    let word = tx_length_status(60);
    report.value("a transmit descriptor's word", word as u64, 0x003C_7FC0);
    report.value("the length it carries", (word >> 16) as u64, 60);
    report.check("it appends the crc", word & 0x0040 != 0);
    report.check("it is the start of the packet", word & 0x2000 != 0);
    report.check("it is the end of the packet", word & 0x4000 != 0);
    report.value("the queue tag", ((word >> 7) & 0x3F) as u64, 0x3F);

    // The largest frame that can be sent, to show the length does not spill
    // out of the twelve bits it has.
    report.value("the length of a full frame", (tx_length_status(1536) >> 16) as u64, 1536);

    // Reading back: the status half is ignored and only the length comes out.
    report.value("a received length", rx_length(0x0040_2000 | 0x6000) as u64, 64);
    report.value("a received length of 1518", rx_length(0x05EE_6000) as u64, 1518);
}

/// The cache maintenance the buffers depend on.
///
/// What can be asserted without a device is the part that is easy to get
/// wrong: invalidating a range that does not start and end on a cache line
/// boundary must not throw away the bytes on either side of it. A plain
/// invalidate of the two end lines would, and the data lost would belong to
/// whatever happened to be next to the buffer in memory.
///
/// The other direction cannot be asserted here. Emulation models no caches, so
/// a line that should have been dropped reads the same either way, and a check
/// that the invalidated bytes came back from memory would pass on the board
/// and fail under QEMU. This one passes under both and only fails on a board
/// where the maintenance is wrong.
fn cache(report: &mut Report) {
    let Some(page) = alloc_contiguous(1) else {
        report.check("a page to scribble on", false);
        return;
    };
    let virt = phys_to_virt(page);
    let bytes = unsafe { core::slice::from_raw_parts_mut(virt as *mut u8, PAGE_SIZE) };

    // Put one pattern in memory, then a different one in the cache on top of
    // it. Whether the second is still only in the cache is not knowable from
    // here, which is the point: the check has to hold either way.
    bytes.fill(0xAA);
    arch::flush_data_cache(virt, PAGE_SIZE);
    bytes.fill(0x55);

    // A range starting and ending in the middle of a line, on any line length
    // a processor has.
    const START: usize = 1000;
    const LEN: usize = 1000;
    arch::invalidate_data_cache(virt + START as u64, LEN);

    let before = bytes[..START].iter().all(|&b| b == 0x55);
    let after = bytes[START + LEN..].iter().all(|&b| b == 0x55);
    report.check("invalidating keeps what is in front of the range", before);
    report.check("invalidating keeps what is behind it", after);

    // Cleaning is asked for over a range whose ends are as awkward, and over
    // one of no length at all, which has to do nothing rather than run away.
    arch::clean_data_cache(virt + 1, PAGE_SIZE - 2);
    arch::clean_data_cache(virt, 0);
    arch::invalidate_data_cache(virt, 0);
    arch::flush_data_cache(virt + 7, 1);
    report.check("cleaning leaves the bytes alone", bytes.iter().all(|&b| b == 0x55));
}

/// The packet buffers, which are cut two to a page.
fn buffers(report: &mut Report) {
    let Some(buffers) = super::genet::allocate_buffers(5) else {
        report.check("buffers are allocated", false);
        return;
    };
    report.value("as many as asked for", buffers.len() as u64, 5);
    report.check(
        "each starts on a buffer boundary",
        buffers.iter().all(|&b| b % 2048 == 0),
    );
    let mut distinct = true;
    for (i, &a) in buffers.iter().enumerate() {
        for &b in &buffers[i + 1..] {
            if a == b {
                distinct = false;
            }
        }
    }
    report.check("no two are the same", distinct);
    report.check(
        "two to a page",
        buffers[0] + 2048 == buffers[1] && buffers[2] + 2048 == buffers[3],
    );
    // A buffer must not straddle the end of its page, or a device writing the
    // whole of it would write into memory nothing lent it.
    report.check(
        "none crosses a page",
        buffers.iter().all(|&b| (b % PAGE_SIZE as u64) + 2048 <= PAGE_SIZE as u64),
    );
}
