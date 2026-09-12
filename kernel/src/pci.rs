//! PCI configuration space, through the legacy address and data ports.
//!
//! Every PCI function has 256 bytes of configuration space laid out by the
//! specification: vendor and device identifiers, a command register, six base
//! address registers naming the memory or port ranges the function decodes,
//! and the interrupt line the chipset routes it to. That space is not memory
//! mapped on a machine this old; it is reached one aligned 32-bit word at a
//! time by writing the word's address to port 0xCF8 and then reading or
//! writing port 0xCFC.
//!
//! The address written to 0xCF8 is
//!
//!     bit 31      enable (nothing happens without it)
//!     bits 30..24 reserved, zero
//!     bits 23..16 bus
//!     bits 15..11 device
//!     bits 10..8  function
//!     bits 7..2   word offset within the function's config space
//!     bits 1..0   zero: the port pair only moves whole words
//!
//! There is no way to ask which slots are populated, so finding a card means
//! reading the vendor id of every function of every device of every bus and
//! taking 0xFFFF, which is what the bus returns when nothing answers, as
//! "empty".

use crate::io::{inl, outl};
use crate::mm::paging::{AddressSpace, NO_CACHE, NO_EXECUTE, PRESENT, WRITABLE};
use crate::mm::{page_align_up, PAGE_SIZE_U64};
use crate::sync::Spinlock;
use alloc::vec::Vec;

const CONFIG_ADDRESS: u16 = 0xCF8;
const CONFIG_DATA: u16 = 0xCFC;

/// Offsets into a function's configuration space header (type 0).
pub const VENDOR_ID: u8 = 0x00;
pub const DEVICE_ID: u8 = 0x02;
pub const COMMAND: u8 = 0x04;
pub const STATUS: u8 = 0x06;
pub const REVISION: u8 = 0x08;
pub const CLASS: u8 = 0x0B;
pub const HEADER_TYPE: u8 = 0x0E;
pub const BAR0: u8 = 0x10;
pub const INTERRUPT_LINE: u8 = 0x3C;
pub const INTERRUPT_PIN: u8 = 0x3D;

/// Command register bits.
pub const CMD_IO_SPACE: u16 = 1 << 0;
pub const CMD_MEMORY_SPACE: u16 = 1 << 1;
pub const CMD_BUS_MASTER: u16 = 1 << 2;
/// Set to stop the function raising its legacy interrupt; must stay clear.
pub const CMD_INTX_DISABLE: u16 = 1 << 10;

#[inline]
fn address(bus: u8, device: u8, function: u8, offset: u8) -> u32 {
    1 << 31
        | (bus as u32) << 16
        | (device as u32 & 0x1F) << 11
        | (function as u32 & 0x07) << 8
        | (offset as u32 & 0xFC)
}

/// Read the aligned 32-bit word containing `offset`.
pub fn read32(bus: u8, device: u8, function: u8, offset: u8) -> u32 {
    unsafe {
        outl(CONFIG_ADDRESS, address(bus, device, function, offset));
        inl(CONFIG_DATA)
    }
}

pub fn write32(bus: u8, device: u8, function: u8, offset: u8, value: u32) {
    unsafe {
        outl(CONFIG_ADDRESS, address(bus, device, function, offset));
        outl(CONFIG_DATA, value);
    }
}

pub fn read16(bus: u8, device: u8, function: u8, offset: u8) -> u16 {
    // The port pair only moves whole words, so a 16-bit field is the half of
    // one word that bit 1 of the offset selects.
    let word = read32(bus, device, function, offset);
    (word >> ((offset as u32 & 2) * 8)) as u16
}

pub fn read8(bus: u8, device: u8, function: u8, offset: u8) -> u8 {
    let word = read32(bus, device, function, offset);
    (word >> ((offset as u32 & 3) * 8)) as u8
}

pub fn write16(bus: u8, device: u8, function: u8, offset: u8, value: u16) {
    let shift = (offset as u32 & 2) * 8;
    let word = read32(bus, device, function, offset);
    let merged = (word & !(0xFFFFu32 << shift)) | ((value as u32) << shift);
    write32(bus, device, function, offset, merged);
}

/// What one base address register decodes.
#[derive(Debug, Clone, Copy)]
pub enum Bar {
    /// A block of physical address space, outside RAM.
    Memory { addr: u64, size: u64, prefetchable: bool },
    /// A range of I/O ports.
    Io { port: u16, size: u32 },
}

