//! Exercises the parts of the Rust standard library that lean hardest on the
//! kernel: threads, synchronisation, subprocesses, files and large heaps.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom, Write};
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
    println!("-- waiting on several things at once --");
    event_and_poll(&mut report);

    println!();
    println!("-- kernel races --");
    failed_exec_and_siblings(&mut report);

    println!();
    println!("=== {} passed, {} failed ===", report.passed, report.failed);
    if report.failed == 0 {
        0
    } else {
        1
    }
}
