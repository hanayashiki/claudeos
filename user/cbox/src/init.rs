//! pid 1: bring the system up, start a shell, and reap orphans.

use crate::sys;
use std::io::Write;
use std::path::Path;
use std::time::{Duration, Instant};

pub fn main(args: &[String]) -> i32 {
    println!();
    println!("  claudeos");
    println!("  a Rust operating system that runs Linux binaries");
    println!();

    for dir in ["/tmp", "/root", "/var", "/var/log", "/usr", "/usr/bin"] {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::env::set_current_dir("/root");

    start_time_keeper();

    // An argument from the kernel command line names a script to run instead
    // of a shell.
    let script: Option<&String> = args.iter().skip(1).find(|a| !a.starts_with('-'));

    // What a shell ending on its own does. On a board the machine is reached
    // only through this shell, over the serial cable or the telnet console, and
    // a board that powered off stays off until someone unplugs it, so the
    // default is to start a new shell. `shell_exit=poweroff` on the kernel
    // command line, which reaches this process as its environment, keeps the
    // older behaviour for an emulator, where leaving the shell is how a session
    // ends.
    let power_off_on_exit = std::env::var("shell_exit").map(|v| v == "poweroff").unwrap_or(false);

    // Shells that died on a signal in a row, each soon after it started.
    let mut deaths = 0;
    loop {
        let started = std::time::Instant::now();
        let child = sys::fork();
        if child == 0 {
            // New process group, and make it the terminal's foreground group.
            sys::setpgid(0, 0);
            sys::set_foreground_group(sys::getpid() as i32);
            let argv: Vec<String> = match script {
                Some(path) => vec!["/bin/sh".into(), path.clone()],
                None => vec!["/bin/sh".into()],
            };
            let envp: Vec<String> = std::env::vars().map(|(k, v)| format!("{}={}", k, v)).collect();
            sys::execve("/bin/sh", &argv, &envp);
            eprintln!("init: cannot start /bin/sh");
            sys::exit_group(1);
        }
        if child < 0 {
            eprintln!("init: fork failed");
            return 1;
        }

        // Reap everything; stop when the shell itself is gone. The time keeper
        // is a child too, and it runs for good, so it is reaped here only if it
        // dies; the ntpd runs it starts are its own children and never reach
        // this loop.
        loop {
            let (pid, status) = sys::wait4(-1, 0);
            if pid < 0 {
                break;
            }
            if pid == child {
                let code = sys::exit_code_of(status);
                if script.is_some() {
                    println!("init: script finished with status {}", code);
                    return code;
                }
                match sys::signal_of(status) {
                    // A shell that exited on its own is the user leaving,
                    // whatever status it reports; `exit` after a failed
                    // command still is.
                    None if power_off_on_exit => {
                        println!("init: session ended");
                        return 0;
                    }
                    None => {
                        println!("init: shell exited; starting a new one");
                        deaths = 0;
                    }
                    Some(signal) => {
                        // A shell that ran a while before it died is one
                        // death, not the start of a loop.
                        if started.elapsed() > std::time::Duration::from_secs(10) {
                            deaths = 0;
                        }
                        deaths += 1;
                        if deaths > 3 {
                            if power_off_on_exit {
                                println!("init: shell keeps dying (signal {}); giving up", signal);
                                return code;
                            }
                            // With no shell there is no way to type `reboot`,
                            // and powering off would leave the board off, so
                            // restart the machine. Booted over the network,
                            // that also fetches whatever build is served now.
                            println!(
                                "init: shell keeps dying (signal {}); restarting the machine",
                                signal
                            );
                            sys::reboot(sys::REBOOT_MAGIC1, sys::REBOOT_MAGIC2, sys::REBOOT_CMD_RESTART);
                            eprintln!("init: the kernel refused to restart; starting a new shell");
                            deaths = 0;
                        } else {
                            println!("init: shell died on signal {}; restarting", signal);
                        }
                    }
                }
                break;
            }
        }
    }
}

/// The servers BusyBox ntpd asks are named in this file, and an image that has
/// it wants the clock kept.
const NTP_CONF: &str = "/etc/ntp.conf";
const BUSYBOX: &str = "/bin/busybox";

/// Where ntpd's output goes, and the keeper's one line about each run.
const NTP_LOG: &str = "/var/log/ntpd.log";

/// After a run that ended well, the wait until the next. `ntpd -q` exits 0
/// both when it stepped the clock and when it found the clock within a second
/// and left it alone. A board's crystal keeps to some tens of parts per
/// million, which is a second or two over six hours, so the clock is back
/// around the second `ntpd -q` does not correct by the time it is asked again.
const RESYNC: Duration = Duration::from_secs(6 * 3600);

/// After a run that failed, the wait before the next starts at `RETRY_FIRST`
/// and doubles with each failure in a row, up to `RETRY_CAP`. The usual failure
/// at boot is a network that is not up yet: a WiFi join and a DHCP lease take
/// tens of seconds, so the first retry comes soon. The cap means a board whose
/// network comes up late still has the date within ten minutes of it, and a
/// board with no network at all runs ntpd, which gives up after about ten
/// seconds, once every ten minutes.
const RETRY_FIRST: Duration = Duration::from_secs(30);
const RETRY_CAP: Duration = Duration::from_secs(600);

/// The longest one run may take before it is killed and counted as a failure.
/// BusyBox 1.36.1's `ntpd -q` ends itself with SIGALRM ten seconds after it
/// starts if no server has answered, and fifty seconds after the first answer.
/// It only notices the alarm between name lookups, and its source puts a
/// lookup that gets no answer at about ten seconds for each of the two servers,
/// so a run finishes on its own inside about ninety seconds. Three minutes is
/// that with room, for a build whose ntpd has no alarm of its own, and it is
/// short against the retry cap.
const RUN_LIMIT: Duration = Duration::from_secs(180);

/// How often a running ntpd is checked on. It only bounds how late the keeper
/// notices the end of a run.
const RUN_POLL: Duration = Duration::from_secs(1);

const SIGKILL: i32 = 9;

/// Start the process that keeps the clock set, if this image has what it
/// needs, and return at once: the shell does not wait on the network.
///
/// The keeper is a child of init in a process group of its own. The terminal's
/// interrupt, quit and suspend keys signal the foreground group, and the shell
/// only ever hands the terminal to itself and its jobs, so no key typed at the
/// console reaches the keeper or the ntpd it runs. Both sides of the fork set
/// the group, so it is set by the time `fork` returns here, whichever side the
/// scheduler runs first, and init starts the shell only after that.
fn start_time_keeper() {
    if !Path::new(NTP_CONF).exists() || !Path::new(BUSYBOX).exists() {
        return;
    }
    let child = sys::fork();
    if child == 0 {
        sys::setpgid(0, 0);
        keep_time();
    }
    if child > 0 {
        sys::setpgid(child as i32, child as i32);
    }
}

/// Run ntpd, wait, run it again, for good.
fn keep_time() -> ! {
    // Standard output and error go to the log and standard input is /dev/null,
    // for the keeper and for every ntpd it starts, which inherit them. The
    // console is the user's terminal and the test harness reads it, so nothing
    // here writes a byte to it or reads a key from it.
    let log = sys::open(NTP_LOG, sys::O_WRONLY | sys::O_CREAT | sys::O_APPEND, 0o644);
    let null = sys::open("/dev/null", sys::O_RDONLY, 0);
    if log < 0 || null < 0 {
        sys::exit_group(1);
    }
    sys::dup2(null as i32, sys::STDIN);
    sys::dup2(log as i32, sys::STDOUT);
    sys::dup2(log as i32, sys::STDERR);
    sys::close(null as i32);
    sys::close(log as i32);

    let mut failures = 0u32;
    loop {
        let started = Instant::now();
        let run = run_ntpd();
        let wait = if matches!(run, Run::Exited(0)) {
            failures = 0;
            RESYNC
        } else {
            failures += 1;
            retry_wait(failures)
        };
        // Not `println!`, which panics when the write fails: a log on a full
        // RAM filesystem would then end the keeper, and the clock with it.
        let _ = writeln!(
            std::io::stdout(),
            "timekeeper: {}s after boot: ntpd {} after {}s; next run in {}s",
            seconds_since_boot(),
            run,
            started.elapsed().as_secs(),
            wait.as_secs()
        );
        std::thread::sleep(wait);
    }
}

/// The wait after `failures` failed runs in a row, the first of them 1.
fn retry_wait(failures: u32) -> Duration {
    let doublings = failures.saturating_sub(1).min(8);
    (RETRY_FIRST * (1u32 << doublings)).min(RETRY_CAP)
}

/// How one run of ntpd ended.
enum Run {
    Exited(i32),
    Signalled(i32),
    /// Still running at `RUN_LIMIT`, and killed.
    Killed,
    NotStarted,
}

impl std::fmt::Display for Run {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Run::Exited(code) => write!(f, "exited with status {}", code),
            Run::Signalled(signal) => write!(f, "ended on signal {}", signal),
            Run::Killed => write!(f, "was killed for running {}s", RUN_LIMIT.as_secs()),
            Run::NotStarted => write!(f, "could not be started"),
        }
    }
}

