//! Process, time and signal system calls.

use crate::abi::*;
use crate::cpu::idt::TrapFrame;
use crate::cpu::msr;
use crate::elf;
use crate::mm::paging::AddressSpace;
use crate::sched;
use crate::task::{self, State, Task};
use crate::uaccess;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

pub fn arch_prctl(code: u64, addr: u64) -> SysResult {
    match code {
        ARCH_SET_FS => {
            msr::write(msr::IA32_FS_BASE, addr);
            sched::current().fs_base = addr;
            Ok(0)
        }
        ARCH_SET_GS => {
            // The user GS base lives in KERNEL_GS_BASE while we are in the
            // kernel; swapgs puts it back on the way out.
            msr::write(msr::IA32_KERNEL_GS_BASE, addr);
            sched::current().gs_base = addr;
            Ok(0)
        }
        ARCH_GET_FS => {
            uaccess::write_u64(addr, msr::read(msr::IA32_FS_BASE))?;
            Ok(0)
        }
        ARCH_GET_GS => {
            uaccess::write_u64(addr, msr::read(msr::IA32_KERNEL_GS_BASE))?;
            Ok(0)
        }
        _ => Err(Errno::EINVAL),
    }
}

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

    let space = if share_vm {
        parent.space
    } else {
        let space = AddressSpace::new_user().ok_or(Errno::ENOMEM)?;
        space.clone_user_from(&parent.space).map_err(|_| Errno::ENOMEM)?;
        space
    };

    let mut child = Task::new(&parent.name, space).ok_or(Errno::ENOMEM)?;
    let child_pid = child.pid;
    let is_thread = flags & CLONE_THREAD != 0;

    child.ppid = if is_thread { parent.ppid } else { parent.pid };
    child.tgid = if is_thread { parent.tgid } else { child_pid };
    child.pgid = parent.pgid;
    child.cwd = parent.cwd.clone();
    child.exe_path = parent.exe_path.clone();
    child.name = parent.name.clone();
    child.fds = parent.fds.clone_table();
    child.brk_start = parent.brk_start;
    child.brk = parent.brk;
    child.mmap_top = parent.mmap_top;
    child.vmas = parent.vmas.clone();
    child.umask = parent.umask;
    child.signal_handlers = parent.signal_handlers;
    child.fs_base = if flags & CLONE_SETTLS != 0 {
        tls
    } else {
        msr::read(msr::IA32_FS_BASE)
    };
    child.gs_base = msr::read(msr::IA32_KERNEL_GS_BASE);
    if flags & CLONE_CHILD_CLEARTID != 0 {
        child.clear_child_tid = child_tid;
    }

    // The child resumes from the same point with a zero return value.
    unsafe {
        let dst = child.trap_frame();
        core::ptr::write(dst, *frame);
        (*dst).rax = 0;
        if stack != 0 {
            (*dst).rsp = stack;
        }
    }
    child.prepare_kernel_frame(sched::user_entry_trampoline as usize as u64);

    // Only safe to write through these pointers while the address space is
    // shared with the parent we are running in.
    if share_vm {
        if flags & CLONE_PARENT_SETTID != 0 && parent_tid != 0 {
            let _ = uaccess::write_u32(parent_tid, child_pid);
        }
        if flags & CLONE_CHILD_SETTID != 0 && child_tid != 0 {
            let _ = uaccess::write_u32(child_tid, child_pid);
        }
    }

    let vfork = flags & CLONE_VFORK != 0;
    if vfork {
        child.vfork_parent = Some(parent.pid);
    }

    parent.children.push(child_pid);
    sched::register(child);

    if vfork {
        // vfork's contract: the parent does not run again until the child has
        // handed back its address space by exec'ing or exiting.
        let parent = sched::current();
        parent.state = State::Sleeping;
        sched::schedule();
    }
    Ok(child_pid as u64)
}

