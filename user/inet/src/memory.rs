//! Memory management, watched from a program.
//!
//! What a process can see of the kernel's bookkeeping is where its mappings
//! land, what its program break is, and how much memory the machine says is
//! free. That is enough: a mapping placed on top of another one, a memory
//! state that an interrupted exec threw away, and a frame handed back twice
//! all show up in one of the three.

use crate::sys;
use crate::Report;

pub fn run(report: &mut Report) {
    exec_that_fails_keeps_the_memory_state(report);
}

/// An executable that passes every check made before the old address space is
/// put away and fails the first one made after it.
///
/// The single segment sits above the highest address a program may be loaded
/// at. Nothing looks at program headers until the task has already been moved
/// onto the new address space, so this is a load that fails with the old image
/// gone and the new one not there.
fn unloadable_elf() -> Vec<u8> {
    #[cfg(target_arch = "x86_64")]
    const MACHINE: u16 = 0x3E;
    #[cfg(target_arch = "aarch64")]
    const MACHINE: u16 = 0xB7;
    // Past USER_MMAP_BASE, which is as high as an image may go.
    const TOO_HIGH: u64 = 0x0000_7FF0_0000_0000;

    let mut out = vec![0u8; 120];
    out[0..4].copy_from_slice(b"\x7FELF");
    out[4] = 2; // 64-bit
    out[5] = 1; // little-endian
    out[6] = 1; // version
    out[16..18].copy_from_slice(&2u16.to_le_bytes()); // ET_EXEC
    out[18..20].copy_from_slice(&MACHINE.to_le_bytes());
    out[20..24].copy_from_slice(&1u32.to_le_bytes());
    out[24..32].copy_from_slice(&TOO_HIGH.to_le_bytes()); // e_entry
    out[32..40].copy_from_slice(&64u64.to_le_bytes()); // e_phoff
    out[52..54].copy_from_slice(&64u16.to_le_bytes()); // e_ehsize
    out[54..56].copy_from_slice(&56u16.to_le_bytes()); // e_phentsize
    out[56..58].copy_from_slice(&1u16.to_le_bytes()); // e_phnum

    let ph = 64;
    out[ph..ph + 4].copy_from_slice(&1u32.to_le_bytes()); // PT_LOAD
    out[ph + 4..ph + 8].copy_from_slice(&5u32.to_le_bytes()); // read, execute
    out[ph + 16..ph + 24].copy_from_slice(&TOO_HIGH.to_le_bytes()); // p_vaddr
    out[ph + 24..ph + 32].copy_from_slice(&TOO_HIGH.to_le_bytes()); // p_paddr
    out[ph + 40..ph + 48].copy_from_slice(&0x1000u64.to_le_bytes()); // p_memsz
    out[ph + 48..ph + 56].copy_from_slice(&0x1000u64.to_le_bytes()); // p_align
    out
}

/// An exec that could not go through has to leave the program that asked for
/// it able to carry on. The program break is the cheapest thing to ask: it
/// comes out of the same record as the list of mappings, so a record thrown
/// away and not put back reports a break of zero.
fn exec_that_fails_keeps_the_memory_state(report: &mut Report) {
    const PATH: &str = "/tmp/unloadable";
    // Without the execute bit the refusal comes before anything is touched,
    // which is not the case being tested.
    let written = std::fs::write(PATH, unloadable_elf()).and_then(|()| {
        std::fs::set_permissions(PATH, std::os::unix::fs::PermissionsExt::from_mode(0o755))
    });
    if let Err(err) = written {
        report.check("an image that cannot be loaded", false, format!("{}", err));
        return;
    }

    // In a child, because a program left with no memory may not survive being
    // asked the question, and the rest of the suite still has to run.
    let pid = sys::fork();
    if pid == 0 {
        // Nothing below allocates: after a failed exec the heap is exactly
        // what is in doubt.
        let before = sys::brk(0);
        let refused = sys::execve(PATH) < 0;
        let after = sys::brk(0);
        let mut code = 0;
        if refused {
            code |= 1;
        }
        if before != 0 && after == before {
            code |= 2;
        }
        sys::exit_group(code);
    }
    if pid < 0 {
        report.check("a child to try it in", false, format!("fork: {}", pid));
        return;
    }
    let (rc, code) = sys::wait4(pid as i32, 0);
    if rc < 0 {
        report.check("a child to try it in", false, format!("wait4: {}", rc));
        return;
    }
    report.check(
        "an image the loader cannot place is refused",
        code & 1 != 0,
        format!("exec reported success, status {}", code),
    );
    report.check(
        "a program keeps its memory state when an exec fails",
        code & 2 != 0,
        format!("the program break did not survive, status {}", code),
    );
    let _ = std::fs::remove_file(PATH);
}
