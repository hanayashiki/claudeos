//! The flattened device tree, read far enough to describe the machine.
//!
//! This is how a board following the Linux AArch64 boot protocol says what it
//! has: the firmware leaves a blob in memory and puts its address in x0.
//! `parse` takes four things out of it at start-up — where memory is, what the
//! command line says, and where an initial ram disk was placed — because
//! everything else this kernel needs about the machine itself it already
//! knows.
//!
//! `find_compatible` is the other half, and it is for devices rather than for
//! the machine. A driver that is not on a bus it can enumerate has no way to
//! discover its registers, its interrupt or its hardware address except by
//! being told, and the tree is where the firmware writes all three.
//!
//! Every number in the blob is big-endian, and the strings are in a separate
//! block indexed by offset, so nothing can be overlaid with a struct.

use crate::boot::BootInfo;
use crate::mm::phys_to_virt;
use core::sync::atomic::{AtomicU64, Ordering};

const MAGIC: u32 = 0xD00D_FEED;

const BEGIN_NODE: u32 = 1;
const END_NODE: u32 = 2;
const PROP: u32 = 3;
const NOP: u32 = 4;
const END: u32 = 9;

unsafe fn be32(virt: u64) -> u32 {
    u32::from_be(core::ptr::read_unaligned(virt as *const u32))
}

unsafe fn be64(virt: u64) -> u64 {
    u64::from_be(core::ptr::read_unaligned(virt as *const u64))
}

/// A big-endian value of one, two or more cells, of which only the low two
/// carry anything an address can hold. A count of zero is a real thing to say
/// -- a bus whose children have addresses but no sizes says `#size-cells = 0`
/// -- and it reads as nothing rather than walking backwards.
unsafe fn cells(virt: u64, count: u32) -> u64 {
    match count {
        0 => 0,
        1 => be32(virt) as u64,
        2 => be64(virt),
        _ => be64(virt + (count as u64 - 2) * 4),
    }
}

unsafe fn cstr(virt: u64) -> &'static [u8] {
    let ptr = virt as *const u8;
    let mut len = 0usize;
    while len < 1024 && *ptr.add(len) != 0 {
        len += 1;
    }
    core::slice::from_raw_parts(ptr, len)
}

#[inline]
fn align4(value: u64) -> u64 {
    (value + 3) & !3
}

/// Is there a device tree at `phys`?
pub fn present(phys: u64) -> bool {
    if phys == 0 || phys & 3 != 0 {
        return false;
    }
    unsafe { be32(phys_to_virt(phys)) == MAGIC }
}

/// The blob `parse` accepted, if there was one. Devices are looked up long
/// after start-up is over, so the address has to be kept rather than passed
/// down.
static BLOB: AtomicU64 = AtomicU64::new(0);

/// Physical address of the device tree this machine was booted with, or
/// nothing when it was booted without one. A board that hands over a tag list
/// instead -- which is what QEMU's emulated Pi 4 does -- leaves this empty,
/// and every device lookup then finds nothing.
pub fn blob() -> Option<u64> {
    match BLOB.load(Ordering::Relaxed) {
        0 => None,
        phys => Some(phys),
    }
}

/// Read the blob at `phys` into `info` and remember it as the machine's own,
/// which is the tree every later device lookup goes through. Returns false if
/// there is no device tree there, in which case `info` is untouched.
pub fn parse(phys: u64, info: &mut BootInfo) -> bool {
    if !read_into(phys, info) {
        return false;
    }
    BLOB.store(phys, Ordering::Relaxed);
    true
}

