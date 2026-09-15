//! How many card writes one `write` of a file costs, by the size of the
//! write: a program writing a large file in small pieces pays this once per
//! piece. Prints block writes per call for appends of several sizes.

use fatdisk::fat::Volume;
use fatdisk::image::{self, Recorder};
use fatdisk::macos;

fn main() {
    let dir = std::env::temp_dir().join(format!("fatdisk-write-cost-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("volume.img");
    macos::newfs_whole(&path, 300, "COST", Some(8)).unwrap();
    let base = std::fs::read(&path).unwrap();
    for size in [8usize, 1024, 4096, 16384, 65536] {
        let mut volume: Volume<Recorder> = image::mount(Recorder::new(base.clone())).unwrap();
        volume.create("", "big.txt").unwrap();
        // Past the first write, which also marks the volume in use.
        volume.write("big.txt", 0, &vec![b'x'; size]).unwrap();
        let before = volume.device().writes.len();
        let calls = (4 * 1024 * 1024 / size).max(1);
        let mut offset = size as u64;
        for _ in 0..calls {
            volume.write("big.txt", offset, &vec![b'x'; size]).unwrap();
            offset += size as u64;
        }
        let writes = volume.device().writes.len() - before;
        // Runs of adjacent blocks, which is how many commands the card gets.
        let blocks: Vec<u64> = volume.device().writes[before..].iter().map(|(block, _)| *block).collect();
        let mut commands = 0;
        let mut previous: Option<u64> = None;
        for block in &blocks {
            if previous.map_or(true, |p| *block != p + 1) {
                commands += 1;
            }
            previous = Some(*block);
        }
        println!(
            "write of {:>6} bytes: {:>6} calls for 4 MiB, {:>7} block writes ({:.2} per call), about {:>6} card commands ({:.2} per call)",
            size,
            calls,
            writes,
            writes as f64 / calls as f64,
            commands,
            commands as f64 / calls as f64
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}