/// `busybox ntpd -n -q`: stay in the foreground, so the keeper can wait for it,
/// and exit once the clock has been set or found close enough.
fn run_ntpd() -> Run {
    let started = Instant::now();
    let child = sys::fork();
    if child == 0 {
        let argv: Vec<String> = ["busybox", "ntpd", "-n", "-q"].iter().map(|a| a.to_string()).collect();
        let envp: Vec<String> = std::env::vars().map(|(k, v)| format!("{}={}", k, v)).collect();
        sys::execve(BUSYBOX, &argv, &envp);
        let _ = writeln!(std::io::stderr(), "timekeeper: cannot start {}", BUSYBOX);
        sys::exit_group(127);
    }
    if child < 0 {
        return Run::NotStarted;
    }
    loop {
        let (pid, status) = sys::wait4(child as i32, sys::WNOHANG);
        if pid == child {
            return match sys::signal_of(status) {
                Some(signal) => Run::Signalled(signal),
                None => Run::Exited(sys::exit_code_of(status)),
            };
        }
        if pid < 0 {
            return Run::NotStarted;
        }
        if started.elapsed() >= RUN_LIMIT {
            sys::kill(child as i32, SIGKILL);
            sys::wait4(child as i32, 0);
            return Run::Killed;
        }
        std::thread::sleep(RUN_POLL);
    }
}

/// Whole seconds of uptime, from the monotonic clock, which setting the date
/// does not move. The log gives these rather than wall-clock times because the
/// wall clock is what the runs being logged are changing.
fn seconds_since_boot() -> u64 {
    std::fs::read_to_string("/proc/uptime")
        .ok()
        .and_then(|text| text.split('.').next()?.trim().parse().ok())
        .unwrap_or(0)
}