/// The same without remembering it. A check reading a tree it built itself
/// wants this: the machine's own tree is the one the drivers have to keep.
pub fn read_into(phys: u64, info: &mut BootInfo) -> bool {
    if !present(phys) {
        return false;
    }
    let base = phys_to_virt(phys);
    unsafe {
        let total_size = be32(base + 4) as u64;
        let struct_offset = be32(base + 8) as u64;
        let strings_offset = be32(base + 12) as u64;
        let reserve_offset = be32(base + 16) as u64;
        let struct_size = be32(base + 36) as u64;

        // The blob itself is read all through start-up, so it has to survive
        // the frame allocator's first pass.
        info.reserve(phys, phys + total_size);

        // Ranges the firmware says are already spoken for.
        let mut entry = base + reserve_offset;
        loop {
            let address = be64(entry);
            let size = be64(entry + 8);
            if size == 0 {
                break;
            }
            info.reserve(address, address + size);
            entry += 16;
        }

        let strings = base + strings_offset;
        let mut cursor = base + struct_offset;
        let end = cursor + struct_size;

        let mut depth = 0usize;
        let mut in_memory = false;
        let mut in_chosen = false;
        // What the specification says to assume when the root does not say.
        let mut address_cells = 2u32;
        let mut size_cells = 1u32;
        // The two ends of the ram disk arrive as separate properties in
        // either order, so they are held until the walk is over.
        let mut initrd_start = 0u64;
        let mut initrd_end = 0u64;

        while cursor + 4 <= end {
            let token = be32(cursor);
            cursor += 4;
            match token {
                BEGIN_NODE => {
                    let name = cstr(cursor);
                    cursor += align4(name.len() as u64 + 1);
                    depth += 1;
                    if depth == 2 {
                        in_memory = name.starts_with(b"memory");
                        in_chosen = name == b"chosen";
                    }
                }
                END_NODE => {
                    if depth == 2 {
                        in_memory = false;
                        in_chosen = false;
                    }
                    depth = depth.saturating_sub(1);
                }
                PROP => {
                    let length = be32(cursor) as u64;
                    let name = cstr(strings + be32(cursor + 4) as u64);
                    let value = cursor + 8;
                    cursor = value + align4(length);

                    if depth == 1 {
                        // The root's cell counts say how wide the addresses
                        // and sizes in its children are.
                        if name == b"#address-cells" {
                            address_cells = be32(value);
                        } else if name == b"#size-cells" {
                            size_cells = be32(value);
                        }
                    } else if in_memory && name == b"reg" {
                        let stride = (address_cells + size_cells) as u64 * 4;
                        let mut at = value;
                        while at + stride <= value + length {
                            let address = cells(at, address_cells);
                            let size = cells(at + address_cells as u64 * 4, size_cells);
                            info.add_region(address, size, true);
                            at += stride;
                        }
                    } else if in_chosen {
                        if name == b"bootargs" {
                            // The property carries its terminator; the command
                            // line does not want it.
                            let text = &core::slice::from_raw_parts(
                                value as *const u8,
                                length as usize,
                            )[..];
                            let text = match text.iter().position(|&b| b == 0) {
                                Some(at) => &text[..at],
                                None => text,
                            };
                            info.set_cmdline(text);
                        } else if name == b"linux,initrd-start" {
                            initrd_start = cells(value, (length / 4) as u32);
                        } else if name == b"linux,initrd-end" {
                            initrd_end = cells(value, (length / 4) as u32);
                        }
                    }
                }
                NOP => {}
                END => break,
                _ => break,
            }
        }

        if initrd_end > initrd_start {
            info.add_module(initrd_start, initrd_end);
        }
    }
    true
}

// ---------------------------------------------------------------------------
// Looking a device up
// ---------------------------------------------------------------------------
//
// A node says where its registers are with `reg`, but the numbers in it are
// addresses on whatever bus the node hangs off, not addresses the processor
// can use. Each bus node above it carries a `ranges` property saying how its
// children's addresses map into its own parent's, and the mapping has to be
// applied at every level on the way up. On this board that is not a
// formality: the Ethernet controller's `reg` says 0x7d580000 and the bus above
// it maps that region to 0xfd580000, which is where the processor finds it.
// Taking the untranslated number would land in the middle of ordinary memory.
//
// How wide those numbers are is also a property of the parent rather than of
// the node: `#address-cells` and `#size-cells` on the parent say how many
// 32-bit cells each half of a `reg` entry takes.

