//! fatdisk's library: the kernel's FAT32 code compiled for the Mac, and what
//! the command and the tests need around it.

extern crate alloc;

/// The kernel's FAT32 code, as it is. Its files use only `core` and `alloc`.
///
/// src/fat is a symbolic link to kernel/src/storage/fat rather than a
/// `#[path]` out of this directory: rustc follows either, but rust-analyzer
/// resolves a module file only inside the directory of the package declaring
/// it, and with the `#[path]` it left every name from this module unresolved.
pub mod fat;

pub mod cut;
pub mod damage;
pub mod image;
pub mod macos;
