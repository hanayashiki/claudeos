//! The machine-dependent half of the kernel.
//!
//! Everything that only makes sense on one instruction set lives under this
//! module. The target is chosen here, once, and the rest of the kernel talks
//! to whichever implementation was selected through the names re-exported
//! below. There is exactly one target compiled at a time, so the selection is
//! a `cfg` and the interface is plain functions and types rather than a trait.

#[cfg(target_arch = "x86_64")]
#[path = "x86_64/mod.rs"]
mod imp;

#[cfg(target_arch = "aarch64")]
#[path = "aarch64/mod.rs"]
mod imp;

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
compile_error!("this target has no implementation under kernel/src/arch");

pub use imp::*;
