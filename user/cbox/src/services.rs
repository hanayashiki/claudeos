//! Services: the programs init starts at boot from two lists, and the
//! processes that keep them running.
//!
//! The system list, `/etc/claudeos/services`, is part of the image and is
//! checked at boot with the rest of it. The user list, `/data/services.txt`, is
//! on the SD card, where it is edited by hand. Both are read once, at boot, and
//! nothing starts, stops or reloads a service while the system runs. The format
//! is in `service_list.rs`.
//!
//! **Processes.** Init forks a starter. The starter reads the system list and
//! forks a keeper for each service to be started, then reads the user list and
//! does the same, prints one summary line and exits. A keeper starts its
//! service, waits for it, starts it again as its policy says, and writes its
//! status file and its log. It follows that:
//!
//! - A list is read in the starter, never in init, so a fault in reading one
//!   ends the starter, and init goes on to the shell (see `start`).
//! - The system services' keepers are forked before the user list is opened,
//!   and keepers share nothing, so nothing the user list holds or does can stop
//!   a system service.
//! - Every keeper, and every run of a service, is in a process group of its
//!   own. The terminal's interrupt, quit and suspend keys signal the foreground
//!   group, which the shell only ever gives to itself and its jobs, so no key
//!   typed at the console reaches a service.
//! - The starter has exited by the time the shell runs, so the keepers are
//!   init's children from then on, and init's wait loop reaps one that ends. A
//!   keeper ends on its own only for a `once` service, once nothing is left
//!   writing to its log.
//!
//! **Output.** A service's standard input is /dev/null. Its standard output and
//! error are the write end of a pipe its keeper reads, and the keeper appends
//! what it reads to `/var/log/NAME.log`. The keeper is the only writer of the
//! log, so it can cut the log down without losing what a service writes while
//! it does, which it could not if the service wrote to the file itself. A
//! keeper holds the write end too, so a process a service leaves behind, such
//! as one a shell started with `&`, can go on writing after the service has
//! ended and never meets a closed pipe.

use crate::service_list::{List, Policy, Service, MAX_BYTES};
use crate::sys;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::path::Path;
use std::time::{Duration, Instant};

pub const SYSTEM_LIST: &str = "/etc/claudeos/services";
pub const USER_LIST: &str = "/data/services.txt";

/// One status file per service, named after it, and the errors file. A
/// service name cannot hold a period, so no service's status file can be the
/// errors file.
const STATUS_DIR: &str = "/run/services";
const ERRORS: &str = "/run/services/errors.txt";

/// When a log passes `LOG_CAP` it is cut to its last `LOG_KEEP` bytes, from
/// the first line that starts in them. /var/log is on the ram filesystem, so a
/// log is memory: 256 KiB for each of the 64 services two lists can hold is
/// 16 MiB at the very most, and a board with a handful of services spends
/// about a megabyte. What is kept is around 1500 lines of 80 characters, many
/// runs of a service that keeps failing. Cutting to half rather than to just
/// under the cap copies the kept 128 KiB once for every 128 KiB written, rather
/// than on every write once a log is full.
const LOG_CAP: u64 = 256 * 1024;
const LOG_KEEP: u64 = 128 * 1024;

/// How often a keeper asks whether its running service has ended. It bounds
/// how late an exit is noticed, and so how much later than its backoff wait a
/// service is started again: a quarter of the shortest wait, 1 s. Output wakes
/// the keeper at once whatever this is.
const RUN_POLL: Duration = Duration::from_millis(250);

/// The longest init waits for the starter before it starts the shell. The
/// starter reads a file from memory and one from a card that the kernel has
/// already given its chance to mount, and forks: it takes milliseconds. The
/// wait bounds a starter that is stuck, on a card that has stopped answering
/// or otherwise, and the shell is how the machine is reached, so it starts
/// after this whatever the starter is doing.
const STARTER_WAIT: Duration = Duration::from_secs(10);
const STARTER_POLL: Duration = Duration::from_millis(10);

const SIGKILL: i32 = 9;

