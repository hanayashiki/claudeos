//! The kernel's page-table walk, run on the Mac against a `Vec` of memory.
//!
//! These are the rules the races in docs/audit-2026-09-18-unsafe.md broke,
//! stated as things the walk does rather than as things a boot does not do: a
//! publish refuses an address that already has something at it and hands the
//! page back; an unmap gives back the frame and the tables the removal
//! emptied, and nothing else; a fork shares pages rather than tables and
//! leaves both sides needing a copy before a write; a teardown releases
//! everything. Each ends by counting the frames still handed out, which is
//! what says nothing was leaked and nothing freed twice.
//!
//! What a boot cannot show and this can is the accounting: the kernel's frame
//! allocator hands the same frame out again immediately, so a reference
//! released once too often reads as a working system until something else
//! writes the page. Here it is a count.

use mmtest::host::{page_flags, Host, COW, PRESENT, USER, WRITABLE};
use mmtest::walk::{Machine, Refused, TableStock, Tables, Tlb, DEPTH, ENTRIES, SPAN};

/// Somewhere in the half a program owns, clear of anything else.
const VIRT: u64 = 0x1000_0000;
/// The next address that needs a last-level table of its own.
const FAR: u64 = VIRT + 0x20_0000;

/// A hierarchy on `host`, and the frames its top table takes.
fn hierarchy(host: &Host) -> (u64, Tables<'_, Host>) {
    let root = host.root();
    // SAFETY: `root` was handed out by this machine for this hierarchy and
    // nothing else names it.
    (root, unsafe { Tables::new(host, root) })
}

/// Frames for the tables a publish may have to create, as `Prepared` brings
/// them in the kernel: taken before the change, given back after it.
fn stock(host: &Host, want: usize) -> TableStock {
    let mut stock = TableStock::new();
    for _ in 0..want {
        stock.push(host.alloc_zeroed().expect("a frame for a table"));
    }
    stock
}

fn give_back(host: &Host, stock: &mut TableStock) {
    while let Some(phys) = stock.take() {
        host.free(phys);
    }
}

/// Put a zeroed page in at `virt` with the frames a walk to it needs.
fn publish(host: &Host, tables: &Tables<'_, Host>, virt: u64, tlb: &mut Tlb) -> Result<u64, Refused> {
    let page = host.alloc_zeroed().expect("a frame for a page");
    let mut spare = stock(host, DEPTH);
    let done = tables.publish(virt, page, page_flags(), &mut spare, tlb);
    give_back(host, &mut spare);
    match done {
        Ok(()) => Ok(page),
        Err(refused) => {
            // The page the publish did not take goes back, which is what the
            // kernel's `Prepared` does when it is dropped outside the lock.
            host.free(page);
            Err(refused)
        }
    }
}

#[test]
fn a_page_goes_in_and_the_address_reaches_it() {
    let host = Host::new(64);
    let (root, tables) = hierarchy(&host);
    let mut tlb = Tlb::new();

    let page = publish(&host, &tables, VIRT, &mut tlb).expect("the page goes in");
    assert_eq!(tables.translate(VIRT), Some(page));
    assert_eq!(tables.translate(VIRT + 0x123), Some(page + 0x123));
    assert_eq!(tables.flags_of(VIRT), Some(page_flags()));
    // Three tables were created on the way down, so three of the frames the
    // publish brought were used and the rest went back.
    assert_eq!(host.live(), 1 + DEPTH + 1, "the top table, three tables and the page");
    assert_eq!(tlb, Tlb::Page(VIRT));

    tables.free_below(SPAN);
    host.free(root);
    assert_eq!(host.live(), 0, "a teardown releases everything");
    assert_eq!(host.freed_twice.get(), 0);
}

#[test]
fn a_second_page_at_one_address_is_refused_and_handed_back() {
    let host = Host::new(64);
    let (root, tables) = hierarchy(&host);
    let mut tlb = Tlb::new();

    let first = publish(&host, &tables, VIRT, &mut tlb).expect("the first page goes in");
    let held = host.live();
    // The loser of a race to one address: whoever got there first is finished,
    // so the page this brought is released rather than written over the top.
    assert_eq!(publish(&host, &tables, VIRT, &mut tlb), Err(Refused::Occupied));
    assert_eq!(host.live(), held, "the page that was turned away is released");
    assert_eq!(tables.translate(VIRT), Some(first), "and the one there is untouched");
    assert_eq!(host.references(first), 1);

    tables.free_below(SPAN);
    host.free(root);
    assert_eq!(host.live(), 0);
    assert_eq!(host.freed_twice.get(), 0);
}

