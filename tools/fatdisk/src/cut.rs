//! Power cuts. A sequence of operations runs on one mounted volume, as the
//! kernel runs them, over blocks that record every block written, one entry
//! per block in the order written. Then, for every count of an operation's
//! writes from none to all of them, the volume as a card would hold it after a
//! cut there is checked with fsck_msdos -n. A block write is taken to be whole
//! or absent, which is what an SD card's controller gives a write of one block.

use crate::fat::boot::Layout;
use crate::fat::{join, Blocks, FsError, Volume};
use crate::image::{self, Memory, Recorder, SECTOR};
use crate::macos;
use std::fs::OpenOptions;
use std::path::Path;

/// fsck_msdos -n after a cut following `writes` of an operation's writes,
/// the last of them to `block`.
pub struct Cut {
    pub writes: usize,
    pub block: Option<u64>,
    pub exit: i32,
    pub findings: Vec<String>,
}

pub struct Outcome {
    pub name: String,
    pub layout: Layout,
    pub cuts: Vec<Cut>,
}

/// What a block of the volume is.
pub fn what(layout: &Layout, block: u64) -> String {
    let fat_end = layout.fat_start as u64 + layout.fats as u64 * layout.fat_blocks as u64;
    if block == 0 {
        String::from("boot sector")
    } else if Some(block) == layout.fsinfo.map(u64::from) {
        String::from("FSInfo")
    } else if block < layout.fat_start as u64 {
        format!("reserved sector {}", block)
    } else if block < fat_end {
        let within = block - layout.fat_start as u64;
        format!("FAT {} sector {}", within / layout.fat_blocks as u64, within % layout.fat_blocks as u64)
    } else {
        let cluster = (block - layout.data_start as u64) / layout.cluster_blocks as u64 + 2;
        format!("cluster {}", cluster)
    }
}

fn write_file<B: Blocks>(volume: &mut Volume<B>, dir: &str, name: &str, data: &[u8]) -> Result<(), FsError> {
    volume.create(dir, name)?;
    volume.write(&join(dir, name), 0, data)?;
    Ok(())
}

type Op = Box<dyn FnOnce(&mut Volume<Recorder>) -> Result<(), FsError>>;

/// The operations, in the order they run.
fn operations(cluster: usize) -> Vec<(&'static str, Op)> {
    let c = cluster as u64;
    vec![
        ("create an empty file with a long name, the first write since mounting", Box::new(|v| v.create("", "A long name for a new page.html").map(|_| ()))),
        ("write 3 clusters and 100 bytes to that empty file", Box::new(move |v| v.write("A long name for a new page.html", 0, &vec![b'w'; 3 * cluster + 100]).map(|_| ()))),
        ("append 2 clusters to a file that has clusters", Box::new(move |v| v.write("notes.txt", 5 * c, &vec![b'a'; 2 * cluster]).map(|_| ()))),
        ("overwrite bytes inside a file", Box::new(|v| v.write("notes.txt", 10, b"overwritten").map(|_| ()))),
        ("extend a file with zeros by truncating it longer", Box::new(move |v| v.truncate("notes.txt", 9 * c + 7))),
        ("shrink a file to one cluster", Box::new(move |v| v.truncate("notes.txt", c))),
        ("shrink a file to nothing", Box::new(|v| v.truncate("notes.txt", 0))),
        ("make a directory", Box::new(|v| v.mkdir("", "a new directory").map(|_| ()))),
        ("create a file in a directory whose one cluster is full", Box::new(|v| v.create("full", "one more.txt").map(|_| ()))),
        ("unlink a file of 4 clusters with a long name", Box::new(|v| v.remove("", "doomed file.bin", false))),
        ("remove an empty directory", Box::new(|v| v.remove("", "empty", true))),
        ("rename a file within its directory", Box::new(|v| v.rename("", "a.txt", "", "a renamed file.txt").map(|_| ()))),
        ("rename a file within its directory, old and new entries in different sectors", Box::new(|v| v.rename("full", "F0000.TXT", "full", "a longer name that needs two long entries.txt").map(|_| ()))),
        ("move a directory to another parent", Box::new(|v| v.rename("site", "moving", "other", "moved").map(|_| ()))),
        ("rename a file over an existing file", Box::new(|v| v.rename("", "new.txt", "", "old.txt").map(|_| ()))),
        ("sync", Box::new(|v| v.sync())),
    ]
}

