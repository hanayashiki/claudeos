//! Trees written here, each describing one case of `/reserved-memory`, the
//! memory reservation block or the memory nodes, read by the kernel's reader.

mod common;

use common::*;
use devicetree::machine::{Placement, Skip, What, MAX_LISTED, MAX_SIZE};

/// Open the root, with the cell counts its children's `reg` is written in.
fn root(tree: &mut Tree, address_cells: u32, size_cells: u32) {
    tree.begin("").cells("#address-cells", &[address_cells]).cells("#size-cells", &[size_cells]);
}

/// `value` written as `count` cells.
fn wide(value: u64, count: u32) -> Vec<u32> {
    match count {
        1 => vec![value as u32],
        2 => vec![(value >> 32) as u32, value as u32],
        _ => panic!("{} cells", count),
    }
}

fn entry(address: u64, size: u64, address_cells: u32, size_cells: u32) -> Vec<u32> {
    let mut cells = wide(address, address_cells);
    cells.extend(wide(size, size_cells));
    cells
}

/// The Pi 4's shape: two address cells and one size cell, memory from zero.
fn pi_root(tree: &mut Tree, memory_end: u64) {
    root(tree, 2, 1);
    tree.begin("memory@0").text("device_type", "memory").cells("reg", &entry(0, memory_end, 2, 1)).end();
}

#[test]
fn one_cell_addresses_and_sizes() {
    let mut tree = Tree::new();
    root(&mut tree, 1, 1);
    tree.begin("memory@0").cells("reg", &[0, 0x4000_0000]).end();
    tree.begin("reserved-memory").cells("#address-cells", &[1]).cells("#size-cells", &[1]).prop("ranges", &[]);
    tree.begin("firmware@1000000").cells("reg", &[0x0100_0000, 0x10_0000, 0x0200_0000, 0x1000]).end();
    tree.end().end();

    let (info, found) = read(&tree.build()).unwrap();
    assert_eq!(regions(&info), vec![(0, 0x4000_0000)]);
    assert_eq!(reserved(&info), vec![(0x0100_0000, 0x0110_0000), (0x0200_0000, 0x0200_1000)]);
    let listed = found.reservations();
    assert_eq!(listed.len(), 2, "one entry per reg entry: {}", found);
    for (entry, (start, end)) in listed.iter().zip([(0x0100_0000, 0x0110_0000), (0x0200_0000, 0x0200_1000)]) {
        assert_eq!(entry.name.as_bytes(), b"firmware@1000000");
        assert_eq!(entry.what, What::Static { no_map: false });
        assert_eq!((entry.start, entry.end), (start, end));
        assert_eq!(entry.placement, Placement::InMemory);
    }
    assert!(!found.damaged);
}

#[test]
fn two_cell_addresses_and_sizes() {
    let mut tree = Tree::new();
    root(&mut tree, 2, 2);
    let mut memory = entry(0, 0x4000_0000, 2, 2);
    memory.extend(entry(0x1_0000_0000, 0x4000_0000, 2, 2));
    tree.begin("memory@0").cells("reg", &memory).end();
    tree.begin("reserved-memory").cells("#address-cells", &[2]).cells("#size-cells", &[2]).prop("ranges", &[]);
    tree.begin("high@110000000").cells("reg", &entry(0x1_1000_0000, 0x1_0000_0000, 2, 2)).end();
    tree.end().end();

    let (info, found) = read(&tree.build()).unwrap();
    assert_eq!(regions(&info), vec![(0, 0x4000_0000), (0x1_0000_0000, 0x4000_0000)]);
    assert_eq!(reserved(&info), vec![(0x1_1000_0000, 0x2_1000_0000)]);
    // Four gigabytes from inside the second bank runs past its end.
    assert_eq!(found.reservations()[0].placement, Placement::PartlyOutside);
}

