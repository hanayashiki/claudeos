//! Exercises the parts of the Rust standard library that lean hardest on the
//! kernel: threads, synchronisation, subprocesses, files and large heaps.

use std::collections::HashMap;
use std::io::{IoSlice, Read, Seek, SeekFrom, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const SIGUSR1: i32 = 10;
const SIGUSR2: i32 = 12;
const SIG_IGN: usize = 1;

static SIGNAL_TOTAL: AtomicUsize = AtomicUsize::new(0);

extern "C" {
    fn signal(signum: i32, handler: usize) -> usize;
    fn raise(signum: i32) -> i32;
    fn pause() -> i32;
}

extern "C" fn handle_signal(signum: i32) {
    SIGNAL_TOTAL.fetch_add(signum as usize, Ordering::SeqCst);
}

struct Report {
    passed: usize,
    failed: usize,
}

impl Report {
    fn check(&mut self, name: &str, condition: bool, detail: String) {
        if condition {
            self.passed += 1;
            println!("PASS  {}", name);
        } else {
            self.failed += 1;
            println!("FAIL  {}: {}", name, detail);
        }
    }
}

/// A counter, a connected socket pair, and an epoll set watching both: the
/// three descriptors a program written against Linux waits on.
fn event_and_poll(report: &mut Report) {
    use crate::sys;
    use std::os::unix::net::UnixStream;

    let (mut left, mut right) = match UnixStream::pair() {
        Ok(pair) => pair,
        Err(err) => {
            report.check("socket pair", false, format!("{}", err));
            return;
        }
    };
    report.check("socket pair", true, String::new());

    let written = left.write_all(b"over the socket").is_ok();
    let mut buf = [0u8; 32];
    let n = right.read(&mut buf).unwrap_or(0);
    report.check(
        "socket carries bytes",
        written && &buf[..n] == b"over the socket",
        format!("{:?}", String::from_utf8_lossy(&buf[..n])),
    );
    // The other direction is a separate stream.
    let _ = right.write_all(b"and back");
    let n = left.read(&mut buf).unwrap_or(0);
    report.check(
        "socket is two-way",
        &buf[..n] == b"and back",
        format!("{:?}", String::from_utf8_lossy(&buf[..n])),
    );

    let event = sys::eventfd(0, 0);
    report.check("eventfd opens", event >= 0, format!("{}", event));
    if event < 0 {
        return;
    }
    let event = event as i32;
    let added = sys::write(event, &7u64.to_le_bytes());
    let mut value = [0u8; 8];
    let read = sys::read(event, &mut value);
    report.check(
        "eventfd counts",
        added == 8 && read == 8 && u64::from_le_bytes(value) == 7,
        format!("wrote {} read {} value {}", added, read, u64::from_le_bytes(value)),
    );

    let epoll = sys::epoll_create();
    report.check("epoll set opens", epoll >= 0, format!("{}", epoll));
    if epoll < 0 {
        return;
    }
    let epoll = epoll as i32;
    let socket_fd = {
        use std::os::unix::io::AsRawFd;
        right.as_raw_fd()
    };
    let ok = sys::epoll_add(epoll, event, sys::EPOLLIN, 1) == 0
        && sys::epoll_add(epoll, socket_fd, sys::EPOLLIN, 2) == 0;
    report.check("epoll takes descriptors", ok, String::new());

    // Nothing has arrived on either, so a zero timeout reports nothing.
    let mut events = [0u8; 2 * sys::EPOLL_EVENT_SIZE];
    let idle = sys::epoll_wait(epoll, &mut events, 0);
    report.check("epoll reports nothing yet", idle == 0, format!("{}", idle));

    // Make the counter ready and wait with no timeout: the wait has to end
    // because of the counter, not because time passed.
    sys::write(event, &1u64.to_le_bytes());
    let count = sys::epoll_wait(epoll, &mut events, -1);
    let data = sys::EPOLL_EVENT_SIZE - 8;
    let which = u64::from_le_bytes(events[data..data + 8].try_into().unwrap_or([0; 8]));
    report.check(
        "epoll wakes for the counter",
        count == 1 && which == 1,
        format!("count {} data {}", count, which),
    );

    // Now the socket as well, and both come back at once.
    let _ = left.write_all(b"ping");
    let count = sys::epoll_wait(epoll, &mut events, 100);
    report.check("epoll reports both", count == 2, format!("{}", count));

    // Drain both, then time a wait that nothing will satisfy: it has to come
    // back when the timeout says so, not when a tick happens to land.
    let mut drain = [0u8; 64];
    let _ = sys::read(event, &mut value);
    let _ = right.read(&mut drain);
    let started = Instant::now();
    let count = sys::epoll_wait(epoll, &mut events, 300);
    let waited = started.elapsed();
    report.check(
        "epoll waits out its timeout",
        count == 0 && waited >= Duration::from_millis(250) && waited < Duration::from_millis(900),
        format!("{} events after {:?}", count, waited),
    );

    // A wait with no timeout has to end when the other side writes, and
    // promptly: this is what a program built on a poll loop depends on.
    let event_copy = event;
    let waker = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(60));
        sys::write(event_copy, &1u64.to_le_bytes());
    });
    let started = Instant::now();
    let count = sys::epoll_wait(epoll, &mut events, -1);
    let waited = started.elapsed();
    let _ = waker.join();
    report.check(
        "epoll wakes on a write",
        count == 1 && waited < Duration::from_millis(400),
        format!("{} events after {:?}", count, waited),
    );

    let _ = sys::close(epoll);
    let _ = sys::close(event);
}

/// A position no file has a byte at, handed to a positional read and write.
///
/// The position arrives in a register and the write path adds the buffer's
/// length to it. Added and cast where it was used, a position near the top of
/// the range wrapped to a small one: the routine that makes room was asked for
/// a file the caller never named, said yes, and the write then indexed past
/// the end of the buffer it had. The two positions below are the ones that did
/// it -- the last byte of the range, and eight short of it with sixteen bytes
/// to write, which straddles the end.
fn a_position_no_file_has(report: &mut Report) {
    use crate::sys;

    const EINVAL: i64 = -22;
    let path = "/tmp/rtest-offset.dat";
    let _ = std::fs::write(path, b"start");
    let fd = sys::open(path, sys::O_RDWR, 0);
    if fd < 0 {
        report.check("open the file to write into", false, format!("{}", fd));
        return;
    }
    let fd = fd as i32;

    let last = sys::pwrite(fd, b"abcde", u64::MAX);
    let straddling = sys::pwrite(fd, &[b'z'; 16], u64::MAX - 7);
    let mut buf = [0u8; 5];
    let reading = sys::pread(fd, &mut buf, u64::MAX);
    report.check(
        "a write at a position past the end of the range is refused",
        last == EINVAL && straddling == EINVAL,
        format!("last {} straddling {}", last, straddling),
    );
    report.check(
        "and so is a read there",
        reading == EINVAL,
        format!("{}", reading),
    );

    // The file is untouched by the refusals, and a position it does have still
    // works: the check is that the range is refused, not that writing is.
    let inside = sys::pwrite(fd, b"XY", 2);
    sys::close(fd);
    let after = std::fs::read(path).unwrap_or_default();
    report.check(
        "a position the file has is written as it was before",
        inside == 2 && after == b"stXYt",
        format!("{} bytes, {:?}", inside, String::from_utf8_lossy(&after)),
    );
    let _ = std::fs::remove_file(path);
}

/// Numbers past the end of the table answer ENOSYS, which is how a program
/// finds out that a call it would rather use is not there.
///
/// This kernel names every call on every machine and gives the ones a machine
/// has no number for a placeholder at 0x10000 and up. Those numbers reach the
/// kernel out of a register like any other, so they are what a probe walking
/// upwards would run into, and answering them with the call they stand for
/// would tell the probe that this machine has `open` at 0x10000.
fn absent_numbers(report: &mut Report) {
    const ENOSYS: i64 = -38;
    let mut answered: Vec<(u64, i64)> = Vec::new();
    for number in 0x1_0000u64..0x1_0020 {
        let rc = crate::sys::probe(number);
        if rc != ENOSYS {
            answered.push((number, rc));
        }
    }
    report.check(
        "numbers past the table answer ENOSYS",
        answered.is_empty(),
        format!("{:?}", answered),
    );
}

/// `reboot` with magic numbers it does not know, and with a command it does
/// not know, is refused with EINVAL; the two commands that change nothing
/// here are taken.
///
/// Every call carries a command that leaves the machine running even if the
/// check it is there for were missing, because the machine is where this runs:
/// a missing check shows as a zero where EINVAL was due.
fn reboot_refuses_what_it_does_not_know(report: &mut Report) {
    use crate::sys;
    const EINVAL: i64 = -22;
    const CAD_ON: u32 = 0x89ABCDEF;
    const CAD_OFF: u32 = 0;
    const MAGIC2C: u32 = 537993216;

    let first = sys::reboot(0xfee1_dea0, sys::REBOOT_MAGIC2, CAD_OFF);
    let second = sys::reboot(sys::REBOOT_MAGIC1, 0x1234_5678, CAD_OFF);
    report.check(
        "reboot refuses magic numbers it does not know",
        first == EINVAL && second == EINVAL,
        format!("wrong first {}, wrong second {}", first, second),
    );

    let unknown = sys::reboot(sys::REBOOT_MAGIC1, sys::REBOOT_MAGIC2, 0xDEAD_BEEF);
    let on = sys::reboot(sys::REBOOT_MAGIC1, MAGIC2C, CAD_ON);
    let off = sys::reboot(sys::REBOOT_MAGIC1, sys::REBOOT_MAGIC2, CAD_OFF);
    report.check(
        "reboot refuses a command it does not know and takes one it does",
        unknown == EINVAL && on == 0 && off == 0,
        format!("unknown {}, ctrl-alt-del on {}, off {}", unknown, on, off),
    );
}

/// A signal a thread blocks waits until the thread unblocks it, and is taken
/// then. The set a program hands `rt_sigprocmask` has signal n at bit n - 1,
/// as Linux's `sigset_t` does. Read with n at bit n, blocking SIGUSR2 set the
/// bit for SIGSEGV instead, and SIGUSR2 was delivered at once.
///
/// In a forked child, so the mask this suite runs with is not the one changed.
/// The child's exit status says what it saw: 1 when the handler ran while the
/// signal was blocked, 2 when it had not run once by the time the signal was
/// unblocked.
fn a_blocked_signal_waits_to_be_unblocked(report: &mut Report) {
    use crate::sys;

    let child = sys::fork();
    if child == 0 {
        unsafe { signal(SIGUSR2, handle_signal as extern "C" fn(i32) as usize) };
        let set = 1u64 << (SIGUSR2 - 1);
        sys::sigprocmask(sys::SIG_BLOCK, set);
        let before = SIGNAL_TOTAL.load(Ordering::SeqCst);
        // To this thread, so that the only thread that could take it is the
        // one blocking it.
        sys::tgkill(sys::getpid() as i32, sys::gettid() as i32, SIGUSR2);
        let while_blocked = SIGNAL_TOTAL.load(Ordering::SeqCst) - before;
        // Taken on the way out of this call.
        sys::sigprocmask(sys::SIG_UNBLOCK, set);
        let unblocked = SIGNAL_TOTAL.load(Ordering::SeqCst) - before;
        let code = if while_blocked != 0 {
            1
        } else if unblocked != SIGUSR2 as usize {
            2
        } else {
            0
        };
        sys::exit_group(code);
    }
    let (reaped, status) = sys::wait4(child as i32, 0);
    report.check(
        "a blocked signal waits until it is unblocked",
        child > 0 && reaped == child && sys::signal_of(status).is_none()
            && sys::exit_code_of(status) == 0,
        format!("forked {} reaped {} status {:#x}", child, reaped, status),
    );
}

/// A thread of a child process is not a child of this one. It is given its
/// process's parent as its own so that an orphan is adopted the same way, and
/// a wait that matches on that alone hands back a task id this process never
/// forked while the child it is waiting for is still running.
fn thread_of_a_child_is_not_a_child(report: &mut Report) {
    use crate::sys;

    let child = sys::fork();
    if child == 0 {
        let worker = std::thread::spawn(|| 1u8);
        let _ = worker.join();
        // Outlive the thread by long enough that a wait which took the thread
        // would have come back well before this task did.
        std::thread::sleep(Duration::from_millis(400));
        sys::exit_group(7);
    }
    let started = Instant::now();
    let (reaped, status) = sys::wait4(-1, 0);
    let waited = started.elapsed();
    report.check(
        "wait skips the threads of a child",
        reaped == child && sys::exit_code_of(status) == 7,
        format!("forked {} reaped {} status {:#x} after {:?}", child, reaped, status, waited),
    );
}