/// Replace the current task's program image.
pub fn exec_into_current(
    path: &str,
    mut argv: Vec<String>,
    envp: Vec<String>,
) -> Result<(), Errno> {
    let (data, shebang) = task::read_executable(path)?;
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

    elf::validate(&data)?;

    let old_space = sched::current().space;
    let new_space = AddressSpace::new_user().ok_or(Errno::ENOMEM)?;

    // Everything below runs against the new address space; the kernel half is
    // shared so the stack and heap stay valid across the switch.
    unsafe { new_space.switch_to() };
    let task = sched::current();
    task.space = new_space;
    task.vmas.clear();
    task.mmap_top = crate::mm::USER_MMAP_BASE;

    let image = match elf::load(&new_space, &data) {
        Ok(image) => image,
        Err(err) => {
            task.space = old_space;
            unsafe { old_space.switch_to() };
            new_space.destroy();
            return Err(err);
        }
    };

    task.brk_start = image.brk_start;
    task.brk = image.brk_start;

    let sp = match task::build_user_stack(task, &image, &argv, &envp, &exec_path) {
        Ok(sp) => sp,
        Err(err) => {
            task.space = old_space;
            unsafe { old_space.switch_to() };
            new_space.destroy();
            return Err(err);
        }
    };

    // The old image is unreachable from this task. Under CLONE_VM another
    // task is still running on it, so only the last user tears it down.
    if old_space.pml4 != new_space.pml4 && !sched::space_in_use(old_space.pml4) {
        old_space.destroy();
    }

    // A vfork parent may resume as soon as the address space is handed back.
    if let Some(parent_pid) = task.vfork_parent.take() {
        sched::wake(parent_pid);
    }

    task.fds.close_on_exec();
    task.fs_base = 0;
    msr::write(msr::IA32_FS_BASE, 0);
    task.name = exec_path
        .rsplit('/')
        .next()
        .unwrap_or(&exec_path)
        .to_string();
    task.exe_path = exec_path;
    task.signal_handlers = [0; 64];

    task::set_user_entry(task, image.entry, sp);
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
    Ok(frame.rax)
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
        if !sched::has_children(me, pid as i32) {
            return Err(Errno::ECHILD);
        }
        if options & WNOHANG != 0 {
            return Ok(0);
        }
        let task = sched::current();
        task.waiting_for = Some(pid as i32);
        task.state = State::Sleeping;
        sched::schedule();
        sched::current().waiting_for = None;
        sched::check_signals();
    }
}

pub fn kill(pid: i64, signal: i32) -> SysResult {
    if signal == 0 {
        return Ok(0);
    }
    let mut delivered = false;
    let me = sched::current().pid;
    sched::for_each(|task| {
        let target = match pid {
            p if p > 0 => task.pid == p as u32,
            0 => task.pgid == sched::current_pgid(),
            -1 => task.pid != me && task.pid != 0,
            p => task.pgid == (-p) as u32,
        };
        if target && task.state != State::Zombie && task.pid != 0 {
            task.pending_signals |= 1u64 << (signal as u64 & 63);
            if task.state == State::Sleeping {
                task.state = State::Runnable;
                task.wake_at = 0;
            }
            delivered = true;
        }
    });
    if delivered {
        sched::check_signals();
        Ok(0)
    } else {
        Err(Errno::ESRCH)
    }
}

pub fn setpgid(pid: u32, pgid: u32) -> SysResult {
    let target = if pid == 0 { sched::current().pid } else { pid };
    let value = if pgid == 0 { target } else { pgid };
    match sched::find(target) {
        Some(task) => {
            task.pgid = value;
            Ok(0)
        }
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
    fill(&mut uts.machine, "x86_64");
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
        let res = Timespec { tv_sec: 0, tv_nsec: 1_000_000_000 / crate::cpu::pit::TICK_HZ as i64 };
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
    if ticks > 0 {
        sched::sleep_ticks(ticks);
    } else {
        sched::yield_now();
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
    if signal >= 64 || signal == SIGKILL as usize || signal == SIGSTOP as usize {
        return Err(Errno::EINVAL);
    }
    let task = sched::current();
    if old != 0 {
        let mut buf = [0u8; 32];
        buf[..8].copy_from_slice(&task.signal_handlers[signal].to_le_bytes());
        uaccess::write_bytes(old, &buf)?;
    }
    if act != 0 {
        task.signal_handlers[signal] = uaccess::read_u64(act)?;
    }
    Ok(0)
}

pub fn rt_sigprocmask(how: u32, set: u64, old: u64) -> SysResult {
    let task = sched::current();
    if old != 0 {
        uaccess::write_u64(old, task.signal_mask)?;
    }
    if set != 0 {
        let value = uaccess::read_u64(set)?;
        task.signal_mask = match how {
            0 => task.signal_mask | value,  // SIG_BLOCK
            1 => task.signal_mask & !value, // SIG_UNBLOCK
            _ => value,                     // SIG_SETMASK
        };
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
            while uaccess::read_u32(uaddr)? == val {
                if crate::trap::ticks() >= deadline {
                    return Err(Errno::ETIMEDOUT);
                }
                sched::yield_now();
                sched::check_signals();
            }
            Ok(0)
        }
        FUTEX_WAKE => {
            // Waiters re-read the word themselves, so nothing to do here.
            sched::yield_now();
            Ok(0)
        }
        _ => Err(Errno::ENOSYS),
    }
}