/// Start the services, and return when the starter has exited or
/// `STARTER_WAIT` has passed, whichever is first.
pub fn start() {
    let asked = Instant::now();
    let starter = sys::fork();
    if starter == 0 {
        sys::setpgid(0, 0);
        run_starter();
        sys::exit_group(0);
    }
    if starter < 0 {
        say("init: the service starter could not be forked, so no services are running");
        return;
    }
    // Both sides set the group, so it is set whichever runs first.
    sys::setpgid(starter as i32, starter as i32);
    loop {
        let (pid, status) = sys::wait4(starter as i32, sys::WNOHANG);
        if pid == starter {
            match sys::signal_of(status) {
                Some(signal) => say(&format!(
                    "init: the service starter ended on signal {} before it finished; the services it had not started are not running",
                    signal
                )),
                None if sys::exit_code_of(status) != 0 => say(&format!(
                    "init: the service starter exited with status {} before it finished; the services it had not started are not running",
                    sys::exit_code_of(status)
                )),
                None => {}
            }
            return;
        }
        if pid < 0 {
            say("init: the service starter could not be waited for; starting the shell");
            return;
        }
        if asked.elapsed() >= STARTER_WAIT {
            say(&format!(
                "init: the service starter has not finished after {} s; starting the shell, and the starter carries on",
                STARTER_WAIT.as_secs()
            ));
            return;
        }
        std::thread::sleep(STARTER_POLL);
    }
}

/// A line on the console. Not `println!`, which panics when the write fails.
fn say(line: &str) {
    let _ = writeln!(std::io::stdout(), "{}", line);
}

fn run_starter() {
    let _ = std::fs::create_dir_all(STATUS_DIR);
    let _ = std::fs::create_dir_all("/var/log");
    let mut errors = Errors { file: File::create(ERRORS).ok() };

    let (system, system_list) = take_list(SYSTEM_LIST, "system", None, &mut errors);
    test_hook();
    // The kernel holds init back until /data is mounted or has failed to be,
    // for at most 20 s, so what /proc/mounts says now is the outcome.
    let user = if data_mounted() {
        take_list(USER_LIST, "user", Some(&system_list), &mut errors).0
    } else {
        errors.line(format!("{}: /data is not mounted, so no user services were started", USER_LIST));
        Outcome::Absent(String::from("/data is not mounted"))
    };
    say(&summary(&system, &user));
}

/// For the harness: `servicetest=hang` or `servicetest=abort` on the kernel
/// command line stops the starter after it has started the system services
/// and before it reads the user list, so that the boot can show the shell
/// arriving and the system services running anyway. Only the test image's
/// cbox, built with the rtest feature, has it.
#[cfg(feature = "rtest")]
fn test_hook() {
    match std::env::var("servicetest").as_deref() {
        Ok("hang") => loop {
            std::thread::sleep(Duration::from_secs(3600));
        },
        Ok("abort") => std::process::abort(),
        _ => {}
    }
}

#[cfg(not(feature = "rtest"))]
fn test_hook() {}

fn data_mounted() -> bool {
    std::fs::read_to_string("/proc/mounts")
        .map(|text| text.lines().any(|line| line.split_whitespace().nth(1) == Some("/data")))
        .unwrap_or(false)
}

/// The lines written to the errors file, as they are found, so that a starter
/// that is stuck has still left what it found before.
struct Errors {
    file: Option<File>,
}

impl Errors {
    fn line(&mut self, text: String) {
        if let Some(file) = self.file.as_mut() {
            let _ = writeln!(file, "{}", text);
        }
    }
}

/// What reading a list and starting its services came to, for the summary.
enum Outcome {
    /// There was no list, for the reason given.
    Absent(String),
    Unreadable,
    Taken(Tally),
}

#[derive(Default)]
struct Tally {
    started: usize,
    not_started: usize,
    skipped: usize,
    unread: bool,
    /// Something went wrong that only the errors file describes.
    trouble: bool,
}

enum Loaded {
    Absent,
    Unreadable(String),
    Text(Vec<u8>, bool),
}

/// At most `MAX_BYTES` of the file, and whether it goes on past them.
fn load(path: &str) -> Loaded {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Loaded::Absent,
        Err(err) => return Loaded::Unreadable(err.to_string()),
    };
    match file.metadata() {
        Ok(meta) if meta.is_file() => {}
        Ok(_) => return Loaded::Unreadable(String::from("it is not a regular file")),
        Err(err) => return Loaded::Unreadable(err.to_string()),
    }
    let mut bytes = Vec::new();
    if let Err(err) = file.take(MAX_BYTES as u64 + 1).read_to_end(&mut bytes) {
        return Loaded::Unreadable(err.to_string());
    }
    let more = bytes.len() > MAX_BYTES;
    bytes.truncate(MAX_BYTES);
    Loaded::Text(bytes, more)
}

