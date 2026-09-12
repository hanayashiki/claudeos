//! The GIC-400 this board routes every device interrupt through.
//!
//! Two blocks of registers: a distributor, which decides what is delivered and
//! to which core, and a per-core interface, which the handler reads to find
//! out what arrived and writes to say it has finished. Both are reached
//! through the direct map.

use crate::mm::phys_to_virt;

const GIC_BASE: u64 = 0xFF84_0000;
const DISTRIBUTOR: u64 = GIC_BASE + 0x1000;
const INTERFACE: u64 = GIC_BASE + 0x2000;

const GICD_CTLR: u64 = 0x000;
const GICD_TYPER: u64 = 0x004;
const GICD_ISENABLER: u64 = 0x100;
const GICD_ICENABLER: u64 = 0x180;
const GICD_ICPENDR: u64 = 0x280;
const GICD_IPRIORITYR: u64 = 0x400;
const GICD_ITARGETSR: u64 = 0x800;
const GICD_ICFGR: u64 = 0xC00;

const GICC_CTLR: u64 = 0x000;
const GICC_PMR: u64 = 0x004;
const GICC_BPR: u64 = 0x008;
const GICC_IAR: u64 = 0x00C;
const GICC_EOIR: u64 = 0x010;

/// What the interface reports when it has nothing to give.
pub const SPURIOUS: u32 = 1023;

/// The first interrupt number that is not per-core: everything below this is a
/// software-generated or private interrupt and belongs to one core.
const SHARED_BASE: u32 = 32;

#[inline]
unsafe fn dist_read(offset: u64) -> u32 {
    core::ptr::read_volatile(phys_to_virt(DISTRIBUTOR + offset) as *const u32)
}

#[inline]
unsafe fn dist_write(offset: u64, value: u32) {
    core::ptr::write_volatile(phys_to_virt(DISTRIBUTOR + offset) as *mut u32, value);
}

#[inline]
unsafe fn cpu_read(offset: u64) -> u32 {
    core::ptr::read_volatile(phys_to_virt(INTERFACE + offset) as *const u32)
}

#[inline]
unsafe fn cpu_write(offset: u64, value: u32) {
    core::ptr::write_volatile(phys_to_virt(INTERFACE + offset) as *mut u32, value);
}

/// How many interrupt numbers this distributor implements.
pub fn line_count() -> u32 {
    unsafe { (32 * ((dist_read(GICD_TYPER) & 0x1F) + 1)).min(1020) }
}

/// Bring the controller up with every line masked.
pub fn init() {
    unsafe {
        dist_write(GICD_CTLR, 0);
        let lines = line_count();

        // Everything off, nothing pending, level-triggered, aimed at this
        // core, and all at the same priority. The per-core interrupts below
        // 32 are not ours to configure: their enable, target and trigger
        // registers belong to the core that takes them.
        for word in (SHARED_BASE / 32)..(lines / 32) {
            dist_write(GICD_ICENABLER + (word * 4) as u64, 0xFFFF_FFFF);
            dist_write(GICD_ICPENDR + (word * 4) as u64, 0xFFFF_FFFF);
        }
        for word in (SHARED_BASE / 16)..(lines / 16) {
            dist_write(GICD_ICFGR + (word * 4) as u64, 0);
        }
        // One byte per line in both of these, so a word covers four lines and
        // every byte of it gets the same value.
        for word in (SHARED_BASE / 4)..(lines / 4) {
            dist_write(GICD_IPRIORITYR + (word * 4) as u64, 0xA0A0_A0A0);
            dist_write(GICD_ITARGETSR + (word * 4) as u64, 0x0101_0101);
        }

        dist_write(GICD_CTLR, 1);

        // Accept every priority, no sub-priority grouping.
        cpu_write(GICC_PMR, 0xF0);
        cpu_write(GICC_BPR, 0);
        cpu_write(GICC_CTLR, 1);
    }
}

pub fn unmask(irq: u8) {
    let line = irq as u32;
    unsafe {
        dist_write(GICD_ISENABLER + ((line / 32) * 4) as u64, 1 << (line % 32));
    }
}

pub fn mask(irq: u8) {
    let line = irq as u32;
    unsafe {
        dist_write(GICD_ICENABLER + ((line / 32) * 4) as u64, 1 << (line % 32));
    }
}

/// Take the next interrupt from the controller. Until `end_of_interrupt` is
/// called with what this returned, no further interrupt of the same or lower
/// priority is delivered.
pub fn acknowledge() -> u32 {
    unsafe { cpu_read(GICC_IAR) & 0x3FF }
}

pub fn end_of_interrupt(irq: u8) {
    unsafe { cpu_write(GICC_EOIR, irq as u32) };
}
