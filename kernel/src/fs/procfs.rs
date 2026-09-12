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
}

pub fn populate() {
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
    ];
    for (name, kind) in files {
        let node = Node::new(NodeKind::Generated(kind), S_IFREG | 0o444);
        let _ = link_node(&format!("{}/{}", dir, name), node);
    }
}

pub fn remove_process(pid: u32) {
    let dir = format!("/proc/{}", pid);
    for name in ["stat", "status", "cmdline"] {
        let _ = unlink(&format!("{}/{}", dir, name), false);
    }
    let _ = unlink(&dir, true);
}

fn state_char(state: crate::task::State) -> char {
    use crate::task::State;
    match state {
        State::Runnable => 'R',
        State::Sleeping => 'S',
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
            let ns = crate::time::monotonic_ns();
            format!("{}.{:02} {}.{:02}\n", ns / 1_000_000_000, (ns / 10_000_000) % 100,
                    ns / 1_000_000_000, (ns / 10_000_000) % 100)
        }
        Generated::Version => String::from(
            "Linux version 6.1.0-claudeos (claudeos) #1 SMP x86_64\n",
        ),
        Generated::CpuInfo => {
            let mut out = String::from("processor\t: 0\nvendor_id\t: ");
            let leaf = crate::cpu::cpuid(0, 0);
            let mut vendor = [0u8; 12];
            vendor[0..4].copy_from_slice(&leaf.ebx.to_le_bytes());
            vendor[4..8].copy_from_slice(&leaf.edx.to_le_bytes());
            vendor[8..12].copy_from_slice(&leaf.ecx.to_le_bytes());
            out.push_str(core::str::from_utf8(&vendor).unwrap_or("unknown"));
            out.push_str("\ncpu family\t: 6\nmodel name\t: claudeos virtual CPU\n");
            out.push_str("flags\t\t: fpu tsc msr pae cx8 apic sse sse2 syscall nx lm\n\n");
            out
        }
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
        Generated::PidStat(pid) => match crate::sched::find(pid) {
            Some(task) => {
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
            }
            None => String::new(),
        },
        Generated::PidStatus(pid) => match crate::sched::find(pid) {
            Some(task) => format!(
                "Name:\t{}\nState:\t{} ({})\nTgid:\t{}\nPid:\t{}\nPPid:\t{}\n\
                 Uid:\t0\t0\t0\t0\nGid:\t0\t0\t0\t0\nThreads:\t1\n\
                 VmSize:\t{} kB\nVmRSS:\t{} kB\nVmData:\t{} kB\n\
                 SigPnd:\t{:016x}\nSigBlk:\t{:016x}\n",
                task.name,
                state_char(task.state),
                match task.state {
                    crate::task::State::Runnable => "running",
                    crate::task::State::Sleeping => "sleeping",
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
            ),
            None => String::new(),
        },
        Generated::PidCmdline(pid) => match crate::sched::find(pid) {
            Some(task) => format!("{}\0", task.exe_path),
            None => String::new(),
        },
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
