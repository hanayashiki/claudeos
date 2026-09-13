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
    pages_shared_by_a_fork_written_from_two_threads(report);
    pages_a_fork_shared_are_out_of_the_parents_reach(report);
    a_user_buffer_unmapped_while_the_kernel_reads_it(report);
    a_page_two_threads_reach_at_once(report);
    exec_that_fails_keeps_the_memory_state(report);
    a_thread_left_unreaped_keeps_the_space(report);
    argument_blocks_larger_than_the_stack_is_mapped_with(report);
    addresses_outside_user_space_are_refused(report);
    a_hint_over_a_live_mapping_is_not_taken(report);
}

/// Pages a fork left shared, written from two threads of the same process at
/// once.
///
/// Taking the private copy of a shared page reads the entry, copies the page
/// and writes the entry back. While that was a call that took the old mapping
/// away and a second one that put the new mapping in, the address had nothing
/// at it in between. A sibling thread that touched it there did not find a
/// page on its way back -- it found one that had never been touched, and was
/// given a fresh page of zeroes over the top. The contents were gone, and the
/// copy that was coming in was then refused for an address that was no longer
/// free, which is EFAULT out of the system call or SIGSEGV in the program.
///
/// The two threads reach the copy by different routes on purpose. The read is
/// a system call, which validates the buffer it is given with interrupts on
/// whichever machine this is. The store is a fault from user mode, which on
/// aarch64 is handled with interrupts in the state the faulting code was in
/// and so also with them on. Either is a way in.
///
/// This is a smoke test. The window it looks for was a page copy wide, so
/// catching it needs the timer to land inside one; what is shipped here is the
/// two threads reaching the same pages while a child holds a share of them,
/// and the contents surviving it.
fn pages_shared_by_a_fork_written_from_two_threads(report: &mut Report) {
    use std::os::unix::io::AsRawFd;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Pages contended per round. Both threads sweep them in the same order,
    /// so the one that is behind walks through whatever the one ahead is in
    /// the middle of.
    const PAGES: usize = 64;
    const ROUNDS: usize = 40;
    /// What every byte of every page holds. Not zero: a page of zeroes is what
    /// an address with nothing at it is filled with, which is the thing being
    /// looked for.
    const MARKER: u8 = 0xA5;
    const PAGE: usize = 4096;
    const LEN: u64 = (PAGES * PAGE) as u64;
    /// A file of nothing but the marker, so a one-byte read into a page writes
    /// back the byte that is already there. The read is only here because it
    /// is the way into the copy from a system call.
    const SOURCE: &str = "/tmp/cow-marker";

    let base = sys::mmap_anon(0, LEN);
    if base <= 0 {
        report.check("pages to share", false, format!("mmap returned {:#x}", base));
        return;
    }
    let base = base as u64;

    if std::fs::write(SOURCE, vec![MARKER; ROUNDS * PAGES + PAGE]).is_err() {
        report.check("a file of the marker byte", false, String::new());
        sys::munmap(base, LEN);
        return;
    }
    let Ok(source) = std::fs::File::open(SOURCE) else {
        report.check("a file of the marker byte", false, String::new());
        sys::munmap(base, LEN);
        return;
    };
    let fd = source.as_raw_fd();

    let refused = AtomicUsize::new(0);
    let mut damaged = 0usize;
    let mut rounds = 0usize;

    for _ in 0..ROUNDS {
        unsafe { std::ptr::write_bytes(base as *mut u8, MARKER, LEN as usize) };

        // The child exists to keep a second reference on every one of those
        // pages, which is what makes the parent's first write to one a copy
        // rather than a mark being cleared. It touches none of them: a touch
        // here would take the copy on this side instead.
        let child = sys::fork();
        if child == 0 {
            sys::sleep_ms(30_000);
            sys::exit_group(0);
        }
        if child < 0 {
            break;
        }

        std::thread::scope(|scope| {
            scope.spawn(|| {
                for page in 0..PAGES {
                    let at = base + (page * PAGE) as u64 + 1;
                    unsafe { std::ptr::write_volatile(at as *mut u8, MARKER) };
                }
            });
            for page in 0..PAGES {
                let at = base + (page * PAGE) as u64;
                let slot = unsafe { std::slice::from_raw_parts_mut(at as *mut u8, 1) };
                if sys::read(fd, slot) != 1 {
                    refused.fetch_add(1, Ordering::Relaxed);
                }
            }
        });

        sys::kill(child as i32, 9);
        sys::wait4(child as i32, 0);

        let seen = unsafe { std::slice::from_raw_parts(base as *const u8, LEN as usize) };
        damaged += seen.iter().filter(|byte| **byte != MARKER).count();
        rounds += 1;
    }

    let refused = refused.load(Ordering::Relaxed);
    report.check(
        "pages a fork shared still hold what was written to them",
        rounds == ROUNDS && damaged == 0,
        format!("{} of {} rounds ran, {} bytes came back changed", rounds, ROUNDS, damaged),
    );
    report.check(
        "and no read into one was refused",
        refused == 0,
        format!("{} of {} reads did not land", refused, rounds * PAGES),
    );

    drop(source);
    let _ = std::fs::remove_file(SOURCE);
    sys::munmap(base, LEN);
}

