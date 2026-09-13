//! The program loader, fed headers a linker would not write.
//!
//! Everything the loader is told about a segment -- where its bytes sit in the
//! file, where they go in memory, how many of them there are -- is a number
//! chosen by whoever wrote the file, and the loader adds those numbers up
//! before checking the sums against the size of the file and against where
//! user space ends. The kernel is built with overflow checks off, so a sum
//! that wraps is a small number and passes the check it was made for.
//!
//! Each image below wraps one of those sums. None of them may load, and the
//! machine has to still be running afterwards: a fault taken while the kernel
//! is copying is not a killed process, it is a stopped machine.

use crate::sys;
use crate::Report;

#[cfg(target_arch = "x86_64")]
const MACHINE: u16 = 0x3E;
#[cfg(target_arch = "aarch64")]
const MACHINE: u16 = 0xB7;

/// The ELF header and the one program header that follows it.
const HEADERS: u64 = 64 + 56;
/// Where these images ask to be loaded.
const LOAD: u64 = 0x0004_0000;
/// An address none of them maps, so an image that loads when it should not
/// faults on its first instruction rather than running what was copied in.
const NOWHERE: u64 = 0x0001_0000;

/// What the child exits with when the exec was refused the way it should be.
const REFUSED: i32 = 42;
/// What a well-formed image exits with, to say it ran.
const RAN: i32 = 7;

pub fn run(report: &mut Report) {
    a_well_formed_image_still_runs(report);
    a_segment_whose_file_extent_wraps(report);
    a_program_header_table_whose_extent_wraps(report);
    a_segment_whose_end_address_wraps(report);
    a_segment_the_size_of_user_space(report);
}

/// A one-segment ELF64 executable, with every number the loader reads left
/// where it can be set. Nothing here is made consistent: the point of most of
/// these is a header a linker would never write.
pub struct Image {
    pub entry: u64,
    pub phoff: u64,
    pub phnum: u16,
    pub phentsize: u16,
    pub p_offset: u64,
    pub p_vaddr: u64,
    pub p_filesz: u64,
    pub p_memsz: u64,
    /// Written at file offset `HEADERS`, inside the segment.
    pub code: Vec<u8>,
}

impl Image {
    /// A sound image: one segment covering the whole file, entered at the code
    /// that follows the headers.
    pub fn new(code: Vec<u8>) -> Image {
        let size = HEADERS + code.len() as u64;
        Image {
            entry: LOAD + HEADERS,
            phoff: 64,
            phnum: 1,
            phentsize: 56,
            p_offset: 0,
            p_vaddr: LOAD,
            p_filesz: size,
            p_memsz: size,
            code,
        }
    }

    pub fn bytes(&self) -> Vec<u8> {
        let mut out = vec![0u8; HEADERS as usize];
        out[0..4].copy_from_slice(b"\x7FELF");
        out[4] = 2; // 64-bit
        out[5] = 1; // little-endian
        out[6] = 1; // version
        out[16..18].copy_from_slice(&2u16.to_le_bytes()); // ET_EXEC
        out[18..20].copy_from_slice(&MACHINE.to_le_bytes());
        out[20..24].copy_from_slice(&1u32.to_le_bytes());
        out[24..32].copy_from_slice(&self.entry.to_le_bytes());
        out[32..40].copy_from_slice(&self.phoff.to_le_bytes());
        out[52..54].copy_from_slice(&64u16.to_le_bytes()); // e_ehsize
        out[54..56].copy_from_slice(&self.phentsize.to_le_bytes());
        out[56..58].copy_from_slice(&self.phnum.to_le_bytes());

        let ph = 64;
        out[ph..ph + 4].copy_from_slice(&1u32.to_le_bytes()); // PT_LOAD
        out[ph + 4..ph + 8].copy_from_slice(&5u32.to_le_bytes()); // read, execute
        out[ph + 8..ph + 16].copy_from_slice(&self.p_offset.to_le_bytes());
        out[ph + 16..ph + 24].copy_from_slice(&self.p_vaddr.to_le_bytes());
        out[ph + 24..ph + 32].copy_from_slice(&self.p_vaddr.to_le_bytes()); // p_paddr
        out[ph + 32..ph + 40].copy_from_slice(&self.p_filesz.to_le_bytes());
        out[ph + 40..ph + 48].copy_from_slice(&self.p_memsz.to_le_bytes());
        out[ph + 48..ph + 56].copy_from_slice(&0x1000u64.to_le_bytes()); // p_align
        out.extend_from_slice(&self.code);
        out
    }
}

/// Machine code for `exit_group(status)` on the machine this is built for.
#[cfg(target_arch = "x86_64")]
fn exit_with(status: u8) -> Vec<u8> {
    let mut code = vec![0xB8, 0xE7, 0x00, 0x00, 0x00]; // mov eax, 231
    code.extend_from_slice(&[0xBF, status, 0x00, 0x00, 0x00]); // mov edi, status
    code.extend_from_slice(&[0x0F, 0x05]); // syscall
    code
}