/// One PCI function that answered.
#[derive(Debug, Clone, Copy)]
pub struct Device {
    pub bus: u8,
    pub device: u8,
    pub function: u8,
    pub vendor: u16,
    pub id: u16,
    pub class: u8,
    pub subclass: u8,
    pub prog_if: u8,
    pub revision: u8,
    /// The chipset's interrupt line, which for the legacy PIC is the IRQ
    /// number. 0xFF means the firmware routed nothing.
    pub irq_line: u8,
    /// Which of the four interrupt pins the function drives, 1..4, or 0 for
    /// a function that raises no legacy interrupt.
    pub irq_pin: u8,
}

impl Device {
    pub fn read32(&self, offset: u8) -> u32 {
        read32(self.bus, self.device, self.function, offset)
    }

    pub fn write32(&self, offset: u8, value: u32) {
        write32(self.bus, self.device, self.function, offset, value)
    }

    pub fn read16(&self, offset: u8) -> u16 {
        read16(self.bus, self.device, self.function, offset)
    }

    pub fn write16(&self, offset: u8, value: u16) {
        write16(self.bus, self.device, self.function, offset, value)
    }

    pub fn command(&self) -> u16 {
        self.read16(COMMAND)
    }

    /// Let the function decode its memory ranges and start DMA of its own.
    /// Without bus mastering the card can read descriptors but never fetch or
    /// store packet data, and the rings simply never advance.
    pub fn enable(&self, bits: u16) {
        let command = (self.command() | bits) & !CMD_INTX_DISABLE;
        self.write16(COMMAND, command);
    }

    /// Decode base address register `index`, 0..6.
    ///
    /// The register holds the base address in its upper bits and a handful of
    /// type bits in the low four (memory) or two (I/O). A 64-bit memory BAR
    /// takes the following register as its upper half and leaves a hole in
    /// the numbering.
    ///
    /// Size is not readable directly: writing all ones makes the function
    /// return its own decode mask, whose lowest set bit is the size. The
    /// register is restored afterwards, and decoding is turned off around the
    /// probe because for the duration of it the function claims a range it
    /// does not own.
    pub fn bar(&self, index: usize) -> Option<Bar> {
        if index >= 6 {
            return None;
        }
        let offset = BAR0 + (index as u8) * 4;
        let value = self.read32(offset);
        if value == 0 {
            return None;
        }

        let saved_command = self.command();
        self.write16(COMMAND, saved_command & !(CMD_MEMORY_SPACE | CMD_IO_SPACE));

        let bar;
        if value & 1 == 0 {
            // Memory BAR: bit 0 clear, bits 2..1 the width, bit 3 prefetchable.
            let sixty_four = (value >> 1) & 3 == 2;
            let prefetchable = value & 8 != 0;
            let mut addr = (value & 0xFFFF_FFF0) as u64;
            let mut mask;
            self.write32(offset, 0xFFFF_FFFF);
            mask = (self.read32(offset) & 0xFFFF_FFF0) as u64;
            self.write32(offset, value);
            if sixty_four {
                let high = self.read32(offset + 4);
                addr |= (high as u64) << 32;
                self.write32(offset + 4, 0xFFFF_FFFF);
                mask |= (self.read32(offset + 4) as u64) << 32;
                self.write32(offset + 4, high);
            } else {
                // Sign-extend the 32-bit mask so the inversion below does not
                // report a 4 GiB region for every 32-bit BAR.
                mask |= 0xFFFF_FFFF_0000_0000;
            }
            let size = if mask == 0 { 0 } else { (!mask).wrapping_add(1) };
            bar = Bar::Memory { addr, size, prefetchable };
        } else {
            let port = (value & 0xFFFF_FFFC) as u16;
            self.write32(offset, 0xFFFF_FFFF);
            let mask = self.read32(offset) & 0xFFFF_FFFC;
            self.write32(offset, value);
            let size = if mask == 0 { 0 } else { (!mask).wrapping_add(1) };
            bar = Bar::Io { port, size };
        }

        self.write16(COMMAND, saved_command);
        Some(bar)
    }
}

fn probe(bus: u8, device: u8, function: u8) -> Option<Device> {
    let vendor = read16(bus, device, function, VENDOR_ID);
    // An unpopulated slot leaves the bus floating high.
    if vendor == 0xFFFF || vendor == 0x0000 {
        return None;
    }
    let class_word = read32(bus, device, function, REVISION);
    Some(Device {
        bus,
        device,
        function,
        vendor,
        id: read16(bus, device, function, DEVICE_ID),
        revision: class_word as u8,
        prog_if: (class_word >> 8) as u8,
        subclass: (class_word >> 16) as u8,
        class: (class_word >> 24) as u8,
        irq_line: read8(bus, device, function, INTERRUPT_LINE),
        irq_pin: read8(bus, device, function, INTERRUPT_PIN),
    })
}