/// A child that aborts is the process that dies of it. musl's `raise`, which
/// `abort` and every Rust panic go through, signals the thread id musl keeps
/// for the calling thread, and only musl's `fork` sets that id in the child.
/// A child made by the bare system call kept its parent's id and sent its
/// SIGABRT to the parent, which here is this suite.
fn a_child_that_aborts_is_the_one_signalled(report: &mut Report) {
    use crate::sys;
    const SIGABRT: i32 = 6;

    let child = sys::fork();
    if child == 0 {
        std::process::abort();
    }
    let (reaped, status) = sys::wait4(child as i32, 0);
    report.check(
        "a child's abort ends the child and not its parent",
        child > 0 && reaped == child && sys::signal_of(status) == Some(SIGABRT),
        format!("forked {} reaped {} status {:#x}", child, reaped, status),
    );
}

/// A process that ends itself from a thread other than its first reports the
/// status it asked for. `exit_group` ends the other threads with SIGKILL, and
/// the first thread, which is the one a wait reports, recorded that signal
/// instead of the status: Go calls `exit_group` from whichever thread ran
/// `os.Exit`, and a parent was told a process that exited with 1 had been
/// killed by signal 9. Linux keeps the status for the thread group and every
/// thread leaves with it. The first thread is asleep throughout, so in every
/// round it is the SIGKILL that ends it.
fn an_exit_from_a_thread_is_the_process_status(report: &mut Report) {
    use crate::sys;
    const CODE: i32 = 3;
    const CHILDREN: usize = 20;

    let mut wrong = Vec::new();
    for _ in 0..CHILDREN {
        let child = sys::fork();
        if child == 0 {
            std::thread::spawn(|| std::process::exit(CODE));
            // Only the other thread's exit is meant to end this one. A status
            // nothing else gives says it did not, rather than a hang.
            std::thread::sleep(Duration::from_secs(10));
            sys::exit_group(99);
        }
        if child < 0 {
            wrong.push(format!("fork returned {}", child));
            continue;
        }
        let (reaped, status) = sys::wait4(child as i32, 0);
        if reaped != child || sys::signal_of(status).is_some() || sys::exit_code_of(status) != CODE {
            wrong.push(format!("forked {} reaped {} status {:#x}", child, reaped, status));
        }
    }
    report.check(
        "an exit from a second thread is the process's status",
        wrong.is_empty(),
        format!("{} of {} children: {}", wrong.len(), CHILDREN, wrong.join("; ")),
    );
}

/// A process with a second thread that a signal from outside ends still
/// reports that signal. The signal ends the thread group with the signal as
/// the group's status, and no `exit_group` call gave it another one.
fn a_signal_from_outside_is_still_reported(report: &mut Report) {
    use crate::sys;
    const SIGKILL: i32 = 9;
    const SIGTERM: i32 = 15;

    for (name, signum) in [("SIGKILL", SIGKILL), ("SIGTERM", SIGTERM)] {
        let check = format!("a child killed by {} from outside reports it", name);
        let Ok((reader, writer)) = sys::pipe() else {
            report.check(&check, false, "no pipe to hear the child is ready".into());
            continue;
        };
        let child = sys::fork();
        if child == 0 {
            // A thread group like the ones above, whose second thread returns
            // on its own after a second.
            std::thread::spawn(|| std::thread::sleep(Duration::from_secs(1)));
            sys::write(writer, b"x");
            std::thread::sleep(Duration::from_secs(10));
            sys::exit_group(99);
        }
        sys::close(writer);
        if child < 0 {
            sys::close(reader);
            report.check(&check, false, format!("fork returned {}", child));
            continue;
        }
        let mut buf = [0u8; 1];
        let ready = sys::read(reader, &mut buf) == 1;
        sys::close(reader);
        sys::kill(child as i32, signum);
        let (reaped, status) = sys::wait4(child as i32, 0);
        report.check(
            &check,
            ready && reaped == child && sys::signal_of(status) == Some(signum),
            format!("forked {} ready {} reaped {} status {:#x}", child, ready, reaped, status),
        );
    }
}

/// Send the calling thread's id down `fd`, four bytes, so that the parent can
/// look for the thread once the process has been reaped.
fn send_tid(fd: i32) {
    let tid = crate::sys::gettid() as i32;
    crate::sys::write(fd, &tid.to_le_bytes());
}

/// Read `count` thread ids a child sent with `send_tid`, or as many as came
/// before every writer closed.
fn read_tids(fd: i32, count: usize) -> Vec<i32> {
    let mut tids = Vec::new();
    let mut buf = [0u8; 4];
    while tids.len() < count {
        let mut got = 0;
        while got < buf.len() {
            let n = crate::sys::read(fd, &mut buf[got..]);
            if n <= 0 {
                return tids;
            }
            got += n as usize;
        }
        tids.push(i32::from_le_bytes(buf));
    }
    tids
}

/// The lines of /proc/tasks, "pid ppid pgid state name", for any of `tids`:
/// the threads the kernel still holds.
fn tasks_listed(tids: &[i32]) -> Vec<String> {
    std::fs::read_to_string("/proc/tasks")
        .unwrap_or_default()
        .lines()
        .filter(|line| {
            let pid = line.split(' ').next().and_then(|field| field.parse::<i32>().ok());
            pid.map_or(false, |pid| tids.contains(&pid))
        })
        .map(String::from)
        .collect()
}

/// A process whose first thread ends with the `exit` system call, while another
/// thread runs on, has not finished. Linux's `wait` passes over the leader
/// until the thread group is empty and then reports the status the process
/// ended with. The leader was reaped the moment it exited, with the code its
/// own `exit` gave: the parent was told the process had ended while its other
/// thread still ran, and never saw the status that thread's `exit_group` set.
fn a_first_thread_that_exits_alone_is_not_the_process(report: &mut Report) {
    use crate::sys;
    const CODE: i32 = 5;
    const CHILDREN: usize = 20;

    let mut wrong = Vec::new();
    for _ in 0..CHILDREN {
        let Ok((reader, writer)) = sys::pipe() else {
            wrong.push("no pipe for the thread ids".to_string());
            break;
        };
        let child = sys::fork();
        if child == 0 {
            sys::close(reader);
            send_tid(writer);
            std::thread::spawn(move || {
                send_tid(writer);
                std::thread::sleep(Duration::from_millis(150));
                sys::exit_group(CODE);
            });
            sys::exit_thread(0);
        }
        sys::close(writer);
        if child < 0 {
            sys::close(reader);
            wrong.push(format!("fork returned {}", child));
            continue;
        }
        let tids = read_tids(reader, 2);
        sys::close(reader);
        let (reaped, status) = sys::wait4(child as i32, 0);
        let left = tasks_listed(&tids);
        let ended = sys::signal_of(status).is_none() && sys::exit_code_of(status) == CODE;
        if reaped != child || !ended || tids.len() != 2 || !left.is_empty() {
            wrong.push(format!(
                "forked {} reaped {} status {:#x}, threads {:?}, still listed {:?}",
                child, reaped, status, tids, left
            ));
        }
    }
    report.check(
        "a process whose first thread exits alone ends with its last thread",
        wrong.is_empty(),
        format!("{} of {} children: {}", wrong.len(), CHILDREN, wrong.join("; ")),
    );
}

/// Fork a child for the checks on a whole thread group. The child sends its
/// first thread's id down a pipe and runs `body` with the pipe's writing end;
/// `body` starts the child's other threads, each of which sends its own id.
/// Returns the child's pid and the `threads` ids, first thread's first, once
/// they have all arrived, which says the threads have all started.
fn thread_group_child(threads: usize, body: impl FnOnce(i32)) -> Result<(i64, Vec<i32>), String> {
    use crate::sys;

    let Ok((reader, writer)) = sys::pipe() else {
        return Err("no pipe for the thread ids".to_string());
    };
    let child = sys::fork();
    if child == 0 {
        sys::close(reader);
        send_tid(writer);
        body(writer);
        // Every body ends the process before this, or is meant to.
        sys::exit_group(99);
    }
    sys::close(writer);
    if child < 0 {
        sys::close(reader);
        return Err(format!("fork returned {}", child));
    }
    let tids = read_tids(reader, threads);
    sys::close(reader);
    if tids.len() != threads {
        return Err(format!("forked {}, {} of {} thread ids arrived", child, tids.len(), threads));
    }
    Ok((child, tids))
}

/// How a process is expected to end.
#[derive(Clone, Copy)]
enum Ending {
    /// Killed by this signal.
    Signal(i32),
    /// Exited with this code.
    Code(i32),
}

impl Ending {
    fn is(self, status: i32) -> bool {
        use crate::sys;
        match self {
            Ending::Signal(signum) => sys::signal_of(status) == Some(signum),
            Ending::Code(code) => status & 0x7F == 0 && sys::exit_code_of(status) == code,
        }
    }
}

/// Wait for a child `thread_group_child` made, for two seconds at most, and
/// say what is wrong if the process did not end whole, promptly, the way
/// `expected` says: a wait that took longer, another status, or any of its
/// threads still in /proc/tasks after the wait reported it.
fn ended(child: i64, tids: &[i32], expected: Ending) -> Option<String> {
    let started = Instant::now();
    let (reaped, status) = wait_or_kill(child, Duration::from_secs(2));
    let waited = started.elapsed();
    let left = tasks_listed(tids);
    let prompt = waited < Duration::from_secs(2);
    if reaped == child && expected.is(status) && prompt && left.is_empty() {
        return None;
    }
    Some(format!(
        "forked {} reaped {} status {:#x} after {:?}, still listed {:?}",
        child, reaped, status, waited, left
    ))
}

/// The same check over `rounds` children made by `make`, reported as `name`.
fn every_child_ends(
    report: &mut Report,
    name: &str,
    rounds: usize,
    expected: Ending,
    mut make: impl FnMut() -> Result<(i64, Vec<i32>), String>,
) {
    let mut wrong = Vec::new();
    for _ in 0..rounds {
        match make() {
            Ok((child, tids)) => wrong.extend(ended(child, &tids, expected)),
            Err(err) => wrong.push(err),
        }
    }
    report.check(
        name,
        wrong.is_empty(),
        format!("{} of {} children: {}", wrong.len(), rounds, wrong.join("; ")),
    );
}

/// A thread that aborts ends its whole process. Only the thread that took
/// SIGABRT used to end: the first thread slept on, the parent's wait was not
/// answered, and nothing of the process that aborted was told to anyone. Linux
/// ends the thread group for a signal whose action is to terminate, and so
/// does this now.
fn a_thread_that_aborts_ends_its_process(report: &mut Report) {
    const SIGABRT: i32 = 6;
    every_child_ends(
        report,
        "a thread's abort ends its whole process",
        20,
        Ending::Signal(SIGABRT),
        || {
            thread_group_child(2, |writer| {
                std::thread::spawn(move || {
                    send_tid(writer);
                    std::process::abort();
                });
                std::thread::sleep(Duration::from_secs(10));
            })
        },
    );
}

/// A thread that takes SIGSEGV with its default action ends its whole process,
/// both when the signal is sent to the thread and when the thread faults.
///
/// The first is what the Go runtime does with a fault it cannot handle: it
/// puts the default action back and sends the signal to its own thread with
/// `tgkill`, to die of it. The second is the kernel's own kill for a fault,
/// which no handler is asked about. Each ended only the thread; the rest of
/// the process ran on and was never reported. A fault prints some eighty lines
/// of registers and regions, so it gets one round: past the way the signal
/// arrives, it ends the group through the same code the twenty rounds of the
/// first form go through.
fn a_thread_that_takes_sigsegv_ends_its_process(report: &mut Report) {
    use crate::sys;
    const SIGSEGV: i32 = 11;
    const SIG_DFL: usize = 0;

    every_child_ends(
        report,
        "a thread that sends itself SIGSEGV ends its whole process",
        20,
        Ending::Signal(SIGSEGV),
        || {
            thread_group_child(2, |writer| {
                std::thread::spawn(move || {
                    send_tid(writer);
                    unsafe { signal(SIGSEGV, SIG_DFL) };
                    sys::tgkill(sys::getpid() as i32, sys::gettid() as i32, SIGSEGV);
                    std::thread::sleep(Duration::from_secs(10));
                });
                std::thread::sleep(Duration::from_secs(10));
            })
        },
    );
    every_child_ends(
        report,
        "a thread that faults ends its whole process",
        1,
        Ending::Signal(SIGSEGV),
        || {
            thread_group_child(2, |writer| {
                std::thread::spawn(move || {
                    send_tid(writer);
                    let address = std::hint::black_box(16usize) as *mut u8;
                    unsafe { std::ptr::write_volatile(address, 1) };
                    std::thread::sleep(Duration::from_secs(10));
                });
                std::thread::sleep(Duration::from_secs(10));
            })
        },
    );
}

