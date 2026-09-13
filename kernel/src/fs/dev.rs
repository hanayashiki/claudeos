//! Character devices under /dev.

use super::{link_node, Node, NodeKind};
use crate::abi::{Errno, S_IFCHR};

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

/// `/dev/random` and `/dev/urandom` are the same device here, as they are on
/// Linux once its pool has been filled: both hand out the kernel generator's
/// output and neither ever blocks. The generator is seeded before the first
/// process runs, so there is no window in which one of them would have to wait
/// for the other.
pub fn fill_random(buf: &mut [u8]) {
    crate::rng::fill(buf)
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
