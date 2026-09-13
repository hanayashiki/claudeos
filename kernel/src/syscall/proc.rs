//! Process, time and signal system calls.

use crate::abi::*;
use crate::arch::paging::AddressSpace;
use crate::arch::{self, TrapFrame};
use crate::elf;
use crate::sched;
use crate::task::{self, State, Task};
use crate::uaccess;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

pub fn fork(
    frame: &TrapFrame,
    flags: u64,
    stack: u64,
    parent_tid: u64,
    child_tid: u64,
    tls: u64,
) -> SysResult {
    let parent = sched::current();
    let share_vm = flags & CLONE_VM != 0;

    // A fresh address space belongs to nothing until the child is registered
    // on it, so anything that goes wrong before then has to hand it back here
    // or it is held by nobody: the tables under it, and the references its
    // entries took on the parent's frames, would stay taken for good.
    let space = if share_vm {
        parent.space()
    } else {
        let space = AddressSpace::new_user().ok_or(Errno::ENOMEM)?;
        if space.clone_user_from(&parent.space()).is_err() {
            space.destroy();
            return Err(Errno::ENOMEM);
        }
        space
    };

    let Some(mut child) = Task::new(&parent.name(), space) else {
        if !share_vm {
            space.destroy();
        }
        return Err(Errno::ENOMEM);
    };
    if share_vm {
        // Threads must see each other's mappings, so they share one record.
        child.share_space_of(&parent);
    } else {
        child.copy_mem_from(&parent);
    }
    let child_pid = child.pid;
    let is_thread = flags & CLONE_THREAD != 0;

    child.ppid.set(if is_thread { parent.ppid.get() } else { parent.pid });
    child.tgid = if is_thread { parent.tgid } else { child_pid };
    child.pgid.set(parent.pgid.get());
    // Two more things a thread shares with the task that started it. Without
    // these a descriptor one thread opens is a descriptor the others do not
    // have, and a directory one changes into is one the others do not resolve
    // against. A fork takes a copy of each instead, which is what makes the
    // two processes independent from that point on.
    child.cwd = if flags & CLONE_FS != 0 {
        parent.cwd.clone()
    } else {
        alloc::sync::Arc::new(crate::sync::Spinlock::new(parent.cwd()))
    };
    child.fds = if flags & CLONE_FILES != 0 {
        parent.fds.share()
    } else {
        parent.fds.clone_table()
    };
    child.set_exe_path(parent.exe_path());
    child.set_name(parent.name());
    child.umask.set(parent.umask.get());
    child.copy_actions_from(&parent);
    // The child carries on from the same instruction, so it starts on the
    // registers the parent is holding right now, thread pointer included
    // unless the caller named a new one.
    child.with_cpu(|cpu| cpu.save());
    if flags & CLONE_SETTLS != 0 {
        child.with_cpu(|cpu| cpu.set_thread_pointer(tls));
    }
    if flags & CLONE_CHILD_CLEARTID != 0 {
        child.clear_child_tid.set(child_tid);
    }

    // The child resumes from the same point with a zero return value.
    unsafe { arch::fork_child_frame(&mut *child.trap_frame(), frame, stack) };
    child.prepare_kernel_frame(sched::user_entry_trampoline as extern "C" fn() -> ! as usize as u64);

    // Only safe to write through these pointers while the address space is
    // shared with the parent we are running in.
    if share_vm {
        if flags & CLONE_PARENT_SETTID != 0 && parent_tid != 0 {
            let _ = uaccess::write_u32_in(&parent, parent_tid, child_pid);
        }
        if flags & CLONE_CHILD_SETTID != 0 && child_tid != 0 {
            let _ = uaccess::write_u32_in(&parent, child_tid, child_pid);
        }
    }

    let vfork = flags & CLONE_VFORK != 0;
    if vfork {
        child.vfork_parent.set(Some(parent.pid));
    }

    sched::register(child);

    if vfork {
        // vfork's contract: the parent does not run again until the child has
        // handed back its address space by exec'ing or exiting. The child is
        // already registered and could reach that point first, so the sleep
        // is set up with interrupts off, where nothing else can run.
        // The child clears the link when it execs or exits, so a link that is
        // still there means the address space has not been handed back yet.
        // Anything else that wakes the parent, a signal from an unrelated
        // child among them, must not end the wait, so this sleeps until the
        // link is gone rather than once.
        loop {
            let waiting = crate::sync::without_interrupts(|irq| {
                let waiting = sched::with_task(child_pid, |c| c.vfork_parent.get().is_some())
                    .unwrap_or(false);
                if waiting {
                    sched::current().sleep(0, irq);
                }
                waiting
            });
            if !waiting {
                break;
            }
            sched::schedule();
        }
    }
    Ok(child_pid as u64)
}

