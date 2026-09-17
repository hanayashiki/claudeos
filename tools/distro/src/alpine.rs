//! The Alpine fixtures: Alpine's minirootfs as Alpine publishes it, with
//! tests/alpine.sh at /root/alpine.sh, for each machine. A tree of its own,
//! since it shares no node with the images; scripts/test.sh boots it with
//! `init=/bin/sh /root/alpine.sh`.
//!
//! Everything in it is dynamically linked against musl and loaded by Alpine's
//! own ld-musl, so booting it exercises the program interpreter path with a
//! userland this project had no hand in building. It has no manifest, and the
//! kernel says there is nothing to check.

use crate::downloads::{Download, ALPINE_MINIROOTFS_AARCH64, ALPINE_MINIROOTFS_X86_64};
use crate::tree::{dir, file, Contents, Node, Source};
use crate::variant::{Variants, ALPINE, ALPINE_AARCH64, ALPINE_X86_64};

pub fn tree() -> Node {
    dir("", ALPINE).holding(vec![
        root(ALPINE_X86_64, &ALPINE_MINIROOTFS_X86_64),
        root(ALPINE_AARCH64, &ALPINE_MINIROOTFS_AARCH64),
    ])
}

fn root(variants: Variants, minirootfs: &'static Download) -> Node {
    dir("root", variants).contents(Contents::Unpack(minirootfs)).note("the image: Alpine's minirootfs").holding(vec![
        // 0700, as the minirootfs has it.
        dir("root", variants).mode(0o700).holding(vec![
            file("alpine.sh", variants, 0o755, Source::Repo("tests/alpine.sh")).note("the suite run inside Alpine"),
        ]),
    ])
}
