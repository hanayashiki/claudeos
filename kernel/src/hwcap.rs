//! The processor's features as a program is told them: the AT_HWCAP and
//! AT_HWCAP2 words of the auxiliary vector.
//!
//! What the bits mean, and the rule for setting each, belongs to the machine,
//! and `arch::elf_hwcaps` holds it. What is common is when: the words are read
//! from the hardware once at boot, before any program starts, and every exec
//! hands out the same two, as Linux's `ELF_HWCAP` and `ELF_HWCAP2` are fixed
//! once its processor features have been set up.

use core::sync::atomic::{AtomicU64, Ordering};

static HWCAP: AtomicU64 = AtomicU64::new(0);
static HWCAP2: AtomicU64 = AtomicU64::new(0);

/// Read the words and keep them. After `arch::init_cpu`, because on x86-64
/// AT_HWCAP2 depends on what that turns on.
pub fn init() {
    let (hwcap, hwcap2) = crate::arch::elf_hwcaps();
    HWCAP.store(hwcap, Ordering::Relaxed);
    HWCAP2.store(hwcap2, Ordering::Relaxed);
    println!("cpu: hwcap {:#x} hwcap2 {:#x}", hwcap, hwcap2);
}

pub fn hwcap() -> u64 {
    HWCAP.load(Ordering::Relaxed)
}

pub fn hwcap2() -> u64 {
    HWCAP2.load(Ordering::Relaxed)
}
