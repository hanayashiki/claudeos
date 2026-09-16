//! Round-robin scheduler.

use crate::abi::*;
use crate::arch;
use crate::sync::{
    disable_interrupts, enable_interrupts, interrupts_enabled, NoInterrupts, Spinlock,
};
use crate::signal::Signal;
use crate::task::{State, Task, TaskPtr};
use alloc::boxed::Box;
use alloc::vec::Vec;

static mut CURRENT: *mut Task = core::ptr::null_mut();
static mut IDLE: *mut Task = core::ptr::null_mut();
static TASKS: Spinlock<Vec<TaskPtr>> = Spinlock::new(Vec::new());
static FOREGROUND_PGID: Spinlock<u32> = Spinlock::new(0);

/// The task the CPU is running, as a guard rather than a reference.
///
/// Which task that is changes at every context switch, so handing out a
/// `&'static Task` says something that is not true: the borrow would outlive
/// the state it describes. The guard reads the current task on each use and
/// cannot be stored anywhere with a longer life.
///
/// It hands out a shared reference and nothing else, which is what makes
/// holding two of these, or one of these alongside a task the table handed
/// out, mean nothing. A task changes its own fields through them.
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

pub fn current() -> Current {
    Current(())
}

pub fn current_ptr() -> *mut Task {
    unsafe { CURRENT }
}

pub fn has_current() -> bool {
    unsafe { !CURRENT.is_null() }
}

/// The process table, held, with interrupts off.
///
/// A state change on one task nearly always has to reach a second one: a stop
/// is reported to a parent, a continue restarts a child and tells the same
/// parent, an exit wakes whoever is in `wait4`. The table's lock does not
/// nest, so the second task has to be found through the hold that is already
/// taken, and that hold is also what proves a tick cannot land between the two
/// halves. Both facts are the same object rather than two arguments.
pub struct Held<'a> {
    tasks: &'a [TaskPtr],
    irq: NoInterrupts<'a>,
}

impl<'a> Held<'a> {
    /// Proof that interrupts are off, for a transition that needs nothing
    /// else from the table.
    pub fn irq(&self) -> NoInterrupts<'a> {
        self.irq
    }

    /// The task with `pid`, borrowed from the hold. Two of these at once name
    /// two entries in one table and say nothing about each other, which is why
    /// asking for the caller's own pid is an ordinary thing to do rather than
    /// a second route to something it already has.
    pub fn find(&self, pid: u32) -> Option<&'a Task> {
        self.tasks.iter().find(|t| t.get().pid == pid).map(|t| t.get())
    }

    pub fn for_each(&self, mut f: impl FnMut(&'a Task)) {
        for entry in self.tasks {
            f(entry.get());
        }
    }

    /// True when every thread of the thread group `tgid` has exited.
    pub fn group_exited(&self, tgid: u32) -> bool {
        group_exited(self.tasks, tgid)
    }

    /// Tell `ppid` that one of its children changed state, and wake it if it
    /// is blocked.
    pub fn notify_parent(&self, ppid: u32) {
        if let Some(parent) = self.find(ppid) {
            parent.child_changed_state(self.irq);
        }
    }
}

/// Take the process table and run `f` with it held.
pub fn with_tasks<R>(f: impl FnOnce(&Held) -> R) -> R {
    let tasks = TASKS.lock();
    f(&Held { tasks: &tasks, irq: tasks.irq() })
}

/// Adopt the boot context as the idle task.
pub fn init() {
    let space = arch::paging::AddressSpace::current();
    let mut idle = Task::new("idle", space).expect("idle task");
    idle.pid = 0;
    idle.tgid = 0;
    let ptr = Box::into_raw(idle);
    unsafe {
        CURRENT = ptr;
        IDLE = ptr;
    }
    TASKS.lock().push(TaskPtr(ptr));
    crate::task::reset_pid_counter(1);
}

