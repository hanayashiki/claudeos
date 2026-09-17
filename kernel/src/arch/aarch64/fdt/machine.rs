//! What a device tree says about the machine as a whole: where memory is, which
//! parts of it the firmware has reserved, the command line, and where the ram
//! disk was placed.
//!
//! This reads a byte slice and checks every offset against it, so a damaged
//! tree ends the read instead of sending it outside the blob. It uses nothing
//! but `core` and `crate::boot`, which is what lets `tools/devicetree` compile
//! this file for the Mac and run it over the Pi's tree file, the tree QEMU
//! builds for its emulated Pi 4, and trees its tests write.
//!
//! Memory the firmware keeps for itself is named in two places, and both are
//! reserved here before the frame allocator exists:
//!
//! - the header's memory reservation block, a list of address and size pairs;
//!   on a Pi 4 it holds the spin tables at 0x0-0x1000;
//! - the children of `/reserved-memory`. In the Pi's tree file, `nvram@0` and
//!   `nvram@1` (compatible "raspberrypi,bootloader-config" and
//!   "raspberrypi,bootloader-public-key") have a `reg` of size zero and are
//!   switched off: they are for the firmware to fill in at boot with where it
//!   left those in memory. The file on the card cannot show whether it did.
//!
//! What Linux does with the second list is `early_init_fdt_scan_reserved_mem`
//! in drivers/of/fdt.c, and what is read here follows it: a child that is not
//! switched on is passed over; every entry of a child's `reg` is reserved; a
//! child with `size` and no `reg` asks the kernel to allocate that much itself.
//! Nothing in this kernel uses such a pool (on the Pi it is `linux,cma`, for
//! Linux's video drivers), so those are listed and not allocated. A child's
//! `reg` is read as physical addresses, and the node's `ranges`, which the
//! binding requires to be empty, is not applied; Linux does the same.
//!
//! `no-map` asks Linux to leave the range out of its linear map as well. The
//! direct map here is built by boot.s over the whole of the low four gigabytes
//! before the tree is read, so such a range stays mapped; it is only kept out
//! of the allocator, which is what stops the kernel writing to it.

use crate::boot::BootInfo;
use core::fmt;

const MAGIC: u32 = 0xD00D_FEED;

/// The largest tree read. Linux on arm64 refuses a larger one for the same
/// reason (MAX_FDT_SIZE in arch/arm64/include/asm/boot.h): the size comes from
/// the header, and a damaged header must not make the kernel read gigabytes.
pub const MAX_SIZE: usize = 2 * 1024 * 1024;

/// The version that added the structure block's size to the header, and the
/// one the Pi firmware and QEMU write. An older header does not say where the
/// structure block ends.
const VERSION: u32 = 17;

const BEGIN_NODE: u32 = 1;
const END_NODE: u32 = 2;
const PROP: u32 = 3;
const NOP: u32 = 4;
const END: u32 = 9;

/// Reservations kept for the boot line. Every one is reserved whether or not
/// it is kept here.
pub const MAX_LISTED: usize = 16;
/// Bytes of a node name kept for the boot line.
const NAME_BYTES: usize = 32;

/// The spec's default cell counts, for a root that gives none.
const DEFAULT_ADDRESS_CELLS: u32 = 2;
const DEFAULT_SIZE_CELLS: u32 = 1;
/// Wider than any address or size a 64-bit machine can use. Cell counts above
/// this are treated as damage rather than read.
const MAX_CELLS: u32 = 4;

// ---------------------------------------------------------------------------
// What the read found
// ---------------------------------------------------------------------------

/// A node name, copied out of the blob and cut at `NAME_BYTES`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Name {
    bytes: [u8; NAME_BYTES],
    len: u8,
}

impl Name {
    const EMPTY: Name = Name { bytes: [0; NAME_BYTES], len: 0 };