#[test]
fn a_publish_with_no_tables_to_spare_writes_nothing() {
    let host = Host::new(64);
    let (root, tables) = hierarchy(&host);
    let mut tlb = Tlb::new();

    let page = host.alloc_zeroed().unwrap();
    let mut none = TableStock::new();
    assert_eq!(
        tables.publish(VIRT, page, page_flags(), &mut none, &mut tlb),
        Err(Refused::ShortOfTables),
    );
    assert_eq!(tables.translate(VIRT), None);
    assert!(tlb.is_clean(), "nothing was written, so nothing is stale");
    host.free(page);

    // One short is still short, and what it did put in is a table the
    // hierarchy owns rather than a leak.
    let page = host.alloc_zeroed().unwrap();
    let mut one = stock(&host, 1);
    assert_eq!(
        tables.publish(VIRT, page, page_flags(), &mut one, &mut tlb),
        Err(Refused::ShortOfTables),
    );
    give_back(&host, &mut one);
    host.free(page);
    assert_eq!(tables.missing_tables(VIRT), 2, "the level it did create is there");

    tables.free_below(SPAN);
    host.free(root);
    assert_eq!(host.live(), 0);
    assert_eq!(host.freed_twice.get(), 0);
}

#[test]
fn how_many_tables_are_missing_is_what_a_publish_needs() {
    let host = Host::new(64);
    let (root, tables) = hierarchy(&host);
    let mut tlb = Tlb::new();

    assert_eq!(tables.missing_tables(VIRT), DEPTH);
    publish(&host, &tables, VIRT, &mut tlb).unwrap();
    assert_eq!(tables.missing_tables(VIRT), 0);
    // Another page in the same last-level table needs none.
    assert_eq!(tables.missing_tables(VIRT + 0x1000), 0);
    // One past what that table covers needs the last level.
    assert_eq!(tables.missing_tables(FAR), 1);

    tables.free_below(SPAN);
    host.free(root);
    assert_eq!(host.live(), 0);
}

#[test]
fn an_unmap_gives_back_the_page_and_the_tables_it_emptied() {
    let host = Host::new(64);
    let (root, tables) = hierarchy(&host);
    let mut tlb = Tlb::new();

    let page = publish(&host, &tables, VIRT, &mut tlb).unwrap();
    tlb.flush(&host);

    let mut reclaimed = TableStock::new();
    let taken = tables.unmap(VIRT, &mut tlb, &mut reclaimed).expect("the mapping is there");
    assert_eq!(taken, page);
    assert_eq!(reclaimed.len(), DEPTH, "the last page out of a table gives the table back");
    // A table descriptor changed, so what the hardware cached of the walk goes
    // as well: nothing narrower than the whole space says that.
    assert_eq!(tlb, Tlb::All);
    assert_eq!(tables.translate(VIRT), None);

    // Nothing is released until the invalidation has happened, which is what
    // `MmGuard::retire` orders in the kernel.
    assert_eq!(host.all_flushes.get(), 0);
    tlb.flush(&host);
    assert_eq!(host.all_flushes.get(), 1);
    host.free(taken);
    give_back(&host, &mut reclaimed);

    assert_eq!(host.live(), 1, "only the top table is left");
    host.free(root);
    assert_eq!(host.live(), 0);
    assert_eq!(host.freed_twice.get(), 0);
}

#[test]
fn an_unmap_keeps_a_table_that_still_holds_a_page() {
    let host = Host::new(64);
    let (root, tables) = hierarchy(&host);
    let mut tlb = Tlb::new();

    publish(&host, &tables, VIRT, &mut tlb).unwrap();
    let neighbour = publish(&host, &tables, VIRT + 0x1000, &mut tlb).unwrap();
    tlb.flush(&host);

    let mut reclaimed = TableStock::new();
    let taken = tables.unmap(VIRT, &mut tlb, &mut reclaimed).unwrap();
    assert!(reclaimed.is_empty(), "the table still holds the neighbour");
    assert_eq!(tlb, Tlb::Page(VIRT), "one address changed, so one goes");
    assert_eq!(tables.translate(VIRT + 0x1000), Some(neighbour));
    tlb.flush(&host);
    host.free(taken);

    tables.free_below(SPAN);
    host.free(root);
    assert_eq!(host.live(), 0);
    assert_eq!(host.freed_twice.get(), 0);
}

#[test]
fn unmapping_what_is_not_there_writes_nothing() {
    let host = Host::new(64);
    let (root, tables) = hierarchy(&host);
    let mut tlb = Tlb::new();
    let mut reclaimed = TableStock::new();

    assert_eq!(tables.unmap(VIRT, &mut tlb, &mut reclaimed), None);
    assert!(tlb.is_clean());
    assert!(reclaimed.is_empty());

    host.free(root);
    assert_eq!(host.live(), 0);
}