/// Every function that answers on any bus.
///
/// This walks all 256 bus numbers rather than following bridges, which costs
/// a few thousand port reads once at boot and finds devices behind a bridge
/// the enumeration would otherwise have to be taught about.
pub fn scan() -> Vec<Device> {
    let mut found = Vec::new();
    for bus in 0..=255u8 {
        for device in 0..32u8 {
            let Some(first) = probe(bus, device, 0) else { continue };
            // Bit 7 of the header type says whether functions 1..7 exist;
            // probing them on a single-function device is not defined to
            // return anything useful.
            let multifunction = read8(bus, device, 0, HEADER_TYPE) & 0x80 != 0;
            found.push(first);
            if !multifunction {
                continue;
            }
            for function in 1..8u8 {
                if let Some(dev) = probe(bus, device, function) {
                    found.push(dev);
                }
            }
        }
    }
    found
}

/// The first function matching a vendor and device identifier, or nothing.
pub fn find(vendor: u16, id: u16) -> Option<Device> {
    for bus in 0..=255u8 {
        for device in 0..32u8 {
            let Some(first) = probe(bus, device, 0) else { continue };
            let multifunction = read8(bus, device, 0, HEADER_TYPE) & 0x80 != 0;
            let last = if multifunction { 8 } else { 1 };
            for function in 0..last {
                let Some(dev) = (if function == 0 { Some(first) } else { probe(bus, device, function) })
                else {
                    continue;
                };
                if dev.vendor == vendor && dev.id == id {
                    return Some(dev);
                }
            }
        }
    }
    None
}

/// Where device register blocks are mapped in the kernel half.
///
/// A base address register names a range of physical address space that is
/// not RAM, so the boot trampoline's direct map does not cover it: the direct
/// map only spans the low 4 GiB of *memory*, and the frame allocator would
/// never hand these addresses out anyway. The registers therefore need
/// mappings of their own, and those need somewhere in the kernel half to go.
///
/// The kernel half is otherwise spoken for at PML4 entry 256 (the direct map,
/// 0xFFFF_8000_...), entry 384 (the heap, 0xFFFF_C000_..., capped at 512 MiB
/// so it stays inside its own PDPT), and entry 511 (the kernel image,
/// 0xFFFF_FFFF_8000_...). Entry 448 is what the address below selects; the
/// boot trampoline leaves it empty and nothing grows into it from either
/// side.
pub const DEVICE_MMIO_BASE: u64 = 0xFFFF_E000_0000_0000;
/// Well inside the single PML4 entry above, so no mapping made here ever
/// needs a second top-level entry: an address space created later copies that
/// entry as it stands and would not see a new one appearing beside it.
pub const DEVICE_MMIO_LIMIT: u64 = DEVICE_MMIO_BASE + 512 * 1024 * 1024;

/// Bump pointer through the window. Device mappings are made once at boot and
/// never taken away, so there is nothing to reclaim.
static MMIO_NEXT: Spinlock<u64> = Spinlock::new(DEVICE_MMIO_BASE);

/// Map `len` bytes of device physical address space and return the kernel
/// address they now answer at.
///
/// Uncached: a register block is not memory. A read has to reach the card to
/// see what the card put there rather than being satisfied from a cache line
/// fetched minutes ago, and a write has to leave the CPU where the program
/// put it rather than being merged with its neighbours. The pages are also
/// marked no-execute, since nothing here is code.
///
/// The mapping goes into the current address space, which at boot is the one
/// every later address space copies its kernel half from. Calling this after
/// a user address space exists would leave that space without the mapping, so
/// devices are brought up before the first process is created.
pub fn map_device(phys: u64, len: u64) -> Option<u64> {
    let offset = phys & (PAGE_SIZE_U64 - 1);
    let base = phys - offset;
    let pages = page_align_up(len + offset) / PAGE_SIZE_U64;
    if pages == 0 {
        return None;
    }

    let mut next = MMIO_NEXT.lock();
    let virt = *next;
    if virt + pages * PAGE_SIZE_U64 > DEVICE_MMIO_LIMIT {
        return None;
    }
    let space = AddressSpace::current();
    for page in 0..pages {
        space
            .map_fixed(
                virt + page * PAGE_SIZE_U64,
                base + page * PAGE_SIZE_U64,
                PRESENT | WRITABLE | NO_CACHE | NO_EXECUTE,
            )
            .ok()?;
    }
    *next = virt + pages * PAGE_SIZE_U64;
    Some(virt + offset)
}