/// The children's `reg` is as wide as `/reserved-memory` says, not as the root
/// says: read with the root's three cells, eight bytes would not be an entry.
#[test]
fn children_are_read_with_the_nodes_own_cells() {
    let mut tree = Tree::new();
    pi_root(&mut tree, 0x3b40_0000);
    tree.begin("reserved-memory").cells("#address-cells", &[1]).cells("#size-cells", &[1]).prop("ranges", &[]);
    tree.begin("narrow@3000000").cells("reg", &[0x0300_0000, 0x1000]).end();
    tree.end().end();

    let (info, found) = read(&tree.build()).unwrap();
    assert_eq!(reserved(&info), vec![(0x0300_0000, 0x0300_1000)], "{}", found);
}

/// A `/reserved-memory` that gives no cell counts is read with the root's,
/// which Linux uses in every case.
#[test]
fn a_node_without_cells_takes_the_roots() {
    let mut tree = Tree::new();
    pi_root(&mut tree, 0x3b40_0000);
    tree.begin("reserved-memory").prop("ranges", &[]);
    tree.begin("plain@3000000").cells("reg", &entry(0x0300_0000, 0x2000, 2, 1)).end();
    tree.end().end();

    let (info, found) = read(&tree.build()).unwrap();
    assert_eq!(reserved(&info), vec![(0x0300_0000, 0x0300_2000)], "{}", found);
}

#[test]
fn no_map_is_reserved_and_shown() {
    let mut tree = Tree::new();
    pi_root(&mut tree, 0x3b40_0000);
    tree.begin("reserved-memory").cells("#address-cells", &[2]).cells("#size-cells", &[1]).prop("ranges", &[]);
    tree.begin("nvram@0")
        .text("compatible", "raspberrypi,bootloader-config")
        .cells("reg", &entry(0x3b3f_e000, 0x400, 2, 1))
        .prop("no-map", &[])
        .end();
    tree.end().end();

    let (info, found) = read(&tree.build()).unwrap();
    assert_eq!(reserved(&info), vec![(0x3b3f_e000, 0x3b3f_e400)]);
    assert_eq!(found.reservations()[0].what, What::Static { no_map: true });
    assert_eq!(found.to_string(), "nvram@0 0x3b3fe000-0x3b3fe400 no-map");
}

/// A child with `size` and no `reg` asks the kernel to find the memory. This
/// kernel has no use for such a pool, so nothing is reserved for it.
#[test]
fn dynamic_children_reserve_nothing() {
    let mut tree = Tree::new();
    pi_root(&mut tree, 0x3b40_0000);
    tree.begin("reserved-memory").cells("#address-cells", &[2]).cells("#size-cells", &[1]).prop("ranges", &[]);
    tree.begin("linux,cma")
        .text("compatible", "shared-dma-pool")
        .cells("size", &[0x0400_0000])
        .cells("alignment", &[0x40_0000])
        .prop("reusable", &[])
        .prop("linux,cma-default", &[])
        .cells("alloc-ranges", &[0, 0, 0x3000_0000])
        .end();
    tree.end().end();

    let (info, found) = read(&tree.build()).unwrap();
    assert_eq!(reserved(&info), Vec::<(u64, u64)>::new());
    assert_eq!(found.reservations()[0].what, What::Dynamic { size: 0x0400_0000 });
    assert_eq!(found.reservations()[0].placement, Placement::NotARange);
    assert_eq!(found.to_string(), "linux,cma dynamic, size 0x4000000, not allocated");
}

