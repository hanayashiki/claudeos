//! Memory-management checks against the page tables and the frame allocator.
//!
//! These reach past what a program can ask for: a page table entry is the one
//! place a frame reference is recorded where the type system cannot see it, so
//! what becomes of an entry that is written over is only visible from here.
//!
//! Run with `net=test` on the kernel command line. The harness reads one
//! summary line out of the boot, so these are counted into the same one as the
//! protocol checks rather than printing a second.

use crate::arch::paging::{AddressSpace, MapError, NO_EXECUTE, PRESENT, USER, WRITABLE};
use crate::mm::frame;

/// Somewhere in the lower half, clear of where a program is loaded. Nothing is
/// running while these checks are, and the spaces they build are never
/// switched to, so the address only has to be one a walk accepts.
const VIRT: u64 = 0x1000_0000;

pub struct Report {
    pub passed: usize,
    pub failed: usize,
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
}

pub fn run(report: &mut Report) {
    refuses_a_second_mapping(report);
    refusing_leaks_nothing(report);
    replacing_hands_the_old_frame_back(report);
    replacing_nothing_maps_nothing(report);
    replacing_leaks_nothing(report);
    #[cfg(target_arch = "aarch64")]
    device_window::run(report);
    #[cfg(target_arch = "aarch64")]
    invalidation_operands(report);
}

/// The operand a translation invalidation by address takes.
///
/// This is the encoding and not the effect. Bits 47 to 44 of the operand are a
/// hint saying which level of the walk the entry came from, and the core on
/// this board does not implement them, so nothing the machine can be asked
/// afterwards tells an operand that fills them in wrongly from one that leaves
/// them clear. What can be pinned is the number the kernel puts in the
/// register.
#[cfg(target_arch = "aarch64")]
fn invalidation_operands(report: &mut Report) {
    use crate::arch::paging::invalidation_operand;
    /// Bits 47 to 44, which are the level hint rather than any of the address.
    const HINT: u64 = 0xF << 44;
    /// The field holds bits 55 to 12 of the address, so an address masked to
    /// 56 bits and shifted is the whole of what belongs in it.
    const ADDRESS_BITS: u64 = 0x00FF_FFFF_FFFF_FFFF;

    let user = 0x1000_0000u64;
    report.check(
        "a user address is its page number",
        invalidation_operand(user) == user >> 12,
    );
    for (name, kernel) in [
        ("the direct map", crate::mm::HHDM_BASE + 0x1000),
        ("the kernel's own base", crate::mm::KERNEL_VMA + 0x1000),
    ] {
        report.check(
            &alloc::format!("an address in {} names no level", name),
            invalidation_operand(kernel) & HINT == 0,
        );
        report.check(
            &alloc::format!("and carries every address bit the field holds, in {}", name),
            invalidation_operand(kernel) == (kernel & ADDRESS_BITS) >> 12,
        );
    }
}

fn flags() -> u64 {
    PRESENT | WRITABLE | USER | NO_EXECUTE
}

/// An address that already has a mapping is refused, and the frame that was
/// turned away is released rather than left with no owner.
fn refuses_a_second_mapping(report: &mut Report) {
    let Some(space) = AddressSpace::new_user() else {
        report.check("an address space to map into", false);
        return;
    };
    let mapped = space.map_new(VIRT, flags());

    // Nothing may print between the calls below and the counts read after
    // them: printing goes through the kernel heap, which takes frames. The
    // tables down to this address are built by the first call, so the only
    // frame the refused one can take is the page it fails to map.
    let (before, _) = frame::stats();
    let refused = space.map_new(VIRT, flags());
    let (after, _) = frame::stats();
    let still_there = space.translate(VIRT);

    report.check("the first mapping goes in", mapped.is_ok());
    report.check(
        "a second mapping at the same address is refused",
        refused == Err(MapError::AlreadyMapped),
    );
    report.check("the mapping that was there is untouched", still_there == mapped.ok());
    report.check("the frame that was turned away is released", after == before);

    let kept = mapped.unwrap_or(0);
    space.destroy();
    report.check(
        "tearing the space down releases what it held",
        frame::frame_references(kept) == 0,
    );
}

/// A mapping written over an entry that was already present would leave the
/// frame that entry named with nobody to release it: one frame each time. A
/// thousand of them cost none.
fn refusing_leaks_nothing(report: &mut Report) {
    const ROUNDS: usize = 1000;
    let (before, _) = frame::stats();
    let mut reached = 0usize;
    for _ in 0..ROUNDS {
        let Some(space) = AddressSpace::new_user() else { break };
        if space.map_new(VIRT, flags()).is_err() {
            space.destroy();
            break;
        }
        let _ = space.map_new(VIRT, flags());
        space.destroy();
        reached += 1;
    }
    let (after, _) = frame::stats();
    report.check(
        "a thousand refused mappings cost no memory",
        reached == ROUNDS && after == before,
    );
}

