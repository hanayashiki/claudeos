//! The boot-time check of the kernel and the core software against the digests
//! the build wrote into the image.
//!
//! The manifest is `/etc/claudeos/checksums`, one line per item in the form
//! shasum prints: forty hex digits, two spaces, a name. The name `kernel` stands
//! for this kernel's own code and read-only data as they are in memory; any
//! other name is a file in the unpacked image. What the build puts in it is
//! listed in scripts/images.sh.
//!
//! SHA-1 is enough for what this is for, which is noticing accidental damage: a
//! card going bad, a transfer cut short, an image copied from the wrong build.
//! It is no defence against someone who can write the image, who can rewrite
//! the manifest beside it just as easily.
//!
//! Nothing here stops the boot. A damaged item, a missing one or a manifest that
//! cannot be read is reported and the machine carries on to init, because a
//! board reached only through its own shell is more use running with a report
//! than stopped with one.

use crate::abi::Errno;
use crate::fs::{self, Node, NodeKind, Offset};
use crate::sync::Spinlock;
use alloc::string::String;
use core::fmt::{self, Write};
use sha1::{Digest, Sha1};

/// Where the build puts the manifest.
pub const MANIFEST: &str = "/etc/claudeos/checksums";

/// The manifest's name for the kernel's own bytes, as opposed to a file.
const KERNEL: &str = "kernel";

/// The largest manifest that is read. The build writes a few hundred bytes. The
/// limit keeps a file that is not a manifest at all from being read line by
/// line into the kernel log.
const MAX_MANIFEST: usize = 64 * 1024;

/// The longest line taken as an entry.
const MAX_LINE: usize = 512;

/// How many malformed lines get a line of their own on the console. The rest
/// are counted in the summary, so a garbled manifest cannot fill the kernel
/// log, which holds 16 KiB, and push the rest of the boot out of it.
const MAX_REPORTED: usize = 8;

/// How much of a file is copied out under one hold of its node's lock. The lock
/// masks interrupts, and a copy this size takes well under a millisecond.
const PIECE: usize = 64 * 1024;

const DIGEST_LEN: usize = 20;
type Sum = [u8; DIGEST_LEN];

extern "C" {
    /// The kernel's checked bytes: from the start of .text to the end of
    /// .rodata, which do not change once the kernel is loaded. .data and .bss
    /// are left out because they do. The linker scripts define both symbols,
    /// and tools/checksums.py reads the same two symbols out of the ELF to hash
    /// the same bytes at build time, so the two ends cannot name different
    /// ranges.
    static __integrity_start: u8;
    static __integrity_end: u8;
}

/// What the check printed, kept for /proc/claudeos/integrity.
static REPORT: Spinlock<Option<String>> = Spinlock::new(None);

/// The text of /proc/claudeos/integrity.
pub fn report() -> String {
    match REPORT.lock().as_ref() {
        Some(text) => text.clone(),
        None => String::from("integrity: not checked yet\n"),
    }
}

/// Check every item the manifest names, printing a line for each and then a
/// summary, and keep what was printed for /proc/claudeos/integrity.
///
/// Boot calls this with interrupts masked, so the watchdog on the board is not
/// fed while it runs. The watchdog allows 15 seconds, and hashing the items the
/// build lists takes a small fraction of one; an item the size of cloudflared
/// would be worth timing before it is added.
pub fn check() {
    let started = crate::time::monotonic_ns();
    let mut report = Report { text: String::new() };

    match read_manifest() {
        Manifest::Absent => report.line(format_args!(
            "no {} in the image, so there is nothing to check",
            MANIFEST
        )),
        Manifest::Unusable(reason) => {
            report.line(format_args!("{} {}, so nothing was checked", MANIFEST, reason))
        }
        Manifest::Text(text) => {
            let tally = check_entries(&text, &mut report);
            let elapsed = crate::time::monotonic_ns().saturating_sub(started);
            report.line(format_args!(
                "{} ok, {} damaged, {} missing, {} malformed lines skipped, in {}.{:03} ms",
                tally.ok,
                tally.damaged,
                tally.missing,
                tally.malformed,
                elapsed / 1_000_000,
                elapsed / 1_000 % 1_000
            ));
        }
    }

    *REPORT.lock() = Some(report.text);
}