#[test]
fn only_children_switched_on_are_reserved() {
    let mut tree = Tree::new();
    pi_root(&mut tree, 0x3b40_0000);
    tree.begin("reserved-memory").cells("#address-cells", &[2]).cells("#size-cells", &[1]).prop("ranges", &[]);
    // `status` after `reg`, so the decision has to wait for the node's end.
    tree.begin("off@1000000").cells("reg", &entry(0x0100_0000, 0x1000, 2, 1)).text("status", "disabled").end();
    tree.begin("okay@2000000").text("status", "okay").cells("reg", &entry(0x0200_0000, 0x1000, 2, 1)).end();
    tree.begin("ok@3000000").cells("reg", &entry(0x0300_0000, 0x1000, 2, 1)).text("status", "ok").end();
    tree.begin("fail@4000000").cells("reg", &entry(0x0400_0000, 0x1000, 2, 1)).text("status", "fail").end();
    // A dynamic child that is switched off is not listed as dynamic.
    tree.begin("pool").cells("size", &[0x1000]).text("status", "disabled").end();
    tree.end().end();

    let (info, found) = read(&tree.build()).unwrap();
    assert_eq!(reserved(&info), vec![(0x0200_0000, 0x0200_1000), (0x0300_0000, 0x0300_1000)]);
    let what: Vec<What> = found.reservations().iter().map(|entry| entry.what).collect();
    assert_eq!(
        what,
        vec![
            What::Disabled,
            What::Static { no_map: false },
            What::Static { no_map: false },
            What::Disabled,
            What::Disabled
        ]
    );
}

/// Each child that cannot be read is listed and skipped, and the walk goes on
/// to reserve the good child after it.
#[test]
fn malformed_children_are_skipped_and_the_rest_read() {
    let mut tree = Tree::new();
    pi_root(&mut tree, 0x3b40_0000);
    tree.begin("reserved-memory").cells("#address-cells", &[2]).cells("#size-cells", &[1]).prop("ranges", &[]);
    tree.begin("short@0").prop("reg", &[0, 0, 0, 0, 0, 0, 0, 1, 0, 0]).end();
    tree.begin("empty@0").prop("reg", &[]).end();
    tree.begin("zero@1000000").cells("reg", &entry(0x0100_0000, 0, 2, 1)).end();
    tree.begin("wide-size").cells("size", &[0, 0x1000]).end();
    tree.begin("nothing").text("compatible", "vendor,thing").end();
    // Two entries, one of size zero: the other is still reserved.
    let mut two = entry(0x0200_0000, 0, 2, 1);
    two.extend(entry(0x0210_0000, 0x1000, 2, 1));
    tree.begin("half@2000000").cells("reg", &two).end();
    tree.begin("good@3000000").cells("reg", &entry(0x0300_0000, 0x1000, 2, 1)).end();
    tree.end().end();

    let (info, found) = read(&tree.build()).unwrap();
    assert_eq!(reserved(&info), vec![(0x0210_0000, 0x0210_1000), (0x0300_0000, 0x0300_1000)]);
    let what: Vec<What> = found.reservations().iter().map(|entry| entry.what).collect();
    assert_eq!(
        what,
        vec![
            What::Skipped(Skip::RegLength),
            What::Skipped(Skip::EmptyReg),
            What::Skipped(Skip::ZeroSize),
            What::Skipped(Skip::SizeLength),
            What::Skipped(Skip::Nothing),
            What::Skipped(Skip::ZeroSize),
            What::Static { no_map: false },
            What::Static { no_map: false },
        ]
    );
    assert!(!found.damaged);
    assert!(found.to_string().contains("short@0 skipped, reg is not a whole number of entries"), "{}", found);
}

#[test]
fn cell_counts_out_of_range_skip_every_child() {
    for (address_cells, size_cells) in [(0u32, 1u32), (2, 0), (5, 1), (2, 5), (0xffff_ffff, 0xffff_ffff)] {
        let mut tree = Tree::new();
        pi_root(&mut tree, 0x3b40_0000);
        tree.begin("reserved-memory")
            .cells("#address-cells", &[address_cells])
            .cells("#size-cells", &[size_cells])
            .prop("ranges", &[]);
        tree.begin("a@1000").cells("reg", &[0, 0x1000, 0x1000]).end();
        tree.end().end();

        let (info, found) = read(&tree.build()).unwrap();
        assert_eq!(reserved(&info), Vec::<(u64, u64)>::new());
        assert_eq!(found.reservations()[0].what, What::Skipped(Skip::Cells));
    }
}