/// What replaces a mapping is one store, so the address it covers is never
/// without one. A machine with one processor cannot be caught in the middle
/// of a store from here; what can be pinned is the accounting either side of
/// it. The old entry's reference comes back in hand rather than being dropped
/// where the caller cannot see it, which is what a copy-on-write fault needs:
/// the frame it was sharing is still held by the address spaces that share it.
fn replacing_hands_the_old_frame_back(report: &mut Report) {
    let Some(space) = AddressSpace::new_user() else {
        report.check("an address space to map into", false);
        return;
    };
    let (Ok(old), Some(fresh)) = (space.map_new(VIRT, flags()), frame::alloc_zeroed()) else {
        report.check("a mapping to replace and a frame to replace it with", false);
        space.destroy();
        return;
    };
    let new = fresh.addr();

    // Every reading is taken before anything is printed, for the reason above.
    let handed_back =
        crate::sync::without_interrupts(|irq| space.replace(VIRT, fresh, flags(), irq));
    let reaches = space.translate(VIRT);
    let named = handed_back.as_ref().map(|frame| frame.addr());
    let held_while_in_hand = frame::frame_references(old);
    drop(handed_back);
    let held_after = frame::frame_references(old);

    report.check("the address reaches the replacement", reaches == Some(new));
    report.check("the frame that was there is handed back", named == Some(old));
    report.check("it is still held while the handle is", held_while_in_hand == 1);
    report.check("and dropping the handle releases it", held_after == 0);

    space.destroy();
    report.check(
        "the replacement goes when the space does",
        frame::frame_references(new) == 0,
    );
}

/// An address with nothing at it has no mapping to replace. Writing one there
/// would be creating a mapping, which is what `map_new` is for, and it would
/// hand back a frame reference that no entry was holding.
fn replacing_nothing_maps_nothing(report: &mut Report) {
    let Some(space) = AddressSpace::new_user() else {
        report.check("an address space to map into", false);
        return;
    };
    let Some(fresh) = frame::alloc_zeroed() else {
        report.check("a frame to offer", false);
        space.destroy();
        return;
    };
    let offered = fresh.addr();

    let handed_back =
        crate::sync::without_interrupts(|irq| space.replace(VIRT, fresh, flags(), irq));
    let refused = handed_back.is_none();
    drop(handed_back);
    let reaches = space.translate(VIRT);
    let released = frame::frame_references(offered);

    report.check("replacing what is not there is refused", refused);
    report.check("and leaves the address with nothing at it", reaches.is_none());
    report.check("and releases the frame it was offered", released == 0);
    space.destroy();
}

/// A replacement that dropped the old entry's reference on the floor would
/// cost one frame each time, and one that took an extra would cost one for
/// every frame it handed back. A thousand of them cost neither.
fn replacing_leaks_nothing(report: &mut Report) {
    const ROUNDS: usize = 1000;
    let (before, _) = frame::stats();
    let mut reached = 0usize;
    for _ in 0..ROUNDS {
        let Some(space) = AddressSpace::new_user() else { break };
        if space.map_new(VIRT, flags()).is_err() {
            space.destroy();
            break;
        }
        let Some(fresh) = frame::alloc_zeroed() else {
            space.destroy();
            break;
        };
        drop(crate::sync::without_interrupts(|irq| {
            space.replace(VIRT, fresh, flags(), irq)
        }));
        space.destroy();
        reached += 1;
    }
    let (after, _) = frame::stats();
    report.check(
        "a thousand replacements cost no memory",
        reached == ROUNDS && after == before,
    );
}

/// What the frame allocator makes of the memory map a 4 GiB Raspberry Pi 4
/// hands over, which is not the map the emulated machine hands over.
///
/// The emulated board reports 960 MiB in one bank and never reaches the
/// gigabyte the peripherals are in, so nothing here can be seen by booting.
/// The map is the input, though, and the map comes out of a device tree, so a
/// tree describing the board the kernel is going to meet is a real input that
/// can be built and read here.
///
/// A 4 GiB board reports its memory in banks, the second of which runs from
/// 1 GiB up to the peripheral base at 0xFC000000. The direct map covers that
/// address and everything above it with device attributes, so a frame there
/// has no cacheable alias: copying it faults on the unaligned accesses a copy
/// makes, a page table in it is written through one attribute and walked
/// through another, and a page of it given to the heap is written through two.
#[cfg(target_arch = "aarch64")]
mod device_window {
    use super::Report;
    use crate::arch::{fdt, DEVICE_PHYS_BASE};
    use crate::boot::BootInfo;
    use crate::mm::frame::{self, Frame};
    use crate::mm::{PAGE_SIZE, PAGE_SIZE_U64};
    use alloc::vec::Vec;

