//! Both directions against macOS's own FAT code: what the kernel's code writes
//! is checked with fsck_msdos -n and read back through a read-only mount on the
//! Mac, and what the Mac writes through its own mount is read by the kernel's
//! code.

mod common;

use common::*;
use fatdisk::image::{self, Memory};
use fatdisk::macos;

/// Write the corpus with the kernel's code, on volumes of 512-byte and 4 KiB
/// clusters, and compare what the Mac finds.
#[test]
fn we_write_mac_reads() {
    for (mib, cluster_sectors) in [(64u64, Some(1u32)), (300, Some(8))] {
        let dir = scratch(&format!("we-write-{}", mib));
        let (path, mut volume) = fresh(&dir, mib, cluster_sectors);
        let cluster = volume.layout().cluster_bytes as usize;
        let mut expected = Tree::new();
        for (target, data) in corpus(cluster) {
            let (parent, name) = target.rsplit_once('/').unwrap_or(("", &target));
            match &data {
                None => {
                    volume.mkdir(parent, name).unwrap_or_else(|e| panic!("mkdir {}: {:?}", target, e));
                }
                Some(bytes) => {
                    volume.create(parent, name).unwrap_or_else(|e| panic!("create {}: {:?}", target, e));
                    write_all(&mut volume, &target, bytes).unwrap_or_else(|e| panic!("write {}: {:?}", target, e));
                }
            }
            expected.insert(target, data);
        }
        assert_eq!(differences(&expected, &walk_ours(&mut volume)), Vec::<String>::new(), "read back through this code");
        volume.sync().unwrap();
        save(volume, &path);
        assert_fsck_clean(&path);

        let mounted = macos::mount(&path, &dir.join("mnt"), true).unwrap();
        let seen = walk_mac(&mounted.point);
        drop(mounted);
        assert_eq!(differences(&expected, &seen), Vec::<String>::new(), "what the Mac read, with {}-byte clusters", cluster);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// Write the corpus through the Mac's own mount, and read it with the kernel's
/// code; the Mac's own listing is what is expected.
#[test]
fn mac_writes_we_read() {
    for (mib, cluster_sectors) in [(64u64, Some(1u32)), (300, Some(8))] {
        let dir = scratch(&format!("mac-writes-{}", mib));
        let path = dir.join("volume.img");
        macos::newfs_whole(&path, mib, "MACWROTE", cluster_sectors).unwrap();
        let cluster = 512 * cluster_sectors.unwrap_or(1) as usize;
        let mounted = macos::mount(&path, &dir.join("mnt"), false).unwrap();
        for (target, data) in corpus(cluster) {
            let at = mounted.point.join(&target);
            match data {
                None => std::fs::create_dir(&at).unwrap_or_else(|e| panic!("{}: {}", target, e)),
                Some(bytes) => std::fs::write(&at, bytes).unwrap_or_else(|e| panic!("{}: {}", target, e)),
            }
        }
        let expected = walk_mac(&mounted.point);
        drop(mounted);

        let mut volume = image::mount(Memory { data: std::fs::read(&path).unwrap() }).unwrap();
        let mut seen = walk_ours(&mut volume);
        // What macOS makes for itself is not compared.
        seen.retain(|path, _| !mac_made(path));
        assert_eq!(differences(&expected, &seen), Vec::<String>::new(), "what this code read, with {}-byte clusters", cluster);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
