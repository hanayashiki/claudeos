//! The trees the kernel meets: the Pi 4's tree file from the pinned firmware,
//! and the tree QEMU's emulated Pi 4 makes of that file.
//!
//! The Pi firmware edits its tree at boot -- it writes the memory node, the
//! command line and the ram disk, and `nvram@0` and `nvram@1` are there for it
//! to fill in -- so the file shows less than the board's own boot line does.
//! QEMU writes the memory node, the command line and the ram disk too, and
//! writes the result out with `-machine dumpdtb`.
//!
//! The files are made by the build, and a missing one fails the test with the
//! command that makes it. PI_DTB and QEMU_DTB name other files.

mod common;

use common::*;
use devicetree::machine::{Placement, What};

const PI_DTB: &str = "build/distro/board-aarch64/boot/bcm2711-rpi-4-b.dtb";
const MAKE_PI_DTB: &str = "cargo run --release -p distro -- build board-aarch64";
const QEMU_DTB: &str = "build/qemu-raspi4b.dtb";
const MAKE_QEMU_DTB: &str = "qemu-system-aarch64 -M raspi4b,dumpdtb=build/qemu-raspi4b.dtb \
     -dtb build/distro/board-aarch64/boot/bcm2711-rpi-4-b.dtb -kernel build/kernel8.img -display none";

/// The /memreserve/ entry and the three `/reserved-memory` children both trees
/// hold: the spin tables' page reserved; `linux,cma` dynamic and not
/// allocated; and the two `nvram` nodes the firmware fills in at boot,
/// disabled until it does.
fn expect_the_firmware_reservations(found: &devicetree::machine::Found, spin_tables: Placement) {
    let listed: Vec<(String, What, u64, u64, Placement)> = found
        .reservations()
        .iter()
        .map(|entry| {
            (String::from_utf8_lossy(entry.name.as_bytes()).into_owned(), entry.what, entry.start, entry.end, entry.placement)
        })
        .collect();
    assert_eq!(
        listed,
        vec![
            ("/memreserve/".to_string(), What::MemReserve, 0, 0x1000, spin_tables),
            ("linux,cma".to_string(), What::Dynamic { size: 0x0400_0000 }, 0, 0, Placement::NotARange),
            ("nvram@0".to_string(), What::Disabled, 0, 0, Placement::NotARange),
            ("nvram@1".to_string(), What::Disabled, 0, 0, Placement::NotARange),
        ]
    );
    assert_eq!(found.unlisted(), 0);
    assert!(!found.damaged);
}

#[test]
fn the_pi_tree_file() {
    let blob = build_file("PI_DTB", PI_DTB, MAKE_PI_DTB);
    let (info, found) = read(&blob).expect("the Pi's tree file is read");
    println!("{}: reserved memory: {}", PI_DTB, found);

    // The memory node's `reg` is <0 0 0> until the firmware writes it, so
    // the spin tables' page lies outside the memory this file describes.
    assert_eq!(regions(&info), vec![(0, 0)]);
    expect_the_firmware_reservations(&found, Placement::Outside);
    assert_eq!(reserved(&info), vec![(0, 0x1000)]);
    assert_eq!(
        found.to_string(),
        "/memreserve/ 0x0-0x1000 (outside memory); linux,cma dynamic, size 0x4000000, not allocated; \
         nvram@0 disabled, not reserved; nvram@1 disabled, not reserved"
    );
    assert!(info.cmdline().starts_with("coherent_pool=1M 8250.nr_uarts=1"), "{}", info.cmdline());
    assert!(info.modules().is_empty());
}

#[test]
fn the_tree_qemu_makes_of_it() {
    let blob = build_file("QEMU_DTB", QEMU_DTB, MAKE_QEMU_DTB);
    let (info, found) = read(&blob).expect("QEMU's tree is read");
    println!("{}: reserved memory: {}", QEMU_DTB, found);

    // QEMU writes the emulated board's 960 MiB into the memory node.
    assert_eq!(regions(&info), vec![(0, 0x3c00_0000)]);
    expect_the_firmware_reservations(&found, Placement::InMemory);
    assert_eq!(reserved(&info), vec![(0, 0x1000)]);
}

/// Every seventh byte of the Pi's tree replaced in turn, with each of three
/// values: the reader may refuse the tree or report damage, and must not panic.
#[test]
fn no_byte_of_the_pi_tree_makes_it_panic() {
    let good = build_file("PI_DTB", PI_DTB, MAKE_PI_DTB);
    let mut blob = good.clone();
    for offset in (0..good.len()).step_by(7) {
        for value in [0x00u8, 0x03, 0xff] {
            blob[offset] = value;
            let _ = read(&blob);
        }
        blob[offset] = good[offset];
    }
}