/// Build everything a task needs and admit it to the scheduler.
///
/// Owning the box is what keeps a half-built task out of the table: nothing
/// else can reach the task until this hands it over, and this is the only way
/// in. The entry in /proc is built here for the same reason it used to be
/// built by every caller in the right order -- the moment the task is in the
/// table the timer can hand it the CPU, and a child scheduled then found its
/// own directory half-made or missing, which our shell walks a few
/// instructions after fork returns.
pub fn register(task: Box<Task>) -> u32 {
    reap_dead_threads();
    let pid = task.pid;
    // Assembling the directory allocates, which is not something to do with
    // interrupts off; linking it into /proc is a pointer's worth of work and
    // happens in the same breath as the table push.
    let entry = crate::fs::procfs::build_process(pid);
    crate::sync::without_interrupts(|irq| {
        crate::fs::procfs::publish_process(entry, irq);
        admit(task, irq);
    });
    pid
}

/// Put a task in the table, where the scheduler can pick it.
fn admit(task: Box<Task>, _irq: NoInterrupts) {
    TASKS.lock().push(TaskPtr(Box::into_raw(task)));
}

/// Run `f` on the task with `pid`, with the table held for as long as it runs.
///
/// A reader that walks a task -- every page of every region, every descriptor
/// it has open -- is holding a pointer that a reap on another task would free
/// underneath it. Holding the table is what stops that reap happening, and
/// taking the closure rather than returning the reference is what keeps the
/// two the same length.
pub fn with_task<R>(pid: u32, f: impl FnOnce(&Task) -> R) -> Option<R> {
    let tasks = TASKS.lock();
    let entry = tasks.iter().find(|t| t.get().pid == pid)?;
    Some(f(entry.get()))
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

/// Run `f` on every task, with the table held for as long as it runs. The
/// closure is handed the hold alongside each task, because a state change it
/// makes may have to reach that task's parent, which is in the same table.
pub fn for_each<F: FnMut(&Task, &Held)>(mut f: F) {
    with_tasks(|table| table.for_each(|task| f(task, table)));
}

pub fn set_foreground(pgid: u32) {
    *FOREGROUND_PGID.lock() = pgid;
}

pub fn foreground() -> u32 {
    *FOREGROUND_PGID.lock()
}

pub fn current_pgid() -> u32 {
    current().pgid.get()
}

/// True when any task on the machine still names `space`.
///
/// A zombie counts. It has not been reaped, so the address space it ran in has
/// not been handed back yet, and under `CLONE_VM` it is one of several tasks
/// naming the same one. The caller counts too: a task that shares its address
/// space with a child is the reason not to tear it down, not an exception to
/// it. A caller that is about to stop naming the space asks after it has
/// stopped.
pub fn space_in_use(space: arch::paging::AddressSpace) -> bool {
    TASKS.lock().iter().any(|t| t.get().space() == space)
}

fn pick_next() -> Option<*mut Task> {
    let tasks = TASKS.lock();
    let irq = tasks.irq();
    let now = crate::trap::ticks();
    let idle = unsafe { IDLE };

    for entry in tasks.iter() {
        let task = entry.get();
        if task.state() == State::Sleeping && task.wake_at() != 0 && now >= task.wake_at() {
            task.deadline_reached(irq);
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
        if task.state() == State::Runnable {
            if entry.0 == cur {
                return None; // already running the only candidate
            }
            return Some(entry.0);
        }
    }

    let current = unsafe { &*cur };
    if cur != idle && current.state() == State::Runnable {
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
    // Shared references, because this runs from the timer: whatever was
    // interrupted is holding one on the task it was running, and an exclusive
    // one taken here would be a second reference to that same task.
    let prev_task = &*prev;
    let next_task = &*next;

    // The registers the trap frame does not hold, the thread pointer and the
    // floating point and vector file among them, are carried across by hand.
    // The kernel never touches the latter, so what is in them here is still
    // the outgoing task's.
    prev_task.with_cpu(|cpu| cpu.save());
    next_task.with_cpu(|cpu| cpu.restore());

    // Entry from user mode must land on the incoming task's kernel stack.
    arch::set_kernel_entry_stack(next_task.kstack_top);
    arch::set_current_task(next as u64);

    if next_task.space() != prev_task.space() {
        next_task.space().switch_to();
    }

    CURRENT = next;
    prev_task.mark_started();
    arch::switch_context(prev_task.kernel_sp.as_ptr(), next_task.kernel_sp.get());
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
        t.0 != cur && t.0 != idle && t.get().state() == State::Runnable
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

/// Put the current task to sleep for `ticks` timer ticks, or until a signal
/// arrives.
pub fn sleep_ticks(ticks: u64) {
    // A signal landing between a caller's own check and this sleep finds a task
    // that is still runnable, so it wakes nothing, and the task then parks
    // itself for the whole of the time it asked for with the signal pending and
    // nothing left that will deliver it. `pause` asks for about 2^62 ticks,
    // which is for good. Turning interrupts off only stops something else
    // starting now, so the question has to be asked again inside the same
    // window and the sleep skipped if the answer has changed.
    crate::sync::without_interrupts(|irq| {
        if has_pending_signal() {
            return;
        }
        current().sleep(crate::trap::ticks() + ticks.max(1), irq);
    });
    schedule();
}

/// Block until `predicate` holds, yielding in between.
pub fn wait_until<F: FnMut() -> bool>(mut predicate: F) {
    while !predicate() {
        yield_now();
    }
}

pub fn wake(pid: u32) {
    with_tasks(|table| {
        if let Some(task) = table.find(pid) {
            task.wake(table.irq());
        }
    });
}

/// Hand the faulting address to the current task's region list to be backed
/// with memory.
pub fn handle_user_page_fault(fault: &arch::PageFault) -> bool {
    if !has_current() {
        return false;
    }
    if fault.present {
        // The page is present, so the only fault that can be repaired is a
        // write to a page still shared with another address space.
        if fault.write {
            // The copy is one step, and this machine's two halves disagree
            // about how it is reached: an interrupt gate arrives here masked,
            // a synchronous exception from user mode arrives with interrupts
            // in the state the faulting code was in. Masking here is what
            // stops the second from depending on the first.
            return crate::sync::without_interrupts(|irq| {
                current().handle_cow(fault.address, irq)
            });
        }
        return false;
    }
    current().fault_in(fault.address)
}

/// Terminate every task in `tgid`'s thread group except the caller, then the
/// caller itself.
///
/// `status` becomes the thread group's, as Linux's `do_group_exit` stores it in
/// `group_exit_code`. The other threads are ended by SIGKILL and leave through
/// `exit_current`, which gives each of them this status rather than the signal.
/// The leader is the task a wait reports, and a program ends itself from
/// whichever thread it is on -- Go from the one that ran `os.Exit` -- so when
/// the leader recorded the signal, a parent was told that a process which
/// exited with 1 had been killed by signal 9. The first thread to get here sets
/// the status; a second caller leaves with that one, as in Linux.
pub fn exit_group(status: i32) -> ! {
    let status = *current().group_exit.lock().get_or_insert(status);
    let tgid = current().tgid;
    let me = current().pid;
    for_each(|task, table| {
        if task.tgid == tgid && task.pid != me && task.state() != State::Zombie {
            task.add_pending(SIGKILL.bit());
            task.wake(table.irq());
        }
    });
    exit_current(status)
}

/// Terminate the current task. `status` is already encoded the way wait4
/// reports it: exit codes in bits 8..15, a killing signal in bits 0..6.
pub fn exit_current(status: i32) -> ! {
    {
        // Threads that finished earlier are still holding a kernel stack and a
        // reference to this process's region list, and the second of those is
        // what decides below whether the user memory may go.
        reap_dead_threads();

        let task = current();
        // Once `exit_group` has run, the thread leaves with the status that
        // call gave the group, whichever way it got here: the SIGKILL the call
        // sent, its own exit, or a fault.
        let group_status = *task.group_exit.lock();
        task.exit_code.set(group_status.unwrap_or(status));

        // A thread that asked for it gets its tid slot cleared so whoever is
        // joining on it can see that it finished. This goes through the
        // checked path: the page may be shared read-only after a fork, and a
        // bare store would fault inside the kernel.
        //
        // Clearing the word is only half of it. A joiner waits on that address
        // with no timeout, which sleeps with no wake-up time set, so the timer
        // will never return it to the run queue; only a wake on the futex
        // will. Without one the joiner is released only if it happens to read
        // the zero before it goes to sleep, and loses that race as soon as
        // anything else on the machine gets the scheduler in first.
        if task.clear_child_tid.get() != 0 {
            let address = task.clear_child_tid.get();
            task.clear_child_tid.set(0);
            let _ = crate::uaccess::write_u32_in(&task, address, 0);
            crate::futex::wake(crate::futex::futex_key(address), u32::MAX);
        }
        // Under CLONE_FILES the table belongs to the whole process, so only
        // the last task holding it may empty it here.
        if task.fds.is_last_reference() {
            task.fds.clear();
        }

        // Threads share an address space and its region list; only the last
        // thread out may tear either of them down.
        let last_thread = task.mm_shares() == 1;
        if last_thread {
            task.space().free_user_memory();
            task.clear_vmas();
        }
        let ppid = task.ppid.get();
        let pid = task.pid;

        if pid == 1 {
            crate::println!();
            crate::println!(
                "claudeos: init exited with status {:#x}; powering off",
                task.exit_code.get()
            );
            arch::power_off();
        }

        // Handing the children on and becoming a zombie are one step. The
        // second takes this task off the run queue for good, so anything still
        // owed on its behalf after it is owed by a task that can no longer pay
        // it: a tick in between hands the CPU to something else and never
        // hands it back.
        with_tasks(|table| {
            // Orphans are adopted by init. One that has already exited still
            // needs reaping, and init is normally asleep in wait4, so it has
            // to be woken here; nothing else will report the adopted zombie
            // to it.
            let mut adopted_zombie = false;
            table.for_each(|other| {
                if other.ppid.get() == pid {
                    other.ppid.set(1);
                    if other.pid == other.tgid && table.group_exited(other.tgid) {
                        adopted_zombie = true;
                    }
                }
            });
            if adopted_zombie && ppid != 1 {
                table.notify_parent(1);
            }

            current().become_zombie(table);
        });
    }
    loop {
        schedule();
        arch::halt();
    }
}

/// Terminate the current task as if `signal` had killed it.
pub fn kill_current(signal: Signal) -> ! {
    let name = current().name();
    let pid = current().pid;
    crate::println!("[kernel] pid {} ({}) killed by signal {}", pid, name, signal.number());
    exit_current(signal.number())
}

/// Raise a signal on the running task.
pub fn raise_on_current(signal: Signal) {
    if !has_current() {
        return;
    }
    current().add_pending(signal.bit());
}

/// Make `signal` pending on `task`.
///
/// Two of the job-control rules act on the task rather than on the handler: a
/// continue restarts a stopped task and discards a stop that has not been
/// taken yet, and a stop discards a continue the same way. Restarting is what
/// the parent has to be told about, and the table is held here, which is where
/// the parent is found.
pub fn post_signal(task: &Task, signal: Signal, table: &Held) {
    const STOPS: u64 = SIGSTOP.bit() | SIGTSTP.bit() | SIGTTIN.bit() | SIGTTOU.bit();
    if signal == SIGKILL {
        task.restart_for_kill(table.irq());
    }
    if signal == SIGCONT {
        task.drop_pending(STOPS);
        task.continue_after_stop(table);
    } else if signal.stops() {
        task.drop_pending(SIGCONT.bit());
    }
    task.add_pending(signal.bit());
    task.wake(table.irq());
}

/// Stop the running task until something sends it SIGCONT.
fn stop_current(signal: Signal) {
    let stopped = with_tasks(|table| {
        let task = current();
        // The stop signal's pending bit was cleared before this was called, so
        // a continue that arrived since then found a runnable task with no
        // stop to discard and nothing to restart: it recorded nothing. Asked
        // again here, where nothing else can run, it is a continue that beat
        // the stop, and stopping now would park the task with a continue
        // pending that nothing would ever act on.
        if task.pending() & SIGCONT.bit() != 0 {
            return false;
        }
        task.stop(signal, table);
        true
    });
    if stopped {
        schedule();
    }
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
pub fn stop_for_signal(signal: Signal) {
    current().drop_pending(signal.bit());
    stop_current(signal);
    // The continue that restarted this task has now had its default action,
    // which is to do exactly that. Leaving it pending would make the caller
    // think a signal is waiting and give up on what it was doing.
    let task = current();
    if task.action(SIGCONT).handler == crate::signal::SIG_DFL {
        task.drop_pending(SIGCONT.bit());
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
        if task.pending() & signal.bit() == 0 {
            continue;
        }
        if signal != SIGSTOP && task.action(signal).handler != crate::signal::SIG_DFL {
            continue;
        }
        stop_for_signal(signal);
        return true;
    }
    false
}

/// Mark every task in the foreground group as having a pending signal.
pub fn signal_foreground(signal: Signal) {
    signal_group(foreground(), signal);
}

/// Mark every task in `pgid` as having a pending signal.
pub fn signal_group(pgid: u32, signal: Signal) {
    if pgid == 0 {
        return;
    }
    for_each(|task, table| {
        if task.pgid.get() == pgid && task.pid != 1 && task.state() != State::Zombie {
            post_signal(task, signal, table);
        }
    });
}

/// True when a signal is waiting that the task has not blocked.
/// True when a signal is waiting that will actually do something.
///
/// A blocking call gives up with EINTR when this holds, so a signal that would
/// be discarded on delivery must not count: SIGCHLD from a finished background
/// job is pending on most processes most of the time, and treating it as a
/// reason to fail turns unrelated reads and opens into spurious errors.
pub fn has_pending_signal() -> bool {
    has_pending_signal_except(0)
}

/// The same question with the signals in `ignore` left out of it, for a call
/// that acts on one itself rather than giving up. `wait4` is here to collect
/// what the child signal is telling it about, so that one is not a reason for
/// it to fail.
pub fn has_pending_signal_except(ignore: u64) -> bool {
    if !has_current() {
        return false;
    }
    let task = current();
    let waiting = task.pending() & !ignore;
    let mut pending = waiting & !task.blocked();
    // Neither of these can be blocked.
    pending |= waiting & (SIGKILL.bit() | SIGSTOP.bit());
    if pending == 0 {
        return false;
    }
    for signal in Signal::all() {
        if pending & signal.bit() == 0 {
            continue;
        }
        let handler = task.action(signal).handler;
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
    let task = current();
    if task.pending() == 0 {
        return;
    }

    for signal in Signal::all() {
        let bit = signal.bit();
        if task.pending() & bit == 0 {
            continue;
        }
        let blocked = task.blocked() & bit != 0;
        if blocked && signal != SIGKILL && signal != SIGSTOP {
            continue;
        }
        // Taken rather than dropped: only a take that finds the bit still set
        // goes on to act on the signal.
        if task.take_pending(bit) == 0 {
            continue;
        }

        if signal == SIGKILL {
            exit_current(signal.number());
        }

        // Stopping and running a handler both leave the kernel in the middle
        // of whatever it was doing, so both wait until the task is on its way
        // back to user mode; until then the signal stays pending.
        let stops = signal == SIGSTOP || {
            let action = task.action(signal);
            signal.stops() && action.handler == crate::signal::SIG_DFL
        };
        if stops {
            let frame = unsafe { &mut *task.trap_frame() };
            if !frame.from_user() {
                task.add_pending(bit);
                return;
            }
            stop_current(signal);
            return;
        }

        let action = task.action(signal);
        match action.handler {
            crate::signal::SIG_IGN => continue,
            crate::signal::SIG_DFL => {
                if crate::signal::default_is_ignore(signal) {
                    continue;
                }
                exit_current(signal.number());
            }
            _ => {}
        }

        // A handler can only run on the way back to user mode.
        let frame = unsafe { &mut *task.trap_frame() };
        if !frame.from_user() {
            task.add_pending(bit);
            return;
        }
        if action.flags & crate::signal::SA_RESETHAND != 0 {
            task.set_action(signal, crate::signal::SigAction::default());
        }
        if !crate::signal::deliver(&task, signal, &action, frame) {
            exit_current(SIGSEGV.number());
        }
        return;
    }
}

/// True when `task` is a child `wait4` may be told about, rather than one of
/// the threads inside one.
///
/// A thread is given its process's parent as its own parent, so that an orphan
/// is adopted the same way a process is. Matching on that field alone offers
/// the thread to that parent as if it were a child of its own: the parent is
/// woken out of its wait and handed a task id it never forked, with the
/// thread's status, while the process it is actually waiting for is still
/// running. A thread is reported to a joiner inside the process and to nothing
/// else.
fn is_child_process(task: &Task, parent_pid: u32) -> bool {
    task.ppid.get() == parent_pid && task.pid == task.tgid
}

/// Does `task` match the pid argument `wait4` was given?
///
/// Above zero it names one task; minus one is any child; below minus one is
/// the process group its negation names, which is how a shell waits for a job
/// rather than for a particular process. Zero is any child here, where Linux
/// reads it as the caller's own process group.
fn matches_want(task: &Task, want: i32) -> bool {
    match want {
        w if w > 0 => task.pid == w as u32,
        w if w < -1 => task.pgid.get() == (-w) as u32,
        _ => true,
    }
}

/// True when every task in thread group `tgid` has exited.
///
/// A process has finished only then. Its first thread can exit alone, with
/// the `exit` system call, while the others run on, and Linux's `wait` passes
/// over such a leader until its thread group is empty (`delay_group_leader`).
fn group_exited(tasks: &[TaskPtr], tgid: u32) -> bool {
    tasks.iter().all(|t| {
        let task = t.get();
        task.tgid != tgid || task.state() == State::Zombie
    })
}

/// True when `task` is a process that `wait4` can collect: its leader, with
/// every thread of it exited.
fn finished_process(tasks: &[TaskPtr], task: &Task) -> bool {
    task.state() == State::Zombie && group_exited(tasks, task.tgid)
}

/// Collect a finished child. Returns (pid, exit code).
pub fn reap_child(parent_pid: u32, want: i32) -> Option<(u32, i32)> {
    let mut group = Vec::new();
    let (pid, code) = {
        let mut tasks = TASKS.lock();
        let leader = tasks.iter().map(|entry| entry.get()).find(|task| {
            is_child_process(task, parent_pid)
                && matches_want(task, want)
                && finished_process(&tasks, task)
        })?;
        // A leader that left before the group was ended -- its own `exit`
        // while other threads ran on -- holds a code that is not the
        // process's. Linux's wait reports `group_exit_code` whenever the
        // group was ended by `exit_group`.
        let status = (*leader.group_exit.lock()).unwrap_or(leader.exit_code.get());
        let (pid, tgid) = (leader.pid, leader.tgid);
        // The leader and whichever of its threads have not been released yet,
        // all exited, found and taken out under one hold of the table. Another
        // thread of the parent waiting for the same child would otherwise find
        // it in between and free it a second time, and a thread left behind
        // would keep its kernel stack and its entry in /proc until the next
        // task was made or exited.
        tasks.retain(|entry| {
            let member = entry.get().tgid == tgid;
            if member {
                group.push(entry.0);
            }
            !member
        });
        (pid, status)
    };
    release(group);
    Some((pid, code))
}

/// Hand back what tasks taken out of the table hold: the entry in /proc, the
/// kernel stack, the task itself, and the address space once nothing names
/// it.
///
/// Tasks released together can run on one address space, and each of them is
/// already out of the table when it is looked at, so the table alone would say
/// the space is free for every one of them; only the last of them to name it
/// may destroy it.
fn release(dead: Vec<*mut Task>) {
    for (i, &ptr) in dead.iter().enumerate() {
        unsafe {
            let mut task = Box::from_raw(ptr);
            crate::fs::procfs::remove_process(task.pid);
            let space = task.space();
            let named_later = dead[i + 1..].iter().any(|&other| (*other).space() == space);
            if !named_later && !space_in_use(space) {
                space.destroy();
            }
            task.free_kernel_stack();
            task.mark_dead();
            drop(task);
        }
    }
}

/// Release the tasks of threads that have finished.
///
/// Nothing waits for a thread, so no `wait4` ever takes its entry out of the
/// table: the kernel stack, the task itself and the share it holds of the
/// process's region list would stay taken for as long as the machine ran.
/// Called where threads are made and where one exits, so a program that starts
/// and joins them in a loop leaves at most the one that has not finished
/// switching away yet.
pub fn reap_dead_threads() {
    let mut dead = Vec::new();
    {
        let cur = unsafe { CURRENT };
        let mut tasks = TASKS.lock();
        tasks.retain(|entry| {
            let task = entry.get();
            // The running task is in the middle of its own exit and is still on
            // the stack this would hand back.
            let finished =
                task.state() == State::Zombie && task.pid != task.tgid && entry.0 != cur;
            if finished {
                dead.push(entry.0);
            }
            !finished
        });
    }
    release(dead);
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
        if !is_child_process(task, parent_pid) {
            continue;
        }
        if !matches_want(task, want) {
            continue;
        }
        if untraced && task.report_stop.get() {
            task.report_stop.set(false);
            let signal = task.stop_signal.get().map_or(0, Signal::number);
            return Some((task.pid, (signal << 8) | 0x7F));
        }
        if continued && task.report_continue.get() {
            task.report_continue.set(false);
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
        if !is_child_process(task, parent_pid) {
            return false;
        }
        if !matches_want(task, want) {
            return false;
        }
        finished_process(&tasks, task)
            || (untraced && task.report_stop.get())
            || (continued && task.report_continue.get())
    })
}

/// True when the parent has at least one live child matching `want`.
pub fn has_children(parent_pid: u32, want: i32) -> bool {
    let tasks = TASKS.lock();
    tasks.iter().any(|t| {
        let task = t.get();
        is_child_process(task, parent_pid) && matches_want(task, want)
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

/// How a round of `wait_until_or_at` ended.
enum Woke {
    Ready,
    TimedOut,
    Parked,
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
            let parked = crate::sync::without_interrupts(|irq| {
                if ready() {
                    return false;
                }
                self.waiters.lock().push(pid);
                current().sleep(0, irq);
                true
            });
            if !parked {
                return;
            }

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
            match crate::sync::without_interrupts(|irq| {
                if ready() {
                    return Woke::Ready;
                }
                if crate::trap::ticks() >= deadline {
                    return Woke::TimedOut;
                }
                self.waiters.lock().push(pid);
                // The timer wakes a sleeping task when its deadline passes, so
                // the same sleep serves both the event and the timeout.
                current().sleep(if deadline == u64::MAX { 0 } else { deadline }, irq);
                Woke::Parked
            }) {
                Woke::Ready => return true,
                Woke::TimedOut => return false,
                Woke::Parked => {}
            }

            // Nothing returns a sleeping task to the run queue without
            // clearing its deadline, so there is none left to clear here.
            schedule();

            self.waiters.lock().retain(|waiter| *waiter != pid);
        }
    }

    pub fn wake_all(&self) {
        let mut waiters = self.waiters.lock();
        with_tasks(|table| {
            for pid in waiters.drain(..) {
                if let Some(task) = table.find(pid) {
                    task.wake(table.irq());
                }
            }
        });
    }

    pub fn is_empty(&self) -> bool {
        self.waiters.lock().is_empty()
    }
}

/// First entry into user mode for a newly created task.
pub extern "C" fn user_entry_trampoline() -> ! {
    let task = current();
    let frame = task.trap_frame();
    unsafe { arch::return_to_user(frame) }
}

/// The idle loop, until `ready` holds or the tick count reaches `deadline`.
/// Returns true when `ready` held, with interrupts off either way.
///
/// For the boot context, which is the idle task and so has no task of its
/// own to sleep in, to wait on work the tasks it has started are doing: the
/// network task taking a lease before init is let run.
pub fn idle_until(deadline: u64, mut ready: impl FnMut() -> bool) -> bool {
    let held = loop {
        if ready() {
            break true;
        }
        if crate::trap::ticks() >= deadline {
            break false;
        }
        enable_interrupts();
        arch::halt();
        schedule();
    };
    crate::sync::disable_interrupts();
    held
}

/// Idle loop: run when nothing else can.
pub fn idle_loop() -> ! {
    loop {
        enable_interrupts();
        arch::halt();
        schedule();
    }
}
