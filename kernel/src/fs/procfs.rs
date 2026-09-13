//! Files under /proc whose contents the kernel generates on read.

use super::{link_node, mkdir_p, unlink, Node, NodeKind};
use crate::abi::{Errno, S_IFREG};
use alloc::format;
use alloc::string::String;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Generated {
    MemInfo,
    Uptime,
    Version,
    CpuInfo,
    Mounts,
    Filesystems,
    Loadavg,
    Tasks,
    PidStat(u32),
    PidStatus(u32),
    PidCmdline(u32),
    PidMaps(u32),
}

/// The inode of /proc itself, so a lookup can tell when it is there.
static PROC_ROOT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

pub fn is_proc_root(ino: u64) -> bool {
    ino != 0 && PROC_ROOT.load(core::sync::atomic::Ordering::Relaxed) == ino
}

pub fn populate() {
    if let Ok(root) = mkdir_p("/proc") {
        PROC_ROOT.store(root.ino, core::sync::atomic::Ordering::Relaxed);
    }
    let _ = mkdir_p("/proc");
    let entries = [
        ("/proc/meminfo", Generated::MemInfo),
        ("/proc/uptime", Generated::Uptime),
        ("/proc/version", Generated::Version),
        ("/proc/cpuinfo", Generated::CpuInfo),
        ("/proc/mounts", Generated::Mounts),
        ("/proc/filesystems", Generated::Filesystems),
        ("/proc/loadavg", Generated::Loadavg),
        ("/proc/tasks", Generated::Tasks),
    ];
    for (path, kind) in entries {
        let node = Node::new(NodeKind::Generated(kind), S_IFREG | 0o444);
        let _ = link_node(path, node);
    }
}

/// The `/proc/<pid>/fd` directories, by inode. Their contents change every
/// time a descriptor is opened or closed, so they are rebuilt when something
/// looks inside rather than being kept up to date.
static FD_DIRS: crate::sync::Spinlock<alloc::collections::BTreeMap<u64, u32>> =
    crate::sync::Spinlock::new(alloc::collections::BTreeMap::new());

/// Rebuild `node` if it is one of those directories. Called on the way into
/// any directory, so the cheap case is a lookup that finds nothing.
pub fn refresh_dir(node: &crate::fs::NodeRef) {
    let pid = match FD_DIRS.lock().get(&node.ino) {
        Some(pid) => *pid,
        None => return,
    };
    // Take a copy of the table and let the process table go again: what follows
    // allocates nodes and formats names, and the task could be reaped in the
    // middle of it.
    let open = match crate::sched::with_task(pid, |task| task.fds.snapshot()) {
        Some(open) => open,
        None => return,
    };
    let mut children = alloc::collections::BTreeMap::new();
    for (fd, file) in open {
        // The entry stands for the descriptor itself: opening it opens that
        // descriptor rather than reopening whatever it is attached to, which
        // is what makes /dev/stdout work when stdout is a pipe. Reading the
        // link gives the file's name, or the label a pipe carries instead.
        let entry = Node::new(NodeKind::Fd(pid, fd), crate::abi::S_IFLNK | 0o777);
        let target = if file.path.is_empty() {
            format!("anon_inode:[{}]", fd)
        } else {
            file.path.clone()
        };
        entry.inner.lock().data = target.into_bytes();
        children.insert(format!("{}", fd), entry);
    }
    node.inner.lock().children = children;
}

/// Create /proc/<pid> for a new task.
pub fn add_process(pid: u32) {
    let dir = format!("/proc/{}", pid);
    if mkdir_p(&dir).is_err() {
        return;
    }
    let files = [
        ("stat", Generated::PidStat(pid)),
        ("status", Generated::PidStatus(pid)),
        ("cmdline", Generated::PidCmdline(pid)),
        ("maps", Generated::PidMaps(pid)),
    ];
    for (name, kind) in files {
        let node = Node::new(NodeKind::Generated(kind), S_IFREG | 0o444);
        let _ = link_node(&format!("{}/{}", dir, name), node);
    }
    if let Ok(fd_dir) = mkdir_p(&format!("{}/fd", dir)) {
        FD_DIRS.lock().insert(fd_dir.ino, pid);
    }
}