/// Read the list at `path` and start its services. A user list is given the
/// system list, whose names it may not use.
fn take_list(path: &str, which: &'static str, system: Option<&List>, errors: &mut Errors) -> (Outcome, List) {
    let mut list = match load(path) {
        Loaded::Absent => {
            errors.line(format!("{}: it does not exist, so no {} services were started", path, which));
            return (Outcome::Absent(format!("{} does not exist", path)), List::default());
        }
        Loaded::Unreadable(why) => {
            errors.line(format!("{}: it cannot be read ({}), so no {} services were started", path, why, which));
            return (Outcome::Unreadable, List::default());
        }
        Loaded::Text(bytes, more) => List::parse(&bytes, more),
    };
    if let Some(system) = system {
        list.refuse_system_names(system);
    }
    for skipped in &list.skipped {
        errors.line(format!("{} line {}: {}", path, skipped.line, skipped.reason));
    }
    if let Some(unread) = &list.unread {
        errors.line(format!("{}: {}", path, unread));
    }
    let mut tally = Tally { skipped: list.skipped.len(), unread: list.unread.is_some(), ..Tally::default() };
    for service in &list.services {
        start_one(service, which, path, &mut tally, errors);
    }
    (Outcome::Taken(tally), list)
}

fn start_one(service: &Service, which: &'static str, path: &str, tally: &mut Tally, errors: &mut Errors) {
    let mut status = Status::new(service, which);
    if service.policy == Policy::Off {
        status.state = String::from("off");
        status.write();
        tally.not_started += 1;
        return;
    }
    // Looked up now, which for a user service is after /data has had its
    // chance to mount, so a path under /data can be needed.
    if let Some(needed) = &service.needs {
        if !Path::new(needed).exists() {
            status.state = format!("not started: it needs {}, which does not exist", needed);
            status.write();
            tally.not_started += 1;
            return;
        }
    }
    status.state = String::from("starting");
    status.write();
    let keeper = sys::fork();
    if keeper == 0 {
        sys::setpgid(0, 0);
        keep(service, status);
    }
    if keeper < 0 {
        status.state = String::from("not started: its keeper could not be forked");
        status.write();
        errors.line(format!(
            "{} line {}: {} was not started: its keeper could not be forked (errno {})",
            path, service.line, service.name, -keeper
        ));
        tally.not_started += 1;
        tally.trouble = true;
        return;
    }
    sys::setpgid(keeper as i32, keeper as i32);
    tally.started += 1;
}

/// The one line the starter prints on the console.
fn summary(system: &Outcome, user: &Outcome) -> String {
    let (system_part, system_trouble) = part(system, "system");
    let (user_part, user_trouble) = part(user, "user");
    let see = if system_trouble || user_trouble { format!(" (see {})", ERRORS) } else { String::new() };
    format!("services: {}; {}{}", system_part, user_part, see)
}

fn part(outcome: &Outcome, which: &str) -> (String, bool) {
    match outcome {
        Outcome::Absent(why) => (format!("no {} list, {}", which, why), false),
        Outcome::Unreadable => (format!("the {} list cannot be read", which), true),
        Outcome::Taken(tally) => {
            let mut text = format!("{} {} started", tally.started, which);
            if tally.not_started > 0 {
                text.push_str(&format!(", {} not started", tally.not_started));
            }
            if tally.skipped > 0 {
                let lines = if tally.skipped == 1 { "line" } else { "lines" };
                text.push_str(&format!(", {} {} skipped", tally.skipped, lines));
            }
            if tally.unread {
                text.push_str(", the end of the file not read");
            }
            (text, tally.skipped > 0 || tally.unread || tally.trouble)
        }
    }
}

/// What `/run/services/NAME` says.
struct Status {
    name: String,
    list: &'static str,
    policy: Policy,
    command: String,
    state: String,
    pid: Option<i32>,
    next: Option<u64>,
    starts: u32,
    last: String,
}

impl Status {
    fn new(service: &Service, list: &'static str) -> Status {
        Status {
            name: service.name.clone(),
            list,
            policy: service.policy,
            command: shown_command(&service.argv),
            state: String::new(),
            pid: None,
            next: None,
            starts: 0,
            last: String::from("none yet"),
        }
    }