/// Lines printed on the console, which also puts them in the kernel log, and
/// kept for the status file.
struct Report {
    text: String,
}

impl Report {
    fn line(&mut self, args: fmt::Arguments) {
        println!("integrity: {}", args);
        let _ = writeln!(self.text, "integrity: {}", args);
    }
}

enum Manifest {
    Absent,
    Unusable(String),
    Text(alloc::vec::Vec<u8>),
}

fn read_manifest() -> Manifest {
    let node = match fs::lookup(MANIFEST) {
        Ok(node) => node,
        Err(Errno::ENOENT) => return Manifest::Absent,
        Err(err) => return Manifest::Unusable(alloc::format!("cannot be looked up ({:?})", err)),
    };
    if node.kind != NodeKind::File {
        return Manifest::Unusable(String::from("is not a regular file"));
    }
    let size = node.size();
    if size > MAX_MANIFEST as u64 {
        return Manifest::Unusable(alloc::format!(
            "is {} bytes, more than the {} a manifest may be",
            size, MAX_MANIFEST
        ));
    }
    let mut text = alloc::vec::Vec::new();
    match read_pieces(&node, |piece| text.extend_from_slice(piece)) {
        Ok(()) => Manifest::Text(text),
        Err(err) => Manifest::Unusable(alloc::format!("cannot be read ({:?})", err)),
    }
}

#[derive(Default)]
struct Tally {
    ok: usize,
    damaged: usize,
    missing: usize,
    malformed: usize,
}

fn check_entries(manifest: &[u8], report: &mut Report) -> Tally {
    let mut tally = Tally::default();
    // Hashed once however many lines name it.
    let mut kernel: Option<Sum> = None;

    for (index, line) in manifest.split(|&byte| byte == b'\n').enumerate() {
        if line.is_empty() {
            continue;
        }
        let (expected, name) = match parse_line(line) {
            Ok(entry) => entry,
            Err(reason) => {
                tally.malformed += 1;
                if tally.malformed <= MAX_REPORTED {
                    report.line(format_args!(
                        "line {} of {} skipped: {}",
                        index + 1,
                        MANIFEST,
                        reason
                    ));
                }
                continue;
            }
        };
        let actual = if name == KERNEL {
            Ok(*kernel.get_or_insert_with(kernel_digest))
        } else {
            file_digest(name)
        };
        match actual {
            Ok(actual) if actual == expected => {
                tally.ok += 1;
                report.line(format_args!("{} ok", name));
            }
            Ok(actual) => {
                tally.damaged += 1;
                report.line(format_args!(
                    "{} DAMAGED: expected {}, got {}",
                    name,
                    Hex(&expected),
                    Hex(&actual)
                ));
            }
            Err(Absent::NotThere) => {
                tally.missing += 1;
                report.line(format_args!("{} missing", name));
            }
            Err(Absent::NotAFile) => {
                tally.missing += 1;
                report.line(format_args!("{} missing: it is not a regular file", name));
            }
            Err(Absent::Unreadable(err)) => {
                tally.missing += 1;
                report.line(format_args!("{} missing: it cannot be read ({:?})", name, err));
            }
        }
    }
    if tally.malformed > MAX_REPORTED {
        report.line(format_args!(
            "{} more malformed lines skipped without a line each",
            tally.malformed - MAX_REPORTED
        ));
    }
    tally
}

/// Why a manifest line was not taken as an entry.
enum Malformed {
    TooLong(usize),
    TooShort,
    NotHex,
    NoSeparator,
    NotUtf8,
    ControlCharacter,
    BadName,
}

