//! The kernel's reading of a device tree's memory nodes, reservations,
//! command line and ram disk, compiled for the Mac.
//!
//! Both modules are links into kernel/src, as tools/fatdisk's src/fat is, so
//! what the tests run is the file the kernel builds. `machine` names the other
//! as `crate::boot`, which is where it sits in both crates.

/// kernel/src/boot.rs: what the reader fills in.
pub mod boot;

/// kernel/src/arch/aarch64/fdt/machine.rs: the reader.
pub mod machine;