/// A fork is a snapshot: once it has shared a page with the child, nothing the
/// parent does may reach that page again.
///
/// Sharing one took the parent's write permission away and then took the
/// child's reference on the frame, with a window between them. A sibling
/// thread that faulted on the page in that window read a reference count of
/// one, took the last-owner path, and cleared the mark, so the parent kept
/// write access to a frame the child was about to share. Both processes then
/// had a writable mapping of one page.
///
/// What the child watches for is its own memory moving under it: it reads
/// every page, sleeps while the sibling keeps writing, and reads them again.
/// A page that changed is one the parent still reaches.
///
/// This is a smoke test. The window was between two calls, so catching it
/// needs the timer to land inside one; the proof was a widened window.
fn pages_a_fork_shared_are_out_of_the_parents_reach(report: &mut Report) {
    use std::sync::atomic::{AtomicBool, Ordering};

    const PAGES: usize = 64;
    const ROUNDS: usize = 40;
    const PAGE: usize = 4096;
    const LEN: u64 = (PAGES * PAGE) as u64;
    /// How long the child leaves the sibling writing before it looks again.
    const WATCH_MS: u64 = 30;

    let base = sys::mmap_anon(0, LEN);
    if base <= 0 {
        report.check("pages to fork over", false, format!("mmap returned {:#x}", base));
        return;
    }
    let base = base as u64;
    // Every page present and writable in this thread before the first fork, so
    // the walk has write permission to take away from each of them.
    unsafe { std::ptr::write_bytes(base as *mut u8, 0, LEN as usize) };

    let stop = AtomicBool::new(false);
    let mut rounds = 0usize;
    let mut moved = 0usize;

    std::thread::scope(|scope| {
        // A rising number, so a write that lands in a page the child holds
        // shows up as a different value rather than the same one again.
        scope.spawn(|| {
            let mut count: u32 = 1;
            while !stop.load(Ordering::Relaxed) {
                for page in 0..PAGES {
                    let at = base + (page * PAGE) as u64;
                    unsafe { std::ptr::write_volatile(at as *mut u32, count) };
                }
                count = count.wrapping_add(1);
            }
        });

        for _ in 0..ROUNDS {
            let child = sys::fork();
            if child == 0 {
                // Nothing here allocates or takes a lock: this is a fork out
                // of a program with a thread running in it, and the only
                // things the child may rely on are its own stack and the
                // system calls it makes itself.
                let mut first = [0u32; PAGES];
                for (page, slot) in first.iter_mut().enumerate() {
                    let at = base + (page * PAGE) as u64;
                    *slot = unsafe { std::ptr::read_volatile(at as *const u32) };
                }
                sys::sleep_ms(WATCH_MS);
                let mut changed = 0;
                for (page, slot) in first.iter().enumerate() {
                    let at = base + (page * PAGE) as u64;
                    if unsafe { std::ptr::read_volatile(at as *const u32) } != *slot {
                        changed += 1;
                    }
                }
                sys::exit_group(i32::from(changed != 0));
            }
            if child < 0 {
                break;
            }
            let (pid, code) = sys::wait4(child as i32, 0);
            if pid < 0 {
                break;
            }
            if code != 0 {
                moved += 1;
            }
            rounds += 1;
        }
        stop.store(true, Ordering::Relaxed);
    });

    report.check(
        "a page a fork shared is beyond the parent's reach afterwards",
        rounds == ROUNDS && moved == 0,
        format!("{} of {} rounds ran, the child's memory moved under it in {}", rounds, ROUNDS, moved),
    );
    sys::munmap(base, LEN);
}