/// Where each range lies is decided against every memory node in the tree,
/// including the ones after `/reserved-memory`, as the Pi's tree has them.
#[test]
fn ranges_against_memory() {
    let mut tree = Tree::new();
    root(&mut tree, 2, 1);
    tree.memreserve(0, 0x1000);
    tree.begin("reserved-memory").cells("#address-cells", &[2]).cells("#size-cells", &[1]).prop("ranges", &[]);
    tree.begin("inside@3000000").cells("reg", &entry(0x0300_0000, 0x1000, 2, 1)).end();
    tree.begin("straddles@3ff00000").cells("reg", &entry(0x3ff0_0000, 0x20_0000, 2, 1)).end();
    tree.begin("across@1ff00000").cells("reg", &entry(0x1ff0_0000, 0x20_0000, 2, 1)).end();
    tree.begin("overlap@2ff00000").cells("reg", &entry(0x2ff0_0000, 0x20_0000, 2, 1)).end();
    tree.begin("outside@80000000").cells("reg", &entry(0x8000_0000, 0x1000, 2, 1)).end();
    tree.end();
    // Banks that touch at 512 MiB, and a third overlapping the second from
    // 640 MiB, so that two ranges are in memory only across two banks.
    let mut memory = entry(0, 0x2000_0000, 2, 1);
    memory.extend(entry(0x2000_0000, 0x1000_0000, 2, 1));
    memory.extend(entry(0x2800_0000, 0x1800_0000, 2, 1));
    tree.begin("memory@0").cells("reg", &memory).end();
    tree.end();

    let (info, found) = read(&tree.build()).unwrap();
    let placements: Vec<Placement> = found.reservations().iter().map(|entry| entry.placement).collect();
    assert_eq!(
        placements,
        vec![
            Placement::InMemory,
            Placement::InMemory,
            Placement::PartlyOutside,
            Placement::InMemory,
            Placement::InMemory,
            Placement::Outside
        ]
    );
    // Outside or not, every range is reserved.
    assert_eq!(reserved(&info).len(), 6);
    assert!(found.to_string().contains("straddles@3ff00000 0x3ff00000-0x40100000 (partly outside memory)"), "{}", found);
    assert!(found.to_string().contains("outside@80000000 0x80000000-0x80001000 (outside memory)"), "{}", found);
}

/// A range that runs past the top of the address space is reserved up to it.
#[test]
fn a_range_past_the_top_is_cut_there() {
    let mut tree = Tree::new();
    pi_root(&mut tree, 0x3b40_0000);
    tree.memreserve(0xffff_ffff_ffff_0000, 0x10_0000);
    tree.begin("reserved-memory").cells("#address-cells", &[2]).cells("#size-cells", &[2]).prop("ranges", &[]);
    tree.begin("top").cells("reg", &entry(0xffff_ffff_0000_0000, 0xffff_ffff_ffff_ffff, 2, 2)).end();
    tree.end().end();

    let (info, found) = read(&tree.build()).unwrap();
    assert_eq!(reserved(&info), vec![(0xffff_ffff_0000_0000, u64::MAX)]);
    assert_eq!(found.reservations().len(), 2);
}

#[test]
fn memreserve_entries_are_reserved_and_listed() {
    let mut tree = Tree::new();
    tree.memreserve(0, 0x1000).memreserve(0x0800_0000, 0x10_0000);
    pi_root(&mut tree, 0x3b40_0000);
    tree.end();

    let (info, found) = read(&tree.build()).unwrap();
    assert_eq!(reserved(&info), vec![(0, 0x1000), (0x0800_0000, 0x0810_0000)]);
    assert_eq!(found.to_string(), "/memreserve/ 0x0-0x1000; /memreserve/ 0x8000000-0x8100000");
}