/// Put the task back on the image it was running when an exec could not be
/// finished.
///
/// Both halves of what was swapped out have to come back. The page tables are
/// the obvious one; the record of regions and the program break is the other,
/// and the fault handler consults it for every page that has not been touched
/// yet, so a task left running with an empty one takes a fault it cannot serve
/// on the first stack page or heap byte it reaches.
fn abandon_exec(
    task: &Task,
    old_space: AddressSpace,
    old_mm: alloc::sync::Arc<crate::sync::Spinlock<crate::task::MemState>>,
    new_space: AddressSpace,
) {
    // A context switch reloads the page table root only when the two tasks'
    // recorded spaces differ, so between the record going back to the old space
    // and the CPU following it the two disagree. A sibling thread recorded on
    // the old space is then resumed with no reload and runs on the half-built
    // exec image. The pair has to move together.
    crate::sync::without_interrupts(|irq| task.run_on_space(old_space, old_mm, irq));
    new_space.destroy();
}

/// Replace the current task's program image.
pub fn exec_into_current(
    path: &str,
    mut argv: Vec<String>,
    envp: Vec<String>,
) -> Result<(), Errno> {
    let (node, shebang) = task::read_executable(path)?;
    let mut exec_path = path.to_string();

    if let Some((interp, extra)) = shebang {
        // "#!/bin/sh -x" runs as: /bin/sh -x <script> <original args...>
        let mut rebuilt = Vec::new();
        rebuilt.push(interp.clone());
        if let Some(arg) = extra {
            rebuilt.push(arg);
        }
        rebuilt.push(path.to_string());
        rebuilt.extend(argv.into_iter().skip(1));
        argv = rebuilt;
        // read_executable already returned the interpreter's image.
        exec_path = interp;
    }

    elf::check(&node)?;

    let old_space = sched::current().space();
    let new_space = AddressSpace::new_user().ok_or(Errno::ENOMEM)?;

    // Everything below runs against the new address space; the kernel half is
    // shared so the stack and heap stay valid across the switch.
    //
    // The task's recorded address space is what a context switch compares to
    // decide whether to reload the page table root, so the record and the CPU
    // have to change together: while they disagree, a sibling thread recorded
    // on the space the record names is resumed with no reload and runs on the
    // other one.
    //
    // exec starts a fresh address space; a shared record must not follow it.
    // The old record is kept until the image is known to load, because the page
    // tables it describes are still there and the task goes back to running on
    // them if it does not.
    let task = sched::current();
    let old_mm = task.mm();
    let new_mm = alloc::sync::Arc::new(crate::sync::Spinlock::new(crate::task::MemState::new()));
    crate::sync::without_interrupts(|irq| task.run_on_space(new_space, new_mm, irq));

    // The loader reads the file in pieces under its lock rather than holding
    // it open: only the first pages are assembled here, and the rest arrives
    // through the fault handler later.
    let image = match elf::load_at(&new_space, &node, None) {
        Ok(image) => image,
        Err(err) => {
            abandon_exec(&task, old_space, old_mm, new_space);
            return Err(err);
        }
    };

    task.set_heap_base(image.brk_start);
    // Record the image so /proc reports it and mmap never lands on top of it.
    // A segment names the file it came from and has not been read in: its
    // pages come from there as the program reaches them.
    for segment in &image.segments {
        task.add_file_vma(
            segment.start,
            segment.end,
            segment.prot,
            MAP_PRIVATE,
            segment.file.clone(),
        );
    }

    // A dynamically linked program names an interpreter that has to be loaded
    // alongside it; control starts there, and it finds the program through
    // AT_PHDR and AT_ENTRY.
    let mut entry = image.entry;
    let mut interp_base = 0u64;
    if let Some(interp_path) = image.interp.clone() {
        let loaded = crate::fs::lookup(&interp_path)
            .map_err(|_| Errno::ENOENT)
            .and_then(|node| elf::load_at(&new_space, &node, Some(elf::INTERP_BASE)));
        match loaded {
            Ok(interp_image) => {
                for segment in &interp_image.segments {
                    task.add_file_vma(
                        segment.start,
                        segment.end,
                        segment.prot,
                        MAP_PRIVATE,
                        segment.file.clone(),
                    );
                }
                task.set_heap_base(image.brk_start.max(interp_image.brk_start));
                interp_base = interp_image.base;
                entry = interp_image.entry;
            }
            Err(err) => {
                crate::println!(
                    "[exec] {}: cannot load interpreter {}: {:?}",
                    exec_path,
                    interp_path,
                    err
                );
                abandon_exec(&task, old_space, old_mm, new_space);
                return Err(Errno::ENOENT);
            }
        }
    }

    let sp = match task::build_user_stack(&task, &image, &argv, &envp, &exec_path, interp_base) {
        Ok(sp) => sp,
        Err(err) => {
            abandon_exec(&task, old_space, old_mm, new_space);
            return Err(err);
        }
    };

    // The old image is unreachable from this task. Under CLONE_VM another
    // task is still running on it, so only the last user tears it down.
    if old_space != new_space && !sched::space_in_use(old_space) {
        old_space.destroy();
    }

    // A vfork parent may resume as soon as the address space is handed back.
    if let Some(parent_pid) = task.vfork_parent.take() {
        sched::wake(parent_pid);
    }

    task.fds.close_on_exec();
    // A new program starts on a clean register file, not the one the program
    // that called exec left behind.
    task.with_cpu(|cpu| cpu.reset_for_exec());
    task.set_name(exec_path.rsplit('/').next().unwrap_or(&exec_path).to_string());
    task.set_exe_path(exec_path);
    task.reset_actions_for_exec();

    task::set_user_entry(&task, entry, sp);
    Ok(())
}

