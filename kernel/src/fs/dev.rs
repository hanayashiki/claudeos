//! Character devices under /dev.

use super::{link_node, Node, NodeKind};
use crate::abi::{Errno, S_IFCHR};
use core::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceKind {
    Null,
    Zero,
    Full,
    Random,
    Urandom,
    Console,
    Tty,
}

impl DeviceKind {
    /// Device number as Linux encodes it in st_rdev.
    pub fn rdev(self) -> u64 {
        let (major, minor) = match self {
            DeviceKind::Null => (1u64, 3u64),
            DeviceKind::Zero => (1, 5),
            DeviceKind::Full => (1, 7),
            DeviceKind::Random => (1, 8),
            DeviceKind::Urandom => (1, 9),
            DeviceKind::Console => (5, 1),
            DeviceKind::Tty => (5, 0),
        };
        (major << 8) | minor
    }

    pub fn is_tty(self) -> bool {
        matches!(self, DeviceKind::Console | DeviceKind::Tty)
    }
}

pub fn populate() {
    let entries = [
        ("/dev/null", DeviceKind::Null),
        ("/dev/zero", DeviceKind::Zero),
        ("/dev/full", DeviceKind::Full),
        ("/dev/random", DeviceKind::Random),
        ("/dev/urandom", DeviceKind::Urandom),
        ("/dev/console", DeviceKind::Console),
        ("/dev/tty", DeviceKind::Tty),
    ];
    for (path, kind) in entries {
        let node = Node::new(NodeKind::Device(kind), S_IFCHR | 0o666);
        let _ = link_node(path, node);
    }
    let _ = super::symlink("/dev/stdin", "/proc/self/fd/0");
    let _ = super::symlink("/dev/stdout", "/proc/self/fd/1");
    let _ = super::symlink("/dev/stderr", "/proc/self/fd/2");
}

static RNG_STATE: AtomicU64 = AtomicU64::new(0x2545F4914F6CDD1D);

/// xorshift64*, seeded from the timestamp counter at first use.
pub fn random_u64() -> u64 {
    let mut x = RNG_STATE.load(Ordering::Relaxed);
    if x == 0x2545F4914F6CDD1D {
        let tsc: u64;
        unsafe {
            core::arch::asm!("rdtsc", out("eax") _, out("edx") _, options(nomem, nostack));
            core::arch::asm!("rdtsc; shl rdx, 32; or rax, rdx", out("rax") tsc, out("rdx") _,
                             options(nomem, nostack));
        }
        x ^= tsc.wrapping_mul(0x9E3779B97F4A7C15);
    }
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    RNG_STATE.store(x, Ordering::Relaxed);
    x.wrapping_mul(0x2545F4914F6CDD1D)
}

pub fn fill_random(buf: &mut [u8]) {
    let mut i = 0;
    while i < buf.len() {
        let value = random_u64().to_le_bytes();
        let n = (buf.len() - i).min(8);
        buf[i..i + n].copy_from_slice(&value[..n]);
        i += n;
    }
}

pub fn read(kind: DeviceKind, buf: &mut [u8]) -> Result<usize, Errno> {
    match kind {
        DeviceKind::Null => Ok(0),
        DeviceKind::Zero | DeviceKind::Full => {
            buf.fill(0);
            Ok(buf.len())
        }
        DeviceKind::Random | DeviceKind::Urandom => {
            fill_random(buf);
            Ok(buf.len())
        }
        DeviceKind::Console | DeviceKind::Tty => crate::console::read(buf),
    }
}

pub fn write(kind: DeviceKind, buf: &[u8]) -> Result<usize, Errno> {
    match kind {
        DeviceKind::Null => Ok(buf.len()),
        DeviceKind::Zero | DeviceKind::Random | DeviceKind::Urandom => Ok(buf.len()),
        DeviceKind::Full => Err(Errno::ENOSPC),
        DeviceKind::Console | DeviceKind::Tty => {
            crate::console::write(buf);
            Ok(buf.len())
        }
    }
}