/// Ancestors tracked while walking. A node deeper than this is not resolved,
/// rather than being resolved from a truncated chain; nothing this kernel
/// looks for is anywhere near it.
const MAX_DEPTH: usize = 12;
/// `reg` entries kept. One is all any device here has.
const MAX_REG: usize = 4;
/// Buses between the root and a device that may say how the device sees
/// memory. On this board one does: `soc`.
const MAX_DMA: usize = 4;

/// What one open node contributes to the addresses of the nodes inside it.
#[derive(Clone, Copy)]
struct Level {
    /// Cells per address in this node's children's `reg`.
    address_cells: u32,
    /// Cells per size in the same.
    size_cells: u32,
    /// Where this node's `ranges` value is and how long it is, or zero for a
    /// node that has none.
    ranges: u64,
    ranges_len: u64,
    /// The same for `dma-ranges`, which runs the other way: it says where
    /// memory the parent addresses appears to a device on this bus.
    dma_ranges: u64,
    dma_ranges_len: u64,
}

impl Level {
    /// What the specification says to assume when a node says neither.
    const DEFAULT: Level = Level {
        address_cells: 2,
        size_cells: 1,
        ranges: 0,
        ranges_len: 0,
        dma_ranges: 0,
        dma_ranges_len: 0,
    };
}

/// One bus's `dma-ranges`, with the cell counts needed to read it: its own
/// address cells for the bus side, its parent's for the other side, and its
/// own size cells.
#[derive(Clone, Copy)]
struct DmaWindow {
    ranges: u64,
    ranges_len: u64,
    child_cells: u32,
    parent_cells: u32,
    size_cells: u32,
}

/// One entry of a node's `reg`.
#[derive(Clone, Copy)]
struct Reg {
    /// The address as the bus the node hangs off names it.
    bus: u64,
    /// The same address as the processor names it, when every bus in between
    /// said how to get there. A bus that is not memory at all -- the
    /// management bus a PHY sits on, where `reg` is a five-bit station address
    /// -- says nothing, and this is then nothing.
    cpu: Option<u64>,
    size: u64,
}

/// One node of the tree, found by what it says it is compatible with or by the
/// handle other nodes refer to it with.
///
/// Its `reg` entries are resolved at the moment it is found, because resolving
/// them needs the chain of buses above it and that chain only exists during
/// the walk. Everything else is read out of the blob on demand.
#[derive(Clone, Copy)]
pub struct Node {
    /// Where this node's first property token sits. Properties always precede
    /// child nodes, so a scan from here that stops at the first `BEGIN_NODE`
    /// or `END_NODE` has seen exactly this node's properties.
    first_prop: u64,
    strings: u64,
    struct_end: u64,
    reg: [Reg; MAX_REG],
    reg_count: usize,
    /// The `dma-ranges` of every bus above the node that has one, from the
    /// one nearest the root down to the node's parent. Like `reg` these need
    /// the chain of buses and are captured during the walk.
    dma: [DmaWindow; MAX_DMA],
    dma_count: usize,
    /// A bus above the node declared more windows than `dma` holds, so no
    /// translation through them can be trusted.
    dma_overflow: bool,
}

impl Node {
    /// The address a device described by this node uses to reach physical
    /// memory at `phys`, which is the address to hand the device for a buffer.
    ///
    /// Every bus between the root and the node that has a `dma-ranges` moves
    /// the address, and they are applied from the root down. A bus without the
    /// property passes addresses through unchanged, which is how Linux's
    /// `of_dma_get_range` treats one. Nothing when a window says the device
    /// cannot see that memory at all.
    ///
    /// On a Pi 4 the `soc` bus says `<0xc0000000 0x0 0x0 0x40000000>`: the
    /// first gigabyte of memory appears to its devices at 0xc0000000, and
    /// nothing above it appears at all.
    pub fn dma_address(&self, phys: u64) -> Option<u64> {
        if self.dma_overflow {
            return None;
        }
        let mut address = phys;
        for window in &self.dma[..self.dma_count] {
            address = unsafe { dma_translate(window, address)? };
        }
        Some(address)
    }