pub fn execve(path_addr: u64, argv_addr: u64, envp_addr: u64, frame: &mut TrapFrame) -> SysResult {
    let raw = uaccess::read_cstr(path_addr, 4096)?;
    let path = super::file::resolve_str(AT_FDCWD, &raw)?;
    let argv = task::read_string_array(argv_addr)?;
    let envp = task::read_string_array(envp_addr)?;

    exec_into_current(&path, argv, envp)?;

    // A successful exec does not return to the caller: hand the freshly built
    // frame back to the entry stub.
    let new_frame = sched::current().trap_frame();
    unsafe { *frame = *new_frame };
    Ok(arch::syscall_result(frame))
}

pub fn wait4(pid: i64, status_addr: u64, options: u64) -> SysResult {
    let me = sched::current().pid;
    loop {
        if let Some((child_pid, status)) = sched::reap_child(me, pid as i32) {
            if status_addr != 0 {
                uaccess::write_u32(status_addr, status as u32)?;
            }
            return Ok(child_pid as u64);
        }
        if let Some((child_pid, status)) = sched::child_status_change(
            me,
            pid as i32,
            options & WUNTRACED != 0,
            options & WCONTINUED != 0,
        ) {
            if status_addr != 0 {
                uaccess::write_u32(status_addr, status as u32)?;
            }
            return Ok(child_pid as u64);
        }
        if !sched::has_children(me, pid as i32) {
            return Err(Errno::ECHILD);
        }
        if options & WNOHANG != 0 {
            return Ok(0);
        }
        // A child can finish between the checks above and this sleep: the
        // checks run with the task on the CPU, and the timer can take it away
        // at any point in between. The wake-up that the child sends then
        // finds a runnable parent and does nothing, and the sleep below would
        // never end. Turning interrupts off here only stops something else
        // starting now, not something that already happened, so the question
        // has to be asked again inside the same window, and the sleep skipped
        // if the answer has changed.
        let sleep = crate::sync::without_interrupts(|irq| {
            if sched::child_event_pending(
                me,
                pid as i32,
                options & WUNTRACED != 0,
                options & WCONTINUED != 0,
            ) {
                return false;
            }
            let task = sched::current();
            task.waiting_for.set(Some(pid as i32));
            task.sleep(0, irq);
            true
        });
        if sleep {
            sched::schedule();
            sched::current().waiting_for.set(None);
        }
        // A signal arriving while blocked interrupts the wait, but only one
        // that will do something when it is delivered. Asking the raw pending
        // set instead turns a terminal resize, or anything else the process has
        // told the kernel to discard, into a failed wait.
        let child_bit = 1u64 << (SIGCHLD as u64 & 63);
        if sched::has_pending_signal_except(child_bit) {
            return Err(Errno::EINTR);
        }
        // The child signal is what this call came for, so it is not a reason to
        // give up; but clearing it outright means a process with a handler
        // never sees that handler run for a child it reaped itself. Only the
        // dispositions that would discard it on delivery are cleared here.
        let task = sched::current();
        let handler = task.action(SIGCHLD as usize).handler;
        if handler == crate::signal::SIG_DFL || handler == crate::signal::SIG_IGN {
            task.drop_pending(child_bit);
        }
    }
}