    const MAGIC: u32 = 0xD00D_FEED;
    const BEGIN_NODE: u32 = 1;
    const END_NODE: u32 = 2;
    const PROP: u32 = 3;
    const END: u32 = 9;
    const VERSION: u32 = 17;
    const LAST_COMPATIBLE_VERSION: u32 = 16;

    /// Where the firmware on a Pi 4 splits the first gigabyte with the video
    /// core, and so where the first bank ends.
    const FIRST_BANK_END: u64 = 0x3B40_0000;
    /// Where the second bank starts, which is the same on every size of board.
    const SECOND_BANK_START: u64 = 0x4000_0000;
    /// What is left of a 4 GiB board once the two banks below four gigabytes
    /// are counted, reported in a third bank above them.
    const THIRD_BANK: (u64, u64) = (0x1_0000_0000, 0x8C0_0000);

    /// A tree carrying nothing but a memory node, whose second bank ends where
    /// the caller says. Only the memory map is read out of it, so the rest of
    /// what a board's tree holds would not be looked at.
    fn memory_tree(second_bank_end: u64) -> Vec<u8> {
        let mut structure: Vec<u8> = Vec::new();
        let mut strings: Vec<u8> = Vec::new();

        let be32 = |out: &mut Vec<u8>, value: u32| out.extend_from_slice(&value.to_be_bytes());
        let intern = |strings: &mut Vec<u8>, name: &str| {
            let offset = strings.len() as u32;
            strings.extend_from_slice(name.as_bytes());
            strings.push(0);
            offset
        };
        let prop = |structure: &mut Vec<u8>, strings: &mut Vec<u8>, name: &str, value: &[u8]| {
            let offset = intern(strings, name);
            structure.extend_from_slice(&PROP.to_be_bytes());
            structure.extend_from_slice(&(value.len() as u32).to_be_bytes());
            structure.extend_from_slice(&offset.to_be_bytes());
            structure.extend_from_slice(value);
            while structure.len() % 4 != 0 {
                structure.push(0);
            }
        };

        // The root, whose cell counts say how wide the numbers in its
        // children's `reg` are: two cells each, which is what a board with
        // memory above four gigabytes has to use.
        be32(&mut structure, BEGIN_NODE);
        structure.push(0);
        while structure.len() % 4 != 0 {
            structure.push(0);
        }
        prop(&mut structure, &mut strings, "#address-cells", &2u32.to_be_bytes());
        prop(&mut structure, &mut strings, "#size-cells", &2u32.to_be_bytes());

        be32(&mut structure, BEGIN_NODE);
        structure.extend_from_slice(b"memory@0\0");
        while structure.len() % 4 != 0 {
            structure.push(0);
        }
        prop(&mut structure, &mut strings, "device_type", b"memory\0");
        let mut reg: Vec<u8> = Vec::new();
        for &(base, size) in &[
            (0u64, FIRST_BANK_END),
            (SECOND_BANK_START, second_bank_end - SECOND_BANK_START),
            THIRD_BANK,
        ] {
            reg.extend_from_slice(&base.to_be_bytes());
            reg.extend_from_slice(&size.to_be_bytes());
        }
        prop(&mut structure, &mut strings, "reg", &reg);
        be32(&mut structure, END_NODE);

        be32(&mut structure, END_NODE);
        be32(&mut structure, END);

        // The header, then an empty list of reserved ranges ending in a pair
        // of zeroes, then the two blocks.
        let reserve_at = 40usize;
        let struct_at = reserve_at + 16;
        let strings_at = struct_at + structure.len();
        let total = strings_at + strings.len();

        let mut blob: Vec<u8> = Vec::with_capacity(total);
        for word in [
            MAGIC,
            total as u32,
            struct_at as u32,
            strings_at as u32,
            reserve_at as u32,
            VERSION,
            LAST_COMPATIBLE_VERSION,
            0,
            strings.len() as u32,
            structure.len() as u32,
        ] {
            blob.extend_from_slice(&word.to_be_bytes());
        }
        blob.resize(struct_at, 0);
        blob.extend_from_slice(&structure);
        blob.extend_from_slice(&strings);
        blob
    }

    /// Frames taken from the real allocator, given back when the check is
    /// over. The synthetic map describes memory this machine does not have, so
    /// the blob and the allocator's own bitmap have to sit in memory it does.
    struct Borrowed {
        first: u64,
        count: usize,
    }

