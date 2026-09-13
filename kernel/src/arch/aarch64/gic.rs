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
const GICD_IGROUPR: u64 = 0x080;
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

/// The first of the four numbers that are not interrupts. 1023 means there was
/// nothing to give; 1022 means there was something but this interface was not
/// allowed to claim it, which is what a group-zero acknowledge gets when the
/// interrupt is in group one. Neither can be ended, so neither may be treated
/// as a line number.
const NOT_A_LINE: u32 = 1020;

/// The one of those four that means there was an interrupt and this interface
/// was not allowed to claim it, because it is in the other group.
const NOT_OURS: u32 = 1022;

/// The widest interrupt number the acknowledge register can report.
const ID_MASK: u32 = 0x3FF;

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

        // Every line in group zero, said out loud rather than left at whatever
        // the controller reset to. The interface below is enabled with a
        // single bit, and the two have to agree: an acknowledge from an
        // interface enabled for one group cannot claim an interrupt in the
        // other, and rather than failing it hands back 1022. The handler then
        // has an interrupt it can neither name nor finish, and it arrives
        // again for ever.
        for word in 0..(lines / 32) {
            dist_write(GICD_IGROUPR + (word * 4) as u64, 0);
        }
        report_grouping();

        dist_write(GICD_CTLR, 1);

        // Accept every priority, no sub-priority grouping.
        cpu_write(GICC_PMR, 0xF0);
        cpu_write(GICC_BPR, 0);
        cpu_write(GICC_CTLR, 1);
    }
}

/// Say at boot whether the grouping above is this kernel's to set, because
/// the code above cannot tell on its own.
///
/// Which group the single enable bit in `GICC_CTLR` names depends on which
/// view of the controller this is: the Secure view's bit zero is group zero,
/// the Non-secure view's is group one. A GIC-400 does implement the two
/// groups, and this kernel runs Non-secure, where the group registers are the
/// Secure view's -- writes to them do nothing and reads give zero whatever
/// the grouping really is. So correct operation there rests on the board's
/// boot stub having put every line in group one, which the Pi's does. The
/// type register says whether the groups exist, and a write read back says
/// whether this view may set them; when either answers otherwise than the
/// code above assumes, the grouping came from the firmware and an
/// acknowledge that hands back 1022 rather than a line number is that
/// assumption failing.
unsafe fn report_grouping() {
    let extensions = dist_read(GICD_TYPER) & (1 << 10) != 0;
    // Lines 32 to 63: shared, and every one of them masked at this point, so
    // moving them between groups for the length of a read delivers nothing.
    let probe = GICD_IGROUPR + 4;
    dist_write(probe, 0xFFFF_FFFF);
    let writable = dist_read(probe) != 0;
    dist_write(probe, 0);
    if extensions || !writable {
        crate::println!(
            "[gic] grouping is the firmware's: security extensions {}, group register {}",
            if extensions { "present" } else { "absent" },
            if writable { "writable" } else { "ignores writes" },
        );
    }
}

pub fn unmask(irq: u8) {
    let line = irq as u32;
    unsafe {
        dist_write(GICD_ISENABLER + ((line / 32) * 4) as u64, 1 << (line % 32));
    }
}

pub fn mask(irq: u8) {
    disable_line(irq as u32);
}

/// Stop the distributor delivering `line`. Takes the full width the
/// acknowledge register reports rather than a handler-table index, because the
/// lines that have to be stopped this way are the ones with no handler.
fn disable_line(line: u32) {
    unsafe {
        dist_write(GICD_ICENABLER + ((line / 32) * 4) as u64, 1 << (line % 32));
    }
}

/// What an acknowledge got.
pub enum Acknowledged {
    /// A line, now claimed by this interface.
    Line(Claimed),
    /// There was an interrupt and this interface was not allowed to claim it,
    /// because it is in the other group. Nothing was claimed.
    NotOurs(Ended),
    /// There was nothing to claim.
    Spurious(Ended),
}

/// A line this interface has claimed.
///
/// Claiming raises the running priority to the line's, and the controller
/// delivers nothing of that priority or lower -- the timer and the console
/// among them -- until the claim is ended. So it has to be ended on every path
/// out, and ending is the only thing that can be done with one: `end` and
/// `mask_and_end` consume the claim, and what they give back is the only way
/// to produce an `Ended`. A caller that owes one cannot drop a claim instead.
#[must_use = "a line that is claimed and never ended stops every interrupt of               its priority or lower"]
pub struct Claimed {
    line: u32,
}

impl Claimed {
    /// Which line was claimed.
    pub fn line(&self) -> u32 {
        self.line
    }

    /// Tell the controller the handler is finished, which drops the running
    /// priority back to what it was.
    pub fn end(self) -> Ended {
        // The number the acknowledge handed out, rather than the line the
        // caller believes it handled: the controller accepts no other.
        unsafe { cpu_write(GICC_EOIR, self.line) };
        Ended(())
    }

    /// Stop the distributor delivering this line, then end the claim.
    ///
    /// For a line nothing will ever clear. Ending it alone drops the running
    /// priority, and a level-triggered line that is still asserting is then
    /// delivered again at once and for ever.
    pub fn mask_and_end(self) -> Ended {
        disable_line(self.line);
        self.end()
    }
}

/// That an acknowledge has been finished with: either the line it claimed was
/// ended, or it claimed nothing. Built here and nowhere else.
pub struct Ended(#[allow(dead_code)] ());

/// Take the next interrupt from the controller.
pub fn acknowledge() -> Acknowledged {
    let id = unsafe { cpu_read(GICC_IAR) & ID_MASK };
    match id {
        NOT_OURS => Acknowledged::NotOurs(Ended(())),
        NOT_A_LINE..=ID_MASK => Acknowledged::Spurious(Ended(())),
        line => Acknowledged::Line(Claimed { line }),
    }
}

/// End `claim` if there still is one. Nothing left to end means someone took
/// the claim and ended it, which is the only way one can be got rid of.
pub fn end_outstanding(claim: Option<Claimed>) -> Ended {
    match claim {
        Some(claim) => claim.end(),
        None => Ended(()),
    }
}
