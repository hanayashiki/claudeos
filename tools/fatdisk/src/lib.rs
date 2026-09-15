//! fatdisk's library: the kernel's FAT32 code compiled for the Mac, and what
//! the command and the tests need around it.

extern crate alloc;

/// The kernel's FAT32 code, as it is. Its files use only `core` and `alloc`.
#[path = "../../../kernel/src/storage/fat/mod.rs"]
pub mod fat;

pub mod cut;
pub mod damage;
pub mod image;
pub mod macos;