#[cfg(target_arch = "aarch64")]
fn exit_with(status: u8) -> Vec<u8> {
    let mut code = Vec::new();
    code.extend_from_slice(&0xD280_0BC8u32.to_le_bytes()); // movz x8, #94
    code.extend_from_slice(&(0xD280_0000u32 | ((status as u32) << 5)).to_le_bytes()); // movz x0
    code.extend_from_slice(&0xD400_0001u32.to_le_bytes()); // svc #0
    code
}

/// Write `image` out and exec it in a child. The child's exit code says what
/// happened: `REFUSED` if the exec came back with the errno a malformed image
/// earns, `RAN` if the image ran and said so, 1 if the exec came back with
/// something else. `None` if the child died of a signal, which is what an
/// image that loaded and then jumped nowhere does.
fn exec_in_child(path: &str, image: &Image) -> Option<i32> {
    const ENOEXEC: i64 = 8;
    const ENOMEM: i64 = 12;

    std::fs::write(path, image.bytes()).ok()?;
    // Without the execute bit the refusal comes before a header is read.
    std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o755)).ok()?;

    let pid = sys::fork();
    if pid == 0 {
        let rc = sys::execve(path, &[path]);
        // execve only comes back when the program could not be started.
        sys::exit_group(if rc == -ENOEXEC || rc == -ENOMEM { REFUSED } else { 1 });
    }
    if pid < 0 {
        return None;
    }
    let (rc, code) = sys::wait4(pid as i32, 0);
    let _ = std::fs::remove_file(path);
    // A child killed by a signal carries no exit code; say so rather than
    // reporting the zero that reads as a clean exit.
    if rc < 0 || code == 0 {
        None
    } else {
        Some(code)
    }
}

/// The guard on the four below: a loader that refused every image would pass
/// all of them having stopped running programs altogether.
fn a_well_formed_image_still_runs(report: &mut Report) {
    let code = exec_in_child("/tmp/elf-sound", &Image::new(exit_with(RAN as u8)));
    report.check(
        "a hand-built image whose headers are sound runs",
        code == Some(RAN),
        format!("the child reported {:?} rather than {}", code, RAN),
    );
}

/// p_offset and p_filesz are added to find where the segment's bytes end in
/// the file, and that sum is the only thing keeping the read inside it. A
/// p_offset near the top of the range and a small p_filesz sum to a small
/// number, so the check passes and the copy reads from before the start of the
/// file -- kernel heap, into a page the program can then read.
fn a_segment_whose_file_extent_wraps(report: &mut Report) {
    let mut image = Image::new(Vec::new());
    image.p_offset = u64::MAX - 0xFFF;
    image.p_filesz = 0x1000; // the sum is exactly zero
    image.p_memsz = 0x1000;
    image.entry = NOWHERE;
    let code = exec_in_child("/tmp/elf-file-extent", &image);
    report.check(
        "a segment whose bytes wrap past the end of the file is refused",
        code == Some(REFUSED),
        format!("the child reported {:?} rather than {}", code, REFUSED),
    );
}

/// The program header table's extent is e_phoff plus the size of the table,
/// and a wrap there passes the same check. The loop then reads the table from
/// far outside the file, which is a bounds-checked index in every build: a
/// panic, and a panic in the kernel is a machine that has to be restarted.
fn a_program_header_table_whose_extent_wraps(report: &mut Report) {
    let mut image = Image::new(Vec::new());
    image.phoff = u64::MAX - 55; // one 56-byte entry past it is zero
    image.entry = NOWHERE;
    let code = exec_in_child("/tmp/elf-phdr-extent", &image);
    report.check(
        "a program header table whose extent wraps is refused",
        code == Some(REFUSED),
        format!("the child reported {:?} rather than {}", code, REFUSED),
    );
}

/// A segment's last address is p_vaddr plus p_memsz rounded up to a page, and
/// the rounding is another addition. A segment in the last page of the address
/// space rounds up to zero, which is below the ceiling the loader checks
/// against, so it is accepted and then written to: in the kernel's half, with
/// the kernel's privileges, at an address nothing is mapped at.
fn a_segment_whose_end_address_wraps(report: &mut Report) {
    let mut image = Image::new(Vec::new());
    image.p_vaddr = u64::MAX - 0xFFF;
    image.p_offset = 0;
    image.p_filesz = 0x40;
    image.p_memsz = 0x1000;
    image.entry = NOWHERE;
    let code = exec_in_child("/tmp/elf-end-address", &image);
    report.check(
        "a segment whose end address wraps is refused",
        code == Some(REFUSED),
        format!("the child reported {:?} rather than {}", code, REFUSED),
    );
}

/// p_memsz has no bound below the top of user space, and the loader keeps one
/// map entry per page of a segment while it works out what protection each
/// page ends up with. A segment claiming most of user space is billions of
/// pages, and the map fills the kernel heap long before anything is mapped.
fn a_segment_the_size_of_user_space(report: &mut Report) {
    let mut image = Image::new(Vec::new());
    image.p_offset = 0;
    image.p_filesz = 0x40;
    image.p_memsz = 0x7E00_0000_0000;
    image.entry = NOWHERE;
    let code = exec_in_child("/tmp/elf-whole-space", &image);
    report.check(
        "a segment the size of user space is refused",
        code == Some(REFUSED),
        format!("the child reported {:?} rather than {}", code, REFUSED),
    );
}