pub fn kill(pid: i64, signal: i32) -> SysResult {
    // Signal zero sends nothing and reports whether the target is there,
    // which is how a program watches something it did not fork.
    let probe = signal == 0;
    let mut delivered = false;
    let me = sched::current().pid;
    let my_pgid = sched::current_pgid();
    sched::for_each(|task, table| {
        let target = match pid {
            p if p > 0 => task.pid == p as u32,
            0 => task.pgid.get() == my_pgid,
            -1 => task.pid != me && task.pid != 0,
            p => task.pgid.get() == (-p) as u32,
        };
        if target && task.state() != State::Zombie && task.pid != 0 {
            if !probe {
                sched::post_signal(task, signal, table);
            }
            delivered = true;
        }
    });
    if !delivered {
        return Err(Errno::ESRCH);
    }
    // A signal sent to this task is not acted on here. The entry path does that
    // once this call's result has been stored, and doing it first means the
    // result is stored over the frame a handler was about to be entered on:
    // on aarch64 the register a syscall returns in is the one a handler takes
    // its signal number in, so the handler was entered with the result in place
    // of the signal and this call returned whatever its first argument was.
    Ok(0)
}

/// Run `f` on the task a priority call names. `which` is PRIO_PROCESS,
/// PRIO_PGRP or PRIO_USER; only the first selects a single task, and `who` of
/// 0 means the caller.
fn with_priority_target<R>(
    which: u64,
    who: u64,
    f: impl FnOnce(&crate::task::Task) -> R,
) -> Option<R> {
    if which != 0 {
        return None;
    }
    let pid = if who == 0 { sched::current().pid } else { who as u32 };
    sched::with_task(pid, f)
}

pub fn setpriority(which: u64, who: u64, value: i64) -> SysResult {
    let nice = value.clamp(-20, 19) as i32;
    match with_priority_target(which, who, |task| task.nice.set(nice)) {
        Some(()) => Ok(0),
        None if which <= 2 => Ok(0),
        None => Err(Errno::EINVAL),
    }
}

pub fn getpriority(which: u64, who: u64) -> SysResult {
    // The raw call returns 20 - nice so the result is never negative.
    match with_priority_target(which, who, |task| task.nice.get()) {
        Some(nice) => Ok((20 - nice) as u64),
        None if which <= 2 => Ok(20),
        None => Err(Errno::EINVAL),
    }
}