    fn new(from: &[u8]) -> Name {
        let mut name = Name::EMPTY;
        let len = from.len().min(NAME_BYTES);
        name.bytes[..len].copy_from_slice(&from[..len]);
        name.len = len as u8;
        name
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len as usize]
    }
}

impl fmt::Display for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for &byte in self.as_bytes() {
            let shown = if byte.is_ascii_graphic() { byte as char } else { '?' };
            fmt::Write::write_char(f, shown)?;
        }
        Ok(())
    }
}

impl fmt::Debug for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "\"{}\"", self)
    }
}

/// What one listed entry is, and what was done with it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum What {
    /// An entry of the header's memory reservation block. Reserved.
    MemReserve,
    /// One entry of a `/reserved-memory` child's `reg`. Reserved.
    Static { no_map: bool },
    /// A `/reserved-memory` child with `size` and no `reg`: a pool the kernel
    /// is asked to allocate. Nothing is reserved or allocated for it.
    Dynamic { size: u64 },
    /// A `/reserved-memory` child whose `status` is not "okay". Not reserved,
    /// as Linux does not reserve it.
    Disabled,
    /// A `/reserved-memory` child, or one entry of it, that could not be read.
    /// Not reserved, as Linux does not reserve it.
    Skipped(Skip),
}

/// Why a `/reserved-memory` child was skipped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Skip {
    /// `/reserved-memory` has an `#address-cells` of zero, a `#size-cells` of
    /// zero, or one wider than four cells.
    Cells,
    /// `reg` is not a whole number of entries. Linux skips the whole node
    /// ("invalid reg property"), since the entries cannot be told apart.
    RegLength,
    /// `reg` has no entries at all.
    EmptyReg,
    /// A `reg` entry of size zero, which reserves nothing.
    ZeroSize,
    /// `size` is not `#size-cells` wide.
    SizeLength,
    /// Neither `reg` nor `size`.
    Nothing,
}

impl Skip {
    fn message(self) -> &'static str {
        match self {
            Skip::Cells => "the cell counts of /reserved-memory are out of range",
            Skip::RegLength => "reg is not a whole number of entries",
            Skip::EmptyReg => "reg is empty",
            Skip::ZeroSize => "an entry of size 0",
            Skip::SizeLength => "size is not #size-cells wide",
            Skip::Nothing => "neither reg nor size",
        }
    }
}

/// Where a reserved range lies against the memory nodes of the same tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Placement {
    /// Wholly inside memory the tree describes.
    InMemory,
    /// Partly outside it. The whole range is reserved all the same.
    PartlyOutside,
    /// Wholly outside it. Reserving it costs nothing: the allocator never
    /// hands out a frame the memory nodes do not describe.
    Outside,
    /// Not a range: a dynamic, disabled or skipped entry.
    NotARange,
}

/// One entry of the boot line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reservation {
    pub name: Name,
    pub what: What,
    /// The range, end exclusive. Both zero for an entry that is not a range.
    pub start: u64,
    pub end: u64,
    pub placement: Placement,
}

impl Reservation {
    const EMPTY: Reservation = Reservation {
        name: Name::EMPTY,
        what: What::Disabled,
        start: 0,
        end: 0,
        placement: Placement::NotARange,
    };
}

/// What a read found beyond what it wrote into `BootInfo`: every reservation
/// and every `/reserved-memory` child it passed over, for the boot line.
#[derive(Clone, Copy)]
pub struct Found {
    listed: [Reservation; MAX_LISTED],
    count: usize,
    /// Entries past `MAX_LISTED`. Those that are ranges are reserved all the
    /// same.
    unlisted: usize,
    /// The reservation block ran off the end of the blob before its
    /// terminator, or the structure block ended before its END token or held
    /// a token, a name or a property that runs outside it. Everything before
    /// the damage was read and applied.
    pub damaged: bool,
}

impl Found {
    fn new() -> Found {
        Found { listed: [Reservation::EMPTY; MAX_LISTED], count: 0, unlisted: 0, damaged: false }
    }