    /// The value of one property, or nothing when the node does not have it.
    pub fn property(&self, name: &[u8]) -> Option<&'static [u8]> {
        unsafe {
            let mut cursor = self.first_prop;
            while cursor + 4 <= self.struct_end {
                let token = be32(cursor);
                cursor += 4;
                match token {
                    PROP => {
                        let length = be32(cursor) as u64;
                        let this = cstr(self.strings + be32(cursor + 4) as u64);
                        let value = cursor + 8;
                        cursor = value + align4(length);
                        if this == name {
                            return Some(core::slice::from_raw_parts(
                                value as *const u8,
                                length as usize,
                            ));
                        }
                    }
                    NOP => {}
                    // A child node has started, or this node has ended; either
                    // way its own properties are all behind us.
                    _ => return None,
                }
            }
            None
        }
    }

    /// Register block `index`: where the processor reaches it, and how big it
    /// is. Already translated through every bus above the node, and nothing
    /// when some bus in between does not map it.
    pub fn reg(&self, index: usize) -> Option<(u64, u64)> {
        if index >= self.reg_count {
            return None;
        }
        Some((self.reg[index].cpu?, self.reg[index].size))
    }

    /// Entry `index` of `reg` as its own bus names it, untranslated. This is
    /// what to ask for when the bus is not memory: a device on a management
    /// bus is numbered on that bus and there is nothing to translate to.
    pub fn bus_address(&self, index: usize) -> Option<u64> {
        if index >= self.reg_count {
            return None;
        }
        Some(self.reg[index].bus)
    }

    /// The value of a property that is a single 32-bit cell.
    pub fn cell(&self, name: &[u8]) -> Option<u32> {
        let value = self.property(name)?;
        if value.len() < 4 {
            return None;
        }
        Some(unsafe { be32(value.as_ptr() as u64) })
    }

    /// Whether the firmware left this device switched on. A node whose
    /// `status` is anything but "okay" describes hardware that is in the tree
    /// and not usable on the board. The Ethernet node in the upstream source
    /// for this chip is disabled and each board's own file turns it on, so a
    /// tree assembled differently can arrive with it still off.
    pub fn enabled(&self) -> bool {
        match self.property(b"status") {
            None => true,
            Some(status) => status.starts_with(b"okay") || status.starts_with(b"ok\0"),
        }
    }

    /// Interrupt `index` as a line number the interrupt controller knows.
    ///
    /// Three cells to an entry, which is what a GIC uses and what everything
    /// on this board is parented to: the kind, the number within that kind,
    /// and how it is triggered. Shared peripheral interrupts are numbered from
    /// 32 and per-core ones from 16, and the number in the tree counts from
    /// the start of its own group.
    pub fn interrupt(&self, index: usize) -> Option<u8> {
        let value = self.property(b"interrupts")?;
        let entry = index * 12;
        if entry + 12 > value.len() {
            return None;
        }
        let at = value.as_ptr() as u64 + entry as u64;
        let (kind, number) = unsafe { (be32(at), be32(at + 4)) };
        let line = match kind {
            0 => number + 32,
            1 => number + 16,
            _ => return None,
        };
        if line > u8::MAX as u32 {
            return None;
        }
        Some(line as u8)
    }

    /// Whether transfers this device makes are coherent with the caches, in
    /// which case a buffer handed to it needs no cache maintenance. Absent
    /// means they are not, which is the answer on this board.
    pub fn dma_coherent(&self) -> bool {
        self.property(b"dma-coherent").is_some()
    }
}

