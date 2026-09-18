//! mmtest's library: the kernel's memory-management code that uses nothing of
//! the kernel, compiled for the Mac so its tests run under `cargo test`.

extern crate alloc;

/// The kernel's `mm::active`, as it is: the reference a processor holds on the
/// address space it is on, and the frees put off until interrupts are on.
///
/// src/active.rs is a symbolic link to kernel/src/mm/active.rs, for the reason
/// tools/fatdisk/src/lib.rs gives for its link to the FAT code.
pub mod active;