    /// Replace the file in one rename, so a reader never sees half of it.
    fn write(&self) {
        let mut text = format!(
            "name: {}\nlist: {}\npolicy: {}\ncommand: {}\nlog: /var/log/{}.log\nstate: {}\n",
            self.name,
            self.list,
            self.policy.word(),
            self.command,
            self.name,
            self.state
        );
        match self.pid {
            Some(pid) => text.push_str(&format!("pid: {}\n", pid)),
            None => text.push_str("pid: none\n"),
        }
        if let Some(next) = self.next {
            text.push_str(&format!("next start: {} s after boot\n", next));
        }
        text.push_str(&format!(
            "starts: {}\nlast run: {}\nchanged: {} s after boot\n",
            self.starts,
            self.last,
            seconds_since_boot()
        ));
        let fresh = format!("{}/.{}.new", STATUS_DIR, self.name);
        if std::fs::write(&fresh, text).is_ok() {
            let _ = std::fs::rename(&fresh, format!("{}/{}", STATUS_DIR, self.name));
        }
    }
}

/// The command as a line of a list would give it.
fn shown_command(argv: &[String]) -> String {
    let words: Vec<String> = argv
        .iter()
        .map(|word| {
            if !word.is_empty() && !word.contains([' ', '\t', '\'', '"', '#']) {
                word.clone()
            } else if !word.contains('\'') {
                format!("'{}'", word)
            } else {
                format!("\"{}\"", word)
            }
        })
        .collect();
    words.join(" ")
}

/// How one run ended.
enum Run {
    Exited(i32),
    Signalled(i32),
    /// Still running at its `limit=`, and killed.
    Killed(Duration),
    /// The program could not be started, with the errno `execve` or `fork`
    /// gave.
    NotStarted(i32),
    /// `wait4` failed on it, which leaves nothing to wait for.
    Lost,
}

impl std::fmt::Display for Run {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Run::Exited(code) => write!(f, "exited with status {}", code),
            Run::Signalled(signal) => write!(f, "ended on signal {}", signal),
            Run::Killed(limit) => write!(f, "was killed at its limit of {} s", limit.as_secs()),
            Run::NotStarted(errno) => {
                write!(f, "could not be started: {}", std::io::Error::from_raw_os_error(*errno))
            }
            Run::Lost => write!(f, "could not be waited for"),
        }
    }
}

/// Run a service, again and again as its policy says. Only a `once` service's
/// keeper ever ends.
fn keep(service: &Service, mut status: Status) -> ! {
    // The keeper's own standard input, output and error are /dev/null: the
    // console is the user's terminal and the test harness reads it, so nothing
    // here writes a byte to it or reads a key from it.
    let null = sys::open("/dev/null", sys::O_RDWR, 0);
    if null >= 0 {
        for fd in [sys::STDIN, sys::STDOUT, sys::STDERR] {
            sys::dup2(null as i32, fd);
        }
        if null > 2 {
            sys::close(null as i32);
        }
    }
    let mut log = Log::open(&service.name);
    let Some(mut output) = Output::new() else {
        status.state = String::from("not started: no pipe could be made for its output");
        status.write();
        sys::exit_group(1);
    };

    let mut failures = 0u32;
    let mut next_start = Instant::now();
    loop {
        loop {
            let now = Instant::now();
            if now >= next_start {
                break;
            }
            output.drain_for(next_start - now, &mut log);
        }

        status.starts += 1;
        let started = Instant::now();
        let run = match spawn(&service.argv, output.write) {
            Err(errno) => Run::NotStarted(errno),
            Ok(pid) => {
                status.state = String::from("running");
                status.pid = Some(pid);
                status.next = None;
                status.write();
                watch(pid, service.limit, started, &output, &mut log)
            }
        };
        let ran = started.elapsed();
        status.pid = None;
        status.last = match run {
            Run::NotStarted(_) => run.to_string(),
            _ => format!("{} after {} s", run, ran.as_secs()),
        };

        if service.policy == Policy::Once {
            status.state = String::from("finished");
            status.write();
            log.line(&format!("{} {} after {} s; it is not started again", service.name, run, ran.as_secs()));
            // Read what anything the service left running still writes, until
            // the last of it has closed the pipe.
            output.close_write();
            output.drain_to_end(&mut log);
            sys::exit_group(0);
        }

        if service.backoff.resets_after(ran) {
            failures = 0;
        }
        let wait = match (service.every, &run) {
            (Some(every), Run::Exited(0)) => {
                failures = 0;
                every
            }
            _ => {
                failures = failures.saturating_add(1);
                service.backoff.wait(failures)
            }
        };
        next_start = Instant::now() + wait;
        status.state = String::from("waiting");
        status.next = Some(seconds_since_boot() + wait.as_secs());
        status.write();
        log.line(&format!(
            "{} {} after {} s; next start in {} s",
            service.name,
            run,
            ran.as_secs(),
            wait.as_secs()
        ));
    }
}

