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
    let (Some(first), Some(second)) = (frame::alloc_zeroed(), frame::alloc_zeroed()) else {
        report.check("two frames to map", false);
        space.destroy();
        return;
    };
    let kept = first.addr();
    let turned_away = second.addr();

    // Nothing may print between the calls below and the counts read after
    // them: printing goes through the kernel heap, which takes frames.
    let mapped = space.map(VIRT, first, flags());
    let refused = space.map(VIRT, second, flags());
    let still_there = space.translate(VIRT);
    let released = frame::frame_references(turned_away);

    report.check("the first mapping goes in", mapped.is_ok());
    report.check(
        "a second mapping at the same address is refused",
        refused == Err(MapError::AlreadyMapped),
    );
    report.check("the mapping that was there is untouched", still_there == Some(kept));
    report.check("the frame that was turned away is released", released == 0);

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
        let (Some(first), Some(second)) = (frame::alloc_zeroed(), frame::alloc_zeroed()) else {
            space.destroy();
            break;
        };
        if space.map(VIRT, first, flags()).is_err() {
            space.destroy();
            break;
        }
        let _ = space.map(VIRT, second, flags());
        space.destroy();
        reached += 1;
    }
    let (after, _) = frame::stats();
    report.check(
        "a thousand refused mappings cost no memory",
        reached == ROUNDS && after == before,
    );
}
