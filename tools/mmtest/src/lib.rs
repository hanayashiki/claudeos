//! mmtest's library: the kernel's memory-management code that uses nothing of
//! the kernel, compiled for the Mac so its tests run under `cargo test`.

extern crate alloc;

/// The kernel's `mm::active`, as it is: the reference a processor holds on the
/// address space it is on, and the frees put off until interrupts are on.
///
/// src/active.rs is a symbolic link to kernel/src/mm/active.rs, for the reason
/// tools/fatdisk/src/lib.rs gives for its link to the FAT code.
pub mod active;

/// The kernel's `mm::walk`, as it is: the walk of a four-level translation
/// table and every change to one, over whatever memory and frames the machine
/// underneath supplies. Here that is `host`, a `Vec`.
///
/// src/walk.rs is a symbolic link to kernel/src/mm/walk.rs, for the reason
/// tools/fatdisk/src/lib.rs gives for its link to the FAT code.
pub mod walk;

pub mod host;
