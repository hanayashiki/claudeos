//! Applets that report on the running system.

use super::{fail, split_flags};
use crate::sys;
use std::fs;

pub fn uname(args: &[String]) -> i32 {
    let (flags, _) = split_flags(args);
    let all = flags.contains('a');
    let version = fs::read_to_string("/proc/version").unwrap_or_default();
    let release = version
        .split_whitespace()
        .nth(2)
        .unwrap_or("6.1.0-claudeos")
        .to_string();

    // The machine name is this binary's own target, which is the only machine
    // it can run on.
    let machine = std::env::consts::ARCH;

    if all {
        println!("Linux claudeos {} #1 SMP {} claudeos", release, machine);
    } else if flags.contains('r') {
        println!("{}", release);
    } else if flags.contains('m') {
        println!("{}", machine);
    } else if flags.contains('n') {
        println!("claudeos");
    } else {
        println!("Linux");
    }
    0
}

pub fn ps(_args: &[String]) -> i32 {
    match fs::read_to_string("/proc/tasks") {
        Ok(text) => {
            println!("{:>5} {:>5} {:>5} {:<5} {}", "PID", "PPID", "PGID", "STAT", "COMMAND");
            let mut rows: Vec<(u32, String)> = Vec::new();
            for line in text.lines() {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() < 5 {
                    continue;
                }
                let pid: u32 = parts[0].parse().unwrap_or(0);
                rows.push((
                    pid,
                    format!(
                        "{:>5} {:>5} {:>5} {:<5} {}",
                        parts[0], parts[1], parts[2], parts[3], parts[4]
                    ),
                ));
            }
            rows.sort_by_key(|(pid, _)| *pid);
            for (_, row) in rows {
                println!("{}", row);
            }
            0
        }
        Err(err) => fail("ps", "/proc/tasks", err),
    }
}

pub fn free(args: &[String]) -> i32 {
    let (flags, _) = split_flags(args);
    let in_mib = flags.contains('m');
    match fs::read_to_string("/proc/meminfo") {
        Ok(text) => {
            let value = |key: &str| -> u64 {
                text.lines()
                    .find(|l| l.starts_with(key))
                    .and_then(|l| l.split_whitespace().nth(1))
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0)
            };
            let divisor = if in_mib { 1024 } else { 1 };
            let unit = if in_mib { "Mem (MiB)" } else { "Mem (KiB)" };
            let total = value("MemTotal:") / divisor;
            let free = value("MemFree:") / divisor;
            println!("{:<12} {:>10} {:>10} {:>10}", unit, "total", "used", "free");
            println!("{:<12} {:>10} {:>10} {:>10}", "", total, total - free, free);
            let heap_total = value("KernelHeap:") / divisor;
            let heap_used = value("KernelHeapUsed:") / divisor;
            println!(
                "{:<12} {:>10} {:>10} {:>10}",
                "kernel heap", heap_total, heap_used, heap_total - heap_used
            );
            0
        }
        Err(err) => fail("free", "/proc/meminfo", err),
    }
}

/// What the kernel has printed since boot.
pub fn dmesg(args: &[String]) -> i32 {
    let mut buffer = vec![0u8; 16 * 1024];
    let n = crate::sys::klog(&mut buffer);
    if n < 0 {
        eprintln!("dmesg: cannot read the kernel log");
        return 1;
    }
    buffer.truncate(n as usize);
    let text = String::from_utf8_lossy(&buffer);
    if args.iter().any(|a| a == "-c" || a == "-C") {
        // Nothing to clear separately: reading took a copy.
    }
    print!("{}", text);
    0
}

pub fn uptime(_args: &[String]) -> i32 {
    match fs::read_to_string("/proc/uptime") {
        Ok(text) => {
            let seconds: f64 = text
                .split_whitespace()
                .next()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0.0);
            let minutes = (seconds / 60.0) as u64;
            let hours = minutes / 60;
            println!(
                "up {} hours, {} minutes, {:.2} seconds total; {} processes",
                hours,
                minutes % 60,
                seconds,
                fs::read_to_string("/proc/tasks").map(|t| t.lines().count()).unwrap_or(0)
            );
            0
        }
        Err(err) => fail("uptime", "/proc/uptime", err),
    }
}