/// Every operation in turn on a 40 MiB volume newfs_msdos made and this code
/// filled, with fsck_msdos -n after every cut.
pub fn outcomes(scratch: &Path) -> Result<Vec<Outcome>, String> {
    std::fs::create_dir_all(scratch).map_err(|e| e.to_string())?;
    let path = scratch.join("base.img");
    macos::newfs_whole(&path, 40, "CUTS", None)?;
    let mut setup = image::mount(Memory { data: std::fs::read(&path).map_err(|e| e.to_string())? })?;
    let cluster = setup.layout().cluster_bytes as usize;
    let fail = |e: FsError| format!("setting up: {:?}", e);
    setup.mkdir("", "site").map_err(fail)?;
    setup.mkdir("site", "moving").map_err(fail)?;
    write_file(&mut setup, "site/moving", "page.html", b"<p>moved with its directory</p>").map_err(fail)?;
    setup.mkdir("", "other").map_err(fail)?;
    setup.mkdir("", "empty").map_err(fail)?;
    setup.mkdir("", "full").map_err(fail)?;
    // `.`, `..` and one-slot names, to exactly one cluster.
    for i in 0..cluster / 32 - 2 {
        setup.create("full", &format!("F{:04}.TXT", i)).map_err(fail)?;
    }
    write_file(&mut setup, "", "notes.txt", &vec![b'n'; cluster * 5]).map_err(fail)?;
    write_file(&mut setup, "", "old.txt", &vec![b'o'; cluster * 3]).map_err(fail)?;
    write_file(&mut setup, "", "new.txt", &vec![b'N'; cluster * 2]).map_err(fail)?;
    write_file(&mut setup, "", "doomed file.bin", &vec![b'd'; cluster * 4]).map_err(fail)?;
    write_file(&mut setup, "", "a.txt", b"a").map_err(fail)?;
    setup.sync().map_err(fail)?;
    let base = setup.into_device().data;

    let file = scratch.join("cut.img");
    std::fs::write(&file, &base).map_err(|e| e.to_string())?;
    let mut handle = OpenOptions::new().write(true).open(&file).map_err(|e| e.to_string())?;
    let mut volume = image::mount(Recorder::new(base))?;
    let layout = volume.layout();
    let mut applied = 0usize;
    let mut out = Vec::new();
    for (name, op) in operations(cluster) {
        let before = volume.device().writes.len();
        op(&mut volume).map_err(|e| format!("{}: {:?}", name, e))?;
        let after = volume.device().writes.len();
        let mut cuts = Vec::new();
        for count in before..=after {
            while applied < count {
                let (at, data) = &volume.device().writes[applied];
                image::write_at(&mut handle, at * SECTOR, data).map_err(|e| e.to_string())?;
                applied += 1;
            }
            let block = if count > before { Some(volume.device().writes[count - 1].0) } else { None };
            let (exit, output) = macos::fsck(&file);
            cuts.push(Cut { writes: count - before, block, exit, findings: macos::findings(&output) });
        }
        out.push(Outcome { name: name.to_string(), layout, cuts });
    }
    Ok(out)
}

/// Print what `outcomes` found, grouping adjacent cuts that fsck_msdos
/// reported the same way.
pub fn print(outcomes: &[Outcome]) {
    for outcome in outcomes {
        println!("== {}: {} block writes", outcome.name, outcome.cuts.len().saturating_sub(1));
        let mut i = 0;
        while let Some(cut) = outcome.cuts.get(i) {
            let mut j = i + 1;
            while outcome.cuts.get(j).is_some_and(|next| next.findings == cut.findings && next.exit == cut.exit) {
                j += 1;
            }
            let last = &outcome.cuts[j - 1];
            let blocks: Vec<String> = outcome.cuts[i..j].iter().filter_map(|cut| cut.block.map(|block| what(&outcome.layout, block))).collect();
            let span = if j - i == 1 { format!("after {} writes", cut.writes) } else { format!("after {} to {} writes", cut.writes, last.writes) };
            let written = if blocks.is_empty() { String::new() } else { format!(" ({})", blocks.join(", ")) };
            let said = if cut.findings.is_empty() { String::from("nothing reported") } else { cut.findings.join(" / ") };
            println!("   {}{}: exit {}: {}", span, written, cut.exit, said);
            i = j;
        }
    }
}

pub fn report() {
    let scratch = std::env::temp_dir().join(format!("fatdisk-cuts-{}", std::process::id()));
    match outcomes(&scratch) {
        Ok(outcomes) => print(&outcomes),
        Err(e) => {
            eprintln!("fatdisk: {}", e);
            std::process::exit(1);
        }
    }
    let _ = std::fs::remove_dir_all(&scratch);
}