    impl Borrowed {
        fn take(bytes: usize) -> Option<Borrowed> {
            let count = (crate::mm::page_align_up(bytes as u64) / PAGE_SIZE_U64) as usize;
            let first = frame::alloc_contiguous(count)?;
            Some(Borrowed { first, count })
        }
    }

    impl Drop for Borrowed {
        fn drop(&mut self) {
            for i in 0..self.count {
                drop(unsafe { Frame::from_recorded(self.first + i as u64 * PAGE_SIZE_U64) });
            }
        }
    }

    /// Read a tree built here into a fresh description of a machine. The
    /// machine's own tree is left alone: every device lookup after this goes
    /// through it.
    fn map_from(tree: &[u8]) -> Option<(BootInfo, Borrowed)> {
        let page = Borrowed::take(tree.len().max(PAGE_SIZE))?;
        unsafe {
            let at = crate::mm::phys_to_virt(page.first) as *mut u8;
            core::ptr::write_bytes(at, 0, PAGE_SIZE);
            core::ptr::copy_nonoverlapping(tree.as_ptr(), at, tree.len());
        }
        let mut boot = BootInfo::new();
        if !fdt::read_into(page.first, &mut boot) {
            return None;
        }
        Some((boot, page))
    }

    /// The two banks of a 4 GiB board that are under four gigabytes, in
    /// frames. The third is above the direct map and cannot be reached at all.
    const EXPECTED_FRAMES: usize =
        ((FIRST_BANK_END + (DEVICE_PHYS_BASE - SECOND_BANK_START)) / PAGE_SIZE_U64) as usize;

    pub fn run(report: &mut Report) {
        reports_only_what_it_can_touch(report);
        hands_out_nothing_in_the_window(report);
    }

    /// A 4 GiB board's own map: everything below the peripherals is memory and
    /// is counted, and the hole between the two banks is neither.
    fn reports_only_what_it_can_touch(report: &mut Report) {
        let tree = memory_tree(DEVICE_PHYS_BASE);
        let Some((boot, _blob)) = map_from(&tree) else {
            report.check("a 4 GiB board's memory map", false);
            return;
        };
        let limit = frame::usable_limit(&boot);
        let Some(metadata) = Borrowed::take(frame::metadata_bytes(limit)) else {
            report.check("a 4 GiB board's memory map", false);
            return;
        };
        let alloc = frame::build(&boot, limit, metadata.first);
        report.check("a 4 GiB board's memory map", true);
        report.check(
            "the total is the memory under the peripherals and nothing else",
            alloc.total_frames() == EXPECTED_FRAMES,
        );
        // 3956 MiB rather than the 2996 MiB that clamping to the start of the
        // last gigabyte would leave.
        report.check(
            "which is 3956 MiB",
            alloc.total_frames() * PAGE_SIZE == 3956 * 1024 * 1024,
        );
    }

    /// A map that claims the whole of the fourth gigabyte is memory. The
    /// allocator has to refuse the part of it the direct map covers with
    /// device attributes whatever the map says, because the attribute is what
    /// makes those frames unusable, not the firmware's opinion of them.
    fn hands_out_nothing_in_the_window(report: &mut Report) {
        let tree = memory_tree(crate::mm::HHDM_LIMIT);
        let Some((boot, _blob)) = map_from(&tree) else {
            report.check("a map that claims the device window is memory", false);
            return;
        };
        let limit = frame::usable_limit(&boot);
        let Some(metadata) = Borrowed::take(frame::metadata_bytes(limit)) else {
            report.check("a map that claims the device window is memory", false);
            return;
        };
        let mut alloc = frame::build(&boot, limit, metadata.first);
        report.check("a map that claims the device window is memory", true);

        // Take every frame it will give, which is the only way to say that
        // none of them is in the window rather than that none of the first few
        // is.
        let mut handed = 0usize;
        let mut highest = 0u64;
        while let Some(addr) = alloc.alloc() {
            handed += 1;
            highest = highest.max(addr);
        }
        report.check("it hands out nothing at or above the peripherals", highest < DEVICE_PHYS_BASE);
        // And stops just short of them rather than well short: the memory
        // between three gigabytes and the peripherals is 960 MiB that a
        // clamp to the start of the gigabyte would throw away.
        report.check(
            "and hands out the memory right up to them",
            highest >= DEVICE_PHYS_BASE - 2 * 1024 * 1024,
        );
        report.check("emptied, it has nothing left", alloc.free_frames() == 0);
        report.check("and gave out no more than the map described", handed <= EXPECTED_FRAMES);
    }
}