/// Start `argv` in a process group of its own, with `output` as its standard
/// output and error and the keeper's /dev/null as its standard input. Returns
/// its pid once it is running the program, or the errno that kept it from
/// starting.
fn spawn(argv: &[String], output: i32) -> Result<i32, i32> {
    let envp: Vec<String> = std::env::vars_os()
        .map(|(key, value)| format!("{}={}", key.to_string_lossy(), value.to_string_lossy()))
        .collect();
    // A failed exec is reported through this pipe as the errno. Both ends are
    // closed across a successful exec, so reading nothing from it means the
    // program is running.
    let (report_read, report_write) = sys::pipe_cloexec().map_err(|err| (-err) as i32)?;
    let child = sys::fork();
    if child == 0 {
        sys::setpgid(0, 0);
        sys::dup2(output, sys::STDOUT);
        sys::dup2(output, sys::STDERR);
        let errno = (-sys::execve(&argv[0], argv, &envp)) as i32;
        sys::write(report_write, &errno.to_le_bytes());
        sys::exit_group(127);
    }
    sys::close(report_write);
    if child < 0 {
        sys::close(report_read);
        return Err((-child) as i32);
    }
    sys::setpgid(child as i32, child as i32);
    let mut report = [0u8; 4];
    let mut got = 0;
    while got < report.len() {
        let count = sys::read(report_read, &mut report[got..]);
        if count <= 0 {
            break;
        }
        got += count as usize;
    }
    sys::close(report_read);
    if got == report.len() {
        sys::wait4(child as i32, 0);
        return Err(i32::from_le_bytes(report));
    }
    Ok(child as i32)
}

/// Wait for the run `pid` to end, reading its output meanwhile, and kill its
/// process group if it reaches `limit`.
fn watch(pid: i32, limit: Option<Duration>, started: Instant, output: &Output, log: &mut Log) -> Run {
    loop {
        let (got, status) = sys::wait4(pid, sys::WNOHANG);
        if got == pid as i64 {
            return match sys::signal_of(status) {
                Some(signal) => Run::Signalled(signal),
                None => Run::Exited(sys::exit_code_of(status)),
            };
        }
        if got < 0 {
            return Run::Lost;
        }
        let mut wait = RUN_POLL;
        if let Some(limit) = limit {
            let elapsed = started.elapsed();
            if elapsed >= limit {
                sys::kill(-pid, SIGKILL);
                sys::wait4(pid, 0);
                return Run::Killed(limit);
            }
            wait = wait.min(limit - elapsed);
        }
        output.drain_for(wait, log);
    }
}

/// The pipe a service writes its output into, and the epoll set that waits on
/// it.
struct Output {
    read: i32,
    write: i32,
    epoll: i32,
}

impl Output {
    fn new() -> Option<Output> {
        let (read, write) = sys::pipe_cloexec().ok()?;
        let epoll = sys::epoll_create_cloexec();
        if epoll < 0 || sys::epoll_add(epoll as i32, read, sys::EPOLLIN, 0) < 0 {
            return None;
        }
        Some(Output { read, write, epoll: epoll as i32 })
    }

    /// Wait up to `wait` for output, and append what has come to the log.
    fn drain_for(&self, wait: Duration, log: &mut Log) {
        let milliseconds = wait.as_micros().div_ceil(1000).min(i64::MAX as u128) as i64;
        let mut events = [0u8; sys::EPOLL_EVENT_SIZE];
        let ready = sys::epoll_wait(self.epoll, &mut events, milliseconds);
        if ready > 0 {
            let mut buffer = [0u8; 16 * 1024];
            let count = sys::read(self.read, &mut buffer);
            if count > 0 {
                log.append(&buffer[..count as usize]);
                return;
            }
        }
        // A wait that failed, or a ready pipe that gave nothing, returns at
        // once; sleeping on it keeps the keeper from a loop that never waits.
        // A wait that timed out has already waited.
        if ready != 0 {
            std::thread::sleep(wait.min(RUN_POLL));
        }
    }

    fn close_write(&mut self) {
        sys::close(self.write);
        self.write = -1;
    }