impl fmt::Display for Malformed {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Malformed::TooLong(len) => {
                write!(f, "it is {} bytes long, and a line may be {}", len, MAX_LINE)
            }
            Malformed::TooShort => f.write_str("too short for a digest, two spaces and a name"),
            Malformed::NotHex => f.write_str("the digest holds a character that is not a hex digit"),
            Malformed::NoSeparator => {
                f.write_str("the 40 hex digits of the digest are not followed by two spaces")
            }
            Malformed::NotUtf8 => f.write_str("the name is not UTF-8"),
            Malformed::ControlCharacter => f.write_str("the name holds a control character"),
            Malformed::BadName => f.write_str("the name is neither `kernel` nor an absolute path"),
        }
    }
}

/// One manifest line, as shasum prints it: the digest in hex, two spaces, and
/// the name.
fn parse_line(line: &[u8]) -> Result<(Sum, &str), Malformed> {
    if line.len() > MAX_LINE {
        return Err(Malformed::TooLong(line.len()));
    }
    let hex_len = DIGEST_LEN * 2;
    if line.len() < hex_len + 3 {
        return Err(Malformed::TooShort);
    }
    let mut sum = [0u8; DIGEST_LEN];
    for (byte, pair) in sum.iter_mut().zip(line[..hex_len].chunks_exact(2)) {
        let (Some(high), Some(low)) = (hex_digit(pair[0]), hex_digit(pair[1])) else {
            return Err(Malformed::NotHex);
        };
        *byte = high << 4 | low;
    }
    // Two spaces, or a space and an asterisk, which is what shasum prints for a
    // file it read in binary mode. The digest is the same either way.
    let separator = &line[hex_len..hex_len + 2];
    if separator != b"  " && separator != b" *" {
        return Err(Malformed::NoSeparator);
    }
    let name = core::str::from_utf8(&line[hex_len + 2..]).map_err(|_| Malformed::NotUtf8)?;
    // The name is printed on the console, where a control character would be
    // taken by the terminal as part of an escape sequence.
    if name.bytes().any(|byte| byte < 0x20 || byte == 0x7f) {
        return Err(Malformed::ControlCharacter);
    }
    if name != KERNEL && !name.starts_with('/') {
        return Err(Malformed::BadName);
    }
    Ok((sum, name))
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn kernel_digest() -> Sum {
    let start = core::ptr::addr_of!(__integrity_start) as usize;
    let end = core::ptr::addr_of!(__integrity_end) as usize;
    // SAFETY: the linker scripts place both symbols in the kernel image, the
    // first at the start of .text and the second at the end of .rodata, with
    // .rodata after .text. That range is mapped for as long as the kernel runs
    // and nothing writes to it.
    let bytes = unsafe { core::slice::from_raw_parts(start as *const u8, end - start) };
    let mut sum = [0u8; DIGEST_LEN];
    sum.copy_from_slice(&Sha1::digest(bytes));
    sum
}

/// Why a named file has no digest.
enum Absent {
    NotThere,
    /// A directory, a device or a /proc file. Devices are refused rather than
    /// read because a read of /dev/zero never comes to an end.
    NotAFile,
    Unreadable(Errno),
}

fn file_digest(path: &str) -> Result<Sum, Absent> {
    let node = fs::lookup(path).map_err(|_| Absent::NotThere)?;
    if node.kind != NodeKind::File {
        return Err(Absent::NotAFile);
    }
    let mut hasher = Sha1::new();
    read_pieces(&node, |piece| hasher.update(piece)).map_err(Absent::Unreadable)?;
    let mut sum = [0u8; DIGEST_LEN];
    sum.copy_from_slice(&hasher.finalize());
    Ok(sum)
}

/// Hand `take` the whole of a regular file, a piece at a time.
fn read_pieces(node: &Node, mut take: impl FnMut(&[u8])) -> Result<(), Errno> {
    let mut buffer = alloc::vec![0u8; PIECE];
    let mut offset = 0u64;
    loop {
        let count = node.read_at(Offset::new(offset), &mut buffer)?;
        if count == 0 {
            return Ok(());
        }
        take(&buffer[..count]);
        offset += count as u64;
    }
}

/// A digest in lower-case hex, as shasum prints it.
struct Hex<'a>(&'a [u8]);

impl fmt::Display for Hex<'_> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{:02x}", byte)?;
        }
        Ok(())
    }
}