#[test]
fn replacing_hands_the_old_frame_back_and_never_empties_the_address() {
    let host = Host::new(64);
    let (root, tables) = hierarchy(&host);
    let mut tlb = Tlb::new();

    let old = publish(&host, &tables, VIRT, &mut tlb).unwrap();
    tlb.flush(&host);
    let copy = host.alloc_zeroed().unwrap();

    let handed_back = tables.replace(VIRT, copy, page_flags(), &mut tlb).expect("one was there");
    assert_eq!(handed_back, old);
    assert_eq!(tables.translate(VIRT), Some(copy), "the address is never without one");
    assert_eq!(host.references(old), 1, "and the old frame is still held");
    tlb.flush(&host);
    host.free(old);
    assert_eq!(host.references(old), 0);

    tables.free_below(SPAN);
    host.free(root);
    assert_eq!(host.live(), 0);
    assert_eq!(host.freed_twice.get(), 0);
}

#[test]
fn replacing_nothing_is_refused_with_nothing_written() {
    let host = Host::new(64);
    let (root, tables) = hierarchy(&host);
    let mut tlb = Tlb::new();

    let offered = host.alloc_zeroed().unwrap();
    assert_eq!(tables.replace(VIRT, offered, page_flags(), &mut tlb), None);
    assert_eq!(tables.translate(VIRT), None);
    assert!(tlb.is_clean());
    host.free(offered);

    host.free(root);
    assert_eq!(host.live(), 0);
}

#[test]
fn protecting_keeps_the_frame_and_changes_the_flags() {
    let host = Host::new(64);
    let (root, tables) = hierarchy(&host);
    let mut tlb = Tlb::new();

    let page = publish(&host, &tables, VIRT, &mut tlb).unwrap();
    tlb.flush(&host);

    let named = tables.protect(VIRT, PRESENT | USER, &mut tlb).expect("one is there");
    assert_eq!(named, page, "the frame is the one that was there");
    assert_eq!(tables.translate(VIRT), Some(page));
    assert_eq!(tables.flags_of(VIRT), Some(PRESENT | USER));
    assert_eq!(host.references(page), 1, "and it is still held exactly once");
    assert_eq!(tlb, Tlb::Page(VIRT));

    assert_eq!(tables.protect(FAR, page_flags(), &mut tlb), None, "nothing there to protect");

    tables.free_below(SPAN);
    host.free(root);
    assert_eq!(host.live(), 0);
    assert_eq!(host.freed_twice.get(), 0);
}

#[test]
fn a_fork_shares_pages_through_tables_of_its_own() {
    let host = Host::new(64);
    let (parent_root, parent) = hierarchy(&host);
    let mut tlb = Tlb::new();
    let page = publish(&host, &parent, VIRT, &mut tlb).unwrap();
    tlb.flush(&host);

    let (child_root, child) = hierarchy(&host);
    let before = host.live();
    let mut spare = stock(&host, DEPTH);
    child
        .share_range(&parent, VIRT & !0x1F_FFFF, ENTRIES, &mut spare, &mut tlb)
        .expect("the block shares");
    give_back(&host, &mut spare);

    assert_eq!(child.translate(VIRT), Some(page), "the child reaches the same page");
    assert_eq!(host.references(page), 2, "which two hierarchies now hold");
    assert_eq!(host.live(), before + DEPTH, "through tables of its own");
    // Both sides have to copy before they write, and each remembers that the
    // page was writable so the copy can put the permission back.
    for side in [&parent, &child] {
        let flags = side.flags_of(VIRT).unwrap();
        assert_eq!(flags & WRITABLE, 0, "neither side may write it");
        assert_ne!(flags & COW, 0, "and both know why");
    }

    // Taking the child's only page away gives back its three tables and leaves
    // the page where the parent has it: a fork shares pages, not tables.
    let mut reclaimed = TableStock::new();
    let taken = child.unmap(VIRT, &mut tlb, &mut reclaimed).unwrap();
    assert_eq!(reclaimed.len(), DEPTH);
    tlb.flush(&host);
    host.free(taken);
    give_back(&host, &mut reclaimed);
    assert_eq!(host.references(page), 1);
    assert_eq!(parent.translate(VIRT), Some(page));

    parent.free_below(SPAN);
    child.free_below(SPAN);
    host.free(parent_root);
    host.free(child_root);
    assert_eq!(host.live(), 0);
    assert_eq!(host.freed_twice.get(), 0);
}

