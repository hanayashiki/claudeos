//! Round-robin scheduler.

use crate::abi::*;
use crate::cpu::idt::{enter_user_mode, TrapFrame};
use crate::cpu::{gdt, msr, per_cpu};
use crate::sync::{disable_interrupts, enable_interrupts, interrupts_enabled, Spinlock};
use crate::task::{State, Task, TaskPtr};
use alloc::boxed::Box;
use alloc::vec::Vec;

extern "C" {
    fn switch_context(save_rsp: *mut u64, new_rsp: u64);
}

static mut CURRENT: *mut Task = core::ptr::null_mut();
static mut IDLE: *mut Task = core::ptr::null_mut();
static TASKS: Spinlock<Vec<TaskPtr>> = Spinlock::new(Vec::new());
static FOREGROUND_PGID: Spinlock<u32> = Spinlock::new(0);

pub fn current() -> &'static mut Task {
    unsafe {
        debug_assert!(!CURRENT.is_null());
        &mut *CURRENT
    }
}

pub fn current_ptr() -> *mut Task {
    unsafe { CURRENT }
}

pub fn has_current() -> bool {
    unsafe { !CURRENT.is_null() }
}

/// Adopt the boot context as the idle task.
pub fn init() {
    let space = crate::mm::paging::AddressSpace::current();
    let mut idle = Task::new("idle", space).expect("idle task");
    idle.pid = 0;
    idle.tgid = 0;
    idle.state = State::Runnable;
    let ptr = Box::into_raw(idle);
    unsafe {
        CURRENT = ptr;
        IDLE = ptr;
    }
    TASKS.lock().push(TaskPtr(ptr));
    crate::task::reset_pid_counter(1);
}

pub fn register(task: Box<Task>) -> u32 {
    let pid = task.pid;
    let ptr = Box::into_raw(task);
    TASKS.lock().push(TaskPtr(ptr));
    crate::fs::procfs::add_process(pid);
    pid
}

pub fn find(pid: u32) -> Option<&'static mut Task> {
    let tasks = TASKS.lock();
    tasks.iter().find(|t| t.get().pid == pid).map(|t| t.get())
}

pub fn task_count() -> usize {
    TASKS.lock().len()
}

pub fn for_each<F: FnMut(&'static mut Task)>(mut f: F) {
    let tasks = TASKS.lock();
    for t in tasks.iter() {
        f(t.get());
    }
}

pub fn set_foreground(pgid: u32) {
    *FOREGROUND_PGID.lock() = pgid;
}

pub fn foreground() -> u32 {
    *FOREGROUND_PGID.lock()
}

pub fn current_pgid() -> u32 {
    current().pgid
}

/// True when a task other than the current one is running on `pml4`.
pub fn space_in_use(pml4: u64) -> bool {
    let cur = unsafe { CURRENT };
    let tasks = TASKS.lock();
    tasks
        .iter()
        .any(|t| t.0 != cur && t.get().space.pml4 == pml4 && t.get().state != State::Zombie)
}

fn pick_next() -> Option<*mut Task> {
    let tasks = TASKS.lock();
    let now = crate::trap::ticks();
    let idle = unsafe { IDLE };

    for entry in tasks.iter() {
        let task = entry.get();
        if task.state == State::Sleeping && task.wake_at != 0 && now >= task.wake_at {
            task.wake_at = 0;
            task.state = State::Runnable;
        }
    }

    let cur = unsafe { CURRENT };
    let len = tasks.len();
    if len == 0 {
        return None;
    }
    let start = tasks.iter().position(|t| t.0 == cur).unwrap_or(0);

    for k in 1..=len {
        let entry = &tasks[(start + k) % len];
        if entry.0 == idle {
            continue;
        }
        let task = entry.get();
        if task.state == State::Runnable {
            if entry.0 == cur {
                return None; // already running the only candidate
            }
            return Some(entry.0);
        }
    }

    let current = unsafe { &mut *cur };
    if cur != idle && current.state == State::Runnable {
        return None;
    }
    if cur == idle {
        return None;
    }
    Some(idle)
}

