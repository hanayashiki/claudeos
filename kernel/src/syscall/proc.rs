//! Process, time and signal system calls.

use crate::abi::*;
use crate::arch::paging::AddressSpace;
use crate::arch::{self, TrapFrame};
use crate::elf;
use crate::sched;
use crate::signal::Signal;
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

    // A process's parent is the process that forked it, whichever of its
    // threads made the call: `getppid` answers with its pid, any thread of it
    // can wait for the child, and the child is handed to init only once all of
    // them have exited. A thread has its process's parent.
    child.ppid.set(if is_thread { parent.ppid.get() } else { parent.tgid });
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
    // Interval timers belong to the process: a thread shares its process's,
    // and a forked child keeps the none-armed set it was made with. The status
    // `exit_group` ends the process with belongs to it the same way, and so do
    // the signals sent to the process and not yet taken.
    if is_thread {
        child.itimers = parent.itimers.clone();
        child.group_exit = parent.group_exit.clone();
        child.share_pending_of(&parent);
    }
    child.set_exe_path(parent.exe_path());
    child.set_name(parent.name());
    child.umask.set(parent.umask.get());
    child.copy_actions_from(&parent);
    // A fork's child has its own copy of the stack the parent named, so it
    // keeps the naming. A thread shares the parent's memory, and two handlers
    // running on one stack would write over each other, so it starts with
    // none and installs its own.
    if !share_vm {
        child.sig_stack.set(parent.sig_stack.get());
    }
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

    // The code a machine supplies to every program, on the one that supplies
    // any: the page an aarch64 signal handler returns through when the
    // program registered no restorer of its own, which is what a program
    // built for Linux there does. It goes in first, so that everything placed
    // afterwards is placed knowing it is there.
    if let Err(err) = arch::map_signal_trampoline(&task, &new_space) {
        abandon_exec(&task, old_space, old_mm, new_space);
        return Err(err);
    }

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
    // The children are the process's, so any of its threads collects them.
    let me = sched::current().tgid;
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
        //
        // A child can also be released as it exits, when this process ignores
        // SIGCHLD or asked for SA_NOCLDWAIT, and then there is nothing to
        // collect: the last one going is a reason to look again and fail with
        // ECHILD, not to sleep.
        let sleep = crate::sync::without_interrupts(|irq| {
            let pending = sched::child_event_pending(
                me,
                pid as i32,
                options & WUNTRACED != 0,
                options & WCONTINUED != 0,
            );
            if pending || !sched::has_children(me, pid as i32) {
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
        let child_bit = SIGCHLD.bit();
        if sched::has_pending_signal_except(child_bit) {
            return Err(Errno::EINTR);
        }
        // The child signal is what this call came for, so it is not a reason to
        // give up; but clearing it outright means a process with a handler
        // never sees that handler run for a child it reaped itself. Only the
        // dispositions that would discard it on delivery are cleared here.
        let task = sched::current();
        let handler = task.action(SIGCHLD).handler;
        if handler == crate::signal::SIG_DFL || handler == crate::signal::SIG_IGN {
            task.drop_pending(child_bit);
        }
    }
}

/// The signal a `kill`, `tkill` or `tgkill` names: `None` for signal zero,
/// which sends nothing and reports whether the target is there, which is how a
/// program watches something it did not fork. Any other number has to name a
/// signal: one that does not is refused here rather than folded into the range
/// further in.
fn signal_to_send(signal: i32) -> Result<Option<Signal>, Errno> {
    match signal {
        0 => Ok(None),
        number => Signal::from_number(number).map(Some).ok_or(Errno::EINVAL),
    }
}

/// `kill`: a signal for a process, or for each process of a group, never for
/// one thread.
///
/// Above zero `pid` names a process, and the signal goes to its thread group,
/// where any thread that does not block it takes it; a thread id that is not
/// the leader's names the process that thread is in, as on Linux. Zero is the
/// caller's process group, minus one every process but init and the caller's
/// own, and below that the process group its negation names. A process whose
/// every thread has exited is not there to be signalled.
pub fn kill(pid: i32, signal: i32) -> SysResult {
    let send = signal_to_send(signal)?;
    let me = sched::current().tgid;
    let my_pgid = sched::current_pgid();
    let mut delivered = false;
    sched::with_tasks(|table| {
        let mut deliver = |leader: &Task| {
            if table.group_exited(leader.tgid) {
                return;
            }
            if let Some(signal) = send {
                sched::post_process_signal(leader, signal, table);
            }
            delivered = true;
        };
        if pid > 0 {
            let leader = table.find(pid as u32).and_then(|task| table.find(task.tgid));
            if let Some(leader) = leader {
                deliver(leader);
            }
            return;
        }
        // Each process once, through its leader, which stays in the table until
        // every thread of it has exited. A process group's members are its
        // processes: sent to every task in the group, a process took the
        // signal once per thread.
        table.for_each(|task| {
            if task.pid != task.tgid || task.pid == 0 {
                return;
            }
            let chosen = match pid {
                0 => task.pgid.get() == my_pgid,
                -1 => task.tgid != 1 && task.tgid != me,
                p => task.pgid.get() == p.unsigned_abs(),
            };
            if chosen {
                deliver(task);
            }
        });
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

/// `tkill` and `tgkill`: a signal for the one thread `tid`, which `tgkill`
/// also requires to be a thread of the process `tgid`. The signal is that
/// thread's alone, as Linux sends it: no other thread takes it, and it waits
/// for this one to unblock it.
pub fn tgkill(tgid: Option<i32>, tid: i32, signal: i32) -> SysResult {
    if tid <= 0 || tgid.map_or(false, |tgid| tgid <= 0) {
        return Err(Errno::EINVAL);
    }
    let send = signal_to_send(signal)?;
    let found = sched::with_tasks(|table| {
        let task = table.find(tid as u32)?;
        if task.state() == State::Zombie || tgid.map_or(false, |tgid| task.tgid != tgid as u32) {
            return None;
        }
        if let Some(signal) = send {
            sched::post_signal(task, signal, table);
        }
        Some(())
    });
    match found {
        Some(()) => Ok(0),
        None => Err(Errno::ESRCH),
    }
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

/// The latest second the wall clock can be set to, the same limit Linux sets.
/// The clock is nanoseconds in a signed 64-bit word, which runs out in 2262,
/// and thirty years are kept back so that the monotonic clock added to a time
/// set at the limit cannot run it past the end within any machine's uptime.
const SETTABLE_SECONDS_MAX: i64 = i64::MAX / 1_000_000_000 - 30 * 365 * 86400;

/// Set the wall clock to `seconds` and `nanos` since the epoch, `nanos` already
/// checked to be under a second, and say so on the console.
fn set_wall_clock(seconds: i64, nanos: i64) -> SysResult {
    if !(0..=SETTABLE_SECONDS_MAX).contains(&seconds) {
        return Err(Errno::EINVAL);
    }
    let target = seconds * 1_000_000_000 + nanos;
    let was = crate::time::set_realtime(target);
    // One line per change, naming the program, so that a step to a wrong time
    // shows on the console and can be traced to what made it.
    let task = sched::current();
    crate::println!(
        "clock: set to {} by pid {} ({}); was {}, {}",
        crate::time::Utc(target),
        task.tgid,
        task.name(),
        crate::time::Utc(was),
        crate::time::Step(target - was)
    );
    Ok(0)
}

/// Only the wall clock can be set. The monotonic clock is what every wait in
/// the kernel is measured against, and Linux refuses to set it too.
pub fn clock_settime(clock: u64, value: u64) -> SysResult {
    if clock != CLOCK_REALTIME {
        return Err(Errno::EINVAL);
    }
    let spec: Timespec = uaccess::read_struct(value)?;
    if !(0..1_000_000_000).contains(&spec.tv_nsec) {
        return Err(Errno::EINVAL);
    }
    set_wall_clock(spec.tv_sec, spec.tv_nsec)
}

/// The time zone argument is not read. Linux keeps it only to hand back from
/// `gettimeofday`, which here always reports a zero zone, and it never moves
/// the clock, which is kept in UTC.
pub fn settimeofday(value: u64, _zone: u64) -> SysResult {
    if value == 0 {
        return Ok(0);
    }
    let tv: Timeval = uaccess::read_struct(value)?;
    if !(0..1_000_000).contains(&tv.tv_usec) {
        return Err(Errno::EINVAL);
    }
    set_wall_clock(tv.tv_sec, tv.tv_usec * 1000)
}

/// `clock_nanosleep`. A relative sleep is `nanosleep` whichever clock it names,
/// because a length of time is counted in the same ticks on every clock here.
///
/// An absolute one is a reading of the clock to wake at. It is turned into a
/// tick deadline, and on the wall clock that deadline is only good until the
/// clock is next set, so the sleep waits on the queue setting the clock wakes
/// and works the deadline out again against the new time when it is woken.
/// That is what Linux does: a sleeper whose time the clock was stepped past
/// wakes at the step, and one the clock was stepped back from sleeps on.
pub fn clock_nanosleep(clock: u64, flags: u32, req: u64, rem: u64) -> SysResult {
    if flags & TIMER_ABSTIME == 0 {
        return nanosleep(req, rem);
    }
    let spec: Timespec = uaccess::read_struct(req)?;
    if spec.tv_sec < 0 || !(0..1_000_000_000).contains(&spec.tv_nsec) {
        return Err(Errno::EINVAL);
    }
    let wake_at = spec.tv_sec.saturating_mul(1_000_000_000).saturating_add(spec.tv_nsec);
    let wall = clock == CLOCK_REALTIME;
    loop {
        if sched::stop_if_requested() {
            continue;
        }
        if sched::has_pending_signal() {
            return Err(Errno::EINTR);
        }
        // The count is read before the clock, so a step that lands after the
        // clock was read has already changed the count by the time the check
        // below runs with interrupts off.
        let changes = crate::time::realtime_changes();
        let now = if wall {
            crate::time::realtime_ns()
        } else {
            crate::time::monotonic_ns() as i64
        };
        if now >= wake_at {
            return Ok(0);
        }
        let deadline = super::deadline_in((wake_at - now) as u64);
        crate::time::REALTIME_SET.wait_until_or_at(deadline, || {
            (wall && crate::time::realtime_changes() != changes) || sched::has_pending_signal()
        });
    }
}

pub fn nanosleep(req: u64, rem: u64) -> SysResult {
    let spec: Timespec = uaccess::read_struct(req)?;
    if spec.tv_sec < 0 || spec.tv_nsec < 0 || spec.tv_nsec >= 1_000_000_000 {
        return Err(Errno::EINVAL);
    }
    let total_ns =
        (spec.tv_sec as u64).saturating_mul(1_000_000_000).saturating_add(spec.tv_nsec as u64);
    let ticks = super::wait_ticks(total_ns);
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

/// `struct itimerval`: the interval, then the value, each a `struct timeval`
/// of two 64-bit words, seconds and microseconds.
fn read_itimerval(addr: u64) -> Result<crate::itimer::Setting, Errno> {
    let words: [i64; 4] = uaccess::read_struct(addr)?;
    let nanos = |seconds: i64, micros: i64| -> Result<u64, Errno> {
        if seconds < 0 || !(0..1_000_000).contains(&micros) {
            return Err(Errno::EINVAL);
        }
        Ok((seconds as u64)
            .saturating_mul(1_000_000_000)
            .saturating_add(micros as u64 * 1_000))
    };
    Ok(crate::itimer::Setting {
        interval_ns: nanos(words[0], words[1])?,
        value_ns: nanos(words[2], words[3])?,
    })
}

fn write_itimerval(addr: u64, setting: crate::itimer::Setting) -> Result<(), Errno> {
    let seconds = |nanos: u64| (nanos / 1_000_000_000) as i64;
    let micros = |nanos: u64| (nanos % 1_000_000_000 / 1_000) as i64;
    let words: [i64; 4] = [
        seconds(setting.interval_ns),
        micros(setting.interval_ns),
        seconds(setting.value_ns),
        micros(setting.value_ns),
    ];
    uaccess::write_struct(addr, &words)
}

pub fn setitimer(which: i32, new: u64, old: u64) -> SysResult {
    // Linux reads a missing new setting as one that disarms the timer.
    let setting = if new == 0 {
        crate::itimer::Setting::default()
    } else {
        read_itimerval(new)?
    };
    let previous = crate::itimer::set(which, setting)?;
    if old != 0 {
        write_itimerval(old, previous)?;
    }
    Ok(0)
}

pub fn getitimer(which: i32, out: u64) -> SysResult {
    write_itimerval(out, crate::itimer::get(which)?)?;
    Ok(0)
}

/// `alarm`: the real timer, set to whole seconds and firing once.
///
/// The answer is what was left of the timer it replaced, in seconds rounded to
/// the nearest, and at least one while a timer was armed at all, because zero
/// would say there was none. Linux rounds the same way.
pub fn alarm(seconds: u32) -> SysResult {
    let setting = crate::itimer::Setting {
        value_ns: seconds as u64 * 1_000_000_000,
        interval_ns: 0,
    };
    let previous = crate::itimer::set(crate::itimer::ITIMER_REAL, setting)?;
    let whole = previous.value_ns / 1_000_000_000;
    let part = previous.value_ns % 1_000_000_000;
    let rounded = if (whole == 0 && part != 0) || part >= 500_000_000 { whole + 1 } else { whole };
    Ok(rounded)
}

pub fn getrandom(buf: u64, len: usize) -> SysResult {
    let mut bytes = alloc::vec![0u8; len.min(1 << 20)];
    crate::fs::dev::fill_random(&mut bytes);
    uaccess::write_bytes(buf, &bytes)?;
    Ok(bytes.len() as u64)
}

pub fn rt_sigaction(signal: i32, act: u64, old: u64) -> SysResult {
    let signal = Signal::from_number(signal).ok_or(Errno::EINVAL)?;
    // Neither of these has a disposition to set: they act on the task.
    if signal == SIGKILL || signal == SIGSTOP {
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

/// `sigaltstack`: report the stack a handler would run on, and name a new one.
///
/// `sp` is where the program's stack pointer is now, which is what says
/// whether it is running on the stack it is asking about: one that is in use
/// may be reported but not replaced, because replacing it would leave the
/// handler running on memory nothing accounts for any more.
pub fn sigaltstack(new: u64, old: u64, sp: u64) -> SysResult {
    let task = sched::current();
    let current = task.sig_stack.get();
    let on_it = current.contains(sp);

    if old != 0 {
        let reported = SigAltStack {
            ss_sp: current.ss_sp,
            ss_flags: current.flags_at(sp),
            _pad: 0,
            ss_size: current.ss_size,
        };
        uaccess::write_struct(old, &reported)?;
    }
    if new == 0 {
        return Ok(0);
    }
    if on_it {
        return Err(Errno::EPERM);
    }

    let asked: SigAltStack = uaccess::read_struct(new)?;
    if asked.ss_flags & !(SS_ONSTACK | SS_DISABLE) != 0 {
        return Err(Errno::EINVAL);
    }
    if asked.ss_flags & SS_DISABLE != 0 {
        task.sig_stack.set(SigAltStack::default());
        return Ok(0);
    }
    if asked.ss_size < arch::MIN_ALT_STACK {
        return Err(Errno::ENOMEM);
    }
    task.sig_stack.set(SigAltStack {
        ss_sp: asked.ss_sp,
        ss_flags: 0,
        _pad: 0,
        ss_size: asked.ss_size,
    });
    Ok(0)
}

pub fn rt_sigreturn(frame: &mut TrapFrame) -> SysResult {
    crate::signal::sigreturn(&sched::current(), frame)
}

pub fn rt_sigprocmask(how: u32, set: u64, old: u64) -> SysResult {
    let task = sched::current();
    if old != 0 {
        uaccess::write_u64_in(&task, old, task.blocked())?;
    }
    if set != 0 {
        let value = uaccess::read_u64_in(&task, set)?;
        let before = task.blocked();
        match how {
            0 => task.block(value),       // SIG_BLOCK
            1 => task.unblock(value),     // SIG_UNBLOCK
            _ => task.set_blocked(value), // SIG_SETMASK
        }
        // A signal sent to the process may be waiting for this thread to take
        // it, and this thread has just said it will not.
        let newly_blocked = task.blocked() & !before;
        if newly_blocked & task.shared_pending() != 0 {
            sched::with_tasks(|table| sched::retarget_shared_pending(&task, newly_blocked, table));
        }
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
                let ns = (spec.tv_sec as u64)
                    .saturating_mul(1_000_000_000)
                    .saturating_add(spec.tv_nsec as u64);
                super::deadline_in(ns)
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