/// What a node has to say about itself for the walk to stop at it.
#[derive(Clone, Copy)]
enum Want<'a> {
    /// Its `compatible` names this.
    Compatible(&'a [u8]),
    /// Its `compatible` names this and its `status` does not switch it off.
    EnabledCompatible(&'a [u8]),
    /// Its `phandle` is this, which is how one node points at another.
    Phandle(u32),
}

/// Find the first node compatible with `compatible` in the tree this machine
/// was booted with. Nothing when there is no tree, or no such node.
pub fn find_compatible(compatible: &[u8]) -> Option<Node> {
    find_compatible_in(blob()?, compatible)
}

/// The same, against a blob named outright, which is how the boot-time check
/// walks a tree it built itself.
pub fn find_compatible_in(phys: u64, compatible: &[u8]) -> Option<Node> {
    find(phys, Want::Compatible(compatible))
}

/// The first node compatible with `compatible` that is switched on, passing
/// over any that are not.
///
/// A tree can describe one block twice. The Pi 4 firmware's tree has two
/// nodes at 0x7e300000 with the same `compatible`: `mmc@7e300000`, disabled,
/// and `mmcnr@7e300000`, enabled, which is the one wired to the WiFi chip. The
/// first match is the disabled one.
pub fn find_enabled_compatible(compatible: &[u8]) -> Option<Node> {
    find_enabled_compatible_in(blob()?, compatible)
}

pub fn find_enabled_compatible_in(phys: u64, compatible: &[u8]) -> Option<Node> {
    find(phys, Want::EnabledCompatible(compatible))
}

/// The node another node pointed at, by the handle it pointed with.
pub fn find_phandle_in(phys: u64, phandle: u32) -> Option<Node> {
    find(phys, Want::Phandle(phandle))
}

pub fn find_phandle(phandle: u32) -> Option<Node> {
    find_phandle_in(blob()?, phandle)
}

fn find(phys: u64, want: Want) -> Option<Node> {
    if !present(phys) {
        return None;
    }
    let base = phys_to_virt(phys);
    unsafe {
        let struct_offset = be32(base + 8) as u64;
        let strings = base + be32(base + 12) as u64;
        let struct_size = be32(base + 36) as u64;

        let mut cursor = base + struct_offset;
        let end = cursor + struct_size;

        // The node at depth d has its own entry at `levels[d - 1]`, and takes
        // the width of its `reg` from its parent's, at `levels[d - 2]`.
        let mut levels = [Level::DEFAULT; MAX_DEPTH];
        let mut depth = 0usize;
        // Depth of the node whose `compatible` matched, once one has; zero
        // until then, which is a depth no node has.
        let mut matched = 0usize;
        let mut first_prop = 0u64;
        let mut reg = 0u64;
        let mut reg_len = 0u64;

        while cursor + 4 <= end {
            let token = be32(cursor);
            cursor += 4;
            match token {
                BEGIN_NODE => {
                    // A child starting means the matching node's properties
                    // are all behind us.
                    if matched != 0 {
                        let node = resolve(&levels, matched, first_prop, strings, end, reg, reg_len);
                        if accepted(want, &node) {
                            return node;
                        }
                        // Passed over: the walk carries on into this child
                        // as though nothing had matched.
                        matched = 0;
                    }
                    let name = cstr(cursor);
                    cursor += align4(name.len() as u64 + 1);
                    depth += 1;
                    if depth <= MAX_DEPTH {
                        levels[depth - 1] = Level::DEFAULT;
                    }
                    first_prop = cursor;
                    reg = 0;
                    reg_len = 0;
                }
                END_NODE => {
                    if matched != 0 && matched == depth {
                        let node = resolve(&levels, matched, first_prop, strings, end, reg, reg_len);
                        if accepted(want, &node) {
                            return node;
                        }
                        matched = 0;
                    }
                    depth = depth.saturating_sub(1);
                }
                PROP => {
                    let length = be32(cursor) as u64;
                    let name = cstr(strings + be32(cursor + 4) as u64);
                    let value = cursor + 8;
                    cursor = value + align4(length);
                    if depth == 0 || depth > MAX_DEPTH {
                        continue;
                    }
                    let level = &mut levels[depth - 1];
                    if name == b"#address-cells" {
                        level.address_cells = be32(value);
                    } else if name == b"#size-cells" {
                        level.size_cells = be32(value);
                    } else if name == b"ranges" {
                        // Zero is "no ranges property", so a property that is
                        // there but empty has to be distinguishable from one
                        // that is absent; the value pointer is never zero.
                        level.ranges = value;
                        level.ranges_len = length;
                    } else if name == b"dma-ranges" {
                        level.dma_ranges = value;
                        level.dma_ranges_len = length;
                    } else if name == b"reg" {
                        reg = value;
                        reg_len = length;
                    }
                    let hit = match want {
                        Want::Compatible(wanted) | Want::EnabledCompatible(wanted) => {
                            name == b"compatible" && compatible_with(value, length, wanted)
                        }
                        // `linux,phandle` is the older spelling of the same
                        // property and some firmware still writes both.
                        Want::Phandle(wanted) => {
                            (name == b"phandle" || name == b"linux,phandle")
                                && length >= 4
                                && be32(value) == wanted
                        }
                    };
                    if hit {
                        matched = depth;
                    }
                }
                NOP => {}
                END => break,
                _ => break,
            }
        }
    }
    None
}

/// Does a `compatible` value, which is a run of strings one after another,
/// contain `wanted`?
unsafe fn compatible_with(value: u64, length: u64, wanted: &[u8]) -> bool {
    let mut at = value;
    let end = value + length;
    while at < end {
        let entry = cstr(at);
        if entry == wanted {
            return true;
        }
        at += entry.len() as u64 + 1;
    }
    false
}

/// Whether a node the walk stopped at is the one asked for. Only the enabled
/// lookup has anything further to check, and it can only check once every
/// property of the node, `status` among them, has been passed.
fn accepted(want: Want, node: &Option<Node>) -> bool {
    match want {
        Want::EnabledCompatible(_) => node.as_ref().is_some_and(|node| node.enabled()),
        Want::Compatible(_) | Want::Phandle(_) => true,
    }
}

/// Where memory the bus's parent addresses as `address` appears to a device
/// on the bus. An entry is the device-side base, the parent-side base and a
/// length; an empty property passes everything through.
unsafe fn dma_translate(window: &DmaWindow, address: u64) -> Option<u64> {
    if window.ranges_len == 0 {
        return Some(address);
    }
    let stride = (window.child_cells + window.parent_cells + window.size_cells) as u64 * 4;
    if stride == 0 {
        return Some(address);
    }
    let mut at = window.ranges;
    let end = window.ranges + window.ranges_len;
    while at + stride <= end {
        let child_base = cells(at, window.child_cells);
        let parent_base = cells(at + window.child_cells as u64 * 4, window.parent_cells);
        let span = cells(at + (window.child_cells + window.parent_cells) as u64 * 4, window.size_cells);
        if address >= parent_base && address - parent_base < span {
            return Some(child_base + (address - parent_base));
        }
        at += stride;
    }
    None
}

/// Turn a found node's `reg` into addresses the processor can use.
unsafe fn resolve(
    levels: &[Level; MAX_DEPTH],
    depth: usize,
    first_prop: u64,
    strings: u64,
    struct_end: u64,
    reg: u64,
    reg_len: u64,
) -> Option<Node> {
    let empty = Reg { bus: 0, cpu: None, size: 0 };
    let no_window =
        DmaWindow { ranges: 0, ranges_len: 0, child_cells: 0, parent_cells: 0, size_cells: 0 };
    let mut node = Node {
        first_prop,
        strings,
        struct_end,
        reg: [empty; MAX_REG],
        reg_count: 0,
        dma: [no_window; MAX_DMA],
        dma_count: 0,
        dma_overflow: false,
    };
    // The buses above the node that say how their devices see memory, from
    // the one nearest the root down to the parent. The node at depth d has
    // its parent at `levels[d - 2]`; the root, at `levels[0]`, has nothing
    // above it to map to and is left out. Each bus reads its entries with its
    // own address cells on the device side and its parent's on the other.
    if (2..=MAX_DEPTH).contains(&depth) {
        for index in 1..depth - 1 {
            let bus = &levels[index];
            if bus.dma_ranges == 0 {
                continue;
            }
            if node.dma_count == MAX_DMA {
                node.dma_overflow = true;
                break;
            }
            node.dma[node.dma_count] = DmaWindow {
                ranges: bus.dma_ranges,
                ranges_len: bus.dma_ranges_len,
                child_cells: bus.address_cells,
                parent_cells: levels[index - 1].address_cells,
                size_cells: bus.size_cells,
            };
            node.dma_count += 1;
        }
    }
    // The root has no parent to take cell counts from, and nothing is looked
    // up there. A node with no `reg` is still a node: the caller may only want
    // a property off it.
    if depth < 2 || depth > MAX_DEPTH || reg == 0 {
        return Some(node);
    }
    let parent = depth - 2;
    let address_cells = levels[parent].address_cells;
    let size_cells = levels[parent].size_cells;
    if address_cells == 0 || address_cells > 4 || size_cells > 4 {
        return Some(node);
    }

    let stride = (address_cells + size_cells) as u64 * 4;
    let mut at = reg;
    while at + stride <= reg + reg_len && node.reg_count < MAX_REG {
        let bus = cells(at, address_cells);
        let size = cells(at + address_cells as u64 * 4, size_cells);
        at += stride;

        // Up through every bus between this node and the processor. A bus
        // that does not map the address gives up and the entry keeps only the
        // number its own bus knows it by.
        let mut address = Some(bus);
        let mut child_cells = address_cells;
        let mut level = parent;
        while level >= 1 {
            let parent_cells = levels[level - 1].address_cells;
            address = address.and_then(|a| translate(&levels[level], child_cells, parent_cells, a));
            child_cells = parent_cells;
            level -= 1;
        }

        node.reg[node.reg_count] = Reg { bus, cpu: address, size };
        node.reg_count += 1;
    }
    Some(node)
}

/// Put one address through a bus node's `ranges`, giving the address its
/// parent knows it by. Nothing when no entry covers it, which means the tree
/// says the processor cannot reach it at all.
unsafe fn translate(
    level: &Level,
    child_cells: u32,
    parent_cells: u32,
    address: u64,
) -> Option<u64> {
    // No `ranges` at all means this bus's addresses are not the parent's
    // addresses and there is no way to turn one into the other. That is the
    // honest answer for the management bus the Ethernet PHY sits on, where a
    // node's `reg` is a five-bit station number. An empty `ranges` is a
    // different statement: the addresses pass through unchanged.
    if level.ranges == 0 {
        return None;
    }
    if level.ranges_len == 0 {
        return Some(address);
    }
    let stride = (child_cells + parent_cells + level.size_cells) as u64 * 4;
    if stride == 0 {
        return Some(address);
    }
    let mut at = level.ranges;
    let end = level.ranges + level.ranges_len;
    while at + stride <= end {
        let child_base = cells(at, child_cells);
        let parent_base = cells(at + child_cells as u64 * 4, parent_cells);
        let span = cells(at + (child_cells + parent_cells) as u64 * 4, level.size_cells);
        if address >= child_base && address - child_base < span {
            return Some(parent_base + (address - child_base));
        }
        at += stride;
    }
    None
}