/// SIGKILL sent to a process with several threads ends all of them. It used to
/// end the task whose id is the process's, the one a wait reports, so the
/// parent was told of a kill while the other threads ran on. One of them spins
/// in user mode and two sleep; they finish by themselves after five seconds,
/// so a kernel that leaves them running still finishes the suite.
fn a_kill_ends_every_thread(report: &mut Report) {
    use crate::sys;
    const SIGKILL: i32 = 9;

    every_child_ends(
        report,
        "SIGKILL to a process ends every thread of it",
        20,
        Ending::Signal(SIGKILL),
        || {
            let made = thread_group_child(4, |writer| {
                for spin in [true, false, false] {
                    std::thread::spawn(move || {
                        send_tid(writer);
                        let started = Instant::now();
                        while started.elapsed() < Duration::from_secs(5) {
                            if !spin {
                                std::thread::sleep(Duration::from_millis(20));
                            }
                        }
                    });
                }
                std::thread::sleep(Duration::from_secs(10));
            });
            if let Ok((child, _)) = &made {
                sys::kill(*child as i32, SIGKILL);
            }
            made
        },
    );
}

/// A signal whose default action is to terminate, sent to a process whose first
/// thread is waiting to join a second one that spins, ends the process: every
/// thread of it, promptly. The signal is the process's, and either thread may
/// take it; whichever does ends the thread group.
fn a_signal_from_outside_ends_every_thread(report: &mut Report) {
    use crate::sys;
    const SIGTERM: i32 = 15;

    every_child_ends(
        report,
        "SIGTERM to a process whose first thread is joining ends every thread",
        20,
        Ending::Signal(SIGTERM),
        || {
            let made = thread_group_child(2, |writer| {
                let spinner = std::thread::spawn(move || {
                    send_tid(writer);
                    let started = Instant::now();
                    while started.elapsed() < Duration::from_secs(5) {}
                });
                let _ = spinner.join();
            });
            if let Ok((child, _)) = &made {
                sys::kill(*child as i32, SIGTERM);
            }
            made
        },
    );
}

