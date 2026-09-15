//! A first round trip: a volume newfs_msdos made, written by the kernel's
//! code, and checked by fsck_msdos.

use fatdisk::fat::FsError;
use fatdisk::{image, macos};
use std::path::PathBuf;

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("fatdisk-test-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn round_trip() {
    let dir = scratch("basic");
    let path = dir.join("a.img");
    macos::newfs_whole(&path, 64, "BASIC", None).unwrap();
    let mut volume = image::mount(image::Memory { data: std::fs::read(&path).unwrap() }).unwrap();
    volume.mkdir("", "www").unwrap();
    volume.create("www", "index.html").unwrap();
    assert_eq!(volume.write("www/index.html", 0, b"<h1>hello</h1>\n").unwrap(), 15);
    let mut buf = [0u8; 64];
    assert_eq!(volume.read("www/index.html", 0, &mut buf).unwrap(), 15);
    assert_eq!(&buf[..15], b"<h1>hello</h1>\n");
    assert_eq!(volume.lookup("www", "INDEX.HTML").unwrap().name, "index.html");
    assert_eq!(volume.create("www", "Index.html").unwrap_err(), FsError::Exists);
    volume.sync().unwrap();
    std::fs::write(&path, volume.into_device().data).unwrap();
    let (code, output) = macos::fsck(&path);
    assert_eq!(macos::findings(&output), Vec::<String>::new(), "fsck_msdos said:\n{}", output);
    assert_eq!(code, 0);
    let _ = std::fs::remove_dir_all(&dir);
}