    /// Append everything read until every writer has closed the pipe.
    fn drain_to_end(&self, log: &mut Log) {
        let mut buffer = [0u8; 16 * 1024];
        loop {
            let count = sys::read(self.read, &mut buffer);
            if count <= 0 {
                return;
            }
            log.append(&buffer[..count as usize]);
        }
    }
}

/// `/var/log/NAME.log`, written only by the service's keeper.
struct Log {
    file: Option<File>,
}

impl Log {
    fn open(name: &str) -> Log {
        let file = OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .mode(0o644)
            .open(format!("/var/log/{}.log", name))
            .ok();
        Log { file }
    }

    fn append(&mut self, bytes: &[u8]) {
        let Some(file) = self.file.as_mut() else {
            return;
        };
        let _ = file.write_all(bytes);
        let Ok(length) = file.metadata().map(|meta| meta.len()) else {
            return;
        };
        if length <= LOG_CAP {
            return;
        }
        let mut tail = vec![0u8; LOG_KEEP as usize];
        if file.read_exact_at(&mut tail, length - LOG_KEEP).is_err() {
            return;
        }
        let from = first_line_start(&tail);
        if file.set_len(0).is_ok() {
            let _ = file.write_all(&tail[from..]);
        }
    }

    /// One line of the keeper's own, among the service's output.
    fn line(&mut self, text: &str) {
        self.append(format!("services: {} s after boot: {}\n", seconds_since_boot(), text).as_bytes());
    }
}

/// Where the first whole line in `tail` starts: just after its first newline,
/// or at 0 when it has none before its last byte, so that what is kept is never
/// nothing.
fn first_line_start(tail: &[u8]) -> usize {
    match tail.iter().position(|&byte| byte == b'\n') {
        Some(end) if end + 1 < tail.len() => end + 1,
        _ => 0,
    }
}

/// Whole seconds of uptime, from the monotonic clock, which setting the date
/// does not move. Status files and logs give these rather than wall-clock
/// times because the wall clock is what ntpd, one of the services, changes.
fn seconds_since_boot() -> u64 {
    std::fs::read_to_string("/proc/uptime")
        .ok()
        .and_then(|text| text.split('.').next()?.trim().parse().ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn taken(started: usize, not_started: usize, skipped: usize, unread: bool) -> Outcome {
        Outcome::Taken(Tally { started, not_started, skipped, unread, trouble: false })
    }

    #[test]
    fn summary_lines() {
        assert_eq!(
            summary(&taken(1, 0, 0, false), &taken(3, 1, 2, false)),
            "services: 1 system started; 3 user started, 1 not started, 2 lines skipped (see /run/services/errors.txt)"
        );
        assert_eq!(
            summary(&taken(0, 1, 0, false), &Outcome::Absent(String::from("/data is not mounted"))),
            "services: 0 system started, 1 not started; no user list, /data is not mounted"
        );
        assert_eq!(
            summary(&taken(1, 0, 0, false), &taken(0, 0, 1, true)),
            "services: 1 system started; 0 user started, 1 line skipped, the end of the file not read (see /run/services/errors.txt)"
        );
        assert_eq!(
            summary(&Outcome::Absent(String::from("/etc/claudeos/services does not exist")), &Outcome::Unreadable),
            "services: no system list, /etc/claudeos/services does not exist; the user list cannot be read (see /run/services/errors.txt)"
        );
        let trouble = Outcome::Taken(Tally { started: 2, trouble: true, ..Tally::default() });
        assert_eq!(
            summary(&trouble, &taken(0, 0, 0, false)),
            "services: 2 system started; 0 user started (see /run/services/errors.txt)"
        );
    }

    #[test]
    fn cutting_a_log_keeps_whole_lines() {
        assert_eq!(first_line_start(b"end of a line\nnext\nlast\n"), 14);
        assert_eq!(first_line_start(b"\nwhole"), 1);
        // No newline, or only the last byte: everything is kept.
        assert_eq!(first_line_start(b"one long line"), 0);
        assert_eq!(first_line_start(b"one long line\n"), 0);
        assert_eq!(first_line_start(b""), 0);
    }

    #[test]
    fn commands_are_shown_as_a_list_would_give_them() {
        let argv: Vec<String> = ["/bin/sh", "-c", "echo hi; exit 1", "", "it's", "#x"].iter().map(|s| s.to_string()).collect();
        assert_eq!(shown_command(&argv), r#"/bin/sh -c 'echo hi; exit 1' '' "it's" '#x'"#);
    }
}
