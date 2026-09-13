//! Renaming, through the system call rather than through `mv`.
//!
//! `mv` turns a destination that is a directory into a move *into* that
//! directory, so the cases where the destination itself is the new name -- a
//! directory over a directory, a file over a directory -- cannot be reached
//! from a shell at all. The ones that can are in tests/suite.sh; these make
//! the call directly.

use crate::sys;
use crate::Report;

const ENOTDIR: i64 = -20;
const EISDIR: i64 = -21;
const ENOTEMPTY: i64 = -39;

pub fn run(report: &mut Report) {
    let _ = std::fs::remove_dir_all("/tmp/rn");
    if std::fs::create_dir_all("/tmp/rn/full/kept")
        .and_then(|()| std::fs::create_dir_all("/tmp/rn/dir"))
        .and_then(|()| std::fs::write("/tmp/rn/file", b"content\n"))
        .is_err()
    {
        report.check("somewhere to rename things", false, String::new());
        return;
    }

    let onto_full = sys::rename("/tmp/rn/dir", "/tmp/rn/full");
    report.check(
        "a directory that still has entries is not replaced",
        onto_full == ENOTEMPTY,
        format!("rename reported {}", onto_full),
    );
    report.check(
        "and what was inside it is still there",
        std::path::Path::new("/tmp/rn/full/kept").is_dir(),
        String::from("the entry under the destination went away"),
    );

    let onto_dir = sys::rename("/tmp/rn/file", "/tmp/rn/dir");
    report.check(
        "a file does not take the name of a directory",
        onto_dir == EISDIR,
        format!("rename reported {}", onto_dir),
    );

    let onto_file = sys::rename("/tmp/rn/dir", "/tmp/rn/file");
    report.check(
        "a directory does not take the name of a file",
        onto_file == ENOTDIR,
        format!("rename reported {}", onto_file),
    );

    let _ = std::fs::remove_dir_all("/tmp/rn");
}