#[test]
fn nothing_reserved_says_none() {
    let mut tree = Tree::new();
    pi_root(&mut tree, 0x3b40_0000);
    tree.end();
    let (_, found) = read(&tree.build()).unwrap();
    assert_eq!(found.to_string(), "none");
}

/// More entries than the boot line keeps: all of them are reserved.
#[test]
fn entries_past_the_list_are_still_reserved() {
    let mut tree = Tree::new();
    pi_root(&mut tree, 0x3b40_0000);
    tree.begin("reserved-memory").cells("#address-cells", &[2]).cells("#size-cells", &[1]).prop("ranges", &[]);
    let count = MAX_LISTED + 4;
    for index in 0..count as u64 {
        let address = 0x0100_0000 + index * 0x10_0000;
        tree.begin(&format!("r@{:x}", address)).cells("reg", &entry(address, 0x1000, 2, 1)).end();
    }
    tree.end().end();

    let (info, found) = read(&tree.build()).unwrap();
    assert_eq!(reserved(&info).len(), count, "each range reserved on its own, none folded into another");
    assert_eq!(found.reservations().len(), MAX_LISTED);
    assert_eq!(found.unlisted(), 4);
    assert!(found.to_string().ends_with("; 4 more not listed"), "{}", found);
}

#[test]
fn memory_nodes_switched_off_add_nothing() {
    let mut tree = Tree::new();
    pi_root(&mut tree, 0x3b40_0000);
    tree.begin("memory@40000000").cells("reg", &entry(0x4000_0000, 0x4000_0000, 2, 1)).text("status", "disabled").end();
    tree.end();
    let (info, _) = read(&tree.build()).unwrap();
    assert_eq!(regions(&info), vec![(0, 0x3b40_0000)]);
}

/// What the reader already took from `/chosen` before `/reserved-memory` was
/// read, unchanged: the command line without its terminator, and the ram disk
/// from ends written one and two cells wide.
#[test]
fn chosen_gives_the_command_line_and_the_ram_disk() {
    let mut tree = Tree::new();
    pi_root(&mut tree, 0x3b40_0000);
    tree.begin("chosen")
        .text("bootargs", "console=ttyAMA0 init=/bin/sh")
        .cells("linux,initrd-start", &[0x0200_0000])
        .cells("linux,initrd-end", &[0, 0x0210_0000])
        .end();
    tree.end();
    let (info, _) = read(&tree.build()).unwrap();
    assert_eq!(info.cmdline(), "console=ttyAMA0 init=/bin/sh");
    let modules: Vec<(u64, u64)> = info.modules().iter().map(|module| (module.start, module.end)).collect();
    assert_eq!(modules, vec![(0x0200_0000, 0x0210_0000)]);
}