unsafe fn switch_to(next: *mut Task) {
    let prev = CURRENT;
    if prev == next {
        return;
    }
    let prev_task = &mut *prev;
    let next_task = &mut *next;

    // The user's FS/GS bases live in MSRs; carry them with the task.
    prev_task.fs_base = msr::read(msr::IA32_FS_BASE);
    prev_task.gs_base = msr::read(msr::IA32_KERNEL_GS_BASE);
    msr::write(msr::IA32_FS_BASE, next_task.fs_base);
    msr::write(msr::IA32_KERNEL_GS_BASE, next_task.gs_base);

    // Entry from user mode must land on the incoming task's kernel stack.
    gdt::set_kernel_stack(next_task.kstack_top);
    per_cpu().kernel_rsp = next_task.kstack_top;
    per_cpu().current = next as u64;

    if next_task.space.pml4 != prev_task.space.pml4 {
        next_task.space.switch_to();
    }

    CURRENT = next;
    prev_task.started = true;
    switch_context(&mut prev_task.rsp, next_task.rsp);
}

pub fn schedule() {
    let was_enabled = interrupts_enabled();
    disable_interrupts();
    if let Some(next) = pick_next() {
        unsafe { switch_to(next) };
    }
    if was_enabled {
        enable_interrupts();
    }
}

pub fn yield_now() {
    schedule();
}

pub fn on_tick() {
    if !has_current() {
        return;
    }
    schedule();
}

/// Put the current task to sleep for `ticks` timer ticks.
pub fn sleep_ticks(ticks: u64) {
    let task = current();
    task.wake_at = crate::trap::ticks() + ticks.max(1);
    task.state = State::Sleeping;
    schedule();
}

/// Block until `predicate` holds, yielding in between.
pub fn wait_until<F: FnMut() -> bool>(mut predicate: F) {
    while !predicate() {
        yield_now();
    }
}

pub fn wake(pid: u32) {
    if let Some(task) = find(pid) {
        if task.state == State::Sleeping {
            task.state = State::Runnable;
            task.wake_at = 0;
        }
    }
}

/// Hand `addr` to the current task's region list to be backed with memory.
pub fn handle_user_page_fault(addr: u64, code: u64, _frame: &mut TrapFrame) -> bool {
    if !has_current() {
        return false;
    }
    // A protection violation on a present page is never satisfied by mapping.
    if code & 1 != 0 {
        return false;
    }
    current().fault_in(addr)
}

/// Terminate the current task. `status` is already encoded the way wait4
/// reports it: exit codes in bits 8..15, a killing signal in bits 0..6.
pub fn exit_current(status: i32) -> ! {
    {
        let task = current();
        task.exit_code = status;
        task.fds.entries.clear();
        task.fds.cloexec.clear();

        // Threads share an address space; only the last one out frees it.
        let pml4 = task.space.pml4;
        let mut sharers = 0usize;
        for_each(|other| {
            if other.space.pml4 == pml4 && other.state != State::Zombie {
                sharers += 1;
            }
        });
        if sharers <= 1 {
            task.space.free_user_memory();
        }
        task.vmas.clear();
        task.state = State::Zombie;

        let ppid = task.ppid;
        let pid = task.pid;

        if let Some(parent_pid) = task.vfork_parent.take() {
            if let Some(parent) = find(parent_pid) {
                if parent.state == State::Sleeping {
                    parent.state = State::Runnable;
                    parent.wake_at = 0;
                }
            }
        }

        if pid == 1 {
            crate::println!();
            crate::println!(
                "claudeos: init exited with status {:#x}; powering off",
                task.exit_code
            );
            crate::power_off();
        }

        // Orphans are adopted by init.
        for_each(|other| {
            if other.ppid == pid {
                other.ppid = 1;
            }
        });

        if let Some(parent) = find(ppid) {
            if parent.state == State::Sleeping {
                let target = parent.waiting_for;
                let matches = match target {
                    None => false,
                    Some(-1) => true,
                    Some(want) if want as u32 == pid => true,
                    Some(_) => false,
                };
                if matches || target == Some(-1) {
                    parent.state = State::Runnable;
                    parent.wake_at = 0;
                }
            }
        }
    }
    loop {
        schedule();
        crate::cpu::halt();
    }
}