/// A user pointer the kernel is about to read through is checked first, and a
/// sibling thread can take the mapping away between the check and the read.
/// The read is then a kernel access to a page that is not present, which is
/// fatal to the machine rather than to the program.
///
/// Two ways in: a `write`, where the kernel copies the buffer out of user
/// memory, and an `openat`, where it reads a path out of it. Both must come
/// back as a result or as a bad address, and the machine has to still be here
/// afterwards to say so.
///
/// A smoke test, for the same reason as above.
fn a_user_buffer_unmapped_while_the_kernel_reads_it(report: &mut Report) {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    const PAGES: usize = 4;
    const PAGE: usize = 4096;
    const LEN: u64 = (PAGES * PAGE) as u64;
    const ROUNDS: usize = 3000;
    const EFAULT: i64 = -14;

    let sink = sys::open("/dev/null", 1);
    if sink < 0 {
        report.check("somewhere to write to", false, format!("open returned {}", sink));
        return;
    }
    let base = sys::mmap_anon(0, LEN);
    if base <= 0 {
        report.check("a buffer to pull away", false, format!("mmap returned {:#x}", base));
        sys::close(sink as i32);
        return;
    }
    let base = base as u64;

    let stop = AtomicBool::new(false);
    let odd = AtomicUsize::new(0);

    std::thread::scope(|scope| {
        scope.spawn(|| {
            while !stop.load(Ordering::Relaxed) {
                sys::munmap(base, LEN);
                sys::mmap_fixed(base, LEN);
            }
        });

        for _ in 0..ROUNDS {
            let wrote = sys::write_raw(sink as i32, base, LEN);
            if wrote != LEN as i64 && wrote != EFAULT {
                odd.fetch_add(1, Ordering::Relaxed);
            }
            // The path is whatever is at that address, which after a remap is
            // a page of zeroes and so the empty name. What matters is the
            // route: the kernel reads the string through the pointer it was
            // given.
            let opened = sys::open_raw(base, 0);
            if opened >= 0 {
                sys::close(opened as i32);
            }
        }
        stop.store(true, Ordering::Relaxed);
    });

    let odd = odd.load(Ordering::Relaxed);
    report.check(
        "a buffer unmapped under a system call reading it does not fault the kernel",
        odd == 0,
        format!("{} of {} writes came back as neither the length nor a bad address", odd, ROUNDS),
    );
    sys::munmap(base, LEN);
    sys::close(sink as i32);
}

/// How much of the program's own read-only data this check reaches into. A page
/// of it is untouched until the check asks for it, because nothing else reads
/// it, and it is file-backed, so the first touch is a page filled from the
/// executable.
const COLD_PAGES: usize = 48;
const COLD_PAGE: usize = 4096;

/// Every byte non-zero, so a read that comes back zero is a read of a page
/// whose contents are not there yet rather than a read of the file's bytes.
static COLD: [u8; COLD_PAGES * COLD_PAGE] = {
    let mut out = [0u8; COLD_PAGES * COLD_PAGE];
    let mut i = 0;
    while i < COLD_PAGES * COLD_PAGE {
        out[i] = (i % 251) as u8 + 1;
        i += 1;
    }
    out
};

/// Two threads reaching one untouched page at the same moment.
///
/// Filling a page from the file mapped it first, read the contents into it
/// through that mapping, and gave it the segment's protection afterwards, so
/// between the first and the last it was reachable by a sibling holding the
/// zeroes of a fresh frame. And the handler read a page a sibling had already
/// put in as a fault it could not repair, which killed the task with a
/// segmentation fault at an address that is mapped.
///
/// A smoke test: one processor runs one of the two threads at a time, so they
/// only overlap if the timer lands inside the handler. The proof was a widened
/// window, where both of these were every page rather than none.
fn a_page_two_threads_reach_at_once(report: &mut Report) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Barrier;

    let blank = AtomicUsize::new(0);
    let barrier = Barrier::new(2);
    // Through an opaque pointer, so the read below is a load from the page and
    // not a constant the compiler read out of the program at build time.
    let base = std::hint::black_box(COLD.as_ptr()) as usize;

    let sweep = |base: usize, barrier: &Barrier, blank: &AtomicUsize| {
        for page in 0..COLD_PAGES {
            // Both threads are let go at the same point, so the one that
            // faults second walks into whatever the first is in the middle of.
            barrier.wait();
            let byte =
                unsafe { std::ptr::read_volatile((base + page * COLD_PAGE) as *const u8) };
            if byte == 0 {
                blank.fetch_add(1, Ordering::Relaxed);
            }
        }
    };

    std::thread::scope(|scope| {
        scope.spawn(|| sweep(base, &barrier, &blank));
        sweep(base, &barrier, &blank);
    });

    let blank = blank.load(Ordering::Relaxed);
    report.check(
        "a page two threads reach at once holds what the file put in it",
        blank == 0,
        format!("{} of {} reads came back zero", blank, COLD_PAGES * 2),
    );
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
/// No wait reports a thread, so nothing reaps this one. It is still on the
/// machine, naming the address space this exec is about to leave, when the exec
/// happens; the kernel lets go of it the next time this process makes a task or
/// ends one.
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

/// One process through that sequence, reaped.
fn round() -> bool {
    let pid = sys::fork();
    if pid == 0 {
        sys::execve("/bin/inet", &["inet", "leave-a-thread"]);
        sys::exit_group(1);
    }
    if pid < 0 {
        return false;
    }
    // Only the process is waited for. A thread is not a child of anything: no
    // wait reports it, and the kernel takes its task away itself once the
    // process it belonged to is finished with the address space.
    sys::wait4(pid as i32, 0).0 >= 0
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
    // Past USER_MMAP_BASE, which is as high as an image may go.
    const TOO_HIGH: u64 = 0x0000_7FF0_0000_0000;

    let mut image = crate::loader::Image::new(Vec::new());
    image.entry = TOO_HIGH;
    image.p_vaddr = TOO_HIGH;
    image.p_filesz = 0;
    image.p_memsz = 0x1000;
    image.bytes()
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