/// `klogctl`: hand back what the kernel has printed. Actions 2, 3 and 4 read
/// it, 9 and 10 report its size, and 5 clears it.
pub fn syslog(action: u64, buf: u64, len: i64) -> SysResult {
    const READ: u64 = 2;
    const READ_ALL: u64 = 3;
    const READ_CLEAR: u64 = 4;
    const CLEAR: u64 = 5;
    const SIZE_UNREAD: u64 = 9;
    const SIZE_BUFFER: u64 = 10;

    match action {
        READ | READ_ALL | READ_CLEAR => {
            if len < 0 {
                return Err(Errno::EINVAL);
            }
            // Copy it out from under the lock: writing to user memory can
            // fault, and the fault handler prints.
            let copy = {
                let log = crate::serial::LOG.lock();
                let bytes = log.bytes();
                let n = bytes.len().min(len as usize);
                // Keep the tail when the buffer cannot take all of it, which
                // is the part a reader wants.
                alloc::vec::Vec::from(&bytes[bytes.len() - n..])
            };
            let n = copy.len();
            uaccess::write_bytes(buf, &copy)?;
            if action == READ_CLEAR {
                crate::serial::LOG.lock().clear();
            }
            Ok(n as u64)
        }
        CLEAR => {
            crate::serial::LOG.lock().clear();
            Ok(0)
        }
        SIZE_UNREAD => Ok(crate::serial::LOG.lock().len() as u64),
        SIZE_BUFFER => Ok(crate::serial::LOG_CAPACITY as u64),
        _ => Ok(0),
    }
}

pub fn setpgid(pid: u32, pgid: u32) -> SysResult {
    let target = if pid == 0 { sched::current().pid } else { pid };
    let value = if pgid == 0 { target } else { pgid };
    match sched::with_task(target, |task| task.pgid.set(value)) {
        Some(()) => Ok(0),
        None => Err(Errno::ESRCH),
    }
}

pub fn uname(out: u64) -> SysResult {
    fn fill(dst: &mut [u8; 65], value: &str) {
        let bytes = value.as_bytes();
        let n = bytes.len().min(64);
        dst[..n].copy_from_slice(&bytes[..n]);
        dst[n] = 0;
    }
    let mut uts = UtsName {
        sysname: [0; 65],
        nodename: [0; 65],
        release: [0; 65],
        version: [0; 65],
        machine: [0; 65],
        domainname: [0; 65],
    };
    fill(&mut uts.sysname, "Linux");
    fill(&mut uts.nodename, "claudeos");
    // Programs gate features on the release number, so report a modern one.
    fill(&mut uts.release, "6.1.0-claudeos");
    fill(&mut uts.version, "#1 claudeos");
    fill(&mut uts.machine, crate::arch::MACHINE);
    fill(&mut uts.domainname, "(none)");
    uaccess::write_struct(out, &uts)?;
    Ok(0)
}

pub fn sysinfo(out: u64) -> SysResult {
    let (used, total) = crate::mm::frame::stats();
    let info = SysInfo {
        uptime: crate::time::monotonic_parts().0,
        loads: [0; 3],
        totalram: total as u64 * 4096,
        freeram: (total - used) as u64 * 4096,
        procs: sched::task_count() as u16,
        mem_unit: 1,
        ..Default::default()
    };
    uaccess::write_struct(out, &info)?;
    Ok(0)
}

const RLIMIT_STACK: u64 = 3;
const RLIMIT_NOFILE: u64 = 7;

fn limit_for(resource: u64) -> RLimit {
    match resource {
        RLIMIT_STACK => RLimit {
            rlim_cur: task::STACK_RESERVE,
            rlim_max: task::STACK_RESERVE,
        },
        RLIMIT_NOFILE => RLimit {
            rlim_cur: crate::fs::MAX_FDS as u64,
            rlim_max: crate::fs::MAX_FDS as u64,
        },
        _ => RLimit { rlim_cur: RLIM_INFINITY, rlim_max: RLIM_INFINITY },
    }
}

