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
    a_thread_left_unreaped_keeps_the_space(report);
    argument_blocks_larger_than_the_stack_is_mapped_with(report);
    addresses_outside_user_space_are_refused(report);
    a_hint_over_a_live_mapping_is_not_taken(report);
}

/// A hint names where a mapping should start, and the mapping is as long as it
/// was asked to be. A hint whose first page is free and whose next one is not
/// is a hint that cannot be honoured, and taking it hands the program an
/// address range it is already using for something else.
fn a_hint_over_a_live_mapping_is_not_taken(report: &mut Report) {
    // Where mappings are placed from. A hint below it is passed over whatever
    // else is true of it, so the check would prove nothing there.
    const MMAP_BASE: u64 = 0x0000_7F00_0000_0000;
    const LEN: u64 = 4 * 4096;

    let below = sys::mmap_anon(0, LEN);
    let live = sys::mmap_anon(0, LEN);
    if below < 0 || live < 0 || (below as u64) < MMAP_BASE + LEN {
        report.check(
            "two mappings to hint across",
            false,
            format!("mmap returned {:#x} and {:#x}", below, live),
        );
        return;
    }
    let (below, live) = (below as u64, live as u64);
    // Freeing the lower one leaves the page just under `live` unclaimed, which
    // is what makes the hint below look available one page at a time.
    sys::munmap(below, LEN);

    let hint = live - 4096;
    let placed = sys::mmap_anon(hint, LEN);
    let overlaps = placed > 0 && (placed as u64) < live + LEN && live < placed as u64 + LEN;
    report.check(
        "a hint whose range is already taken is not honoured",
        placed > 0 && !overlaps,
        format!("{:#x} was given for a hint of {:#x}, over a mapping at {:#x}", placed, hint, live),
    );

    if placed > 0 {
        sys::munmap(placed as u64, LEN);
    }
    sys::munmap(live, LEN);
}

/// mmap takes an address from the program, and has to satisfy itself that it
/// is one the program could reach. The upper half of the tables is the
/// kernel's and is shared by reference with every address space, so taking a
/// mapping away there takes it away from the kernel too and gives the frame
/// back to the allocator while it is still being read.
fn addresses_outside_user_space_are_refused(report: &mut Report) {
    const EINVAL: i64 = -22;
    // The base of the kernel heap, which every address space maps.
    const KERNEL: u64 = 0xFFFF_C000_0000_0000;
    // The lowest address the upper half starts at.
    const USER_END: u64 = 0x0000_8000_0000_0000;
    const LEN: u64 = 4096;

    let demanded = sys::mmap_fixed(KERNEL, LEN);
    report.check(
        "a mapping demanded outside user space is refused",
        demanded == EINVAL,
        format!("mmap returned {:#x}", demanded),
    );

    let hinted = sys::mmap_anon(KERNEL, LEN);
    let in_user_space = hinted > 0 && (hinted as u64) < USER_END;
    report.check(
        "a hint outside user space is not taken",
        in_user_space,
        format!("mmap returned {:#x}", hinted),
    );
    if in_user_space {
        sys::munmap(hinted as u64, LEN);
    }

    // The kernel half is still there and still being used: anything that
    // allocates in it after the calls above would have found a hole.
    report.check(
        "the machine still reads its own memory afterwards",
        free_kib().is_some(),
        String::from("/proc/meminfo could not be read"),
    );
}

/// Exec `/bin/true` with `count` arguments of `bytes` each, in a child, and
/// report what became of it: zero if the program ran, 2 if the exec came back
/// with E2BIG, 3 if it came back with anything else.
fn exec_with_arguments(bytes: usize, count: usize) -> Option<i32> {
    const E2BIG: i64 = 7;
    let filler = "x".repeat(bytes);
    let mut argv: Vec<&str> = Vec::with_capacity(count + 1);
    argv.push("true");
    for _ in 0..count {
        argv.push(&filler);
    }
    let pid = sys::fork();
    if pid == 0 {
        let rc = sys::execve("/bin/true", &argv);
        sys::exit_group(if rc == -E2BIG { 2 } else { 3 });
    }
    if pid < 0 {
        return None;
    }
    let (rc, code) = sys::wait4(pid as i32, 0);
    if rc < 0 {
        None
    } else {
        Some(code)
    }
}

