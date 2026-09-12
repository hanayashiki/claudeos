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

/// The task the CPU is running, as a guard rather than a reference.
///
/// Which task that is changes at every context switch, so handing out a
/// `&'static mut Task` says something that is not true: the borrow would
/// outlive the state it describes. The guard reads the current task on each
/// use and cannot be stored anywhere with a longer life.
pub struct Current(());

impl core::ops::Deref for Current {
    type Target = Task;

    fn deref(&self) -> &Task {
        unsafe {
            debug_assert!(!CURRENT.is_null());
            &*CURRENT
        }
    }
}

impl core::ops::DerefMut for Current {
    fn deref_mut(&mut self) -> &mut Task {
        unsafe {
            debug_assert!(!CURRENT.is_null());
            &mut *CURRENT
        }
    }
}

pub fn current() -> Current {
    Current(())
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

/// The running task's pid, or zero before there is one.
pub fn current_pid() -> u32 {
    if has_current() {
        current().pid
    } else {
        0
    }
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

    // So do the floating point and vector registers. The kernel never uses
    // them, so what is in them here is still the outgoing task's.
    prev_task.fpu.save();
    next_task.fpu.restore();

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

/// True when some other task could run right now.
pub fn other_runnable() -> bool {
    let cur = unsafe { CURRENT };
    let idle = unsafe { IDLE };
    let tasks = TASKS.lock();
    tasks.iter().any(|t| {
        t.0 != cur && t.0 != idle && t.get().state == State::Runnable
    })
}

/// Give up the CPU from a loop that is waiting for something it cannot be
/// woken for. Hands over to another task when there is one, and otherwise
/// sleeps for a tick rather than spinning the core.
pub fn yield_or_sleep() {
    if other_runnable() {
        schedule();
    } else {
        sleep_ticks(1);
    }
}

/// Ticks that found the machine with nothing to run. The difference between
/// this and the uptime is the time something was actually on the CPU.
static IDLE_TICKS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

pub fn idle_ticks() -> u64 {
    IDLE_TICKS.load(core::sync::atomic::Ordering::Relaxed)
}

pub fn on_tick() {
    if !has_current() {
        return;
    }
    if unsafe { CURRENT == IDLE } {
        IDLE_TICKS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    }
    schedule();
}

/// Put the current task to sleep for `ticks` timer ticks.
pub fn sleep_ticks(ticks: u64) {
    let mut task = current();
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
    if code & 1 != 0 {
        // The page is present, so the only fault that can be repaired is a
        // write to a page still shared with another address space.
        if code & 2 != 0 {
            return current().handle_cow(addr);
        }
        return false;
    }
    current().fault_in(addr)
}

/// Terminate the current task. `status` is already encoded the way wait4
/// reports it: exit codes in bits 8..15, a killing signal in bits 0..6.
/// Terminate every task in `tgid`'s thread group except the caller, then the
/// caller itself.
pub fn exit_group(status: i32) -> ! {
    let tgid = current().tgid;
    let me = current().pid;
    for_each(|task| {
        if task.tgid == tgid && task.pid != me && task.state != State::Zombie {
            task.pending_signals |= 1u64 << (SIGKILL as u64 & 63);
            if task.state == State::Sleeping {
                task.state = State::Runnable;
                task.wake_at = 0;
            }
        }
    });
    exit_current(status)
}

pub fn exit_current(status: i32) -> ! {
    {
        let mut task = current();
        task.exit_code = status;

        // A thread that asked for it gets its tid slot cleared so whoever is
        // joining on it can see that it finished. This goes through the
        // checked path: the page may be shared read-only after a fork, and a
        // bare store would fault inside the kernel.
        if task.clear_child_tid != 0 {
            let address = task.clear_child_tid;
            task.clear_child_tid = 0;
            let _ = crate::uaccess::write_u32(address, 0);
        }
        task.fds.entries.clear();
        task.fds.cloexec.clear();

        // Threads share an address space and its region list; only the last
        // thread out may tear either of them down.
        let last_thread = alloc::sync::Arc::strong_count(&task.mm) == 1;
        if last_thread {
            task.space.free_user_memory();
            task.clear_vmas();
        }
        let ppid = task.ppid;
        let pid = task.pid;

        if pid == 1 {
            crate::println!();
            crate::println!(
                "claudeos: init exited with status {:#x}; powering off",
                task.exit_code
            );
            crate::power_off();
        }

        // Becoming a zombie takes this task off the run queue for good, so
        // everything that has to be done on its behalf has to be done in the
        // same breath. A timer landing between the two would hand the CPU to
        // something else and never hand it back, leaving the parent's
        // wake-up undelivered by a task that can no longer deliver it.
        crate::sync::without_interrupts(|| {
            let mut task = current();
            task.state = State::Zombie;

            if let Some(parent_pid) = task.vfork_parent.take() {
                if let Some(parent) = find(parent_pid) {
                    if parent.state == State::Sleeping {
                        parent.state = State::Runnable;
                        parent.wake_at = 0;
                    }
                }
            }

            // Orphans are adopted by init. One that has already exited still
            // needs reaping, and init is normally asleep in wait4, so it has
            // to be woken here; nothing else will report the adopted zombie
            // to it.
            let mut adopted_zombie = false;
            for_each(|other| {
                if other.ppid == pid {
                    other.ppid = 1;
                    if other.state == State::Zombie {
                        adopted_zombie = true;
                    }
                }
            });
            if adopted_zombie && ppid != 1 {
                if let Some(init) = find(1) {
                    init.pending_signals |= 1u64 << (SIGCHLD as u64 & 63);
                    if init.state == State::Sleeping && init.waiting_for.is_some() {
                        init.state = State::Runnable;
                        init.wake_at = 0;
                    }
                }
            }

            notify_parent(ppid);
        });
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

/// Raise a signal on the running task.
pub fn raise_on_current(signal: i32) {
    if !has_current() {
        return;
    }
    current().pending_signals |= 1u64 << (signal as u64 & 63);
}

/// Tell `ppid` that one of its children changed state, and wake it if it is
/// blocked in wait. Takes the task list, so it must not be called from inside
/// `for_each`.
pub fn notify_parent(ppid: u32) {
    if let Some(parent) = find(ppid) {
        parent.pending_signals |= 1u64 << (SIGCHLD as u64 & 63);
        if parent.state == State::Sleeping && parent.waiting_for.is_some() {
            parent.state = State::Runnable;
            parent.wake_at = 0;
        }
    }
}

/// Make `signal` pending on `task`.
///
/// Two of the job-control rules act on the task rather than on the handler: a
/// continue restarts a stopped task and discards a stop that has not been
/// taken yet, and a stop discards a continue the same way. Returns the parent
/// to notify when the task was actually restarted.
pub fn post_signal(task: &mut Task, signal: i32) -> Option<u32> {
    const STOPS: u64 = (1 << (SIGSTOP as u64 & 63))
        | (1 << (SIGTSTP as u64 & 63))
        | (1 << (SIGTTIN as u64 & 63))
        | (1 << (SIGTTOU as u64 & 63));
    let mut restarted = None;
    if signal == SIGKILL && task.state == State::Stopped {
        // Nothing else gets a stopped task running again, and a kill it can
        // never look at is not a kill.
        task.state = State::Runnable;
        task.wake_at = 0;
    }
    if signal == SIGCONT {
        task.pending_signals &= !STOPS;
        if task.state == State::Stopped {
            task.state = State::Runnable;
            task.wake_at = 0;
            task.report_continue = true;
            restarted = Some(task.ppid);
        }
    } else if crate::abi::is_stop_signal(signal) {
        task.pending_signals &= !(1u64 << (SIGCONT as u64 & 63));
    }
    task.pending_signals |= 1u64 << (signal as u64 & 63);
    if task.state == State::Sleeping {
        task.state = State::Runnable;
        task.wake_at = 0;
    }
    restarted
}

/// Stop the running task until something sends it SIGCONT.
fn stop_current(signal: i32) {
    let mut task = current();
    task.stop_signal = signal;
    task.report_stop = true;
    task.state = State::Stopped;
    let ppid = task.ppid;
    notify_parent(ppid);
    schedule();
}

/// Woken whenever a descriptor changes what it would report to a poll: bytes
/// arrive or are taken, a counter is posted, an end is closed.
///
/// Waiting on one queue per descriptor would mean queueing on several at once,
/// which nothing here can do. One queue for all of them means a waiter looks
/// again more often than it has to, but never sleeps through a change.
pub static IO_READY: WaitQueue = WaitQueue::new();

/// Tell anyone waiting in poll, select or epoll to look again.
pub fn io_ready() {
    IO_READY.wake_all();
}

/// Stop the running task here, rather than on the way back to user mode.
///
/// A syscall that cannot make progress until the job is continued calls this
/// instead of failing, so the operation resumes where it left off. Only safe
/// from a point in the kernel that holds no locks.
pub fn stop_for_signal(signal: i32) {
    current().pending_signals &= !(1u64 << (signal as u64 & 63));
    stop_current(signal);
    // The continue that restarted this task has now had its default action,
    // which is to do exactly that. Leaving it pending would make the caller
    // think a signal is waiting and give up on what it was doing.
    let mut task = current();
    if task.signal_actions[SIGCONT as usize].handler == crate::signal::SIG_DFL {
        task.pending_signals &= !(1u64 << (SIGCONT as u64 & 63));
    }
}

/// Take a pending job-control stop here, if there is one. Returns true once
/// the task has been continued, so the caller can carry on where it was.
///
/// A syscall that is waiting for time to pass calls this rather than failing:
/// being suspended is not an error, and the operation still has the rest of
/// its wait to do afterwards.
pub fn stop_if_requested() -> bool {
    let task = current();
    for signal in [SIGSTOP, SIGTSTP, SIGTTIN, SIGTTOU] {
        if task.pending_signals & (1u64 << (signal as u64 & 63)) == 0 {
            continue;
        }
        if signal != SIGSTOP
            && task.signal_actions[signal as usize].handler != crate::signal::SIG_DFL
        {
            continue;
        }
        stop_for_signal(signal);
        return true;
    }
    false
}

/// Mark every task in the foreground group as having a pending signal.
pub fn signal_foreground(signal: i32) {
    signal_group(foreground(), signal);
}

/// Mark every task in `pgid` as having a pending signal.
pub fn signal_group(pgid: u32, signal: i32) {
    if pgid == 0 {
        return;
    }
    let mut parents = Vec::new();
    for_each(|task| {
        if task.pgid == pgid && task.pid != 1 && task.state != State::Zombie {
            if let Some(ppid) = post_signal(task, signal) {
                parents.push(ppid);
            }
        }
    });
    for ppid in parents {
        notify_parent(ppid);
    }
}

/// True when a signal is waiting that the task has not blocked.
/// True when a signal is waiting that will actually do something.
///
/// A blocking call gives up with EINTR when this holds, so a signal that would
/// be discarded on delivery must not count: SIGCHLD from a finished background
/// job is pending on most processes most of the time, and treating it as a
/// reason to fail turns unrelated reads and opens into spurious errors.
pub fn has_pending_signal() -> bool {
    if !has_current() {
        return false;
    }
    let task = current();
    let mut pending = task.pending_signals & !task.signal_mask;
    // Neither of these can be blocked.
    pending |= task.pending_signals
        & ((1u64 << (SIGKILL as u64 & 63)) | (1u64 << (SIGSTOP as u64 & 63)));
    if pending == 0 {
        return false;
    }
    for signal in 1..64i32 {
        if pending & (1u64 << (signal as u64 & 63)) == 0 {
            continue;
        }
        let handler = task.signal_actions[signal as usize].handler;
        if handler == crate::signal::SIG_IGN {
            continue;
        }
        if handler == crate::signal::SIG_DFL && crate::signal::default_is_ignore(signal) {
            continue;
        }
        return true;
    }
    false
}

/// Act on pending signals before returning to user mode. Called once the
/// syscall result has been stored, so a handler may run on the way out.
pub fn check_signals() {
    if !has_current() {
        return;
    }
    let mut task = current();
    if task.pending_signals == 0 {
        return;
    }

    for signal in 1..64i32 {
        let bit = 1u64 << (signal as u64 & 63);
        if task.pending_signals & bit == 0 {
            continue;
        }
        let blocked = task.signal_mask & bit != 0;
        if blocked && signal != SIGKILL && signal != SIGSTOP {
            continue;
        }
        task.pending_signals &= !bit;

        if signal == SIGKILL {
            exit_current(signal & 0x7F);
        }

        // Stopping and running a handler both leave the kernel in the middle
        // of whatever it was doing, so both wait until the task is on its way
        // back to user mode; until then the signal stays pending.
        let stops = signal == SIGSTOP || {
            let action = task.signal_actions[signal as usize];
            crate::abi::is_stop_signal(signal)
                && action.handler == crate::signal::SIG_DFL
        };
        if stops {
            let frame = unsafe { &mut *task.trap_frame() };
            if !frame.from_user() {
                task.pending_signals |= bit;
                return;
            }
            stop_current(signal);
            return;
        }

        let action = task.signal_actions[signal as usize];
        match action.handler {
            crate::signal::SIG_IGN => continue,
            crate::signal::SIG_DFL => {
                if crate::signal::default_is_ignore(signal) {
                    continue;
                }
                exit_current(signal & 0x7F);
            }
            _ => {}
        }

        // A handler can only run on the way back to user mode.
        let frame = unsafe { &mut *task.trap_frame() };
        if !frame.from_user() {
            task.pending_signals |= bit;
            return;
        }
        if action.flags & crate::signal::SA_RESETHAND != 0 {
            task.signal_actions[signal as usize] = crate::signal::SigAction::default();
        }
        if !crate::signal::deliver(&mut task, signal, &action, frame) {
            exit_current(SIGSEGV & 0x7F);
        }
        return;
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

/// Report a child that stopped or was continued since the last report. The
/// child stays where it is; this is a status change, not an exit.
pub fn child_status_change(
    parent_pid: u32,
    want: i32,
    untraced: bool,
    continued: bool,
) -> Option<(u32, i32)> {
    let tasks = TASKS.lock();
    for entry in tasks.iter() {
        let task = entry.get();
        if task.ppid != parent_pid {
            continue;
        }
        if want > 0 && task.pid != want as u32 {
            continue;
        }
        if untraced && task.report_stop {
            task.report_stop = false;
            return Some((task.pid, ((task.stop_signal & 0xFF) << 8) | 0x7F));
        }
        if continued && task.report_continue {
            task.report_continue = false;
            return Some((task.pid, 0xFFFF));
        }
    }
    None
}

/// True when `parent_pid` has a child matching `want` with something for
/// `wait4` to collect: one that has finished, or one whose stop or continue
/// has not been reported yet. Takes nothing, so the answer can be asked for
/// again without consuming the event.
pub fn child_event_pending(
    parent_pid: u32,
    want: i32,
    untraced: bool,
    continued: bool,
) -> bool {
    let tasks = TASKS.lock();
    tasks.iter().any(|t| {
        let task = t.get();
        if task.ppid != parent_pid {
            return false;
        }
        if want > 0 && task.pid != want as u32 {
            return false;
        }
        task.state == State::Zombie
            || (untraced && task.report_stop)
            || (continued && task.report_continue)
    })
}

/// True when the parent has at least one live child matching `want`.
pub fn has_children(parent_pid: u32, want: i32) -> bool {
    let tasks = TASKS.lock();
    tasks.iter().any(|t| {
        let task = t.get();
        task.ppid == parent_pid && (want <= 0 || task.pid == want as u32)
    })
}

/// A set of tasks waiting for one condition.
///
/// A blocking read used to spin on `yield_now`, which kept a core busy while
/// the machine had nothing to do. Waiting on a queue lets the scheduler fall
/// through to the idle task, which halts until an interrupt arrives.
pub struct WaitQueue {
    waiters: Spinlock<Vec<u32>>,
}

impl WaitQueue {
    pub const fn new() -> WaitQueue {
        WaitQueue { waiters: Spinlock::new(Vec::new()) }
    }

    /// Block until `ready` holds.
    ///
    /// The condition is re-checked after this task is on the queue and with
    /// interrupts off, so a wake-up that arrives between a caller's own check
    /// and the sleep cannot be missed. `ready` must not block.
    pub fn wait_until(&self, mut ready: impl FnMut() -> bool) {
        let pid = current().pid;
        loop {
            disable_interrupts();
            if ready() {
                enable_interrupts();
                return;
            }
            self.waiters.lock().push(pid);
            current().state = State::Sleeping;
            enable_interrupts();

            schedule();

            self.waiters.lock().retain(|waiter| *waiter != pid);
        }
    }

    /// Block until `ready` holds or the tick count reaches `deadline`.
    ///
    /// Returns true when `ready` held. A deadline of `u64::MAX` waits
    /// indefinitely, which is what a poll with no timeout asks for.
    pub fn wait_until_or_at(&self, deadline: u64, mut ready: impl FnMut() -> bool) -> bool {
        let pid = current().pid;
        loop {
            disable_interrupts();
            if ready() {
                enable_interrupts();
                return true;
            }
            if crate::trap::ticks() >= deadline {
                enable_interrupts();
                return false;
            }
            self.waiters.lock().push(pid);
            current().state = State::Sleeping;
            // The timer wakes a sleeping task when its deadline passes, so the
            // same sleep serves both the event and the timeout.
            current().wake_at = if deadline == u64::MAX { 0 } else { deadline };
            enable_interrupts();

            schedule();

            current().wake_at = 0;
            self.waiters.lock().retain(|waiter| *waiter != pid);
        }
    }

    pub fn wake_all(&self) {
        let mut waiters = self.waiters.lock();
        for pid in waiters.drain(..) {
            if let Some(task) = find(pid) {
                if task.state == State::Sleeping {
                    task.state = State::Runnable;
                    task.wake_at = 0;
                }
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.waiters.lock().is_empty()
    }
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