/// Wait up to two seconds for a stop of `child` to be reported, and return the
/// signal that stopped it.
fn stop_reported(child: i64) -> Option<i32> {
    use crate::sys;
    let started = Instant::now();
    loop {
        let (pid, status) = sys::wait4(child as i32, sys::WNOHANG | sys::WUNTRACED);
        if pid == child {
            return sys::stop_signal_of(status);
        }
        if pid != 0 || started.elapsed() > Duration::from_secs(2) {
            return None;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// A process with three threads, stopped with SIGSTOP and then killed with
/// SIGKILL, is reaped promptly with signal 9 and leaves no thread behind.
///
/// SIGSTOP stops every thread of the process, as Linux's group stop does, and
/// the check waits until /proc/tasks shows all three stopped before it sends
/// the kill. Each of them has to be restarted to die: a stopped thread was not
/// returned to the run queue by a kill marked pending on it, and outlived its
/// process. SIGSTOP used to reach the first thread alone, so this also checks
/// that the other two stop.
fn a_stopped_process_with_threads_is_killed(report: &mut Report) {
    use crate::sys;
    const SIGKILL: i32 = 9;
    const SIGSTOP: i32 = 19;

    let all_stopped = |tids: &[i32]| {
        let listed = tasks_listed(tids);
        listed.len() == tids.len() && listed.iter().all(|line| line.split(' ').nth(3) == Some("T"))
    };
    every_child_ends(
        report,
        "a stopped process with threads is killed whole",
        20,
        Ending::Signal(SIGKILL),
        || {
            let (child, tids) = thread_group_child(3, |writer| {
                for _ in 0..2 {
                    std::thread::spawn(move || {
                        send_tid(writer);
                        let started = Instant::now();
                        while started.elapsed() < Duration::from_secs(5) {
                            std::thread::sleep(Duration::from_millis(20));
                        }
                    });
                }
                std::thread::sleep(Duration::from_secs(10));
            })?;
            sys::kill(child as i32, SIGSTOP);
            let stop = stop_reported(child);
            let started = Instant::now();
            while !all_stopped(&tids) && started.elapsed() < Duration::from_secs(2) {
                std::thread::sleep(Duration::from_millis(10));
            }
            let listed = tasks_listed(&tids);
            let stopped = all_stopped(&tids);
            sys::kill(child as i32, SIGKILL);
            if stop != Some(SIGSTOP) || !stopped {
                let (reaped, status) = wait_or_kill(child, Duration::from_secs(2));
                return Err(format!(
                    "forked {}: stop reported as {:?}, threads {:?}; then reaped {} status {:#x}",
                    child, stop, listed, reaped, status
                ));
            }
            Ok((child, tids))
        },
    );
}

/// A signal sent to a process whose first thread blocks it is taken by a thread
/// that does not, and the process carries on. The signal went to the task
/// whose id is the process's alone, where it waited for good: the handler never
/// ran, although another thread would have taken it at once.
///
/// The child's first thread blocks SIGUSR1 before it starts the second, which
/// unblocks it for itself, since a thread starts with the mask of the thread
/// that made it. The second thread waits two seconds at most for the handler,
/// and the child's exit status says what happened: 0 when the handler ran in
/// the second thread, 1 when it did not run, 2 when it ran in the first.
fn a_signal_for_the_process_reaches_a_thread_that_takes_it(report: &mut Report) {
    use crate::sys;
    use std::sync::atomic::AtomicI32;

    static TAKER: AtomicI32 = AtomicI32::new(0);
    static HANDLED_BY: AtomicI32 = AtomicI32::new(0);
    extern "C" fn note_the_thread(_: i32) {
        HANDLED_BY.store(crate::sys::gettid() as i32, Ordering::SeqCst);
    }

    every_child_ends(
        report,
        "a signal for a process is taken by a thread that does not block it",
        20,
        Ending::Code(0),
        || {
            let made = thread_group_child(2, |writer| {
                unsafe { signal(SIGUSR1, note_the_thread as extern "C" fn(i32) as usize) };
                let set = 1u64 << (SIGUSR1 - 1);
                sys::sigprocmask(sys::SIG_BLOCK, set);
                let taker = std::thread::spawn(move || {
                    sys::sigprocmask(sys::SIG_UNBLOCK, set);
                    TAKER.store(sys::gettid() as i32, Ordering::SeqCst);
                    send_tid(writer);
                    let started = Instant::now();
                    while HANDLED_BY.load(Ordering::SeqCst) == 0
                        && started.elapsed() < Duration::from_secs(2)
                    {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                });
                let _ = taker.join();
                let handled_by = HANDLED_BY.load(Ordering::SeqCst);
                sys::exit_group(if handled_by == TAKER.load(Ordering::SeqCst) {
                    0
                } else if handled_by == 0 {
                    1
                } else {
                    2
                });
            });
            if let Ok((child, _)) = &made {
                sys::kill(*child as i32, SIGUSR1);
            }
            made
        },
    );
}

/// A process blocked on something other than a child still has to learn that a
/// child finished: the child signal is a signal, and every other one returns a
/// sleeping task to the run queue. A shell waiting for a key is the case that
/// matters.
fn a_child_exit_reaches_a_blocked_parent(report: &mut Report) {
    use crate::sys;
    use std::sync::atomic::AtomicBool;

    const SIGCHLD: i32 = 17;
    const SIG_DFL: usize = 0;
    static DONE: AtomicBool = AtomicBool::new(false);

    let Ok((reader, writer)) = sys::pipe() else {
        report.check("a pipe for the blocked read", false, String::new());
        return;
    };
    unsafe { signal(SIGCHLD, handle_signal as extern "C" fn(i32) as usize) };
    let before = SIGNAL_TOTAL.load(Ordering::SeqCst);

    let child = sys::fork();
    if child == 0 {
        std::thread::sleep(Duration::from_millis(250));
        sys::exit_group(0);
    }
    // Nothing will ever be written to this pipe, so only the child signal ends
    // the read. The watchdog writes a byte if it does not, so a regression is a
    // failed check rather than a suite that never finishes.
    let watchdog = std::thread::spawn(move || {
        for _ in 0..30 {
            if DONE.load(Ordering::Relaxed) {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        sys::write(writer, b"x");
    });

    let started = Instant::now();
    let mut buf = [0u8; 1];
    let n = sys::read(reader, &mut buf);
    let waited = started.elapsed();
    DONE.store(true, Ordering::Relaxed);
    let _ = watchdog.join();

    let ran = SIGNAL_TOTAL.load(Ordering::SeqCst) - before == SIGCHLD as usize;
    unsafe { signal(SIGCHLD, SIG_DFL) };
    let _ = sys::wait4(child as i32, 0);
    sys::close(reader);
    sys::close(writer);

    report.check(
        "a child's exit ends a read the parent was blocked in",
        n == -4 && ran && waited < Duration::from_millis(1500),
        format!("read returned {} after {:?}, handler ran: {}", n, waited, ran),
    );
}

/// What a wait does with signals: a signal that would be discarded on delivery
/// is no reason to give the wait up, and the child signal has to survive the
/// wait so that a process with a handler sees it run for the child it reaped.
fn what_a_wait_does_with_signals(report: &mut Report) {
    use crate::sys;

    const SIGCHLD: i32 = 17;
    const SIGWINCH: i32 = 28;
    const SIG_DFL: usize = 0;

    // A terminal resize is ignored by default, so a wait must not fail on it.
    let child = sys::fork();
    if child == 0 {
        sys::kill(sys::getppid() as i32, SIGWINCH);
        std::thread::sleep(Duration::from_millis(300));
        sys::exit_group(7);
    }
    let (pid, status) = sys::wait4(child as i32, 0);
    report.check(
        "a discarded signal does not interrupt a wait",
        pid == child && sys::exit_code_of(status) == 7,
        format!("reaped {} status {:#x}", pid, status),
    );

    // And the child signal is still there to be delivered afterwards.
    unsafe { signal(SIGCHLD, handle_signal as extern "C" fn(i32) as usize) };
    let before = SIGNAL_TOTAL.load(Ordering::SeqCst);
    let child = sys::fork();
    if child == 0 {
        std::thread::sleep(Duration::from_millis(100));
        sys::exit_group(0);
    }
    let (pid, _) = sys::wait4(child as i32, 0);
    // The handler runs on the way out of the wait.
    for _ in 0..4 {
        std::thread::yield_now();
    }
    let ran = SIGNAL_TOTAL.load(Ordering::SeqCst) - before == SIGCHLD as usize;
    unsafe { signal(SIGCHLD, SIG_DFL) };
    report.check(
        "a handler runs for a child the process reaped itself",
        pid == child && ran,
        format!("reaped {}, handler ran: {}", pid, ran),
    );
}

/// A stop nobody asked to be told about has to stop being reportable once the
/// job is running again, or the next wait that does ask is handed a suspension
/// that has already ended.
fn a_continued_job_has_no_stop_to_report(report: &mut Report) {
    use crate::sys;

    const SIGKILL: i32 = 9;
    const SIGCONT: i32 = 18;
    const SIGTSTP: i32 = 20;
    const WNOHANG: u64 = 1;
    const WUNTRACED: u64 = 2;
    const WCONTINUED: u64 = 8;

    let child = sys::fork();
    if child == 0 {
        loop {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    let child = child as i32;

    sys::kill(child, SIGTSTP);
    std::thread::sleep(Duration::from_millis(150));
    // This wait does not ask about stops, so it is told nothing.
    let (idle, _) = sys::wait4(child, WNOHANG);
    sys::kill(child, SIGCONT);
    std::thread::sleep(Duration::from_millis(150));
    // This one asks about both, and the job is running.
    let (pid, status) = sys::wait4(child, WNOHANG | WUNTRACED | WCONTINUED);

    sys::kill(child, SIGKILL);
    let _ = sys::wait4(child, 0);
    report.check(
        "a continued job has no stop left to report",
        idle == 0 && pid == child as i64 && sys::is_continued(status),
        format!("first wait {}, then {} status {:#x}", idle, pid, status),
    );
}

/// A wait for a negative value below minus one names a process group, which is
/// how a shell waits for a job rather than for one process of it.
fn waiting_on_a_process_group(report: &mut Report) {
    use crate::sys;

    // One child in a group of its own, taking its time, and one in this
    // process's group that finishes at once.
    let member = sys::fork();
    if member == 0 {
        sys::setpgid(0, 0);
        std::thread::sleep(Duration::from_millis(300));
        sys::exit_group(4);
    }
    // Set it from here as well, so the group is right whichever task runs next.
    sys::setpgid(member as i32, member as i32);

    let outsider = sys::fork();
    if outsider == 0 {
        sys::exit_group(5);
    }
    std::thread::sleep(Duration::from_millis(50));

    let (pid, status) = sys::wait4(-(member as i32), 0);
    let ok = pid == member && sys::exit_code_of(status) == 4;
    let _ = sys::wait4(outsider as i32, 0);
    report.check(
        "a wait for a process group skips a child outside it",
        ok,
        format!("group {} outsider {} reaped {} status {:#x}", member, outsider, pid, status),
    );
}

/// A task's entry in /proc has to be there before the task can run. The first
/// thing a forked child does here is open its own status file, which is what a
/// shell applying a redirection through its own descriptor directory amounts
/// to. Fifteen hundred rounds is a smoke test: the window is however long the
/// entry takes to build, and it is missed only when a tick lands inside it.
fn a_child_finds_its_own_proc_entry(report: &mut Report) {
    use crate::sys;

    let mut misses = 0;
    for _ in 0..1500 {
        let child = sys::fork();
        if child == 0 {
            let fd = sys::open("/proc/self/status", sys::O_RDONLY, 0);
            if fd < 0 {
                sys::exit_group(1);
            }
            sys::close(fd as i32);
            sys::exit_group(0);
        }
        let (_, status) = sys::wait4(child as i32, 0);
        if sys::exit_code_of(status) != 0 {
            misses += 1;
        }
    }
    report.check(
        "a forked child finds its own entry in /proc",
        misses == 0,
        format!("{} of 1500 missed it", misses),
    );
}

/// A fork takes write permission away from every page of the address space it
/// copies, the parent's included. A sibling thread inside a write to user
/// memory has already had its buffer checked by then, so the copy that follows
/// stores into a page that has just become read-only.
fn a_fork_while_a_sibling_writes(report: &mut Report) {
    use crate::sys;
    use std::sync::atomic::AtomicBool;

    static RUNNING: AtomicBool = AtomicBool::new(true);
    let devnull = sys::open("/dev/null", sys::O_WRONLY, 0);
    let writes = Arc::new(AtomicUsize::new(0));
    let writer_count = Arc::clone(&writes);
    let writer = std::thread::spawn(move || {
        let buf = vec![7u8; 8192];
        let mut back = vec![0u8; 8192];
        let fd = sys::open("/tmp/forkrace.dat", sys::O_WRONLY | 0o100 | 0o1000, 0o644);
        while RUNNING.load(Ordering::Relaxed) {
            if fd >= 0 {
                sys::write(fd as i32, &buf);
            }
            if devnull >= 0 {
                sys::write(devnull as i32, &buf);
            }
            // A read is what puts the kernel on the writing side of a user
            // buffer, which is where the store happens.
            let rd = sys::open("/tmp/forkrace.dat", sys::O_RDONLY, 0);
            if rd >= 0 {
                sys::read(rd as i32, &mut back);
                sys::close(rd as i32);
            }
            writer_count.fetch_add(1, Ordering::Relaxed);
        }
        if fd >= 0 {
            sys::close(fd as i32);
        }
    });

    let mut rounds = 0;
    for _ in 0..60 {
        let child = sys::fork();
        if child == 0 {
            sys::exit_group(0);
        }
        let (pid, _) = sys::wait4(child as i32, 0);
        if pid == child {
            rounds += 1;
        }
    }
    RUNNING.store(false, Ordering::Relaxed);
    let _ = writer.join();
    if devnull >= 0 {
        sys::close(devnull as i32);
    }
    let _ = std::fs::remove_file("/tmp/forkrace.dat");

    report.check(
        "a fork does not fault a sibling's copy out of the kernel",
        rounds == 60 && writes.load(Ordering::Relaxed) > 0,
        format!("{} rounds, {} writes", rounds, writes.load(Ordering::Relaxed)),
    );
}

/// Reading a process's entry in /proc walks every page of every region it has.
/// The task can be reaped while that walk is running, by another thread of the
/// same process, so the reader has to hold the process table rather than a
/// pointer it took out of it.
fn reading_proc_while_a_child_is_reaped(report: &mut Report) {
    use crate::sys;
    use std::sync::atomic::{AtomicBool, AtomicUsize as Atomic};

    static WATCHED: Atomic = Atomic::new(0);
    static RUNNING: AtomicBool = AtomicBool::new(true);

    let reads = Arc::new(AtomicUsize::new(0));
    let reader_count = Arc::clone(&reads);
    let reader = std::thread::spawn(move || {
        while RUNNING.load(Ordering::Relaxed) {
            let pid = WATCHED.load(Ordering::Relaxed);
            if pid == 0 {
                std::thread::yield_now();
                continue;
            }
            let _ = std::fs::read_to_string(format!("/proc/{}/stat", pid));
            let _ = std::fs::read_to_string(format!("/proc/{}/maps", pid));
            reader_count.fetch_add(1, Ordering::Relaxed);
        }
    });

    let mut rounds = 0;
    for _ in 0..100 {
        let child = sys::fork();
        if child == 0 {
            std::thread::sleep(Duration::from_millis(5));
            sys::exit_group(0);
        }
        WATCHED.store(child as usize, Ordering::Relaxed);
        let (pid, _) = sys::wait4(child as i32, 0);
        WATCHED.store(0, Ordering::Relaxed);
        if pid == child {
            rounds += 1;
        }
    }
    RUNNING.store(false, Ordering::Relaxed);
    let _ = reader.join();

    let reads = reads.load(Ordering::Relaxed);
    report.check(
        "reading /proc survives the task being reaped",
        rounds == 100 && reads > 0,
        format!("{} rounds, {} reads", rounds, reads),
    );
}

/// A child that has not written to its stack since the fork still shares those
/// pages with the parent, so the signal frame the kernel writes there goes
/// through the path that breaks the sharing first.
fn a_signal_frame_on_a_shared_page(report: &mut Report) {
    use crate::sys;

    let before = SIGNAL_TOTAL.load(Ordering::SeqCst);
    let child = sys::fork();
    if child == 0 {
        // Straight to the kernel rather than through libc's `raise`: this task
        // was made by a bare fork, so the thread id libc remembers is still the
        // parent's and the signal would go there.
        // The result of this call is stored over the frame a handler is
        // entered on, so a signal sent to oneself has to come back as zero for
        // the handler to be given the right number.
        let sent = sys::kill(sys::getpid() as i32, SIGUSR2);
        // The handler runs on the way out, and execution has to carry on from
        // where it left off afterwards.
        let ran = SIGNAL_TOTAL.load(Ordering::SeqCst) - before == SIGUSR2 as usize;
        sys::exit_group(if ran && sent == 0 { 0 } else { 1 });
    }
    let (pid, status) = sys::wait4(child as i32, 0);
    report.check(
        "a signal frame lands on a page shared after a fork",
        pid == child && sys::exit_code_of(status) == 0,
        format!("reaped {} status {:#x}", pid, status),
    );
}

/// A handler installed with no restorer.
///
/// Linux on aarch64 does not read that field. It maps a page of its own
/// holding the return sequence into every program and sends a handler back
/// through that, so a program built for that machine has no reason to fill the
/// field in, and a Go program does not. x86-64 has no such page: the return
/// address on the frame is the only way back, and Linux refuses to deliver a
/// signal to a disposition that names none. So the two machines are checked
/// against different answers, and each against the one its ABI gives.
///
/// musl fills the field in on both, which is why every program shipped here
/// would pass this either way. The call below goes straight to the kernel with
/// the field zero, which is the only way to ask the question from a program
/// linked against a libc.
fn a_handler_with_no_restorer(report: &mut Report) {
    use crate::sys;

    const SIGSEGV: i32 = 11;

    let child = sys::fork();
    if child == 0 {
        let handler = handle_signal as extern "C" fn(i32) as usize;
        let installed = sys::set_handler_without_restorer(SIGUSR1, handler);
        let before = SIGNAL_TOTAL.load(Ordering::SeqCst);
        // Straight to the kernel rather than through libc's `raise`, which
        // sends to the thread id libc remembers -- still the parent's after a
        // bare fork.
        let sent = sys::kill(sys::getpid() as i32, SIGUSR1);
        let ran = SIGNAL_TOTAL.load(Ordering::SeqCst) - before == SIGUSR1 as usize;
        // On aarch64 this is reached only if the handler returned, so reaching
        // it at all is the check; on x86-64 the delivery is refused and the
        // child never gets here. The sum says the code after the handler runs
        // on the registers the handler was entered with.
        let mut accumulator = 0u64;
        for i in 0..1000u64 {
            accumulator = accumulator.wrapping_add(i * i);
        }
        let ok = installed == 0 && sent == 0 && ran && accumulator == 332_833_500;
        sys::exit_group(if ok { 0 } else { 1 });
    }
    let (pid, status) = sys::wait4(child as i32, 0);
    let detail = format!("reaped {} status {:#x}", pid, status);
    if cfg!(target_arch = "aarch64") {
        report.check(
            "a handler with no restorer is delivered and returns",
            pid == child && sys::exit_code_of(status) == 0,
            detail,
        );
    } else {
        report.check(
            "a handler with no restorer is refused, and the program told so",
            pid == child && sys::signal_of(status) == Some(SIGSEGV),
            detail,
        );
    }
}

/// Stopping a job and telling its parent are one step. A tick between them
/// takes the CPU away from a task that is no longer runnable, so the parent is
/// never told and sleeps in wait4 for good: the shell that pressed the suspend
/// key never gets its prompt back.
///
/// Forty rounds is a smoke test, not proof: the window is two instructions
/// wide. A watchdog kills the child if a round takes too long, so a regression
/// reads as a failed check rather than a suite that never finishes.
fn stopping_a_job_reaches_the_parent(report: &mut Report) {
    use crate::sys;
    use std::sync::atomic::AtomicBool;

    const SIGKILL: i32 = 9;
    const SIGCONT: i32 = 18;
    const SIGTSTP: i32 = 20;
    const WUNTRACED: u64 = 2;
    const WCONTINUED: u64 = 8;

    static DONE: AtomicBool = AtomicBool::new(false);

    let child = sys::fork();
    if child == 0 {
        loop {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    let child = child as i32;

    let watchdog = std::thread::spawn(move || {
        for _ in 0..100 {
            if DONE.load(Ordering::Relaxed) {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        sys::kill(child, SIGKILL);
    });

    let mut rounds = 0;
    let mut detail = String::new();
    for round in 0..40 {
        sys::kill(child, SIGTSTP);
        let (pid, status) = sys::wait4(child, WUNTRACED);
        if pid != child as i64 || sys::stop_signal_of(status) != Some(SIGTSTP) {
            detail = format!("round {}: stop reported as {} {:#x}", round, pid, status);
            break;
        }
        sys::kill(child, SIGCONT);
        let (pid, status) = sys::wait4(child, WCONTINUED);
        if pid != child as i64 || !sys::is_continued(status) {
            detail = format!("round {}: continue reported as {} {:#x}", round, pid, status);
            break;
        }
        rounds += 1;
    }
    // A continue that lands while the task is on its way into a stop finds it
    // still runnable, so it has nothing to restart; the stop that follows must
    // give way to it rather than park the task with a continue pending that
    // nothing will ever look at. Back-to-back pairs is the closest a program
    // can get to that from outside.
    for _ in 0..60 {
        sys::kill(child, SIGTSTP);
        sys::kill(child, SIGCONT);
    }
    std::thread::sleep(Duration::from_millis(200));
    let state = std::fs::read_to_string(format!("/proc/{}/stat", child))
        .ok()
        .and_then(|line| line.split(' ').nth(2).map(|s| s.to_string()))
        .unwrap_or_default();

    DONE.store(true, Ordering::Relaxed);
    let _ = watchdog.join();
    sys::kill(child, SIGCONT);
    sys::kill(child, SIGKILL);
    let _ = sys::wait4(child, 0);

    report.check(
        "a stop and a continue both reach the parent",
        rounds == 40,
        format!("{} rounds; {}", rounds, detail),
    );
    report.check(
        "a continue is not lost to the stop it races",
        state != "T" && !state.is_empty(),
        format!("child state {:?} after 60 stop-continue pairs", state),
    );
}

/// A number no signal has, sent to a stopped child.
///
/// The number arrives in a register, and it used to be folded into the range
/// on its way to the pending set: 73 masked to six bits is 9, so a send Linux
/// refuses set the bit for the kill signal instead. The code that restarts a
/// stopped task for a kill compared the number it was given, saw 73, and did
/// nothing, so the child stayed stopped with a kill pending that nothing could
/// take away -- and took it the moment anything continued it. Negative numbers
/// fold into the range the same way.
fn a_number_no_signal_has(report: &mut Report) {
    use crate::sys;

    const EINVAL: i64 = -22;
    const SIGKILL: i32 = 9;
    const SIGCONT: i32 = 18;
    const SIGSTOP: i32 = 19;
    const WNOHANG: u64 = 1;

    let child = sys::fork();
    if child == 0 {
        loop {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    let child = child as i32;

    let stopping = sys::kill(child, SIGSTOP);
    std::thread::sleep(Duration::from_millis(150));
    let above = sys::kill(child, 73);
    let below = sys::kill(child, -55);
    report.check(
        "a number no signal has is refused",
        stopping == 0 && above == EINVAL && below == EINVAL,
        format!("stop {} 73 {} -55 {}", stopping, above, below),
    );

    // Continuing the child is what shows whether either send left a kill
    // behind: a child carrying one dies here rather than running on.
    sys::kill(child, SIGCONT);
    std::thread::sleep(Duration::from_millis(200));
    let (reaped, status) = sys::wait4(child, WNOHANG);
    let state = std::fs::read_to_string(format!("/proc/{}/stat", child))
        .ok()
        .and_then(|line| line.split(' ').nth(2).map(|s| s.to_string()))
        .unwrap_or_default();
    report.check(
        "and the child it was sent to is running afterwards",
        reaped == 0 && (state == "S" || state == "R"),
        format!("wait4 {} status {:#x} state {:?}", reaped, status, state),
    );

    sys::kill(child, SIGKILL);
    let (pid, status) = sys::wait4(child, 0);
    report.check(
        "a kill that is one still lands",
        pid == child as i64 && sys::signal_of(status) == Some(SIGKILL),
        format!("{} status {:#x}", pid, status),
    );
}

/// A signal that arrives while a task is still runnable finds nothing to wake.
/// If the task then parks itself without asking again, the signal waits out the
/// whole sleep: a minute for a bounded one, and for good for the unbounded
/// sleep `pause` asks for.
///
/// Forty rounds with the signal walked across the child's way into the sleep is
/// a smoke test, not proof: the window is a handful of instructions.
fn a_signal_ends_a_sleep(report: &mut Report) {
    use crate::sys;

    // A signal whose default action kills: the handler for SIGUSR1 is set to
    // ignore by the time this runs, and an ignored signal is no reason to end a
    // sleep.
    const SIGTERM: i32 = 15;

    let mut rounds = 0;
    let mut worst = Duration::ZERO;
    let mut detail = String::new();
    for k in 0..40 {
        let child = sys::fork();
        if child == 0 {
            // Long enough that a sleep which ignored the signal is unmistakable.
            std::thread::sleep(Duration::from_secs(4));
            sys::exit_group(0);
        }
        // Walk the signal's arrival across the child's way into the sleep.
        std::thread::sleep(Duration::from_millis(5 + (k % 7)));
        let started = Instant::now();
        sys::kill(child as i32, SIGTERM);
        let (pid, _) = sys::wait4(child as i32, 0);
        let waited = started.elapsed();
        worst = worst.max(waited);
        if pid != child {
            detail = format!("round {}: reaped {} not {}", k, pid, child);
            break;
        }
        if waited > Duration::from_millis(1500) {
            detail = format!("round {}: the sleep ran on for {:?}", k, waited);
            break;
        }
        rounds += 1;
    }
    report.check(
        "a signal ends a bounded sleep",
        rounds == 40,
        format!("{} rounds, worst {:?}; {}", rounds, worst, detail),
    );

    // And the unbounded one. A pause with nothing pending never ends on its
    // own, so the signal goes in after the child is certainly inside it.
    let child = sys::fork();
    if child == 0 {
        unsafe { pause() };
        sys::exit_group(0);
    }
    std::thread::sleep(Duration::from_millis(250));
    let started = Instant::now();
    sys::kill(child as i32, SIGTERM);
    let (pid, _) = sys::wait4(child as i32, 0);
    let waited = started.elapsed();
    report.check(
        "a signal ends a pause",
        pid == child && waited < Duration::from_millis(1500),
        format!("reaped {} after {:?}", pid, waited),
    );
}

/// A failed exec has to put the task back on the address space it came from
/// and on the page tables that go with it together. A thread running in the
/// same address space is what notices if it does not: it is resumed on the
/// record's word that nothing reloaded, which is the half-built image the exec
/// was assembling.
///
/// Running the path many times is a smoke test, not proof: the window is a few
/// instructions wide and lands only if a tick falls inside it.
fn failed_exec_and_siblings(report: &mut Report) {
    use crate::sys;
    use std::sync::atomic::AtomicBool;

    static RUNNING: AtomicBool = AtomicBool::new(true);
    let devnull = sys::open("/dev/null", sys::O_WRONLY, 0);
    let rounds = Arc::new(AtomicUsize::new(0));
    let refused = Arc::new(AtomicUsize::new(0));

    // Both halves run on threads rather than on the main one. The scheduler
    // takes the next task in the table, and the one after the main thread is
    // the kernel's network task, which runs on the kernel's own page tables
    // and so reloads them on the way past; a task whose neighbour is its own
    // sibling is what gets handed straight over.
    let execer_refused = Arc::clone(&refused);
    let execer = std::thread::spawn(move || {
        // An argument list past what the stack may hold fails after the new
        // image is loaded and the CPU is running on it, which is the path that
        // has to put the old one back.
        let argv: Vec<String> = (0..600).map(|_| "x".repeat(4000)).collect();
        for _ in 0..40 {
            if sys::execve("/bin/echo", &argv, &[]) < 0 {
                execer_refused.fetch_add(1, Ordering::Relaxed);
            }
        }
        RUNNING.store(false, Ordering::Relaxed);
    });

    let worker_rounds = Arc::clone(&rounds);
    let worker = std::thread::spawn(move || {
        let mut heap = vec![0u8; 512 * 1024];
        while RUNNING.load(Ordering::Relaxed) {
            let mut i = 0;
            while i < heap.len() {
                heap[i] = heap[i].wrapping_add(1);
                i += 4096;
            }
            // And once through the kernel: a buffer the kernel itself reads is
            // where page tables that do not match the record are fatal rather
            // than a fault the handler can retry.
            if devnull >= 0 {
                sys::write(devnull as i32, &heap[..4096]);
            }
            worker_rounds.fetch_add(1, Ordering::Relaxed);
        }
    });

    let _ = execer.join();
    let _ = worker.join();
    if devnull >= 0 {
        sys::close(devnull as i32);
    }

    let rounds = rounds.load(Ordering::Relaxed);
    let refused = refused.load(Ordering::Relaxed);
    report.check(
        "an oversized argument list is refused",
        refused == 40,
        format!("{} of 40 refused", refused),
    );
    report.check(
        "a thread runs through its sibling's failed execs",
        rounds > 0,
        format!("{} rounds", rounds),
    );
}

/// How long a tick lasts by the monotonic clock, measured over `SPAN` of them
/// with nothing else to run.
///
/// The two clocks are independent: the tick is counted by the timer interrupt
/// and the monotonic clock is a cycle counter, which keeps running whatever
/// the interrupt mask says and whose rate comes from the machine rather than
/// from the tick. This is the rate between them while nothing is holding the
/// timer off.
fn tick_period() -> Duration {
    use crate::sys;
    const SPAN: u64 = 20;
    // Start on an edge, or part of a tick is counted as a whole one. Both
    // ends are read the same way, a call late, so the two errors cancel.
    let entry = sys::tick_count();
    while sys::tick_count() == entry {}
    let first = sys::tick_count();
    let started = Instant::now();
    while sys::tick_count() < first + SPAN {}
    started.elapsed() / SPAN as u32
}

/// A tick has to last as long as the kernel says a tick lasts.
///
/// Every deadline in the kernel is a whole number of timer ticks and the
/// tick's length is asserted rather than measured: `clock_getres` says a
/// hundredth of a second and a sleep of one second waits a hundred ticks.
/// Nothing inside the kernel contradicts a tick that is really 15 ms, and with
/// one every sleep, every poll and select timeout and every scheduling quantum
/// is half as long again in real time as it was asked for. A timer rearmed
/// with a fresh interval from inside its own handler, rather than moved on
/// from the deadline that just passed, is one way to get one: the delivery
/// latency is then added to every period instead of being absorbed by it.
///
/// The reference is the monotonic clock, whose rate does not come from the
/// tick. It is the cycle counter's rate, read out of `cntfrq_el0` on one
/// machine and measured against the interval timer's own countdown on the
/// other, so the two clocks can disagree and this is the disagreement.
///
/// What it cannot catch: a counter whose stated rate is itself wrong, because
/// then the reference is wrong by the same factor and the two agree; a clock
/// that is right on average and arrives in bursts, since this measures twenty
/// ticks and divides; and anything at all about the wall clock, which is this
/// same counter with a date added.
fn a_tick_is_the_length_it_claims(report: &mut Report) {
    use crate::sys;
    // Wide enough to pass on an emulated machine, where the measurement is
    // worth a per cent or two, and far tighter than the 50 per cent a tick
    // that loses its delivery latency every period costs.
    const TOLERANCE: u32 = 10;
    let measured = tick_period();
    let claimed = sys::tick_nanoseconds();
    let off = if claimed == 0 {
        100
    } else {
        let claimed = claimed as i128;
        ((measured.as_nanos() as i128 - claimed).abs() * 100 / claimed) as u32
    };
    println!(
        "      the kernel calls a tick {} us; against the monotonic clock it is {} us",
        claimed / 1000,
        measured.as_micros(),
    );
    report.check(
        "a tick lasts as long as the kernel says it does",
        claimed != 0 && off <= TOLERANCE,
        format!("{} us claimed, {} us measured", claimed / 1000, measured.as_micros()),
    );
}

/// The timer has to keep arriving while another task is inside a long system
/// call. A sleep's deadline is counted in timer ticks, and a tick that finds
/// interrupts masked is delivered late rather than twice, so a call that runs
/// masked from entry to return costs its own length out of every sleep and
/// every timeout in the system that spans it.
///
/// The time that costs is what is measured here, by playing the two clocks
/// against each other: the cycle counter behind CLOCK_MONOTONIC runs whatever
/// the mask says and the tick count does not, so a round that took longer than
/// the ticks it consumed account for is a round with the timer held off for
/// the difference.
///
/// Bounding the wall-clock lateness of the sleeps instead measures something
/// else, which is why it was replaced. A sleep of 100 ms waits ten whole ticks
/// however long a tick turns out to be, so its lateness is ten times the
/// difference between the tick's real length and the tenth of a second it is
/// supposed to be, plus whatever the timer was held off for. The first term
/// settles per boot and swamps the second: on an emulated board a tick ran
/// 14.8 ms and every sleep came back 48 per cent late whatever the mask was
/// doing. `a_tick_is_the_length_it_claims` is where that term is checked now.
fn timer_under_load(report: &mut Report) {
    use crate::sys;
    const ROUNDS: usize = 12;
    // Each sleep spans several of the writes below, so what is measured is
    // the delay they add over a stretch of time rather than whichever part of
    // one of them a shorter sleep happened to overlap.
    const NAP: Duration = Duration::from_millis(100);
    // Each write copies 32 MiB, which is more than two ticks' worth, so with
    // the timer masked for a whole call every round of this length loses one
    // of them: 78 ms in the middle round, measured here against a kernel put
    // back the way it was before an interrupt was let through a system call on
    // aarch64. With the timer getting through, the middle round came to 2.2 ms
    // at worst over ten boots. The bound sits between the two.
    //
    // The middle round decides rather than the worst, which is printed. Not
    // every round is measured against as much of the emulator's own latency as
    // the tick period above was: the worst of ten boots was 9.5 ms where the
    // middle round of the same twelve was 0.4 ms. That lands in the tail. A
    // call that runs masked holds the timer off in every round it spans, so it
    // lands in the middle.
    const BOUND: Duration = Duration::from_millis(30);

    // Two of these in one call: a system call is what has to stay
    // interruptible, and the kernel copies each piece separately, so this is
    // one call that copies twice as much rather than two calls.
    let block = vec![0u8; 16 << 20];
    let mut sink = match std::fs::OpenOptions::new().write(true).open("/dev/null") {
        Ok(sink) => sink,
        Err(err) => {
            report.check("/dev/null opens for writing", false, format!("{}", err));
            return;
        }
    };
    // The kernel copies a write into a buffer of its own, and the first one
    // this large grows the kernel heap, which maps the new pages with the
    // heap locked and so with interrupts off. That is a one-off and is not
    // what is being measured here, so pay it before the clock starts.
    let _ = sink.write_vectored(&[IoSlice::new(&block), IoSlice::new(&block)]);

    // Before the load starts, so that the rate between the two clocks is
    // taken while nothing in the system can be holding the timer off. Taken
    // with the load running it would absorb the very delay this is looking
    // for and every round would measure zero.
    let tick = tick_period();
    // A sleep waits whole ticks of the length the kernel counts deadlines in,
    // which it reports as the resolution of the clock, and not of the length
    // the monotonic clock measured above. A resolution that is not roughly
    // that tick is not the unit the rounds below are counted in, and neither
    // is no resolution at all: either way the count means nothing, so it
    // becomes no ticks due and the check fails rather than passing on a
    // number nothing stands behind.
    let resolution = sys::tick_nanoseconds() as u128;
    let due = if resolution * 2 > tick.as_nanos() && resolution < tick.as_nanos() * 2 {
        (NAP.as_nanos() / resolution) as u64
    } else {
        0
    };

    let stop = Arc::new(AtomicUsize::new(0));
    let running = Arc::clone(&stop);
    let load = std::thread::spawn(move || {
        while running.load(Ordering::Relaxed) == 0 {
            let _ = sink.write_vectored(&[IoSlice::new(&block), IoSlice::new(&block)]);
        }
    });

    let mut held = Vec::with_capacity(ROUNDS);
    let mut slept = Vec::with_capacity(ROUNDS);
    let mut overran = 0;
    for _ in 0..ROUNDS {
        let entry = sys::tick_count();
        let started = Instant::now();
        std::thread::sleep(NAP);
        let took = started.elapsed();
        let ticks = (sys::tick_count() - entry) as u32;
        held.push(took.saturating_sub(tick * ticks));
        slept.push(took.as_micros());
        if ticks as u64 > due {
            overran += 1;
        }
    }
    stop.store(1, Ordering::Relaxed);
    let _ = load.join();

    held.sort();
    slept.sort();
    let middle = held[ROUNDS / 2];
    println!(
        "      {} sleeps of {} ms over a {} us tick: timer held off {} us in the middle \
         round, {} us at worst; the middle sleep itself took {} us",
        ROUNDS,
        NAP.as_millis(),
        tick.as_micros(),
        middle.as_micros(),
        held[ROUNDS - 1].as_micros(),
        slept[ROUNDS / 2],
    );
    report.check(
        "the timer keeps arriving while another task is in a long call",
        middle < BOUND,
        format!(
            "held off {} us in the middle round, {} us at worst",
            middle.as_micros(),
            held[ROUNDS - 1].as_micros()
        ),
    );
    // The other half of a sleep waking on time, and the half that survives the
    // calibration above: the tick it is due on has to be the one it wakes on,
    // which needs the woken task to run before the next tick arrives. One
    // round of the twelve is allowed a tick more, because a tick landing
    // between the count read here and the kernel's own read inside the sleep
    // buys that round a later deadline; that is a phase, not a delay.
    report.check(
        "a sleep wakes on the tick it is due on",
        due > 0 && overran <= 1,
        format!(
            "{} of {} rounds woke later than the {} ticks of {} us they were due",
            overran,
            ROUNDS,
            due,
            resolution / 1000
        ),
    );
}

// ---- interval timers ------------------------------------------------------

const SIGALRM: i32 = 14;
const SIGVTALRM: i32 = 26;
const SIGPROF: i32 = 27;
const SIG_DFL: usize = 0;
const ITIMER_REAL: i32 = 0;
const ITIMER_VIRTUAL: i32 = 1;
const ITIMER_PROF: i32 = 2;

/// `struct itimerval`: the interval, then the value, each in seconds and
/// microseconds.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq)]
struct ItimerVal {
    interval_sec: i64,
    interval_usec: i64,
    value_sec: i64,
    value_usec: i64,
}

impl ItimerVal {
    fn new(value: Duration, interval: Duration) -> ItimerVal {
        ItimerVal {
            interval_sec: interval.as_secs() as i64,
            interval_usec: interval.subsec_micros() as i64,
            value_sec: value.as_secs() as i64,
            value_usec: value.subsec_micros() as i64,
        }
    }

    fn value(&self) -> Duration {
        Duration::from_secs(self.value_sec as u64) + Duration::from_micros(self.value_usec as u64)
    }

    fn interval(&self) -> Duration {
        Duration::from_secs(self.interval_sec as u64)
            + Duration::from_micros(self.interval_usec as u64)
    }
}

extern "C" {
    fn setitimer(which: i32, new: *const ItimerVal, old: *mut ItimerVal) -> i32;
    fn getitimer(which: i32, out: *mut ItimerVal) -> i32;
    fn alarm(seconds: u32) -> u32;
}

static REAL_ALARMS: AtomicUsize = AtomicUsize::new(0);
static VIRTUAL_ALARMS: AtomicUsize = AtomicUsize::new(0);
static PROFILE_ALARMS: AtomicUsize = AtomicUsize::new(0);

extern "C" fn count_timer_signal(signum: i32) {
    let counter = match signum {
        SIGALRM => &REAL_ALARMS,
        SIGVTALRM => &VIRTUAL_ALARMS,
        _ => &PROFILE_ALARMS,
    };
    counter.fetch_add(1, Ordering::SeqCst);
}

/// Set a timer; the call's result, and the setting it replaced.
fn arm_timer(which: i32, value: Duration, interval: Duration) -> (i32, ItimerVal) {
    let new = ItimerVal::new(value, interval);
    let mut old = ItimerVal::default();
    let rc = unsafe { setitimer(which, &new, &mut old) };
    (rc, old)
}

fn read_timer(which: i32) -> (i32, ItimerVal) {
    let mut now = ItimerVal::default();
    let rc = unsafe { getitimer(which, &mut now) };
    (rc, now)
}

/// Wait for a child, killing it if it has not finished within `limit`, so a
/// kernel that never delivers what the child is waiting for fails the check
/// instead of hanging the suite.
///
/// The kill is followed by a continue: a kernel that leaves a stopped thread
/// stopped with the kill pending would otherwise hold the wait below for good,
/// and a continue is what makes such a thread take the kill.
fn wait_or_kill(child: i64, limit: Duration) -> (i64, i32) {
    use crate::sys;
    const WNOHANG: u64 = 1;
    const SIGKILL: i32 = 9;
    const SIGCONT: i32 = 18;
    let started = Instant::now();
    loop {
        let (pid, status) = sys::wait4(child as i32, WNOHANG);
        if pid != 0 {
            return (pid, status);
        }
        if started.elapsed() > limit {
            sys::kill(child as i32, SIGKILL);
            sys::kill(child as i32, SIGCONT);
            return sys::wait4(child as i32, 0);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Run in user mode until `counter` moves off `before` or `limit` has passed,
/// and say for how long.
fn spin_until(counter: &AtomicUsize, before: usize, limit: Duration) -> Duration {
    let started = Instant::now();
    let mut turns = 0u64;
    loop {
        for _ in 0..100_000 {
            turns = std::hint::black_box(turns.wrapping_add(1));
        }
        if counter.load(Ordering::SeqCst) != before || started.elapsed() > limit {
            return started.elapsed();
        }
    }
}

/// setitimer, getitimer and alarm. BusyBox wget bounds each step of a fetch
/// with alarm, and the call used to be missing.
fn interval_timers(report: &mut Report) {
    use crate::sys;
    unsafe {
        let handler = count_timer_signal as extern "C" fn(i32) as usize;
        signal(SIGALRM, handler);
        signal(SIGVTALRM, handler);
        signal(SIGPROF, handler);
    }

    let before = REAL_ALARMS.load(Ordering::SeqCst);
    let started = Instant::now();
    let (rc, _) = arm_timer(ITIMER_REAL, Duration::from_millis(150), Duration::ZERO);
    while REAL_ALARMS.load(Ordering::SeqCst) == before && started.elapsed() < Duration::from_secs(2)
    {
        std::thread::sleep(Duration::from_millis(5));
    }
    let waited = started.elapsed();
    std::thread::sleep(Duration::from_millis(250));
    let fired = REAL_ALARMS.load(Ordering::SeqCst) - before;
    report.check(
        "setitimer raises SIGALRM once, after its time has passed",
        rc == 0
            && fired == 1
            && waited >= Duration::from_millis(150)
            && waited < Duration::from_secs(1),
        format!("setitimer returned {}; {} signals, the first seen after {:?}", rc, fired, waited),
    );

    let (armed, _) = arm_timer(ITIMER_REAL, Duration::from_secs(10), Duration::from_secs(3));
    let (asked, now) = read_timer(ITIMER_REAL);
    report.check(
        "getitimer reports the time left and the interval",
        armed == 0
            && asked == 0
            && now.value() > Duration::from_secs(9)
            && now.value() <= Duration::from_secs(10)
            && now.interval() == Duration::from_secs(3),
        format!("{:?}", now),
    );
    let (disarmed, replaced) = arm_timer(ITIMER_REAL, Duration::ZERO, Duration::ZERO);
    let (_, after) = read_timer(ITIMER_REAL);
    report.check(
        "disarming answers with the setting it replaced and leaves nothing armed",
        disarmed == 0
            && replaced.value() > Duration::from_secs(9)
            && replaced.interval() == Duration::from_secs(3)
            && after == ItimerVal::default(),
        format!("replaced {:?}, then {:?}", replaced, after),
    );

    arm_timer(ITIMER_REAL, Duration::from_secs(10), Duration::ZERO);
    let seen = std::thread::spawn(|| read_timer(ITIMER_REAL).1).join().unwrap_or_default();
    arm_timer(ITIMER_REAL, Duration::ZERO, Duration::ZERO);
    report.check(
        "a thread sees the timer its process armed",
        seen.value() > Duration::from_secs(9),
        format!("{:?}", seen),
    );

    let before = REAL_ALARMS.load(Ordering::SeqCst);
    let (rc, _) = arm_timer(ITIMER_REAL, Duration::from_millis(50), Duration::from_millis(50));
    std::thread::sleep(Duration::from_millis(525));
    arm_timer(ITIMER_REAL, Duration::ZERO, Duration::ZERO);
    let fired = REAL_ALARMS.load(Ordering::SeqCst) - before;
    std::thread::sleep(Duration::from_millis(150));
    let later = REAL_ALARMS.load(Ordering::SeqCst) - before;
    report.check(
        "an interval timer fires every interval until it is disarmed",
        // Ten is what a sleep that ends on time sees. A stall on the host
        // lengthens the sleep, which lets one more fire, and makes a tick late,
        // which skips an interval rather than firing it twice.
        rc == 0 && (5..=12).contains(&fired) && later == fired,
        format!("{} signals in 525 ms, {} after disarming", fired, later),
    );

    let first = unsafe { alarm(7) };
    let second = unsafe { alarm(0) };
    report.check(
        "alarm answers with the seconds left on the alarm it replaced",
        first == 0 && second == 7,
        format!("{} and then {}", first, second),
    );
    #[cfg(target_arch = "x86_64")]
    {
        let first = sys::alarm_call(3);
        let second = sys::alarm_call(0);
        report.check(
            "and so does the alarm system call x86-64 has of its own",
            first == 0 && second == 3,
            format!("{} and then {}", first, second),
        );
    }

    let child = sys::fork();
    if child == 0 {
        unsafe { signal(SIGALRM, SIG_DFL) };
        let Ok((read_end, _write_end)) = sys::pipe() else {
            sys::exit_group(2)
        };
        arm_timer(ITIMER_REAL, Duration::from_millis(100), Duration::ZERO);
        // The write end is open in this process, so nothing ends this read
        // but a signal.
        let mut byte = [0u8; 1];
        sys::read(read_end, &mut byte);
        sys::exit_group(3);
    }
    let started = Instant::now();
    let (pid, status) = wait_or_kill(child, Duration::from_secs(3));
    let waited = started.elapsed();
    report.check(
        "SIGALRM ends a process blocked in a read when nothing handles it",
        pid == child && status & 0x7F == SIGALRM && waited < Duration::from_millis(1500),
        format!("reaped {} with status {:#x} after {:?}", pid, status, waited),
    );

    arm_timer(ITIMER_REAL, Duration::from_secs(10), Duration::ZERO);
    let child = sys::fork();
    if child == 0 {
        let (rc, now) = read_timer(ITIMER_REAL);
        sys::exit_group(if rc == 0 && now == ItimerVal::default() { 0 } else { 1 });
    }
    let (pid, status) = wait_or_kill(child, Duration::from_secs(3));
    let (_, parent) = arm_timer(ITIMER_REAL, Duration::ZERO, Duration::ZERO);
    report.check(
        "a forked child starts with no timer armed, and its parent keeps its own",
        pid == child && status == 0 && parent.value() > Duration::from_secs(9),
        format!("child status {:#x}; the parent's had {:?} left", status, parent.value()),
    );

    let before = VIRTUAL_ALARMS.load(Ordering::SeqCst);
    let (rc, _) = arm_timer(ITIMER_VIRTUAL, Duration::from_millis(50), Duration::ZERO);
    std::thread::sleep(Duration::from_millis(300));
    let while_asleep = VIRTUAL_ALARMS.load(Ordering::SeqCst) - before;
    let spun = spin_until(&VIRTUAL_ALARMS, before, Duration::from_secs(3));
    let fired = VIRTUAL_ALARMS.load(Ordering::SeqCst) - before;
    report.check(
        "a virtual timer counts time spent running, not time asleep",
        rc == 0 && while_asleep == 0 && fired == 1,
        format!(
            "setitimer returned {}; {} signals while asleep, {} after running for {:?}",
            rc, while_asleep, fired, spun
        ),
    );

    let before = PROFILE_ALARMS.load(Ordering::SeqCst);
    let (rc, _) = arm_timer(ITIMER_PROF, Duration::from_millis(50), Duration::ZERO);
    let spun = spin_until(&PROFILE_ALARMS, before, Duration::from_secs(3));
    let fired = PROFILE_ALARMS.load(Ordering::SeqCst) - before;
    report.check(
        "a profiling timer counts time spent running",
        rc == 0 && fired == 1,
        format!("setitimer returned {}; {} signals after running for {:?}", rc, fired, spun),
    );

    let quiet = ItimerVal::default();
    let no_such = unsafe { setitimer(3, &quiet, std::ptr::null_mut()) };
    let no_such_errno = std::io::Error::last_os_error().raw_os_error();
    let too_many = ItimerVal { value_usec: 1_000_000, ..ItimerVal::default() };
    let bad = unsafe { setitimer(ITIMER_REAL, &too_many, std::ptr::null_mut()) };
    let bad_errno = std::io::Error::last_os_error().raw_os_error();
    report.check(
        "setitimer refuses a timer that does not exist, and a second's worth of microseconds",
        no_such == -1 && no_such_errno == Some(22) && bad == -1 && bad_errno == Some(22),
        format!("{} ({:?}) and {} ({:?})", no_such, no_such_errno, bad, bad_errno),
    );

    unsafe {
        signal(SIGALRM, SIG_DFL);
        signal(SIGVTALRM, SIG_DFL);
        signal(SIGPROF, SIG_DFL);
    }
}

static STRESS_PROFILE: AtomicUsize = AtomicUsize::new(0);
static STRESS_RAISED: AtomicUsize = AtomicUsize::new(0);

extern "C" fn count_stress_signal(signum: i32) {
    if signum == SIGPROF {
        STRESS_PROFILE.fetch_add(1, Ordering::SeqCst);
    } else {
        STRESS_RAISED.fetch_add(1, Ordering::SeqCst);
    }
}

/// A signal the timer tick raises, arriving while the same process raises and
/// takes another signal as fast as it can.
///
/// The tick adds its signal to the pending set from an interrupt, and a system
/// call on its way out takes signals off the same set. When the set was changed
/// by reading it, changing the copy and writing the copy back, a tick between
/// the read and the write had its signal written over and lost. With the
/// profiling timer at its shortest, every tick this process runs through raises
/// SIGPROF once, so the tick count says how many to expect; the ticks other
/// tasks take are the allowance. `raise` takes SIGUSR2 through three system
/// calls, each of which takes pending signals on its way out.
///
/// An emulator that looks for interrupts only between the blocks of code it
/// translates cannot put a tick between two instructions with no branch between
/// them, which is what the old read and write were, so there this passes with
/// the old code as well. A processor takes an interrupt between any two.
fn signals_raised_while_others_are_taken(report: &mut Report) {
    use crate::sys;
    // The handlers in place before are put back afterwards: checks further on
    // raise SIGUSR2 in a forked child and count on the handler installed at
    // the start.
    let (earlier_profile, earlier_user) = unsafe {
        let handler = count_stress_signal as extern "C" fn(i32) as usize;
        (signal(SIGPROF, handler), signal(SIGUSR2, handler))
    };
    let profile_before = STRESS_PROFILE.load(Ordering::SeqCst);
    let taken_before = STRESS_RAISED.load(Ordering::SeqCst);
    let first_tick = sys::tick_count();
    arm_timer(ITIMER_PROF, Duration::from_millis(1), Duration::from_millis(1));
    let started = Instant::now();
    let mut raised = 0usize;
    while started.elapsed() < Duration::from_secs(2) {
        for _ in 0..64 {
            unsafe { raise(SIGUSR2) };
            raised += 1;
        }
    }
    arm_timer(ITIMER_PROF, Duration::ZERO, Duration::ZERO);
    let last_tick = sys::tick_count();
    unsafe {
        signal(SIGPROF, earlier_profile);
        signal(SIGUSR2, earlier_user);
    }
    let taken = STRESS_RAISED.load(Ordering::SeqCst) - taken_before;
    let profiled = STRESS_PROFILE.load(Ordering::SeqCst) - profile_before;
    let ticks = last_tick.saturating_sub(first_tick) as usize;
    println!("      {} raised, {} taken; {} ticks, {} SIGPROF", raised, taken, ticks, profiled);
    report.check(
        "every signal raised is taken once while the tick raises another",
        taken == raised,
        format!("{} raised, {} taken", raised, taken),
    );
    report.check(
        "and every tick the process ran through raised SIGPROF",
        // The ticks between reading the count and arming the timer, and
        // between disarming it and reading the count again, raise nothing.
        // A tick another task is running through raises nothing either; in
        // the runs this was written against that was none of them.
        profiled <= ticks && profiled + 3 >= ticks,
        format!("{} ticks, {} SIGPROF", ticks, profiled),
    );
}

/// Setting the wall clock, which is what BusyBox ntpd does on a board with no
/// clock of its own, and the things that must not move when it is set.
///
/// The clock is put back at the end to where it would have been had nothing
/// here set it, because the sections after this one read the wall clock too.
fn setting_the_wall_clock(report: &mut Report) {
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct Timespec {
        tv_sec: i64,
        tv_nsec: i64,
    }
    #[repr(C)]
    struct Timeval {
        tv_sec: i64,
        tv_usec: i64,
    }
    extern "C" {
        fn clock_gettime(clock: i32, out: *mut Timespec) -> i32;
        fn clock_settime(clock: i32, value: *const Timespec) -> i32;
        fn settimeofday(value: *const Timeval, zone: *const u8) -> i32;
        fn clock_nanosleep(clock: i32, flags: i32, wake: *const Timespec, left: *mut Timespec) -> i32;
        fn syscall(number: i64, ...) -> i64;
    }
    const CLOCK_REALTIME: i32 = 0;
    const CLOCK_MONOTONIC: i32 = 1;
    const TIMER_ABSTIME: i32 = 1;
    const EINVAL: i32 = 22;
    const NS: i128 = 1_000_000_000;
    const HOUR: i128 = 3600 * NS;
    // How far a reading taken just after a set may be from the value set: the
    // calls in between, and a tick of another task running.
    const SLACK: i128 = 250_000_000;
    // musl makes its settimeofday out of clock_settime, so the call of this
    // name is only reached by number.
    #[cfg(target_arch = "x86_64")]
    const SYS_SETTIMEOFDAY: i64 = 164;
    #[cfg(target_arch = "aarch64")]
    const SYS_SETTIMEOFDAY: i64 = 170;

    fn read(clock: i32) -> i128 {
        let mut value = Timespec::default();
        unsafe { clock_gettime(clock, &mut value) };
        value.tv_sec as i128 * NS + value.tv_nsec as i128
    }
    fn spec(ns: i128) -> Timespec {
        Timespec { tv_sec: ns.div_euclid(NS) as i64, tv_nsec: ns.rem_euclid(NS) as i64 }
    }
    fn errno() -> i32 {
        std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
    }
    /// clock_settime with the fields as given: the result, and errno if it failed.
    fn set(clock: i32, value: Timespec) -> (i32, i32) {
        let rc = unsafe { clock_settime(clock, &value) };
        (rc, if rc == 0 { 0 } else { errno() })
    }
    /// The wall clock reads `target`, give or take the calls since it was set.
    fn reads(target: i128) -> (bool, i128) {
        let off = read(CLOCK_REALTIME) - target;
        (off >= 0 && off < SLACK, off)
    }

    let real_start = read(CLOCK_REALTIME);
    let mono_start = read(CLOCK_MONOTONIC);
    // What the wall clock would read now had nothing here set it.
    let undisturbed = || real_start + (read(CLOCK_MONOTONIC) - mono_start);

    let mono_before = read(CLOCK_MONOTONIC);
    let target = undisturbed() + HOUR;
    let (rc, err) = set(CLOCK_REALTIME, spec(target));
    let (close, off) = reads(target);
    let mono_after = read(CLOCK_MONOTONIC);
    report.check(
        "clock_settime moves CLOCK_REALTIME",
        rc == 0 && close,
        format!("rc {} errno {}; reads {} ns from the time set", rc, err, off),
    );
    report.check(
        "CLOCK_MONOTONIC runs straight through a step",
        mono_after >= mono_before && mono_after - mono_before < SLACK,
        format!("moved {} ns across the call", mono_after - mono_before),
    );

    // 2001: earlier than the dates on the ram disk, which the clock is held
    // above only while the machine boots.
    let target = 1_000_000_000 * NS;
    let (rc, err) = set(CLOCK_REALTIME, spec(target));
    let (close, off) = reads(target);
    report.check(
        "a time before the boot floor is taken",
        rc == 0 && close,
        format!("rc {} errno {}; reads {} ns from the time set", rc, err, off),
    );

    // Each refusal names a time far from the present, so one that was taken
    // shows as the clock moving as well as a wrong result.
    let real_before = read(CLOCK_REALTIME);
    let refusals = [
        ("CLOCK_MONOTONIC", set(CLOCK_MONOTONIC, spec(2_000_000_000 * NS))),
        (
            "a billion nanoseconds",
            set(CLOCK_REALTIME, Timespec { tv_sec: 2_000_000_000, tv_nsec: 1_000_000_000 }),
        ),
        ("negative nanoseconds", set(CLOCK_REALTIME, Timespec { tv_sec: 2_000_000_000, tv_nsec: -1 })),
    ];
    let moved = read(CLOCK_REALTIME) - real_before;
    let wrong: Vec<String> = refusals
        .iter()
        .filter(|(_, result)| *result != (-1, EINVAL))
        .map(|(name, result)| format!("{}: {:?}", name, result))
        .collect();
    report.check(
        "clock_settime refuses another clock and a bad tv_nsec with EINVAL",
        wrong.is_empty() && moved >= 0 && moved < SLACK,
        format!("{:?}; the clock moved {} ns", wrong, moved),
    );

    let target = undisturbed() + 2 * HOUR;
    let value = Timeval { tv_sec: (target / NS) as i64, tv_usec: (target % NS / 1000) as i64 };
    let target = value.tv_sec as i128 * NS + value.tv_usec as i128 * 1000;
    let rc = unsafe { settimeofday(&value, std::ptr::null()) };
    let err = if rc == 0 { 0 } else { errno() };
    let (close, off) = reads(target);
    report.check(
        "settimeofday sets the wall clock",
        rc == 0 && close,
        format!("rc {} errno {}; reads {} ns from the time set", rc, err, off),
    );

    let target = undisturbed() + 3 * HOUR;
    let value = Timeval { tv_sec: (target / NS) as i64, tv_usec: (target % NS / 1000) as i64 };
    let target = value.tv_sec as i128 * NS + value.tv_usec as i128 * 1000;
    let rc = unsafe { syscall(SYS_SETTIMEOFDAY, &value as *const Timeval, 0usize) };
    let err = if rc == 0 { 0 } else { errno() };
    let (close, off) = reads(target);
    let bad = Timeval { tv_sec: 2_000_000_000, tv_usec: 1_000_000 };
    let rc_bad = unsafe { syscall(SYS_SETTIMEOFDAY, &bad as *const Timeval, 0usize) };
    let err_bad = errno();
    let (still, _) = reads(target);
    report.check(
        "the settimeofday system call sets the clock and refuses a second of microseconds",
        rc == 0 && close && rc_bad == -1 && err_bad == EINVAL && still,
        format!("rc {} errno {}, off {} ns; bad: rc {} errno {}", rc, err, off, rc_bad, err_bad),
    );

    let wake = read(CLOCK_MONOTONIC) + 200_000_000;
    let started = Instant::now();
    let rc = unsafe { clock_nanosleep(CLOCK_MONOTONIC, TIMER_ABSTIME, &spec(wake), std::ptr::null_mut()) };
    let slept = started.elapsed();
    report.check(
        "clock_nanosleep to a CLOCK_MONOTONIC reading",
        rc == 0 && slept >= Duration::from_millis(150) && slept < Duration::from_secs(1),
        format!("rc {} after {:?}", rc, slept),
    );

    // A thread sleeps until the wall clock reads `ahead` from now, and the
    // clock is moved by `step` while it sleeps. The sleep's length is measured
    // on the monotonic clock.
    let across_a_step = |ahead: i128, step: i128| -> (i32, Duration) {
        let wake = read(CLOCK_REALTIME) + ahead;
        let started = Instant::now();
        let sleeper = std::thread::spawn(move || unsafe {
            clock_nanosleep(CLOCK_REALTIME, TIMER_ABSTIME, &spec(wake), std::ptr::null_mut())
        });
        std::thread::sleep(Duration::from_millis(300));
        let _ = set(CLOCK_REALTIME, spec(read(CLOCK_REALTIME) + step));
        let rc = sleeper.join().unwrap_or(-1);
        (rc, started.elapsed())
    };
    let (rc, slept) = across_a_step(5 * NS, HOUR);
    report.check(
        "a sleep to a wall-clock time the clock is stepped past ends at the step",
        rc == 0 && slept >= Duration::from_millis(250) && slept < Duration::from_secs(2),
        format!("rc {} after {:?}, where the sleep asked for 5 s", rc, slept),
    );
    let (rc, slept) = across_a_step(NS, -NS);
    report.check(
        "a sleep to a wall-clock time the clock is stepped back from runs on",
        rc == 0 && slept >= Duration::from_millis(1800) && slept < Duration::from_secs(3),
        format!("rc {} after {:?}, where 2 s was due", rc, slept),
    );

    let sleeper = std::thread::spawn(|| {
        let started = Instant::now();
        std::thread::sleep(Duration::from_secs(1));
        started.elapsed()
    });
    std::thread::sleep(Duration::from_millis(200));
    let _ = set(CLOCK_REALTIME, spec(read(CLOCK_REALTIME) - HOUR));
    let slept = sleeper.join().unwrap_or_default();
    report.check(
        "a one-second sleep lasts a second across a step back",
        slept >= Duration::from_millis(950) && slept < Duration::from_millis(1500),
        format!("{:?}", slept),
    );

    let (rc, err) = set(CLOCK_REALTIME, spec(undisturbed()));
    let off = read(CLOCK_REALTIME) - undisturbed();
    report.check(
        "the wall clock is put back",
        rc == 0 && off.abs() < SLACK,
        format!("rc {} errno {}; {} ns from where it would have been", rc, err, off),
    );
}

pub fn main(_args: &[String]) -> i32 {
    let mut report = Report { passed: 0, failed: 0 };
    println!("=== Rust standard library on claudeos ===");
    println!();

    println!("-- memory --");
    let mut big: Vec<u64> = (0..2_000_000u64).collect();
    let sum: u64 = big.iter().sum();
    report.check("64 MiB vector", sum == 1_999_999 * 2_000_000 / 2, format!("sum {}", sum));

    big.reverse();
    big.sort_unstable();
    report.check("sort 2M elements", big[0] == 0 && big[1_999_999] == 1_999_999, "order".into());
    drop(big);

    let mut map: HashMap<String, usize> = HashMap::new();
    for i in 0..20_000 {
        map.insert(format!("key-{}", i), i);
    }
    report.check(
        "hash map with 20k entries",
        map.len() == 20_000 && map.get("key-19999") == Some(&19_999),
        format!("len {}", map.len()),
    );

    let mut growth: Vec<Vec<u8>> = Vec::new();
    for i in 0..64 {
        growth.push(vec![i as u8; 1 << 20]);
    }
    let spot = growth[40][12345];
    report.check("64 one-MiB allocations", spot == 40, format!("byte {}", spot));
    drop(growth);

    println!();
    println!("-- threads --");
    let counter = Arc::new(AtomicUsize::new(0));
    let mut handles = Vec::new();
    for _ in 0..8 {
        let counter = Arc::clone(&counter);
        handles.push(std::thread::spawn(move || {
            for _ in 0..10_000 {
                counter.fetch_add(1, Ordering::Relaxed);
            }
        }));
    }
    for handle in handles {
        let _ = handle.join();
    }
    let total = counter.load(Ordering::Relaxed);
    report.check("8 threads, atomic counter", total == 80_000, format!("counter {}", total));

    let shared = Arc::new(Mutex::new(Vec::new()));
    let mut handles = Vec::new();
    for id in 0..4 {
        let shared = Arc::clone(&shared);
        handles.push(std::thread::spawn(move || {
            for n in 0..100 {
                shared.lock().unwrap().push(id * 100 + n);
            }
            id
        }));
    }
    let ids: Vec<i32> = handles.into_iter().filter_map(|h| h.join().ok()).collect();
    let collected = shared.lock().unwrap().len();
    report.check(
        "mutex shared between threads",
        collected == 400 && ids.len() == 4,
        format!("{} items, {} joins", collected, ids.len()),
    );

    let (sender, receiver) = mpsc::channel();
    let producer = std::thread::spawn(move || {
        for i in 0..1000 {
            if sender.send(i).is_err() {
                break;
            }
        }
    });
    let received: i64 = receiver.iter().sum();
    let _ = producer.join();
    report.check("channel between threads", received == 499_500, format!("sum {}", received));

    let start = Instant::now();
    std::thread::sleep(Duration::from_millis(120));
    let slept = start.elapsed();
    report.check(
        "thread sleep and monotonic clock",
        slept >= Duration::from_millis(90),
        format!("{:?}", slept),
    );

    println!();
    println!("-- the clock --");
    a_tick_is_the_length_it_claims(&mut report);
    timer_under_load(&mut report);

    println!();
    println!("-- files --");
    let path = "/tmp/rtest.dat";
    let payload: Vec<u8> = (0..=255u8).cycle().take(100_000).collect();
    let write_result = std::fs::write(path, &payload);
    report.check("write 100 KB", write_result.is_ok(), format!("{:?}", write_result));

    let read_back = std::fs::read(path).unwrap_or_default();
    report.check(
        "read it back",
        read_back == payload,
        format!("{} bytes", read_back.len()),
    );

    let metadata = std::fs::metadata(path);
    report.check(
        "metadata length",
        metadata.as_ref().map(|m| m.len()).unwrap_or(0) == 100_000,
        format!("{:?}", metadata.map(|m| m.len())),
    );

    let seek_result = (|| -> std::io::Result<u8> {
        let mut file = std::fs::File::open(path)?;
        file.seek(SeekFrom::Start(1000))?;
        let mut byte = [0u8; 1];
        file.read_exact(&mut byte)?;
        Ok(byte[0])
    })();
    report.check(
        "seek and read",
        seek_result.as_ref().copied().unwrap_or(0) == payload[1000],
        format!("{:?}", seek_result),
    );

    let append_result = (|| -> std::io::Result<u64> {
        let mut file = std::fs::OpenOptions::new().append(true).open(path)?;
        file.write_all(b"tail")?;
        Ok(std::fs::metadata(path)?.len())
    })();
    report.check(
        "append",
        append_result.as_ref().copied().unwrap_or(0) == 100_004,
        format!("{:?}", append_result),
    );

    let _ = std::fs::create_dir_all("/tmp/rtest-dir/nested");
    let _ = std::fs::write("/tmp/rtest-dir/a", b"a");
    let _ = std::fs::write("/tmp/rtest-dir/b", b"b");
    let entries = std::fs::read_dir("/tmp/rtest-dir")
        .map(|d| d.filter_map(|e| e.ok()).count())
        .unwrap_or(0);
    report.check("directory listing", entries == 3, format!("{} entries", entries));

    let _ = std::fs::remove_file(path);
    report.check("remove", std::fs::metadata(path).is_err(), "still present".into());
    let _ = std::fs::remove_dir_all("/tmp/rtest-dir");

    a_position_no_file_has(&mut report);

    println!();
    println!("-- processes --");
    let output = std::process::Command::new("/bin/echo")
        .arg("spawned")
        .output();
    let text = output
        .as_ref()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    report.check(
        "Command::output captures stdout",
        text == "spawned",
        format!("{:?}", text),
    );

    let status = std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg("exit 5")
        .status();
    report.check(
        "child exit status",
        status.as_ref().ok().and_then(|s| s.code()) == Some(5),
        format!("{:?}", status),
    );

    let piped = std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg("seq 1 5 | wc -l")
        .output();
    let count = piped
        .as_ref()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    report.check("child runs a pipeline", count == "5", format!("{:?}", count));

    println!();
    println!("-- signals --");
    unsafe {
        let handler = handle_signal as extern "C" fn(i32) as usize;
        signal(SIGUSR1, handler);
        signal(SIGUSR2, handler);
    }
    let before = SIGNAL_TOTAL.load(Ordering::SeqCst);
    unsafe {
        raise(SIGUSR1);
        raise(SIGUSR2);
    }
    // The handler runs on the way out of a system call.
    for _ in 0..4 {
        std::thread::yield_now();
    }
    let delivered = SIGNAL_TOTAL.load(Ordering::SeqCst) - before;
    report.check(
        "handlers run for two signals",
        delivered == (SIGUSR1 + SIGUSR2) as usize,
        format!("sum {}", delivered),
    );

    // Execution has to continue normally after the handler returns, which
    // means rt_sigreturn restored the interrupted state.
    let mut accumulator = 0u64;
    for i in 0..1000u64 {
        accumulator = accumulator.wrapping_add(i * i);
    }
    report.check(
        "execution resumes after a handler",
        accumulator == 332_833_500,
        format!("{}", accumulator),
    );

    unsafe {
        signal(SIGUSR1, SIG_IGN);
        raise(SIGUSR1);
    }
    for _ in 0..2 {
        std::thread::yield_now();
    }
    report.check(
        "ignored signal is dropped",
        SIGNAL_TOTAL.load(Ordering::SeqCst) - before == (SIGUSR1 + SIGUSR2) as usize,
        "handler ran while ignored".into(),
    );
    a_blocked_signal_waits_to_be_unblocked(&mut report);

    println!();
    println!("-- interval timers --");
    interval_timers(&mut report);
    signals_raised_while_others_are_taken(&mut report);

    println!();
    println!("-- time and environment --");
    let now = SystemTime::now().duration_since(UNIX_EPOCH);
    report.check(
        "wall clock is set",
        now.as_ref().map(|d| d.as_secs()).unwrap_or(0) > 1_600_000_000,
        format!("{:?}", now.map(|d| d.as_secs())),
    );

    let path_var = std::env::var("PATH").unwrap_or_default();
    report.check("PATH inherited", path_var.contains("/bin"), path_var.clone());

    let cwd = std::env::current_dir();
    report.check("current directory", cwd.is_ok(), format!("{:?}", cwd));

    let args: Vec<String> = std::env::args().collect();
    report.check("argv[0] present", !args.is_empty(), format!("{:?}", args));

    println!();
    println!("-- setting the wall clock --");
    setting_the_wall_clock(&mut report);

    println!();
    println!("-- system call numbers --");
    absent_numbers(&mut report);
    reboot_refuses_what_it_does_not_know(&mut report);

    println!();
    println!("-- waiting on several things at once --");
    event_and_poll(&mut report);

    println!();
    println!("-- threads, processes and waiting --");
    thread_of_a_child_is_not_a_child(&mut report);
    a_child_that_aborts_is_the_one_signalled(&mut report);
    an_exit_from_a_thread_is_the_process_status(&mut report);
    a_signal_from_outside_is_still_reported(&mut report);
    a_first_thread_that_exits_alone_is_not_the_process(&mut report);
    a_thread_that_aborts_ends_its_process(&mut report);
    a_thread_that_takes_sigsegv_ends_its_process(&mut report);
    a_kill_ends_every_thread(&mut report);
    a_signal_from_outside_ends_every_thread(&mut report);
    a_stopped_process_with_threads_is_killed(&mut report);
    a_signal_for_the_process_reaches_a_thread_that_takes_it(&mut report);
    a_child_exit_reaches_a_blocked_parent(&mut report);
    what_a_wait_does_with_signals(&mut report);
    a_continued_job_has_no_stop_to_report(&mut report);
    waiting_on_a_process_group(&mut report);
    a_child_finds_its_own_proc_entry(&mut report);
    a_fork_while_a_sibling_writes(&mut report);
    reading_proc_while_a_child_is_reaped(&mut report);
    a_signal_frame_on_a_shared_page(&mut report);
    a_handler_with_no_restorer(&mut report);
    stopping_a_job_reaches_the_parent(&mut report);
    a_number_no_signal_has(&mut report);
    a_signal_ends_a_sleep(&mut report);
    failed_exec_and_siblings(&mut report);

    println!();
    println!("=== {} passed, {} failed ===", report.passed, report.failed);
    if report.failed == 0 {
        0
    } else {
        1
    }
}