    pub fn reservations(&self) -> &[Reservation] {
        &self.listed[..self.count]
    }

    pub fn unlisted(&self) -> usize {
        self.unlisted
    }

    /// Keep an entry for the boot line. Its placement is filled in once the
    /// whole tree has been read.
    fn list(&mut self, name: Name, what: What, start: u64, end: u64) {
        if self.count == MAX_LISTED {
            self.unlisted += 1;
            return;
        }
        let placement = Placement::NotARange;
        self.listed[self.count] = Reservation { name, what, start, end, placement };
        self.count += 1;
    }
}

/// The boot line's text after "reserved memory: ": each entry and what was
/// done with it, separated by "; ", since node names such as `linux,cma` hold
/// commas.
impl fmt::Display for Found {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.count == 0 && self.unlisted == 0 {
            f.write_str("none")?;
        }
        for (index, entry) in self.reservations().iter().enumerate() {
            if index > 0 {
                f.write_str("; ")?;
            }
            match entry.what {
                What::MemReserve => write!(f, "/memreserve/ {:#x}-{:#x}", entry.start, entry.end)?,
                What::Static { no_map } => {
                    write!(f, "{} {:#x}-{:#x}", entry.name, entry.start, entry.end)?;
                    if no_map {
                        f.write_str(" no-map")?;
                    }
                }
                What::Dynamic { size } => {
                    write!(f, "{} dynamic, size {:#x}, not allocated", entry.name, size)?
                }
                What::Disabled => write!(f, "{} disabled, not reserved", entry.name)?,
                What::Skipped(skip) => {
                    write!(f, "{} skipped, {}", entry.name, skip.message())?
                }
            }
            match entry.placement {
                Placement::PartlyOutside => f.write_str(" (partly outside memory)")?,
                Placement::Outside => f.write_str(" (outside memory)")?,
                Placement::InMemory | Placement::NotARange => {}
            }
        }
        if self.unlisted > 0 {
            if self.count > 0 {
                f.write_str("; ")?;
            }
            write!(f, "{} more not listed", self.unlisted)?;
        }
        if self.damaged {
            f.write_str("; the tree is damaged and was read up to the damage")?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------

fn be32(bytes: &[u8], at: usize) -> Option<u32> {
    let word = bytes.get(at..at.checked_add(4)?)?;
    Some(u32::from_be_bytes([word[0], word[1], word[2], word[3]]))
}

fn be64(bytes: &[u8], at: usize) -> Option<u64> {
    Some((be32(bytes, at)? as u64) << 32 | be32(bytes, at + 4)? as u64)
}

/// A number `count` cells wide at the start of `value`, which the caller has
/// checked holds that many. Only the low two cells carry anything a 64-bit
/// address can hold, which is what Linux's `of_read_number` keeps as well.
fn cells(value: &[u8], count: u32) -> u64 {
    match count {
        0 => 0,
        1 => be32(value, 0).unwrap_or(0) as u64,
        _ => be64(value, (count as usize - 2) * 4).unwrap_or(0),
    }
}

/// The bytes from `at` up to the next NUL, or nothing when the block ends
/// first.
fn cstr(block: &[u8], at: usize) -> Option<&[u8]> {
    let rest = block.get(at..)?;
    let len = rest.iter().position(|&byte| byte == 0)?;
    Some(&rest[..len])
}

fn align4(value: usize) -> Option<usize> {
    Some(value.checked_add(3)? & !3)
}

/// Whether a `status` value leaves a node switched on: absent, "okay", or the
/// older "ok". The same test as Linux's `of_fdt_device_is_available`.
fn available(status: Option<&[u8]>) -> bool {
    match status {
        None => true,
        Some(value) => {
            let text = match value.iter().position(|&byte| byte == 0) {
                Some(end) => &value[..end],
                None => value,
            };
            text == b"okay" || text == b"ok"
        }
    }
}

/// The parts of a blob the walk reads, each already checked to lie inside it.
struct Blocks<'a> {
    structure: &'a [u8],
    strings: &'a [u8],
    reservations: &'a [u8],
}

fn blocks(blob: &[u8]) -> Option<Blocks<'_>> {
    if be32(blob, 0)? != MAGIC {
        return None;
    }
    let total = be32(blob, 4)? as usize;
    if total > blob.len() || total > MAX_SIZE {
        return None;
    }
    let blob = &blob[..total];
    let struct_offset = be32(blob, 8)? as usize;
    let strings_offset = be32(blob, 12)? as usize;
    let reserve_offset = be32(blob, 16)? as usize;
    let version = be32(blob, 20)?;
    let strings_size = be32(blob, 32)? as usize;
    let struct_size = be32(blob, 36)? as usize;
    if version < VERSION {
        return None;
    }
    Some(Blocks {
        structure: blob.get(struct_offset..struct_offset.checked_add(struct_size)?)?,
        strings: blob.get(strings_offset..strings_offset.checked_add(strings_size)?)?,
        reservations: blob.get(reserve_offset..)?,
    })
}