pub fn getrlimit(resource: u64, out: u64) -> SysResult {
    uaccess::write_struct(out, &limit_for(resource))?;
    Ok(0)
}

pub fn prlimit64(resource: u64, _new_limit: u64, old_limit: u64) -> SysResult {
    if old_limit != 0 {
        uaccess::write_struct(old_limit, &limit_for(resource))?;
    }
    Ok(0)
}

pub fn getrusage(out: u64) -> SysResult {
    if out != 0 {
        let zeros = [0u8; 144];
        uaccess::write_bytes(out, &zeros)?;
    }
    Ok(0)
}

pub fn sched_getaffinity(mask_addr: u64, size: usize) -> SysResult {
    if size < 8 {
        return Err(Errno::EINVAL);
    }
    uaccess::write_u64(mask_addr, 1)?; // a single CPU
    Ok(8)
}

pub fn clock_gettime(clock: u64, out: u64) -> SysResult {
    let (sec, nsec) = match clock {
        CLOCK_REALTIME => crate::time::realtime_parts(),
        _ => crate::time::monotonic_parts(),
    };
    uaccess::write_struct(out, &Timespec { tv_sec: sec, tv_nsec: nsec })?;
    Ok(0)
}

pub fn clock_getres(_clock: u64, out: u64) -> SysResult {
    if out != 0 {
        let res = Timespec { tv_sec: 0, tv_nsec: 1_000_000_000 / arch::TICK_HZ as i64 };
        uaccess::write_struct(out, &res)?;
    }
    Ok(0)
}

pub fn gettimeofday(tv: u64, tz: u64) -> SysResult {
    if tv != 0 {
        let (sec, nsec) = crate::time::realtime_parts();
        uaccess::write_struct(tv, &Timeval { tv_sec: sec, tv_usec: nsec / 1000 })?;
    }
    if tz != 0 {
        uaccess::write_bytes(tz, &[0u8; 8])?;
    }
    Ok(0)
}

pub fn nanosleep(req: u64, rem: u64) -> SysResult {
    let spec: Timespec = uaccess::read_struct(req)?;
    if spec.tv_sec < 0 || spec.tv_nsec < 0 || spec.tv_nsec >= 1_000_000_000 {
        return Err(Errno::EINVAL);
    }
    let total_ns = spec.tv_sec as u64 * 1_000_000_000 + spec.tv_nsec as u64;
    let ticks = crate::time::ns_to_ticks(total_ns);
    if ticks == 0 {
        sched::yield_now();
        if rem != 0 {
            uaccess::write_struct(rem, &Timespec::default())?;
        }
        return Ok(0);
    }

    // Sleep to a deadline rather than for a duration: anything that wakes the
    // task early, a continue after a suspend among them, leaves the rest of
    // the time still to run.
    let deadline = crate::trap::ticks() + ticks;
    loop {
        let now = crate::trap::ticks();
        if now >= deadline {
            break;
        }
        // Being suspended is not the end of the sleep; it resumes afterwards
        // with whatever time is left.
        if sched::stop_if_requested() {
            continue;
        }
        if sched::has_pending_signal() {
            if rem != 0 {
                let left = crate::time::ticks_to_ns(deadline - now);
                let spec = Timespec {
                    tv_sec: (left / 1_000_000_000) as i64,
                    tv_nsec: (left % 1_000_000_000) as i64,
                };
                uaccess::write_struct(rem, &spec)?;
            }
            return Err(Errno::EINTR);
        }
        sched::sleep_ticks(deadline - now);
    }
    if rem != 0 {
        uaccess::write_struct(rem, &Timespec::default())?;
    }
    Ok(0)
}

pub fn getrandom(buf: u64, len: usize) -> SysResult {
    let mut bytes = alloc::vec![0u8; len.min(1 << 20)];
    crate::fs::dev::fill_random(&mut bytes);
    uaccess::write_bytes(buf, &bytes)?;
    Ok(bytes.len() as u64)
}