pub fn remove_process(pid: u32) {
    let dir = format!("/proc/{}", pid);
    for name in ["stat", "status", "cmdline", "maps"] {
        let _ = unlink(&format!("{}/{}", dir, name), false);
    }
    if let Ok(fd_dir) = crate::fs::lookup_nofollow(&format!("{}/fd", dir)) {
        FD_DIRS.lock().remove(&fd_dir.ino);
        fd_dir.inner.lock().children.clear();
    }
    let _ = unlink(&format!("{}/fd", dir), true);
    let _ = unlink(&dir, true);
}

fn state_char(state: crate::task::State) -> char {
    use crate::task::State;
    match state {
        State::Runnable => 'R',
        State::Sleeping => 'S',
        State::Stopped => 'T',
        State::Zombie => 'Z',
        State::Dead => 'X',
    }
}

pub fn render(kind: Generated) -> String {
    match kind {
        Generated::MemInfo => {
            let (used, total) = crate::mm::frame::stats();
            let (heap_used, heap_total) = crate::mm::heap::stats();
            format!(
                "MemTotal:       {:>8} kB\nMemFree:        {:>8} kB\nMemAvailable:   {:>8} kB\n\
                 Buffers:               0 kB\nCached:                0 kB\n\
                 SwapTotal:             0 kB\nSwapFree:              0 kB\n\
                 KernelHeap:     {:>8} kB\nKernelHeapUsed: {:>8} kB\n",
                total * 4,
                (total - used) * 4,
                (total - used) * 4,
                heap_total / 1024,
                heap_used / 1024,
            )
        }
        Generated::Uptime => {
            // Seconds since boot, then seconds spent with nothing to run.
            let ns = crate::time::monotonic_ns();
            let idle = crate::sched::idle_ticks() * (1_000_000_000 / crate::arch::TICK_HZ as u64);
            format!(
                "{}.{:02} {}.{:02}\n",
                ns / 1_000_000_000,
                (ns / 10_000_000) % 100,
                idle / 1_000_000_000,
                (idle / 10_000_000) % 100
            )
        }
        Generated::Version => format!(
            "Linux version 6.1.0-claudeos (claudeos) #1 SMP {}\n",
            crate::arch::MACHINE
        ),
        Generated::CpuInfo => crate::arch::cpu_info_text(),
        Generated::Mounts => String::from(
            "rootfs / rootfs rw 0 0\nproc /proc proc rw 0 0\ndevtmpfs /dev devtmpfs rw 0 0\n",
        ),
        Generated::Filesystems => String::from("nodev\tproc\nnodev\tdevtmpfs\n\trootfs\n"),
        Generated::Loadavg => {
            format!("0.00 0.00 0.00 1/{} {}\n", crate::sched::task_count(), 1)
        }
        Generated::Tasks => {
            // pid ppid pgid state name -- one process per line.
            let mut out = String::new();
            crate::sched::for_each(|task| {
                out.push_str(&format!(
                    "{} {} {} {} {}\n",
                    task.pid,
                    task.ppid,
                    task.pgid,
                    state_char(task.state),
                    task.name
                ));
            });
            out
        }
        Generated::PidStat(pid) => crate::sched::with_task(pid, |task| {
            // Readers skip to fields by counting separators, so all 52
            // fields Linux documents have to be present.
            let vsize = task.virtual_size();
            let rss = task.resident_pages();
            let ticks = crate::trap::ticks();
            let mut out = String::new();
            out.push_str(&format!(
                "{} ({}) {} {} {} {} 0 -1 0 ",
                task.pid,
                task.name,
                state_char(task.state),
                task.ppid,
                task.pgid,
                task.pgid, // session
            ));
            // minflt cminflt majflt cmajflt utime stime cutime cstime
            out.push_str(&format!("0 0 0 0 {} 0 0 0 ", ticks));
            // priority nice num_threads itrealvalue starttime
            out.push_str("20 0 1 0 0 ");
            // vsize rss rsslim
            out.push_str(&format!("{} {} 18446744073709551615 ", vsize, rss));
            // startcode endcode startstack kstkesp kstkeip
            out.push_str("0 0 0 0 0 ");
            // signal blocked sigignore sigcatch wchan nswap cnswap
            out.push_str(&format!("{} {} 0 0 0 0 0 ", task.pending_signals, task.signal_mask));
            // exit_signal processor rt_priority policy delayacct_blkio
            out.push_str("17 0 0 0 0 ");
            // guest_time cguest_time start_data end_data start_brk
            out.push_str(&format!("0 0 0 0 {} ", task.brk_start()));
            // arg_start arg_end env_start env_end exit_code
            out.push_str("0 0 0 0 0\n");
            out
        })
        .unwrap_or_default(),
        Generated::PidStatus(pid) => crate::sched::with_task(pid, |task| {
            format!(
                "Name:\t{}\nState:\t{} ({})\nTgid:\t{}\nPid:\t{}\nPPid:\t{}\n\
                 Uid:\t0\t0\t0\t0\nGid:\t0\t0\t0\t0\nThreads:\t1\n\
                 VmSize:\t{} kB\nVmRSS:\t{} kB\nVmData:\t{} kB\n\
                 SigPnd:\t{:016x}\nSigBlk:\t{:016x}\n",
                task.name,
                state_char(task.state),
                match task.state {
                    crate::task::State::Runnable => "running",
                    crate::task::State::Sleeping => "sleeping",
                    crate::task::State::Stopped => "stopped",
                    crate::task::State::Zombie => "zombie",
                    crate::task::State::Dead => "dead",
                },
                task.tgid,
                task.pid,
                task.ppid,
                task.virtual_size() / 1024,
                task.resident_pages() * 4,
                (task.brk().saturating_sub(task.brk_start())) / 1024,
                task.pending_signals,
                task.signal_mask,
            )
        })
        .unwrap_or_default(),
        Generated::PidMaps(pid) => crate::sched::with_task(pid, |task| {
            let mut out = String::new();
            let mut regions = task.snapshot_vmas();
            regions.sort_by_key(|region| region.start);
            let (brk_start, brk) = (task.brk_start(), task.brk());
            let stack_top = crate::mm::USER_STACK_TOP;

            let mut emit = |start: u64, end: u64, prot: u64, label: &str| {
                out.push_str(&format!(
                    "{:012x}-{:012x} {}{}{}p 00000000 00:00 0 {}{}\n",
                    start,
                    end,
                    if prot & crate::abi::PROT_READ != 0 { "r" } else { "-" },
                    if prot & crate::abi::PROT_WRITE != 0 { "w" } else { "-" },
                    if prot & crate::abi::PROT_EXEC != 0 { "x" } else { "-" },
                    if label.is_empty() { "" } else { "                    " },
                    label,
                ));
            };

            for region in &regions {
                let label = if region.end > stack_top - crate::task::STACK_RESERVE
                    && region.end <= stack_top
                {
                    "[stack]"
                } else {
                    ""
                };
                emit(region.start, region.end, region.prot, label);
            }
            if brk > brk_start {
                emit(
                    brk_start,
                    brk,
                    crate::abi::PROT_READ | crate::abi::PROT_WRITE,
                    "[heap]",
                );
            }
            out
        })
        .unwrap_or_default(),
        Generated::PidCmdline(pid) => {
            crate::sched::with_task(pid, |task| format!("{}\0", task.exe_path)).unwrap_or_default()
        }
    }
}

pub fn read(kind: Generated, offset: u64, buf: &mut [u8]) -> Result<usize, Errno> {
    let text = render(kind);
    let bytes = text.as_bytes();
    let start = offset as usize;
    if start >= bytes.len() {
        return Ok(0);
    }
    let n = buf.len().min(bytes.len() - start);
    buf[..n].copy_from_slice(&bytes[start..start + n]);
    Ok(n)
}

pub fn size(kind: Generated) -> u64 {
    render(kind).len() as u64
}