/// A header that points outside the blob, is too old or claims too much is
/// not read, and the BootInfo is left as it was.
#[test]
fn a_bad_header_is_refused() {
    let mut tree = Tree::new();
    tree.memreserve(0, 0x1000);
    pi_root(&mut tree, 0x3b40_0000);
    tree.end();
    let good = tree.build();
    assert!(read(&good).is_some());

    let set = |offset: usize, value: u32| {
        let mut blob = good.clone();
        blob[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
        blob
    };
    let cases = [
        ("magic", set(0, 0xEDFE_0DD0)),
        ("total size past the blob", set(4, good.len() as u32 + 1)),
        ("total size past the largest tree", {
            let mut blob = set(4, MAX_SIZE as u32 + 4);
            blob.resize(MAX_SIZE + 4, 0);
            blob
        }),
        ("structure block past the end", set(8, good.len() as u32)),
        ("strings block past the end", set(12, 0xffff_fff0)),
        ("reservation block past the end", set(16, good.len() as u32 + 16)),
        ("version 16", set(20, 16)),
        ("structure size past the end", set(36, 0xffff_ffff)),
        ("strings size past the end", set(32, good.len() as u32)),
    ];
    for (what, blob) in cases {
        assert!(read(&blob).is_none(), "{}", what);
    }
    assert!(read(&good[..39]).is_none(), "shorter than a header");
    assert!(read(&[]).is_none(), "empty");
}

/// Damage inside the blocks ends the read where it is and says so; it never
/// panics, and what came before it is kept.
#[test]
fn damage_is_reported_and_read_up_to() {
    let mut tree = Tree::new();
    tree.memreserve(0, 0x1000);
    pi_root(&mut tree, 0x3b40_0000);
    tree.begin("reserved-memory").cells("#address-cells", &[2]).cells("#size-cells", &[1]).prop("ranges", &[]);
    tree.begin("first@1000000").cells("reg", &entry(0x0100_0000, 0x1000, 2, 1)).end();
    tree.begin("second@2000000").cells("reg", &entry(0x0200_0000, 0x1000, 2, 1)).end();
    tree.end().end();
    let good = tree.build();

    // The structure block cut short in the second child: the first is
    // reserved, the second is not, and the line says the tree is damaged.
    let struct_at = u32::from_be_bytes(good[8..12].try_into().unwrap()) as usize;
    let second = good.windows(14).position(|window| window == b"second@2000000").unwrap();
    let mut cut = good.clone();
    let struct_size = (second - struct_at + 16) as u32;
    cut[36..40].copy_from_slice(&struct_size.to_be_bytes());
    let (info, found) = read(&cut).unwrap();
    assert!(found.damaged);
    assert_eq!(reserved(&info), vec![(0, 0x1000), (0x0100_0000, 0x0100_1000)]);
    assert!(found.to_string().ends_with("; the tree is damaged and was read up to the damage"), "{}", found);

    // The reservation block with no terminator before the end of the blob:
    // one entry of ones, then four bytes, which are not a whole entry.
    let mut endless = good.clone();
    let near_end = good.len() - 20;
    endless[near_end..].fill(0xff);
    endless[16..20].copy_from_slice(&(near_end as u32).to_be_bytes());
    let (_, found) = read(&endless).unwrap();
    assert!(found.damaged);
}

/// Every byte of a tree replaced by each of a few values, and every length it
/// could be cut to: the reader may refuse or report damage, and must not panic
/// or loop.
#[test]
fn no_single_byte_makes_it_panic() {
    let mut tree = Tree::new();
    tree.memreserve(0, 0x1000);
    pi_root(&mut tree, 0x3b40_0000);
    tree.begin("chosen").text("bootargs", "a b c").cells("linux,initrd-start", &[0x100]).cells("linux,initrd-end", &[0x200]).end();
    tree.begin("reserved-memory").cells("#address-cells", &[2]).cells("#size-cells", &[1]).prop("ranges", &[]);
    tree.begin("linux,cma").cells("size", &[0x0400_0000]).end();
    tree.begin("nvram@0").cells("reg", &entry(0x0300_0000, 0x400, 2, 1)).prop("no-map", &[]).text("status", "okay").end();
    tree.begin("nvram@1").cells("reg", &entry(0, 0, 2, 1)).text("status", "disabled").end();
    tree.end().end();
    let good = tree.build();

    for offset in 0..good.len() {
        for value in [0x00u8, 0x01, 0x02, 0x03, 0x04, 0x09, 0x7f, 0x80, 0xfe, 0xff] {
            let mut blob = good.clone();
            blob[offset] = value;
            let _ = read(&blob);
        }
    }
    for length in 0..good.len() {
        let mut blob = good[..length].to_vec();
        if blob.len() >= 8 {
            blob[4..8].copy_from_slice(&(length as u32).to_be_bytes());
        }
        let _ = read(&blob);
    }
}