/// Terminate the current task as if `signal` had killed it.
pub fn kill_current(signal: i32) -> ! {
    let name = current().name.clone();
    let pid = current().pid;
    crate::println!("[kernel] pid {} ({}) killed by signal {}", pid, name, signal);
    exit_current(signal & 0x7F)
}

/// Mark every task in the foreground group as having a pending signal.
pub fn signal_foreground(signal: i32) {
    let pgid = foreground();
    if pgid == 0 {
        return;
    }
    for_each(|task| {
        if task.pgid == pgid && task.pid != 1 && task.state != State::Zombie {
            task.pending_signals |= 1u64 << (signal as u64 & 63);
            if task.state == State::Sleeping {
                task.state = State::Runnable;
                task.wake_at = 0;
            }
        }
    });
}

/// Act on any pending signal before returning to user mode.
pub fn check_signals() {
    if !has_current() {
        return;
    }
    let task = current();
    if task.pending_signals == 0 {
        return;
    }
    for signal in [SIGKILL, SIGINT, SIGTERM, SIGQUIT, SIGHUP, SIGSEGV, SIGPIPE, SIGABRT] {
        let bit = 1u64 << (signal as u64 & 63);
        if task.pending_signals & bit != 0 {
            task.pending_signals &= !bit;
            // Only the default action is implemented, and SIGKILL cannot be
            // caught in any case.
            let handler = task.signal_handlers[signal as usize];
            if signal != SIGKILL && (handler == 1 /* SIG_IGN */) {
                continue;
            }
            exit_current(signal & 0x7F);
        }
    }
}

/// Collect a finished child. Returns (pid, exit code).
pub fn reap_child(parent_pid: u32, want: i32) -> Option<(u32, i32)> {
    let mut found: Option<(u32, i32, *mut Task)> = None;
    {
        let tasks = TASKS.lock();
        for entry in tasks.iter() {
            let task = entry.get();
            if task.ppid != parent_pid || task.state != State::Zombie {
                continue;
            }
            if want > 0 && task.pid != want as u32 {
                continue;
            }
            found = Some((task.pid, task.exit_code, entry.0));
            break;
        }
    }
    let (pid, code, ptr) = found?;
    {
        let mut tasks = TASKS.lock();
        tasks.retain(|t| t.0 != ptr);
    }
    crate::fs::procfs::remove_process(pid);
    unsafe {
        let mut task = Box::from_raw(ptr);
        let pml4 = task.space.pml4;
        let shared = {
            let tasks = TASKS.lock();
            tasks.iter().any(|t| t.get().space.pml4 == pml4)
        };
        if !shared {
            task.space.destroy();
        }
        task.free_kernel_stack();
        task.state = State::Dead;
        drop(task);
    }
    Some((pid, code))
}

/// True when the parent has at least one live child matching `want`.
pub fn has_children(parent_pid: u32, want: i32) -> bool {
    let tasks = TASKS.lock();
    tasks.iter().any(|t| {
        let task = t.get();
        task.ppid == parent_pid && (want <= 0 || task.pid == want as u32)
    })
}

/// First entry into user mode for a newly created task.
pub extern "C" fn user_entry_trampoline() -> ! {
    let task = current();
    let frame = task.trap_frame();
    unsafe { enter_user_mode(frame) }
}

/// Idle loop: run when nothing else can.
pub fn idle_loop() -> ! {
    loop {
        enable_interrupts();
        crate::cpu::halt();
        schedule();
    }
}