/// A `/reserved-memory` child, gathered property by property, because its
/// `status` can come after its `reg`.
struct Child<'a> {
    name: Name,
    reg: Option<&'a [u8]>,
    size: Option<&'a [u8]>,
    no_map: bool,
    status: Option<&'a [u8]>,
}

/// A memory node, gathered the same way.
struct Memory<'a> {
    reg: Option<&'a [u8]>,
    status: Option<&'a [u8]>,
}

/// Read the tree in `blob` into `info`: the memory nodes as usable regions,
/// the header's reservation block and the ranges `/reserved-memory` names as
/// reserved, `/chosen`'s command line, and its ram disk as a module.
///
/// Nothing when `blob` is not a tree, or its header points outside it, in
/// which case `info` is untouched. A tree whose structure block is damaged is
/// read up to the damage and says so in what is returned.
pub fn read(blob: &[u8], info: &mut BootInfo) -> Option<Found> {
    let blocks = blocks(blob)?;
    let mut found = Found::new();

    // The header's list, ended by an entry of size zero. Linux stops at the
    // first zero size too (early_init_fdt_scan_reserved_mem).
    let mut at = 0usize;
    loop {
        let (Some(address), Some(size)) =
            (be64(blocks.reservations, at), be64(blocks.reservations, at + 8))
        else {
            // The list ran off the end of the blob before its terminator.
            found.damaged = true;
            break;
        };
        if size == 0 {
            break;
        }
        let end = address.saturating_add(size);
        info.reserve(address, end);
        found.list(Name::new(b"/memreserve/"), What::MemReserve, address, end);
        at += 16;
    }

    let structure = blocks.structure;
    let mut cursor = 0usize;
    let mut depth = 0usize;
    let mut ended = false;

    let mut address_cells = DEFAULT_ADDRESS_CELLS;
    let mut size_cells = DEFAULT_SIZE_CELLS;
    // Which node at depth 2 the walk is inside.
    let mut in_chosen = false;
    let mut memory: Option<Memory> = None;
    let mut in_reserved = false;
    // `/reserved-memory`'s own cell counts, which say how wide its children's
    // `reg` and `size` are. When it gives none the root's are used, which is
    // what Linux reads them with in every case.
    let mut reserved_address_cells: Option<u32> = None;
    let mut reserved_size_cells: Option<u32> = None;
    let mut child: Option<Child> = None;
    // The two ends of the ram disk arrive as separate properties in either
    // order, so they are held until the walk is over.
    let mut initrd_start = 0u64;
    let mut initrd_end = 0u64;

    while !ended {
        let Some(token) = be32(structure, cursor) else {
            break;
        };
        cursor += 4;
        match token {
            BEGIN_NODE => {
                let Some(name) = cstr(structure, cursor) else {
                    break;
                };
                let Some(next) = align4(cursor + name.len() + 1) else {
                    break;
                };
                cursor = next;
                depth += 1;
                if depth == 2 {
                    in_chosen = name == b"chosen";
                    in_reserved = name == b"reserved-memory";
                    if name.starts_with(b"memory") {
                        memory = Some(Memory { reg: None, status: None });
                    }
                } else if depth == 3 && in_reserved {
                    child = Some(Child {
                        name: Name::new(name),
                        reg: None,
                        size: None,
                        no_map: false,
                        status: None,
                    });
                }
            }
            END_NODE => {
                if depth == 0 {
                    break;
                }
                if depth == 3 {
                    if let Some(done) = child.take() {
                        let cells = (
                            reserved_address_cells.unwrap_or(address_cells),
                            reserved_size_cells.unwrap_or(size_cells),
                        );
                        reserve_child(&done, cells, info, &mut found);
                    }
                } else if depth == 2 {
                    if let Some(done) = memory.take() {
                        add_memory(&done, (address_cells, size_cells), info);
                    }
                    in_chosen = false;
                    in_reserved = false;
                } else if depth == 1 {
                    // The root has closed; only END may follow.
                    ended = true;
                }
                depth -= 1;
            }
            PROP => {
                let (Some(length), Some(name_offset)) =
                    (be32(structure, cursor), be32(structure, cursor + 4))
                else {
                    break;
                };
                let start = cursor + 8;
                let Some(value) = start
                    .checked_add(length as usize)
                    .and_then(|end| structure.get(start..end))
                else {
                    break;
                };
                let Some(next) = align4(start + value.len()) else {
                    break;
                };
                cursor = next;
                let Some(name) = cstr(blocks.strings, name_offset as usize) else {
                    // A name outside the strings block. The property cannot
                    // be told from any other, but the walk can carry on past
                    // it, since its length was readable.
                    found.damaged = true;
                    continue;
                };

                if depth == 1 {
                    // The root's cell counts say how wide the addresses and
                    // sizes in its children are.
                    if name == b"#address-cells" && value.len() >= 4 {
                        address_cells = cells(value, 1) as u32;
                    } else if name == b"#size-cells" && value.len() >= 4 {
                        size_cells = cells(value, 1) as u32;
                    }
                } else if depth == 2 {
                    if let Some(node) = memory.as_mut() {
                        if name == b"reg" {
                            node.reg = Some(value);
                        } else if name == b"status" {
                            node.status = Some(value);
                        }
                    } else if in_reserved {
                        if name == b"#address-cells" && value.len() >= 4 {
                            reserved_address_cells = Some(cells(value, 1) as u32);
                        } else if name == b"#size-cells" && value.len() >= 4 {
                            reserved_size_cells = Some(cells(value, 1) as u32);
                        }
                    } else if in_chosen {
                        if name == b"bootargs" {
                            // The property carries its terminator; the
                            // command line does not want it.
                            let text = match value.iter().position(|&byte| byte == 0) {
                                Some(end) => &value[..end],
                                None => value,
                            };
                            info.set_cmdline(text);
                        } else if name == b"linux,initrd-start" {
                            initrd_start = cells(value, (value.len() / 4) as u32);
                        } else if name == b"linux,initrd-end" {
                            initrd_end = cells(value, (value.len() / 4) as u32);
                        }
                    }
                } else if depth == 3 {
                    if let Some(node) = child.as_mut() {
                        if name == b"reg" {
                            node.reg = Some(value);
                        } else if name == b"size" {
                            node.size = Some(value);
                        } else if name == b"no-map" {
                            node.no_map = true;
                        } else if name == b"status" {
                            node.status = Some(value);
                        }
                    }
                }
            }
            NOP => {}
            END => ended = true,
            _ => break,
        }
    }
    if !ended {
        found.damaged = true;
    }

    if initrd_end > initrd_start {
        info.add_module(initrd_start, initrd_end);
    }

    // Memory nodes can come after `/reserved-memory`, as they do in the Pi's
    // tree, so where each range lies is decided once all of them are known.
    for entry in &mut found.listed[..found.count] {
        if let What::MemReserve | What::Static { .. } = entry.what {
            entry.placement = placement(info, entry.start, entry.end);
        }
    }
    Some(found)
}

