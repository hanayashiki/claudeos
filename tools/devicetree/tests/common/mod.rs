//! What the tests share: a writer of flattened device trees, and a read of one
//! into a fresh BootInfo.

#![allow(dead_code)]

use devicetree::boot::{BootInfo, PhysRange};
use devicetree::machine::{self, Found};

const MAGIC: u32 = 0xD00D_FEED;
const BEGIN_NODE: u32 = 1;
const END_NODE: u32 = 2;
const PROP: u32 = 3;
const END: u32 = 9;

/// A tree written node by node, in the order the calls make, laid out as dtc
/// lays one out: header, reservation block, structure block, strings block.
pub struct Tree {
    structure: Vec<u8>,
    strings: Vec<u8>,
    reservations: Vec<(u64, u64)>,
}

impl Tree {
    pub fn new() -> Tree {
        Tree { structure: Vec::new(), strings: Vec::new(), reservations: Vec::new() }
    }

    fn word(&mut self, value: u32) {
        self.structure.extend_from_slice(&value.to_be_bytes());
    }

    fn pad(&mut self) {
        while self.structure.len() % 4 != 0 {
            self.structure.push(0);
        }
    }

    pub fn memreserve(&mut self, address: u64, size: u64) -> &mut Tree {
        self.reservations.push((address, size));
        self
    }

    pub fn begin(&mut self, name: &str) -> &mut Tree {
        self.word(BEGIN_NODE);
        self.structure.extend_from_slice(name.as_bytes());
        self.structure.push(0);
        self.pad();
        self
    }

    pub fn end(&mut self) -> &mut Tree {
        self.word(END_NODE);
        self
    }

    pub fn prop(&mut self, name: &str, value: &[u8]) -> &mut Tree {
        let offset = self.strings.len() as u32;
        self.strings.extend_from_slice(name.as_bytes());
        self.strings.push(0);
        self.word(PROP);
        self.word(value.len() as u32);
        self.word(offset);
        self.structure.extend_from_slice(value);
        self.pad();
        self
    }

    /// A property of 32-bit cells.
    pub fn cells(&mut self, name: &str, values: &[u32]) -> &mut Tree {
        let bytes: Vec<u8> = values.iter().flat_map(|value| value.to_be_bytes()).collect();
        self.prop(name, &bytes)
    }

    /// A string property, with its terminator.
    pub fn text(&mut self, name: &str, value: &str) -> &mut Tree {
        let mut bytes = value.as_bytes().to_vec();
        bytes.push(0);
        self.prop(name, &bytes)
    }

    pub fn build(&self) -> Vec<u8> {
        let mut structure = self.structure.clone();
        structure.extend_from_slice(&END.to_be_bytes());

        let reserve_at = 40usize;
        let struct_at = reserve_at + (self.reservations.len() + 1) * 16;
        let strings_at = struct_at + structure.len();
        let total = strings_at + self.strings.len();

        let mut blob = Vec::with_capacity(total);
        for word in [
            MAGIC,
            total as u32,
            struct_at as u32,
            strings_at as u32,
            reserve_at as u32,
            17,
            16,
            0,
            self.strings.len() as u32,
            structure.len() as u32,
        ] {
            blob.extend_from_slice(&word.to_be_bytes());
        }
        for &(address, size) in self.reservations.iter().chain([(0, 0)].iter()) {
            blob.extend_from_slice(&address.to_be_bytes());
            blob.extend_from_slice(&size.to_be_bytes());
        }
        blob.extend_from_slice(&structure);
        blob.extend_from_slice(&self.strings);
        blob
    }
}

/// Read `blob` into a fresh BootInfo, as kmain reads the firmware's.
pub fn read(blob: &[u8]) -> Option<(BootInfo, Found)> {
    let mut info = BootInfo::new();
    let found = machine::read(blob, &mut info)?;
    Some((info, found))
}

/// The reserved ranges as pairs, for comparing.
pub fn reserved(info: &BootInfo) -> Vec<(u64, u64)> {
    info.reserved().iter().map(|&PhysRange { start, end }| (start, end)).collect()
}

/// The regions as (address, length), for comparing.
pub fn regions(info: &BootInfo) -> Vec<(u64, u64)> {
    info.regions().iter().map(|region| (region.addr, region.len)).collect()
}

/// A path under the repository's build/, from `variable` when it is set.
/// Missing, the test fails and says how to make it: a skip would pass.
pub fn build_file(variable: &str, relative: &str, how: &str) -> Vec<u8> {
    let path = match std::env::var_os(variable) {
        Some(path) => std::path::PathBuf::from(path),
        None => std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").join(relative),
    };
    std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {} (make it with: {})", path.display(), e, how))
}