/// Month and day-of-month names, shared by date and by ls -l.
pub const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// `date [+FORMAT]` with the conversions scripts actually use.
pub fn date(args: &[String]) -> i32 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    let format = args.iter().skip(1).find(|a| a.starts_with('+'));
    let Some(format) = format else {
        println!("{} (unix time {})", format_time(now), now);
        return 0;
    };

    let days = now.div_euclid(86400);
    let seconds = now.rem_euclid(86400);
    let (year, month, day) = civil_from_days(days);
    let (hour, minute, second) = (seconds / 3600, (seconds % 3600) / 60, seconds % 60);
    // 1970-01-01 was a Thursday.
    let weekday = ((days % 7) + 7 + 4) % 7;
    const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

    let mut out = String::new();
    let mut chars = format[1..].chars().peekable();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('Y') => out.push_str(&format!("{:04}", year)),
            Some('y') => out.push_str(&format!("{:02}", year % 100)),
            Some('m') => out.push_str(&format!("{:02}", month)),
            Some('d') => out.push_str(&format!("{:02}", day)),
            Some('H') => out.push_str(&format!("{:02}", hour)),
            Some('M') => out.push_str(&format!("{:02}", minute)),
            Some('S') => out.push_str(&format!("{:02}", second)),
            Some('s') => out.push_str(&now.to_string()),
            Some('b') | Some('h') => out.push_str(MONTHS[(month - 1) as usize]),
            Some('a') => out.push_str(DAYS[weekday as usize]),
            Some('T') => out.push_str(&format!("{:02}:{:02}:{:02}", hour, minute, second)),
            Some('F') => out.push_str(&format!("{:04}-{:02}-{:02}", year, month, day)),
            Some('Z') => out.push_str("UTC"),
            Some('%') => out.push('%'),
            Some(other) => {
                out.push('%');
                out.push(other);
            }
            None => out.push('%'),
        }
    }
    println!("{}", out);
    0
}

/// Format a Unix timestamp as UTC, without pulling in a date library.
pub fn format_time(unix: i64) -> String {
    let days = unix.div_euclid(86400);
    let seconds = unix.rem_euclid(86400);
    let (year, month, day) = civil_from_days(days);
    const NAMES: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    format!(
        "{} {:>2} {:04} {:02}:{:02}:{:02} UTC",
        NAMES[(month - 1) as usize],
        day,
        year,
        seconds / 3600,
        (seconds % 3600) / 60,
        seconds % 60
    )
}

pub fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

pub fn env(_args: &[String]) -> i32 {
    let mut vars: Vec<(String, String)> = std::env::vars().collect();
    vars.sort();
    for (key, value) in vars {
        println!("{}={}", key, value);
    }
    0
}

pub fn id(_args: &[String]) -> i32 {
    println!("uid=0(root) gid=0(root) groups=0(root)");
    0
}

pub fn whoami(_args: &[String]) -> i32 {
    println!("root");
    0
}

pub fn hostname(_args: &[String]) -> i32 {
    println!("claudeos");
    0
}

pub fn mount(_args: &[String]) -> i32 {
    match fs::read_to_string("/proc/mounts") {
        Ok(text) => {
            for line in text.lines() {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 4 {
                    println!("{} on {} type {} ({})", parts[0], parts[1], parts[2], parts[3]);
                }
            }
            0
        }
        Err(err) => fail("mount", "/proc/mounts", err),
    }
}

pub fn sleep(args: &[String]) -> i32 {
    let (_, operands) = split_flags(args);
    let seconds: f64 = operands.first().and_then(|a| a.parse().ok()).unwrap_or(1.0);
    std::thread::sleep(std::time::Duration::from_secs_f64(seconds));
    0
}

pub fn kill(args: &[String]) -> i32 {
    let mut signal = 15;
    let mut targets = Vec::new();
    for arg in args.iter().skip(1) {
        if let Some(rest) = arg.strip_prefix('-') {
            if let Ok(value) = rest.parse::<i32>() {
                signal = value;
                continue;
            }
        }
        if let Ok(pid) = arg.parse::<i32>() {
            targets.push(pid);
        }
    }
    if targets.is_empty() {
        eprintln!("usage: kill [-SIGNAL] pid...");
        return 2;
    }
    let mut status = 0;
    for pid in targets {
        if sys::kill(pid, signal) < 0 {
            eprintln!("kill: {}: no such process", pid);
            status = 1;
        }
    }
    status
}

pub fn sync(_args: &[String]) -> i32 {
    sys::sync();
    0
}

/// `reboot`, `halt` and `poweroff` ask the kernel directly. A Linux system's
/// versions ask init to shut down first unless given `-f`; init here has
/// nothing to shut down, so options are accepted and make no difference.
pub fn reboot(args: &[String]) -> i32 {
    ask_kernel(args, sys::REBOOT_CMD_RESTART)
}

pub fn halt(args: &[String]) -> i32 {
    ask_kernel(args, sys::REBOOT_CMD_HALT)
}

pub fn poweroff(args: &[String]) -> i32 {
    ask_kernel(args, sys::REBOOT_CMD_POWER_OFF)
}

fn ask_kernel(args: &[String], command: u32) -> i32 {
    let rc = sys::reboot(sys::REBOOT_MAGIC1, sys::REBOOT_MAGIC2, command);
    let program = args.first().map(|name| name.rsplit('/').next().unwrap_or(name));
    let err = std::io::Error::from_raw_os_error((-rc) as i32);
    eprintln!("{}: {}", program.unwrap_or("reboot"), super::describe(&err));
    1
}

pub fn clear(_args: &[String]) -> i32 {
    print!("\x1b[2J\x1b[H");
    use std::io::Write;
    let _ = std::io::stdout().flush();
    0
}
