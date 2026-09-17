//! The variants a tree is built for, and sets of them.

use std::fmt;

/// One folder under build/distro, and the image packed from it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Variant {
    /// What scripts/test.sh boots on x86-64 under QEMU.
    TestX86_64,
    /// What scripts/test.sh boots on aarch64 under QEMU's raspi4b.
    TestAarch64,
    /// What a Raspberry Pi 4 boots, with the card's /data and boot files.
    BoardAarch64,
    /// Alpine's own root filesystem with tests/alpine.sh, on x86-64.
    AlpineX86_64,
    /// The same on aarch64.
    AlpineAarch64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Arch {
    X86_64,
    Aarch64,
}

impl Variant {
    pub const ALL: [Variant; 5] = [
        Variant::TestX86_64,
        Variant::TestAarch64,
        Variant::BoardAarch64,
        Variant::AlpineX86_64,
        Variant::AlpineAarch64,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Variant::TestX86_64 => "test-x86_64",
            Variant::TestAarch64 => "test-aarch64",
            Variant::BoardAarch64 => "board-aarch64",
            Variant::AlpineX86_64 => "alpine-x86_64",
            Variant::AlpineAarch64 => "alpine-aarch64",
        }
    }

    pub fn parse(name: &str) -> Option<Variant> {
        Variant::ALL.into_iter().find(|variant| variant.name() == name)
    }

    pub fn arch(self) -> Arch {
        match self {
            Variant::TestX86_64 | Variant::AlpineX86_64 => Arch::X86_64,
            Variant::TestAarch64 | Variant::BoardAarch64 | Variant::AlpineAarch64 => Arch::Aarch64,
        }
    }

    const fn bit(self) -> u8 {
        1 << self as u8
    }
}

impl fmt::Display for Variant {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl Arch {
    /// The target the programs in its images are built for.
    pub fn musl_target(self) -> &'static str {
        match self {
            Arch::X86_64 => "x86_64-unknown-linux-musl",
            Arch::Aarch64 => "aarch64-unknown-linux-musl",
        }
    }

    /// Go's word for the machine.
    pub fn goarch(self) -> &'static str {
        match self {
            Arch::X86_64 => "amd64",
            Arch::Aarch64 => "arm64",
        }
    }

    /// The kernel ELF scripts/build.sh writes for this machine, from the
    /// repository root. On x86-64 it is the ELF32 copy QEMU boots, whose
    /// addresses are the ELF64's truncated to 32 bits, symbols and segments
    /// alike, so the digest of its checked bytes is the same.
    pub fn kernel_elf(self) -> &'static str {
        match self {
            Arch::X86_64 => "build/kernel.elf",
            Arch::Aarch64 => "build/kernel-aarch64.elf",
        }
    }
}

/// A set of variants: which variants have a node.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Variants(u8);

impl Variants {
    pub const fn of(list: &[Variant]) -> Variants {
        let mut bits = 0;
        let mut i = 0;
        while i < list.len() {
            bits |= list[i].bit();
            i += 1;
        }
        Variants(bits)
    }

    pub fn has(self, variant: Variant) -> bool {
        self.0 & variant.bit() != 0
    }

    /// Whether every variant in `other` is in this set.
    pub fn covers(self, other: Variants) -> bool {
        other.0 & !self.0 == 0
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub fn list(self) -> impl Iterator<Item = Variant> {
        Variant::ALL.into_iter().filter(move |variant| self.has(*variant))
    }
}

pub const TEST_X86_64: Variants = Variants::of(&[Variant::TestX86_64]);
pub const TEST_AARCH64: Variants = Variants::of(&[Variant::TestAarch64]);
pub const BOARD: Variants = Variants::of(&[Variant::BoardAarch64]);
pub const TESTS: Variants = Variants::of(&[Variant::TestX86_64, Variant::TestAarch64]);
/// The two aarch64 images.
pub const AARCH64: Variants = Variants::of(&[Variant::TestAarch64, Variant::BoardAarch64]);
/// The three images of this project's own userland.
pub const IMAGES: Variants = Variants::of(&[Variant::TestX86_64, Variant::TestAarch64, Variant::BoardAarch64]);
pub const ALPINE_X86_64: Variants = Variants::of(&[Variant::AlpineX86_64]);
pub const ALPINE_AARCH64: Variants = Variants::of(&[Variant::AlpineAarch64]);
pub const ALPINE: Variants = Variants::of(&[Variant::AlpineX86_64, Variant::AlpineAarch64]);