/// The arguments and the environment are written onto the new program's stack
/// before it starts, and only the top of that stack is mapped when the writing
/// begins. A block past that has to grow the stack; a block past the whole
/// stack has to come back as an error. Neither may be a fault taken in the
/// kernel, which is fatal.
fn argument_blocks_larger_than_the_stack_is_mapped_with(report: &mut Report) {
    // About 300 KiB, past the quarter megabyte mapped up front.
    report.check(
        "an argument block past what the stack is mapped with is written anyway",
        exec_with_arguments(4000, 77) == Some(0),
        String::from("the program did not run"),
    );
    // About 2.8 MiB, past the whole stack reserve.
    let refused = exec_with_arguments(4000, 700);
    report.check(
        "an argument block past the whole stack is refused",
        refused == Some(2),
        format!("the child reported {:?} rather than E2BIG", refused),
    );
}

/// How much memory the machine says is unspoken for, in kibibytes.
fn free_kib() -> Option<u64> {
    let text = std::fs::read_to_string("/proc/meminfo").ok()?;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("MemFree:") {
            return rest.trim().trim_end_matches("kB").trim().parse().ok();
        }
    }
    None
}

/// The half of the check below that runs in a process of its own: leave behind
/// a thread that has exited and has not been reaped, then replace this image
/// while it is still there.
///
/// The thread's task belongs to whoever started this process, not to this one,
/// so nothing here can reap it and it is still on the machine, naming the
/// address space this exec is about to leave, when the exec happens.
pub fn leave_a_thread_and_exec() -> ! {
    let handle = std::thread::spawn(|| {});
    let _ = handle.join();
    // A join returns once the thread's tid has been cleared, which is a moment
    // before its task reaches its final state. The exec has to come after
    // that, or there is no unreaped thread for it to trip over.
    std::thread::sleep(std::time::Duration::from_millis(40));
    sys::execve("/bin/true", &["true"]);
    sys::exit_group(1);
}

/// One process through that sequence, and both of the tasks it leaves reaped.
fn round() -> bool {
    let pid = sys::fork();
    if pid == 0 {
        sys::execve("/bin/inet", &["inet", "leave-a-thread"]);
        sys::exit_group(1);
    }
    if pid < 0 {
        return false;
    }
    // Named, not "whichever is ready": the thread is a child of this process
    // too and becomes reapable first, and reaping it before the exec would
    // take away the very thing the exec has to trip over.
    if sys::wait4(pid as i32, 0).0 < 0 {
        return false;
    }
    sys::wait4(-1, 0).0 >= 0
}

/// A task that has exited and has not been reaped still names the address
/// space it ran in, so an exec by one of its siblings must not hand that space
/// back. Handing it back twice shows up as the machine claiming more free
/// memory than it has, because the second release takes a frame that has since
/// been given to something else.
fn a_thread_left_unreaped_keeps_the_space(report: &mut Report) {
    const WARM_UP: usize = 3;
    const ROUNDS: usize = 40;
    // The kernel heap takes frames as it grows and does not give them back, so
    // a few kilobytes of drift is ordinary. A frame released twice a round is
    // many times that.
    const SLACK: i64 = 64;

    for _ in 0..WARM_UP {
        if !round() {
            report.check("a process to leave a thread in", false, String::new());
            return;
        }
    }
    let Some(before) = free_kib() else {
        report.check("free memory is reported", false, String::new());
        return;
    };
    for _ in 0..ROUNDS {
        if !round() {
            report.check("a process to leave a thread in", false, String::new());
            return;
        }
    }
    let Some(after) = free_kib() else {
        report.check("free memory is reported", false, String::new());
        return;
    };
    let drift = after as i64 - before as i64;
    report.check(
        "a process execs with a thread unreaped and its memory is given back once",
        drift.abs() <= SLACK,
        format!("free memory moved by {} kB over {} rounds", drift, ROUNDS),
    );
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
        let refused = sys::execve(PATH, &[PATH]) < 0;
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
