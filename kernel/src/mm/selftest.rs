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
    the_margin_keeps_an_allocation_from_mapping(report);
    refuses_a_second_mapping(report);
    refusing_leaks_nothing(report);
    replacing_hands_the_old_frame_back(report);
    replacing_nothing_maps_nothing(report);
    replacing_leaks_nothing(report);
    a_clone_reaches_a_shared_page_through_tables_of_its_own(report);
    #[cfg(target_arch = "aarch64")]
    device_window::run(report);
    #[cfg(target_arch = "aarch64")]
    invalidation_operands(report);
}

/// An allocation made inside another lock's critical section must not be the
/// one that maps pages: the lock masks interrupts over the top of the mapping,
/// and the heap cannot see who is holding what.
///
/// The free list is driven down past the margin first, which is the state the
/// defect needs. Topping up from there -- which is what the way out of a
/// system call does -- has to leave enough that the allocation below, made
/// with interrupts masked as a lock would leave them, moves nothing.
///
/// Without the top-up the last two checks fail: the ballast leaves less free
/// than the block asks for, so the block is answered by mapping eight
/// megabytes, which held the timer off for 6.1 ms on x86-64 and 14.5 ms on
/// aarch64 when it was measured that way.
fn the_margin_keeps_an_allocation_from_mapping(report: &mut Report) {
    use crate::mm::{heap, HEAP_MARGIN};
    use alloc::vec::Vec;

    /// Larger than what the ballast leaves free, so a block the free list can
    /// answer is one the margin paid for and not one it happened to have.
    const BLOCK: usize = 1024 * 1024;

    // Each round takes all but a little of what is free, so this converges on
    // an empty free list rather than stepping towards it by a fixed size. The
    // bound is there because an allocation can grow the heap and put the free
    // bytes back.
    let mut ballast: Vec<Vec<u8>> = Vec::new();
    for _ in 0..16 {
        let (used, total) = heap::stats();
        let free = total - used;
        if free <= BLOCK / 2 {
            break;
        }
        ballast.push(alloc::vec![0u8; (free - BLOCK / 2).min(4 * BLOCK)]);
    }
    let (used, total) = heap::stats();
    report.check("the free list can be emptied", total - used < BLOCK);

    heap::top_up();
    let (used, total) = heap::stats();
    report.check("topping up puts the margin back", total - used >= HEAP_MARGIN);

    let mapped = heap::mapped_bytes();
    let block = crate::sync::without_interrupts(|_| alloc::vec![0u8; BLOCK]);
    report.check(
        "an allocation made with interrupts masked maps nothing",
        heap::mapped_bytes() == mapped,
    );

    drop(block);
    drop(ballast);
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

/// Taking the last page out of a table gives the table back, and a table is
/// safe to give back because a fork does not share one.
///
/// The two are one decision: freeing a table another address space still
/// named would take that space's mappings away with it. The counts say which
/// it is. A clone of a space with one page in it takes four frames -- its top
/// table and the three under it -- and shares the page rather than the tables,
/// so taking that one page away hands back exactly the three and leaves the
/// page where the space it was cloned from has it.
fn a_clone_reaches_a_shared_page_through_tables_of_its_own(report: &mut Report) {
    let Some(parent) = AddressSpace::new_user() else {
        report.check("an address space to map into", false);
        return;
    };
    let Ok(page) = parent.map_new(VIRT, flags()) else {
        report.check("a page to share", false);
        parent.destroy();
        return;
    };

    // Nothing may print between the counts below: printing goes through the
    // kernel heap, which takes frames.
    let (mapped, _) = frame::stats();
    let Some(child) = AddressSpace::new_user() else {
        report.check("a second address space", false);
        parent.destroy();
        return;
    };
    let cloned = child.clone_user_from(&parent).is_ok();
    let (after_clone, _) = frame::stats();
    let shares = frame::frame_references(page);
    drop(crate::sync::without_interrupts(|irq| child.unmap(VIRT, irq)));
    let (after_unmap, _) = frame::stats();
    let parent_reaches = parent.translate(VIRT);
    let held = frame::frame_references(page);

    report.check("a clone of an address space is built", cloned);
    report.check(
        "it reaches the page through tables of its own",
        after_clone == mapped + 4,
    );
    report.check("and holds a share of the page itself", shares == 2);
    report.check(
        "taking its only page away gives those tables back",
        after_unmap == mapped + 1,
    );
    report.check(
        "and leaves the page where the space it was cloned from has it",
        parent_reaches == Some(page) && held == 1,
    );

    child.destroy();
    parent.destroy();
    report.check("both spaces release what is left", frame::frame_references(page) == 0);
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

    /// A tree written node by node: the structure block and the strings block
    /// it names properties out of.
    struct Writer {
        structure: Vec<u8>,
        strings: Vec<u8>,
    }

    impl Writer {
        fn new() -> Writer {
            Writer { structure: Vec::new(), strings: Vec::new() }
        }

        fn word(&mut self, value: u32) {
            self.structure.extend_from_slice(&value.to_be_bytes());
        }

        fn pad(&mut self) {
            while self.structure.len() % 4 != 0 {
                self.structure.push(0);
            }
        }

        fn begin(&mut self, name: &str) {
            self.word(BEGIN_NODE);
            self.structure.extend_from_slice(name.as_bytes());
            self.structure.push(0);
            self.pad();
        }

        fn end(&mut self) {
            self.word(END_NODE);
        }

        fn prop(&mut self, name: &str, value: &[u8]) {
            let offset = self.strings.len() as u32;
            self.strings.extend_from_slice(name.as_bytes());
            self.strings.push(0);
            self.word(PROP);
            self.word(value.len() as u32);
            self.word(offset);
            self.structure.extend_from_slice(value);
            self.pad();
        }

        /// A `reg` of two-cell addresses and two-cell sizes.
        fn reg(&mut self, entries: &[(u64, u64)]) {
            let mut reg: Vec<u8> = Vec::new();
            for &(base, size) in entries {
                reg.extend_from_slice(&base.to_be_bytes());
                reg.extend_from_slice(&size.to_be_bytes());
            }
            self.prop("reg", &reg);
        }

        /// The header, then an empty list of reserved ranges ending in a pair
        /// of zeroes, then the two blocks.
        fn finish(mut self) -> Vec<u8> {
            self.word(END);
            let reserve_at = 40usize;
            let struct_at = reserve_at + 16;
            let strings_at = struct_at + self.structure.len();
            let total = strings_at + self.strings.len();

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
                self.strings.len() as u32,
                self.structure.len() as u32,
            ] {
                blob.extend_from_slice(&word.to_be_bytes());
            }
            blob.resize(struct_at, 0);
            blob.extend_from_slice(&self.structure);
            blob.extend_from_slice(&self.strings);
            blob
        }
    }

    /// The root, whose cell counts say how wide the numbers in its children's
    /// `reg` are: two cells each, which is what a board with memory above four
    /// gigabytes has to use.
    fn root() -> Writer {
        let mut tree = Writer::new();
        tree.begin("");
        tree.prop("#address-cells", &2u32.to_be_bytes());
        tree.prop("#size-cells", &2u32.to_be_bytes());
        tree
    }

    /// A tree carrying nothing but a memory node, whose second bank ends where
    /// the caller says. Only the memory map is read out of it, so the rest of
    /// what a board's tree holds would not be looked at.
    fn memory_tree(second_bank_end: u64) -> Vec<u8> {
        let mut tree = root();
        tree.begin("memory@0");
        tree.prop("device_type", b"memory\0");
        tree.reg(&[
            (0u64, FIRST_BANK_END),
            (SECOND_BANK_START, second_bank_end - SECOND_BANK_START),
            THIRD_BANK,
        ]);
        tree.end();
        tree.end();
        tree.finish()
    }

    /// Memory of the tree `reserving_tree` writes.
    const RESERVING_MEMORY: u64 = 0x0800_0000;
    /// The ranges its `/reserved-memory` children name and leave switched on:
    /// one of a megabyte with `no-map`, and a node of two entries, the first of
    /// which starts and ends inside a page.
    const RESERVING_NAMED: [(u64, u64); 3] =
        [(0x0500_0000, 0x10_0000), (0x0600_0c00, 0x1800), (0x0680_0000, 0x2000)];
    /// The range of its child that is switched off.
    const RESERVING_DISABLED: (u64, u64) = (0x0700_0000, 0x10_0000);
    /// Where its child whose size runs past the top of the address space
    /// starts.
    const RESERVING_TO_THE_TOP: u64 = 0x07f0_0000;

    /// A tree for 128 MiB whose `/reserved-memory` holds what the Pi's can
    /// once the firmware has filled it in -- a range with `no-map`, a pool to
    /// be allocated, a node switched off -- and a node with two ranges, and one
    /// whose size runs past the top of the address space.
    fn reserving_tree() -> Vec<u8> {
        let mut tree = root();
        tree.begin("reserved-memory");
        tree.prop("#address-cells", &2u32.to_be_bytes());
        tree.prop("#size-cells", &2u32.to_be_bytes());
        tree.prop("ranges", &[]);

        tree.begin("linux,cma");
        tree.prop("size", &0x0100_0000u64.to_be_bytes());
        tree.prop("reusable", &[]);
        tree.end();

        tree.begin("firmware@5000000");
        tree.reg(&RESERVING_NAMED[..1]);
        tree.prop("no-map", &[]);
        tree.end();

        tree.begin("split@6000c00");
        tree.reg(&RESERVING_NAMED[1..]);
        tree.end();

        tree.begin("off@7000000");
        tree.reg(&[RESERVING_DISABLED]);
        tree.prop("status", b"disabled\0");
        tree.end();

        tree.begin("top@7f00000");
        tree.reg(&[(RESERVING_TO_THE_TOP, u64::MAX)]);
        tree.end();
        tree.end();

        // After `/reserved-memory`, where the Pi's tree has it.
        tree.begin("memory@0");
        tree.prop("device_type", b"memory\0");
        tree.reg(&[(0, RESERVING_MEMORY)]);
        tree.end();
        tree.end();
        tree.finish()
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
        if fdt::read_into(page.first, &mut boot).is_none() {
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
        hands_out_nothing_the_firmware_reserved(report);
    }

    /// The ranges a tree's `/reserved-memory` names, read by the kernel's own
    /// reader out of memory and applied by the allocator. Every frame the
    /// allocator will give is taken, so the check covers all of them rather
    /// than the first few.
    fn hands_out_nothing_the_firmware_reserved(report: &mut Report) {
        let tree = reserving_tree();
        let Some((boot, _blob)) = map_from(&tree) else {
            report.check("a tree whose /reserved-memory names ranges", false);
            return;
        };
        let limit = frame::usable_limit(&boot);
        let Some(metadata) = Borrowed::take(frame::metadata_bytes(limit)) else {
            report.check("a tree whose /reserved-memory names ranges", false);
            return;
        };
        let mut alloc = frame::build(&boot, limit, metadata.first);
        report.check("a tree whose /reserved-memory names ranges", true);

        let touches = |addr: u64, (start, size): (u64, u64)| addr < start + size && start < addr + PAGE_SIZE_U64;
        let mut named = 0usize;
        let mut disabled = 0usize;
        let mut highest = 0u64;
        while let Some(addr) = alloc.alloc() {
            if RESERVING_NAMED.iter().any(|&range| touches(addr, range)) {
                named += 1;
            }
            if touches(addr, RESERVING_DISABLED) {
                disabled += 1;
            }
            highest = highest.max(addr);
        }
        report.check("it hands out no page holding any part of a range they name", named == 0);
        report.check(
            "nor anything of the range that runs to the top of the address space",
            highest < RESERVING_TO_THE_TOP,
        );
        // Most of its megabyte: the blob and the metadata borrowed from the
        // real allocator for this check, eighteen pages, could sit in it.
        report.check(
            "and hands out the range of the node that is switched off",
            disabled >= (RESERVING_DISABLED.1 / PAGE_SIZE_U64) as usize - 32,
        );
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