pub fn rt_sigaction(signal: usize, act: u64, old: u64) -> SysResult {
    if signal == 0 || signal >= 64 || signal == SIGKILL as usize || signal == SIGSTOP as usize {
        return Err(Errno::EINVAL);
    }
    let task = sched::current();
    if old != 0 {
        // struct sigaction: handler, flags, restorer, mask.
        let existing = task.action(signal);
        let mut buf = [0u8; 32];
        buf[0..8].copy_from_slice(&existing.handler.to_le_bytes());
        buf[8..16].copy_from_slice(&existing.flags.to_le_bytes());
        buf[16..24].copy_from_slice(&existing.restorer.to_le_bytes());
        buf[24..32].copy_from_slice(&existing.mask.to_le_bytes());
        uaccess::write_bytes_in(&task, old, &buf)?;
    }
    if act != 0 {
        let mut buf = [0u8; 32];
        uaccess::read_bytes_in(&task, act, &mut buf)?;
        let read = |offset: usize| {
            let mut bytes = [0u8; 8];
            bytes.copy_from_slice(&buf[offset..offset + 8]);
            u64::from_le_bytes(bytes)
        };
        task.set_action(
            signal,
            crate::signal::SigAction {
                handler: read(0),
                flags: read(8),
                restorer: read(16),
                mask: read(24),
            },
        );
    }
    Ok(0)
}

pub fn rt_sigreturn(frame: &mut TrapFrame) -> SysResult {
    crate::signal::sigreturn(&sched::current(), frame)
}

pub fn rt_sigprocmask(how: u32, set: u64, old: u64) -> SysResult {
    let task = sched::current();
    if old != 0 {
        uaccess::write_u64_in(&task, old, task.signal_mask.get())?;
    }
    if set != 0 {
        let value = uaccess::read_u64_in(&task, set)?;
        task.signal_mask.set(match how {
            0 => task.signal_mask.get() | value,  // SIG_BLOCK
            1 => task.signal_mask.get() & !value, // SIG_UNBLOCK
            _ => value,                           // SIG_SETMASK
        });
    }
    Ok(0)
}

/// Minimal futex: waiters spin-yield until the word changes or time runs out.
pub fn futex(uaddr: u64, op: u32, val: u32, timeout: u64) -> SysResult {
    match op & FUTEX_CMD_MASK {
        FUTEX_WAIT => {
            let current = uaccess::read_u32(uaddr)?;
            if current != val {
                return Err(Errno::EAGAIN);
            }
            let deadline = if timeout == 0 {
                u64::MAX
            } else {
                let spec: Timespec = uaccess::read_struct(timeout)?;
                let ns = spec.tv_sec as u64 * 1_000_000_000 + spec.tv_nsec as u64;
                crate::trap::ticks() + crate::time::ns_to_ticks(ns)
            };
            let key = crate::futex::futex_key(uaddr);
            loop {
                // Register first: a wake that lands between the read below and
                // the sleep is then a flag on the registration, not a wake-up
                // nobody received.
                crate::futex::register(key);
                let result = (|| -> Result<Option<u64>, Errno> {
                    if uaccess::read_u32(uaddr)? != val {
                        return Ok(Some(0));
                    }
                    if sched::has_pending_signal() {
                        return Err(Errno::EINTR);
                    }
                    Ok(None)
                })();
                match result {
                    Err(err) => {
                        crate::futex::unregister(key);
                        return Err(err);
                    }
                    Ok(Some(value)) => {
                        crate::futex::unregister(key);
                        return Ok(value);
                    }
                    Ok(None) => {}
                }
                let in_time = crate::futex::sleep_until(key, deadline);
                crate::futex::unregister(key);
                if !in_time {
                    return Err(Errno::ETIMEDOUT);
                }
            }
        }
        FUTEX_WAKE => Ok(crate::futex::wake(crate::futex::futex_key(uaddr), val) as u64),
        _ => Err(Errno::ENOSYS),
    }
}