/// Add a memory node's ranges as usable, unless its `status` switches it off.
/// Linux passes over such a node (`early_init_dt_scan_memory`).
fn add_memory(node: &Memory, (address_cells, size_cells): (u32, u32), info: &mut BootInfo) {
    if !available(node.status) {
        return;
    }
    let Some(reg) = node.reg else {
        return;
    };
    if address_cells == 0 || address_cells > MAX_CELLS || size_cells > MAX_CELLS {
        return;
    }
    let stride = (address_cells + size_cells) as usize * 4;
    for entry in reg.chunks_exact(stride) {
        let address = cells(entry, address_cells);
        let size = cells(&entry[address_cells as usize * 4..], size_cells);
        info.add_region(address, size, true);
    }
}

/// Reserve what one `/reserved-memory` child names, and list it.
///
/// This is `__reserved_mem_reserve_reg` and the dynamic case after it in
/// Linux's `fdt_scan_reserved_mem`.
fn reserve_child(
    child: &Child,
    (address_cells, size_cells): (u32, u32),
    info: &mut BootInfo,
    found: &mut Found,
) {
    let name = child.name;
    if !available(child.status) {
        found.list(name, What::Disabled, 0, 0);
        return;
    }
    if address_cells == 0 || address_cells > MAX_CELLS || size_cells == 0 || size_cells > MAX_CELLS
    {
        found.list(name, What::Skipped(Skip::Cells), 0, 0);
        return;
    }
    let stride = (address_cells + size_cells) as usize * 4;

    let Some(reg) = child.reg else {
        match child.size {
            Some(size) if size.len() == size_cells as usize * 4 => {
                found.list(name, What::Dynamic { size: cells(size, size_cells) }, 0, 0);
            }
            Some(_) => found.list(name, What::Skipped(Skip::SizeLength), 0, 0),
            None => found.list(name, What::Skipped(Skip::Nothing), 0, 0),
        }
        return;
    };
    if reg.len() % stride != 0 {
        found.list(name, What::Skipped(Skip::RegLength), 0, 0);
        return;
    }
    if reg.is_empty() {
        found.list(name, What::Skipped(Skip::EmptyReg), 0, 0);
        return;
    }
    for entry in reg.chunks_exact(stride) {
        let address = cells(entry, address_cells);
        let size = cells(&entry[address_cells as usize * 4..], size_cells);
        if size == 0 {
            found.list(name, What::Skipped(Skip::ZeroSize), address, address);
            continue;
        }
        // A range that runs past the top of the address space is cut there,
        // as memblock_reserve cuts it in Linux.
        let end = address.saturating_add(size);
        info.reserve(address, end);
        found.list(name, What::Static { no_map: child.no_map }, address, end);
    }
}

/// Where `start..end` lies against the usable regions `info` holds.
///
/// The regions may overlap or touch, so the range is walked from its start,
/// each step jumping to the furthest end of a region that covers the current
/// address.
fn placement(info: &BootInfo, start: u64, end: u64) -> Placement {
    let usable = || info.regions().iter().filter(|region| region.usable && region.len > 0);
    let overlaps = usable().any(|region| region.addr < end && start < region.addr.saturating_add(region.len));
    if !overlaps {
        return Placement::Outside;
    }
    let mut at = start;
    while at < end {
        let reach = usable()
            .filter(|region| region.addr <= at && at < region.addr.saturating_add(region.len))
            .map(|region| region.addr.saturating_add(region.len))
            .max();
        match reach {
            Some(next) => at = next,
            None => return Placement::PartlyOutside,
        }
    }
    Placement::InMemory
}