#[test]
fn a_fork_leaves_a_read_only_page_as_it_was() {
    let host = Host::new(64);
    let (parent_root, parent) = hierarchy(&host);
    let mut tlb = Tlb::new();

    let page = host.alloc_zeroed().unwrap();
    let mut spare = stock(&host, DEPTH);
    parent.publish(VIRT, page, PRESENT | USER, &mut spare, &mut tlb).unwrap();
    give_back(&host, &mut spare);
    tlb.flush(&host);

    let (child_root, child) = hierarchy(&host);
    let mut spare = stock(&host, DEPTH);
    child.share_range(&parent, VIRT & !0x1F_FFFF, ENTRIES, &mut spare, &mut tlb).unwrap();
    give_back(&host, &mut spare);

    assert_eq!(parent.flags_of(VIRT), Some(PRESENT | USER), "nothing to take away");
    assert_eq!(child.flags_of(VIRT), Some(PRESENT | USER));
    assert_eq!(host.references(page), 2);

    parent.free_below(SPAN);
    child.free_below(SPAN);
    host.free(parent_root);
    host.free(child_root);
    assert_eq!(host.live(), 0);
    assert_eq!(host.freed_twice.get(), 0);
}

#[test]
fn the_next_populated_block_is_how_a_walk_is_driven_without_a_pointer() {
    let host = Host::new(64);
    let (root, tables) = hierarchy(&host);
    let mut tlb = Tlb::new();

    assert_eq!(tables.next_populated(0, SPAN), None, "nothing is in an empty space");

    publish(&host, &tables, VIRT, &mut tlb).unwrap();
    publish(&host, &tables, FAR, &mut tlb).unwrap();
    let first = tables.next_populated(0, SPAN).expect("the first block");
    assert_eq!(first, VIRT & !0x1F_FFFF);
    let second = tables.next_populated(first + 0x20_0000, SPAN).expect("the second");
    assert_eq!(second, FAR & !0x1F_FFFF);
    assert_eq!(tables.next_populated(second + 0x20_0000, SPAN), None);
    // A limit of everything is the top of what four levels translate, not the
    // top of a `u64`: above that the top index comes from bits the walk does
    // not read, and a scan that ran on would find these same tables again.
    assert_eq!(tables.next_populated(second + 0x20_0000, u64::MAX), None);
    // A limit below a block leaves it out, which is how the user half is
    // walked without touching the kernel's.
    assert_eq!(tables.next_populated(0, VIRT & !0x1F_FFFF), None);

    tables.free_below(SPAN);
    host.free(root);
    assert_eq!(host.live(), 0);
}

#[test]
fn a_teardown_stops_at_the_limit_it_is_given() {
    let host = Host::new(128);
    let (root, tables) = hierarchy(&host);
    let mut tlb = Tlb::new();

    // One page below the limit and one above it, in different top-level
    // descriptors: the kernel half is above, and a teardown frees none of it.
    const LIMIT: u64 = 1 << 39;
    publish(&host, &tables, VIRT, &mut tlb).unwrap();
    let kept = publish(&host, &tables, LIMIT + VIRT, &mut tlb).unwrap();
    let before = host.live();

    tables.free_below(LIMIT);
    assert_eq!(tables.translate(VIRT), None, "what was below is gone");
    assert_eq!(tables.translate(LIMIT + VIRT), Some(kept), "what was above is not");
    assert_eq!(before - host.live(), DEPTH + 1, "three tables and a page");

    tables.free_below(SPAN);
    host.free(root);
    assert_eq!(host.live(), 0);
    assert_eq!(host.freed_twice.get(), 0);
}

#[test]
fn what_is_stale_collects_rather_than_being_acted_on_each_time() {
    let host = Host::new(64);
    let mut tlb = Tlb::new();

    assert!(tlb.is_clean());
    tlb.flush(&host);
    assert_eq!((host.page_flushes.get(), host.all_flushes.get()), (0, 0));

    // One address, however many times.
    tlb.note(VIRT);
    tlb.note(VIRT);
    tlb.flush(&host);
    assert_eq!((host.page_flushes.get(), host.all_flushes.get()), (1, 0));

    // Two, and past the first it is cheaper to take the whole space than to
    // name every address a range has.
    tlb.note(VIRT);
    tlb.note(FAR);
    tlb.flush(&host);
    assert_eq!((host.page_flushes.get(), host.all_flushes.get()), (1, 1));

    // A table descriptor covers every address under it.
    tlb.note_all();
    assert_eq!(tlb, Tlb::All);
    tlb.flush(&host);
    assert_eq!(host.all_flushes.get(), 2);
    assert!(tlb.is_clean(), "and acting on it starts again");
}

#[test]
fn every_descriptor_is_written_through_the_barrier() {
    let host = Host::new(64);
    let (root, tables) = hierarchy(&host);
    let mut tlb = Tlb::new();

    // What a descriptor makes reachable has to be in memory before the
    // descriptor naming it is. Nothing on this machine can check the ordering
    // itself; what it can check is that no store goes round it.
    let before = host.barriers.get();
    publish(&host, &tables, VIRT, &mut tlb).unwrap();
    assert_eq!(host.barriers.get() - before, DEPTH + 1, "three tables and the leaf");

    tables.free_below(SPAN);
    host.free(root);
    assert_eq!(host.live(), 0);
}
