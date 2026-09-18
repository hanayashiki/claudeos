//! Tasks: kernel stacks, the address space each runs in, and the process table.
//!
//! The kernel runs on a single CPU with interrupt-disabling locks, so the
//! current task is reached through a raw pointer rather than a lock that a
//! blocking syscall would have to hold across a context switch. What keeps a
//! change to a task whole is the interrupt mask, not a lock on the task.
//!
//! A task that is in the table is reachable from two directions at once: the
//! task itself has the running-task guard, and anything holding the table can
//! look it up by pid. Neither direction hands out an exclusive reference, for
//! the reason set out on `Task` below.

use crate::abi::*;
use crate::arch::paging::{COW, NO_EXECUTE, PRESENT, USER, WRITABLE};
use crate::mm::tables::{MapError, Prepared};
use crate::arch::{self, TaskContext, TrapFrame};
use crate::fs::{FdTable, OpenFile};
use crate::mm::space::{Displaced, MemState, Mm};
use crate::mm::{page_align_down, PAGE_SIZE_U64, USER_STACK_TOP};
use alloc::alloc::{alloc, dealloc, Layout};
use alloc::string::{String, ToString};
use crate::sync::{NoInterrupts, Spinlock};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::Cell;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

pub const KERNEL_STACK_SIZE: usize = 32 * 1024;

/// Total address range reserved for the main thread stack.
pub const STACK_RESERVE: u64 = 8 * 1024 * 1024;
/// How much of it is mapped up front; the rest faults in on demand.
pub const STACK_PREFAULT: u64 = 256 * 1024;
/// The most the arguments, the environment and the strings alongside them may
/// come to. They are written onto the new stack before the program starts, so
/// a block larger than the stack has nowhere to go; a quarter of the reserve
/// is the share Linux gives them and leaves the program the rest.
pub const MAX_ARG_BYTES: u64 = STACK_RESERVE / 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Runnable,
    /// Waiting for a wake-up: a tick deadline, a child, or I/O.
    Sleeping,
    /// Stopped by a job-control signal. Only SIGCONT makes it runnable again.
    Stopped,
    Zombie,
    Dead,
}

/// What a page is to the kernel about to touch it on a task's behalf.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageAccess {
    /// Memory is there and the access may go ahead.
    Ready,
    /// Nothing is mapped at the address. Backing it is the caller's to do.
    Absent,
    /// Something is mapped, and this access is not one it allows.
    Refused,
}

pub use crate::mm::space::{FileMap, Vma};

/// A task control block.
///
/// Everything a task changes after it is in the process table sits in a cell.
/// That is not to make the changes atomic -- a single processor with interrupts
/// masked is what does that, and a cell's load-modify-store is the same
/// instructions the plain field was -- but so that changing it asks for a
/// shared reference. A task in the table is reachable by two routes at once,
/// through the running-task guard and through the table, and an exclusive
/// reference from either would be an exclusive reference to something the other
/// route hands out as well. With nothing to mutate through, the two references
/// cannot be written down.
///
/// The fields that are not in cells are the ones fixed before the task is
/// admitted. Until `sched::register` takes the box, the task is owned by one
/// caller and reachable from nowhere else, which is the one place an exclusive
/// reference to it is the truth.
pub struct Task {
    pub pid: u32,
    pub tgid: u32,
    pub ppid: Cell<u32>,
    pub pgid: Cell<u32>,
    /// What the scheduler will do with this task, and when the timer should
    /// put it back on the run queue. Private, and changed only by the
    /// transitions below: every one of them is a step that has a second half
    /// somewhere else -- a parent to tell, a waiter to wake -- and a field
    /// anyone could assign to is how the two halves came apart.
    state: Cell<State>,
    /// Tick count to wake at when sleeping, or zero for no deadline.
    wake_at: Cell<u64>,

    /// Saved kernel stack pointer between context switches. The switch stores
    /// through the cell's pointer, which is what the assembly is given.
    pub kernel_sp: Cell<u64>,
    kstack: *mut u8,
    pub kstack_top: u64,

    /// The address space the task runs in: its page tables and the record of
    /// what is in them. Shared with every thread cloned with `CLONE_VM`. Exec
    /// puts a new one here and leaves the old to whatever still holds it, and
    /// exit takes it away; the last holder to let go frees it. `None` for a
    /// kernel task, which runs on whatever tables the processor is on, and for
    /// a task that has exited.
    ///
    /// Private for the same reason the scheduling state is: the record and
    /// the tables the processor is on have to change together, and
    /// `run_on_mm` is where that happens. A lock rather than a cell, because
    /// other tasks read it through the process table.
    mm: Spinlock<Option<Arc<Mm>>>,
    /// Shared with every task that cloned with `CLONE_FILES`.
    pub fds: FdTable,
    /// Shared with every task that cloned with `CLONE_FS`, so that a directory
    /// one thread changes into is the one its siblings resolve against.
    pub cwd: Arc<Spinlock<String>>,
    name: Spinlock<String>,
    /// Path of the running executable, reported through /proc/self/exe.
    exe_path: Spinlock<String>,

    pub exit_code: Cell<i32>,
    /// The status `exit_group` ended the thread group with, once a thread has
    /// called it. One cell for the whole group, the way Linux keeps
    /// `group_exit_code` in the signal struct its threads share: every thread
    /// the call ends leaves with this status, not with the SIGKILL that ended
    /// it. A forked child starts with none.
    pub group_exit: Arc<Spinlock<Option<i32>>>,
    /// Registers only this architecture has, carried across context switches.
    cpu: Cell<TaskContext>,
    pub clear_child_tid: Cell<u64>,
    pub set_child_tid: Cell<u64>,
    pub robust_list: Cell<u64>,

    /// Signals raised on this task and not yet taken, one bit each.
    ///
    /// An atomic rather than a cell, and private, so that the only changes
    /// are a single `fetch_or` or `fetch_and`. Interrupts raise signals: the
    /// tick for an interval timer, the console for the interrupt key. They can
    /// land in the middle of a system call that is taking a signal off the
    /// same set. When a change was a read, an or or an and, and a write, an
    /// interrupt between the read and the write had its signal written over
    /// with the copy that was read before it arrived, and the signal was lost.
    pending_signals: AtomicU64,
    /// Signals sent to the process rather than to one of its threads, and not
    /// yet taken: one set for the whole thread group, as Linux keeps
    /// `shared_pending` in the signal struct its threads share. Whichever
    /// thread does not block a signal takes it from here. A forked child starts
    /// with an empty one.
    shared_pending: Arc<AtomicU64>,
    /// The fault the kernel raised a signal pending on this task for, until
    /// that signal is taken: what its handler is told in `si_code` and
    /// `si_addr`.
    pub fault: Cell<Option<crate::signal::Fault>>,
    /// The scheduling nice value. Round robin does not act on it, but a
    /// program that sets it reads it back.
    pub nice: Cell<i32>,
    /// The signal that stopped this task, and whether the stop and the
    /// following continue have been reported to whoever is waiting.
    pub stop_signal: Cell<Option<crate::signal::Signal>>,
    pub report_stop: Cell<bool>,
    pub report_continue: Cell<bool>,
    /// One cell holding the whole table, rather than sixty-four holding an
    /// entry each, so that a fork copies it in one move the way the plain
    /// field did. `as_slice_of_cells` gives the entries back one at a time.
    signal_actions: Cell<[crate::signal::SigAction; 64]>,
    /// The signals this task blocks. Atomic and private for the same reason as
    /// the pending set: delivering a handler from the timer tick blocks
    /// signals, and so does the task in its own system calls.
    blocked_signals: AtomicU64,
    /// The stack a handler whose disposition says `SA_ONSTACK` runs on.
    /// Per-thread: a thread that shares its siblings' stack would have two
    /// handlers writing over each other, so a `CLONE_VM` child starts without
    /// one and installs its own. Empty means none is installed.
    pub sig_stack: Cell<crate::abi::SigAltStack>,
    /// The interval timers `setitimer` arms. They belong to the process, so
    /// every thread in it holds the same ones, and a fork's child is given
    /// its own.
    pub itimers: Arc<Spinlock<crate::itimer::IntervalTimers>>,

    /// Pid this task is waiting for, if it is in wait4.
    pub waiting_for: Cell<Option<i32>>,

    pub umask: Cell<u32>,
    /// Set once the task has been switched away from at least once, so a
    /// freshly created task is not resumed from a stale frame.
    started: Cell<bool>,
    /// Program a freshly created task should exec before reaching user mode.
    pending_exec: Spinlock<Option<(String, Vec<String>, Vec<String>)>>,
    /// Parent blocked in vfork, to be woken when this task execs or exits.
    pub vfork_parent: Cell<Option<u32>>,
    /// Set on a process's leader when its last thread exits and its parent
    /// has said it will not wait for it: SIGCHLD ignored, or `SA_NOCLDWAIT`.
    /// Nothing reports such a process to `wait4`, and its tasks are released
    /// without one, as Linux's `exit_notify` releases a task
    /// `do_notify_parent` says to reap itself.
    pub released_at_exit: Cell<bool>,
}

unsafe impl Send for Task {}

static NEXT_PID: AtomicU32 = AtomicU32::new(1);

pub fn allocate_pid() -> u32 {
    NEXT_PID.fetch_add(1, Ordering::Relaxed)
}

/// Restart pid numbering, so the first user process is pid 1.
pub fn reset_pid_counter(next: u32) {
    NEXT_PID.store(next, Ordering::Relaxed);
}

/// What a demand-paging map means for the fault that asked for it.
///
/// An address a sibling thread on the same address space got to first is not a
/// failure: the page is there, which is what the fault wanted, and what is
/// there is finished, because a page is published with its contents already in
/// it. Reading the refusal as a failure killed the task with a segmentation
/// fault at an address that is mapped.
fn served(mapped: Result<u64, MapError>) -> bool {
    matches!(mapped, Ok(_) | Err(MapError::AlreadyMapped))
}

fn kstack_layout() -> Layout {
    Layout::from_size_align(KERNEL_STACK_SIZE, 16).unwrap()
}

impl Task {
    /// Allocate a task with a fresh kernel stack, running in `mm`: `None` for
    /// a task that runs only in the kernel.
    pub fn new(name: &str, mm: Option<Arc<Mm>>) -> Option<alloc::boxed::Box<Task>> {
        let kstack = unsafe { alloc(kstack_layout()) };
        if kstack.is_null() {
            return None;
        }
        let kstack_top = kstack as u64 + KERNEL_STACK_SIZE as u64;
        let pid = allocate_pid();
        Some(alloc::boxed::Box::new(Task {
            pid,
            tgid: pid,
            ppid: Cell::new(0),
            pgid: Cell::new(pid),
            state: Cell::new(State::Runnable),
            kernel_sp: Cell::new(0),
            kstack,
            kstack_top,
            mm: Spinlock::new(mm),
            fds: FdTable::new(),
            cwd: Arc::new(Spinlock::new(String::from("/"))),
            name: Spinlock::new(name.to_string()),
            exe_path: Spinlock::new(String::new()),
            exit_code: Cell::new(0),
            group_exit: Arc::new(Spinlock::new(None)),
            cpu: Cell::new(TaskContext::new()),
            clear_child_tid: Cell::new(0),
            set_child_tid: Cell::new(0),
            robust_list: Cell::new(0),
            pending_signals: AtomicU64::new(0),
            shared_pending: Arc::new(AtomicU64::new(0)),
            fault: Cell::new(None),
            nice: Cell::new(0),
            stop_signal: Cell::new(None),
            report_stop: Cell::new(false),
            report_continue: Cell::new(false),
            signal_actions: Cell::new([crate::signal::SigAction::default(); 64]),
            blocked_signals: AtomicU64::new(0),
            sig_stack: Cell::new(crate::abi::SigAltStack::default()),
            itimers: Arc::new(Spinlock::new(crate::itimer::IntervalTimers::default())),
            wake_at: Cell::new(0),
            waiting_for: Cell::new(None),
            umask: Cell::new(0o022),
            started: Cell::new(false),
            pending_exec: Spinlock::new(None),
            vfork_parent: Cell::new(None),
            released_at_exit: Cell::new(false),
        }))
    }

    pub fn name(&self) -> String {
        self.name.lock().clone()
    }

    pub fn set_name(&self, value: String) {
        *self.name.lock() = value;
    }

    pub fn exe_path(&self) -> String {
        self.exe_path.lock().clone()
    }

    pub fn set_exe_path(&self, value: String) {
        *self.exe_path.lock() = value;
    }

    /// The program a task created by the kernel is to run, taken by the
    /// bootstrap that runs it. Taking it is what leaves nothing to run twice.
    pub fn take_pending_exec(&self) -> Option<(String, Vec<String>, Vec<String>)> {
        self.pending_exec.lock().take()
    }

    pub fn set_pending_exec(&self, program: (String, Vec<String>, Vec<String>)) {
        *self.pending_exec.lock() = Some(program);
    }

    /// The dispositions, one cell each.
    fn actions(&self) -> &[Cell<crate::signal::SigAction>] {
        let table: &Cell<[crate::signal::SigAction]> = &self.signal_actions;
        table.as_slice_of_cells()
    }

    /// The disposition of one signal, and the way to change it.
    pub fn action(&self, signal: crate::signal::Signal) -> crate::signal::SigAction {
        self.actions()[signal.index()].get()
    }

    pub fn set_action(&self, signal: crate::signal::Signal, action: crate::signal::SigAction) {
        self.actions()[signal.index()].set(action);
    }

    /// Take another task's dispositions, which is what a fork gives the child.
    pub fn copy_actions_from(&self, other: &Task) {
        self.signal_actions.set(other.signal_actions.get());
    }

    /// Handlers do not survive exec, but ignored signals stay ignored. Nor
    /// does the alternate stack: it was an address in the image that has gone.
    pub fn reset_actions_for_exec(&self) {
        for action in self.actions() {
            if action.get().handler != crate::signal::SIG_IGN {
                action.set(crate::signal::SigAction::default());
            }
        }
        self.sig_stack.set(crate::abi::SigAltStack::default());
    }

    /// The signals this task could take: its own pending set and its thread
    /// group's, as one reading of each.
    pub fn pending(&self) -> u64 {
        self.own_pending() | self.shared_pending()
    }

    /// The signals sent to this thread alone.
    pub fn own_pending(&self) -> u64 {
        self.pending_signals.load(Ordering::Acquire)
    }

    /// The signals sent to the thread group and not yet taken by any thread.
    pub fn shared_pending(&self) -> u64 {
        self.shared_pending.load(Ordering::Acquire)
    }

    /// Add signals to this thread's own pending set, in one step.
    pub fn add_pending(&self, signals: u64) {
        self.pending_signals.fetch_or(signals, Ordering::AcqRel);
    }

    /// Add signals to the thread group's pending set, in one step.
    pub fn add_shared_pending(&self, signals: u64) {
        self.shared_pending.fetch_or(signals, Ordering::AcqRel);
    }

    /// Remove signals from both pending sets, one step each.
    pub fn drop_pending(&self, signals: u64) {
        self.pending_signals.fetch_and(!signals, Ordering::AcqRel);
        self.shared_pending.fetch_and(!signals, Ordering::AcqRel);
    }

    /// Remove `signals` from the pending sets and return which of them were
    /// there, one step per set. A delivery takes its signal this way, so that
    /// two deliveries cannot both find the same bit and both act on it, and a
    /// bit raised again after the take stays raised. This thread's own set is
    /// taken from first and the group's only for what that did not hold, the
    /// order Linux's `dequeue_signal` uses, so a signal sent both ways is
    /// taken twice.
    pub fn take_pending(&self, signals: u64) -> u64 {
        let own = self.pending_signals.fetch_and(!signals, Ordering::AcqRel) & signals;
        let rest = signals & !own;
        if rest == 0 {
            return own;
        }
        own | (self.shared_pending.fetch_and(!rest, Ordering::AcqRel) & rest)
    }

    /// Share `other`'s thread-group pending set, which is what a thread is
    /// given. Only for a task that has not been admitted yet.
    pub fn share_pending_of(&mut self, other: &Task) {
        self.shared_pending = other.shared_pending.clone();
    }

    /// The signals this task blocks, as one reading.
    pub fn blocked(&self) -> u64 {
        self.blocked_signals.load(Ordering::Acquire)
    }

    /// Block more signals, in one step.
    pub fn block(&self, signals: u64) {
        self.blocked_signals.fetch_or(signals, Ordering::AcqRel);
    }

    /// Unblock signals, in one step.
    pub fn unblock(&self, signals: u64) {
        self.blocked_signals.fetch_and(!signals, Ordering::AcqRel);
    }

    /// Replace the blocked set.
    pub fn set_blocked(&self, signals: u64) {
        self.blocked_signals.store(signals, Ordering::Release);
    }

    /// Run `f` on the registers a context switch carries by hand, with
    /// interrupts off.
    ///
    /// Reached through the cell's own pointer rather than by copying the value
    /// out and back: on one of the two machines this is half a kilobyte of
    /// vector state, and it is saved and restored at every switch. `as_ptr` is
    /// what a shared reference to a cell yields for exactly this, and nothing
    /// holds a reference into this one -- the field is private and this is the
    /// only way to reach it.
    ///
    /// Interrupts are off because a switch away from the running task copies
    /// the processor's registers into this record. Several callers change the
    /// record and then load it into the processor: exec clearing the registers
    /// for the new program, sigreturn putting back the ones a handler
    /// interrupted. A tick between the two copied the old registers over the
    /// record that had just been changed, and they were what got loaded.
    pub fn with_cpu<R>(&self, f: impl FnOnce(&mut TaskContext) -> R) -> R {
        crate::sync::without_interrupts(|_| f(unsafe { &mut *self.cpu.as_ptr() }))
    }

    /// Record that the task has been switched away from at least once, so it
    /// is not resumed from a frame it never built.
    pub fn mark_started(&self) {
        self.started.set(true);
    }

    /// The user register frame, which always sits at the top of the kernel
    /// stack because both entry paths start with RSP at `kstack_top`.
    pub fn trap_frame(&self) -> *mut TrapFrame {
        arch::trap_frame_at(self.kstack_top)
    }

    /// Lay out the kernel stack so the first context switch into this task
    /// lands in `entry`.
    pub fn prepare_kernel_frame(&mut self, entry: u64) {
        self.kernel_sp.set(arch::prepare_kernel_entry(self.kstack_top, entry));
    }

    /// The record of what is in the address space, held for as long as the
    /// call that asked for it runs. `None` for a task with no address space,
    /// which no system call from a program reaches here for.
    ///
    /// The task's own lock is held only to take a reference, so it is never
    /// held around anything else: a fault report reads it too, and a fault
    /// inside a call that held it would hang the report rather than print it.
    fn with_mem<R>(&self, f: impl FnOnce(&mut MemState) -> R) -> Option<R> {
        let mm = self.mm()?;
        let mut mem = mm.lock();
        Some(f(&mut mem))
    }

    pub fn cwd(&self) -> String {
        self.cwd.lock().clone()
    }

    pub fn set_cwd(&self, path: String) {
        *self.cwd.lock() = path;
    }

    pub fn brk_start(&self) -> u64 {
        self.with_mem(|mm| mm.brk_start).unwrap_or(0)
    }

    pub fn brk(&self) -> u64 {
        self.with_mem(|mm| mm.brk).unwrap_or(0)
    }

    pub fn set_brk(&self, value: u64) {
        self.with_mem(|mm| mm.brk = value);
    }

    pub fn set_heap_base(&self, value: u64) {
        self.with_mem(|mm| {
            mm.brk_start = value;
            mm.brk = value;
        });
    }

    pub fn add_vma(&self, start: u64, end: u64, prot: u64, flags: u64) {
        self.with_mem(|mm| mm.vmas.push(Vma { start, end, prot, flags, file: None }));
    }

    pub fn add_file_vma(&self, start: u64, end: u64, prot: u64, flags: u64, file: FileMap) {
        self.with_mem(|mm| mm.vmas.push(Vma { start, end, prot, flags, file: Some(file) }));
    }

    pub fn find_vma(&self, addr: u64) -> Option<Vma> {
        self.with_mem(|mm| mm.find_vma(addr).cloned()).flatten()
    }

    /// True when no recorded region overlaps `[start, end)`.
    pub fn range_is_free(&self, start: u64, end: u64) -> bool {
        self.with_mem(|mm| mm.range_is_free(start, end)).unwrap_or(false)
    }

    /// Total size of every recorded region plus the heap, for /proc reporting.
    pub fn virtual_size(&self) -> u64 {
        self.with_mem(|mm| mm.virtual_size()).unwrap_or(0)
    }

    /// Pages actually backed by memory right now, for /proc.
    ///
    /// The ranges are read under the lock and then walked in pieces, with the
    /// lock taken again for each. It is a statistic, so a page that arrives or
    /// goes while it is being counted may be counted either way; what it may
    /// not do is hold the timer off for as long as walking every page of the
    /// largest program takes. One reader of /proc/<pid>/statm over a 64 MiB
    /// program was sixteen thousand walks under one hold, measured at eight
    /// milliseconds with interrupts masked.
    pub fn resident_pages(&self) -> u64 {
        let Some(mm) = self.mm() else {
            return 0;
        };
        let ranges: Vec<(u64, u64)> = {
            let space = mm.lock();
            space
                .vmas
                .iter()
                .map(|vma| (vma.start, vma.end))
                .chain(core::iter::once((space.brk_start, space.brk)))
                .collect()
        };
        /// Pages counted per turn of the lock: one last-level table's worth.
        const PER_TURN: u64 = crate::mm::walk::ENTRIES as u64;
        let mut pages = 0u64;
        for (start, end) in ranges {
            let mut page = start;
            while page < end {
                let stop = end.min(page + PER_TURN * PAGE_SIZE_U64);
                let space = mm.lock();
                while page < stop {
                    if space.translate(page).is_some() {
                        pages += 1;
                    }
                    page += PAGE_SIZE_U64;
                }
            }
        }
        pages
    }

    /// A copy of the region list.
    ///
    /// The room for it is taken before the lock and the copy made under it, so
    /// nothing under the lock allocates. An allocation there is the one that
    /// can find the kernel heap full and map the pages it grows by, which is
    /// thousands of page table writes with interrupts masked.
    pub fn snapshot_vmas(&self) -> Vec<Vma> {
        let Some(mm) = self.mm() else {
            return Vec::new();
        };
        let mut out = Vec::with_capacity(mm.lock().vmas.len() + 4);
        let space = mm.lock();
        out.extend_from_slice(&space.vmas);
        out
    }

    /// The auxiliary vector the running program was started with, as words.
    pub fn saved_auxv(&self) -> Vec<u64> {
        let Some(mm) = self.mm() else {
            return Vec::new();
        };
        // The room first, for the reason `snapshot_vmas` gives.
        let mut out = Vec::with_capacity(mm.lock().saved_auxv.len() + 4);
        let space = mm.lock();
        out.extend_from_slice(&space.saved_auxv);
        out
    }

    /// Give every region overlapping `[start, end)` the new protection.
    pub fn set_vma_prot(&self, start: u64, end: u64, prot: u64) {
        self.with_mem(|mm| mm.set_vma_prot(start, end, prot));
    }

    /// Remove `[start, end)` from the recorded regions, splitting as needed.
    pub fn remove_vma_range(&self, start: u64, end: u64) {
        self.with_mem(|mm| mm.remove_vma_range(start, end));
    }

    /// Find a free span of `len` bytes in the mmap area. Zero for a task with
    /// no address space, which has nowhere to put one.
    pub fn find_free_region(&self, len: u64) -> u64 {
        self.with_mem(|mm| mm.find_free_region(len)).unwrap_or(0)
    }

    /// Give this task a private copy of a shared page it is trying to write.
    /// Returns whether a write to the page may be made now, which is false for
    /// a page no write of this task's can complete on.
    ///
    /// Nothing in the task itself changes: the page tables and the region list
    /// are reached through the address space the task holds a reference to,
    /// and the whole of the change is made under that space's lock
    /// (`Mm::break_cow`). The token the caller holds is no longer what makes
    /// the change whole -- the lock is -- but it is still what says the caller
    /// is somewhere a lock that masks interrupts may be taken at all.
    pub fn handle_cow(&self, addr: u64, _irq: NoInterrupts) -> bool {
        match self.mm() {
            Some(mm) => mm.break_cow(addr),
            None => false,
        }
    }

    /// Whether the kernel may touch `addr`'s page on this task's behalf,
    /// taking the private copy of a shared page when a write needs one.
    ///
    /// Reading what the descriptor says and repairing it are one operation
    /// under the address space's lock rather than two calls a caller makes in
    /// order. Apart, a sibling thread that takes the copy in between leaves
    /// the repair looking at a page that is no longer shared, and a page that
    /// was never shared looks exactly the same to it.
    pub fn access_page(&self, addr: u64, write: bool, irq: NoInterrupts) -> PageAccess {
        let page = page_align_down(addr);
        let Some(mm) = self.mm() else {
            // No address space, so nothing can be mapped for the access either.
            return PageAccess::Refused;
        };
        let Some(flags) = mm.lock().flags_of(page) else {
            return PageAccess::Absent;
        };
        // A page shared after a fork is read-only until someone writes to it.
        // The kernel writing on the task's behalf counts, so take the private
        // copy here rather than reporting a bad address.
        //
        // The mark decides, not the write permission. aarch64 keeps the
        // permission the caller asked for and derives read-only from the mark,
        // so a shared page there reads back as writable while the hardware
        // refuses the store; asking the permission alone would let the copy
        // through and fault in the kernel.
        if write && (flags & COW != 0 || flags & WRITABLE == 0) && !self.handle_cow(page, irq) {
            return PageAccess::Refused;
        }
        PageAccess::Ready
    }

    /// Back `addr`'s page with memory if the heap or a region covers it.
    ///
    /// True means the address has memory at it now, not that this call is what
    /// put it there. Threads share an address space, so two of them reaching
    /// one page of a program's text at the same moment is ordinary, and the
    /// one that arrives second has nothing to do and nothing to report.
    ///
    /// The reference is taken first and held for the whole of the fault, which
    /// can sleep on the card: an exec on this task, or its exit, cannot free
    /// the tables under it.
    pub fn fault_in(&self, addr: u64) -> bool {
        match self.mm() {
            Some(mm) => mm.fault_in(addr),
            None => false,
        }
    }

    pub fn free_kernel_stack(&mut self) {
        if !self.kstack.is_null() {
            unsafe { dealloc(self.kstack, kstack_layout()) };
            self.kstack = core::ptr::null_mut();
        }
    }

    pub fn state(&self) -> State {
        self.state.get()
    }

    /// The tick this task is due to be woken at, or zero for no deadline.
    pub fn wake_at(&self) -> u64 {
        self.wake_at.get()
    }

    /// The address space the task runs in, shared with this task's threads.
    /// `None` for a kernel task and for one that has exited.
    ///
    /// A reference of the caller's own: what it reaches stays alive for as
    /// long as the caller holds it, whatever this task does meanwhile.
    pub fn mm(&self) -> Option<Arc<Mm>> {
        self.mm.lock().clone()
    }

    /// The `id` of the address space the task runs in, or zero for none.
    pub fn mm_id(&self) -> u64 {
        self.mm.lock().as_ref().map_or(0, |mm| mm.id())
    }

    // -----------------------------------------------------------------------
    // Transitions
    // -----------------------------------------------------------------------
    //
    // Each of these is one step with two halves: the task stops being runnable
    // and something is told about it, or it becomes runnable and the reason it
    // was waiting is cleared, or the record of the address space moves and the
    // register follows it. A tick landing between the halves is what the six
    // defects these replace all were, so each asks for proof that interrupts
    // are off for the whole of it. The three that have to reach a second task
    // take the process table, held, which is that proof and is also where the
    // other task is found.

    /// Park the task until `wake_at` ticks, or until something wakes it. Zero
    /// means no deadline: only a wake-up ends it.
    ///
    /// Whatever the caller is waiting for has to have been checked for inside
    /// the same section. A wake-up that lands between the check and this call
    /// finds a runnable task and wakes nothing, and the task then sleeps out
    /// the whole of the time it asked for with the reason to run already
    /// delivered.
    pub fn sleep(&self, wake_at: u64, _irq: NoInterrupts) {
        self.state.set(State::Sleeping);
        self.wake_at.set(wake_at);
    }

    /// Put a sleeping task back on the run queue, clearing its deadline.
    /// A task that is stopped stays stopped: only a continue restarts one.
    pub fn wake(&self, _irq: NoInterrupts) {
        if self.state.get() == State::Sleeping {
            self.state.set(State::Runnable);
            self.wake_at.set(0);
        }
    }

    /// Restart a stopped task without the report a continue owes its parent.
    /// A kill it can never look at is not a kill, and nothing else gets a
    /// stopped task running again.
    pub fn restart_for_kill(&self, _irq: NoInterrupts) {
        if self.state.get() == State::Stopped {
            self.state.set(State::Runnable);
            self.wake_at.set(0);
        }
    }

    /// Take the task off the run queue and tell its parent, in one step.
    ///
    /// The parent is normally asleep in wait4. If a tick lands between the two
    /// halves it hands the CPU to something else and never hands it back,
    /// because this task is no longer runnable, so the notification is left
    /// undelivered by a task that can no longer deliver it.
    pub fn stop(&self, signal: crate::signal::Signal, table: &crate::sched::Held) {
        self.stop_signal.set(Some(signal));
        self.report_stop.set(true);
        self.state.set(State::Stopped);
        table.notify_parent(self.ppid.get());
    }

    /// Restart a stopped task and record the continue for whoever waits.
    ///
    /// Returns false when the task was not stopped, which is a continue with
    /// nothing to undo.
    pub fn continue_after_stop(&self, table: &crate::sched::Held) -> bool {
        if self.state.get() != State::Stopped {
            return false;
        }
        self.state.set(State::Runnable);
        self.wake_at.set(0);
        // A stop nobody has been told about yet has stopped being true. Left
        // standing it is handed to whatever asks next, which is a suspension
        // reported after the job is running again.
        self.report_stop.set(false);
        self.report_continue.set(true);
        table.notify_parent(self.ppid.get());
        true
    }

    /// Make this task a zombie and wake whoever is waiting for it.
    ///
    /// This takes the task off the run queue for good, so everything owed on
    /// its behalf is owed now: a parent in vfork that has been holding the
    /// address space open, and, when this was the last thread of its process
    /// still running, the parent that may be in wait4. A process ends with its
    /// last thread, whichever thread that is: Linux's `exit_notify` tells the
    /// parent at the leader's exit only when the thread group is empty, and
    /// `release_task` tells it at the last other thread's exit when the leader
    /// went first. Any other thread's exit is not a child exit; whoever joins
    /// it is woken through its cleared tid word.
    pub fn become_zombie(&self, table: &crate::sched::Held) {
        self.state.set(State::Zombie);
        if let Some(parent_pid) = self.vfork_parent.take() {
            if let Some(parent) = table.find(parent_pid) {
                parent.wake(table.irq());
            }
        }
        if table.group_exited(self.tgid) {
            if let Some(leader) = table.find(self.tgid) {
                table.notify_parent_of_exit(leader);
            }
        }
        // Nothing will wait for a thread: the next task out of a system call
        // releases this one, which is still on the stack that would be handed
        // back. `notify_parent_of_exit` asks the same for a process whose
        // parent will not wait for it.
        if self.pid != self.tgid {
            crate::sched::tasks_to_release();
        }
    }

    /// The timer found this task's deadline passed.
    pub fn deadline_reached(&self, _irq: NoInterrupts) {
        self.wake_at.set(0);
        self.state.set(State::Runnable);
    }

    /// The task's memory has been handed back and its entry is out of the
    /// table. Nothing can reach it from here.
    pub fn mark_dead(&mut self) {
        self.state.set(State::Dead);
    }

    /// Run in `mm` from here on, or in no address space of the task's own for
    /// `None`, putting the processor on the matching tables: `mm`'s, or the
    /// kernel's.
    ///
    /// A context switch loads the incoming task's tables only when they are not
    /// the ones already loaded, so between the record changing and the
    /// processor following it the two disagree, and a sibling thread recorded
    /// on the other space is resumed with no load. The record and the register
    /// move together or not at all.
    ///
    /// What this lets go of comes back to the caller rather than being dropped
    /// inside the section, because the last reference to an address space
    /// frees it, and that is not something to do with interrupts off.
    pub fn run_on_mm(&self, mm: Option<Arc<Mm>>, irq: NoInterrupts) -> Displaced {
        let processor = match &mm {
            Some(mm) => crate::mm::space::switch_mm(mm, irq),
            None => crate::mm::space::switch_to_kernel(irq),
        };
        let task = core::mem::replace(&mut *self.mm.lock(), mm);
        Displaced { task, processor }
    }
}

/// Build the initial user stack: argv, envp and the auxiliary vector, laid out
/// the way a Linux process expects to find them.
pub fn build_user_stack(
    task: &Task,
    image: &crate::elf::LoadedImage,
    argv: &[String],
    envp: &[String],
    exec_path: &str,
    interp_base: u64,
) -> Result<u64, Errno> {
    // The strings go on the stack before the program exists to grow it, so
    // what they come to has to be known to fit before any of it is written.
    // A quarter of the reserve is the share Linux gives them out of the stack
    // limit, and it leaves the program the rest to run in.
    let text: u64 = argv
        .iter()
        .chain(envp.iter())
        .map(|value| value.len() as u64 + 1)
        .sum::<u64>()
        + crate::arch::MACHINE.len() as u64
        + 1
        + exec_path.len() as u64
        + 1;
    if text > MAX_ARG_BYTES {
        return Err(Errno::E2BIG);
    }

    let stack_low = USER_STACK_TOP - STACK_RESERVE;
    task.add_vma(stack_low, USER_STACK_TOP, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS);

    // Map the top of the stack eagerly; the rest grows in on demand.
    //
    // These pages are written through their user addresses below rather than
    // filled before they are published, which is the one caller here outside
    // that rule. What makes it safe is not the rule: the pages go in with the
    // protection they keep, so nothing is ever reachable with more permission
    // than it ends with, and the address space belongs to the one task
    // building it until the exec that is building it finishes, so there is no
    // sibling to see a page before its contents are there. The writes go
    // through the checked path on purpose, because a block longer than what is
    // mapped here has to grow the stack rather than fault in the kernel.
    let prefault_from = USER_STACK_TOP - STACK_PREFAULT;
    let mm = task.mm().ok_or(Errno::ENOMEM)?;
    let mut page = prefault_from;
    while page < USER_STACK_TOP {
        // Prepared outside the lock and published under it, one page at a
        // time, which is the shape of every publish in the kernel.
        let mut fresh = Prepared::new(0).ok_or(Errno::ENOMEM)?;
        let done = mm.publish_page(page, &mut fresh, PRESENT | WRITABLE | USER | NO_EXECUTE);
        drop(fresh);
        done.map_err(|_| Errno::ENOMEM)?;
        page += PAGE_SIZE_U64;
    }

    let mut sp = USER_STACK_TOP;

    // Strings first, from the very top down. These go through the checked
    // path: only the top of the stack is mapped at this point, and a block
    // longer than that lands on a page nothing has faulted in, which in the
    // kernel is fatal rather than a fault the handler can serve.
    // These go through the form that takes the task rather than reading it
    // back out of the scheduler: this function was handed a reference to it and
    // a second one alongside would be two references to the same task.
    let push_bytes = |sp: &mut u64, bytes: &[u8]| -> Result<u64, Errno> {
        *sp -= bytes.len() as u64 + 1;
        crate::uaccess::write_bytes_in(task, *sp, bytes)?;
        crate::uaccess::write_bytes_in(task, *sp + bytes.len() as u64, &[0])?;
        Ok(*sp)
    };

    let mut envp_addrs = Vec::with_capacity(envp.len());
    for value in envp.iter().rev() {
        envp_addrs.push(push_bytes(&mut sp, value.as_bytes())?);
    }
    envp_addrs.reverse();

    let mut argv_addrs = Vec::with_capacity(argv.len());
    for value in argv.iter().rev() {
        argv_addrs.push(push_bytes(&mut sp, value.as_bytes())?);
    }
    argv_addrs.reverse();

    let platform_addr = push_bytes(&mut sp, crate::arch::MACHINE.as_bytes())?;
    let execfn_addr = push_bytes(&mut sp, exec_path.as_bytes())?;

    // 16 bytes of randomness for AT_RANDOM (stack guard, pointer mangling).
    sp -= 16;
    sp &= !0xF;
    let random_addr = sp;
    let mut bytes = [0u8; 16];
    crate::fs::dev::fill_random(&mut bytes);
    crate::uaccess::write_bytes_in(task, random_addr, &bytes)?;

    let auxv: [(u64, u64); 14] = [
        (AT_PHDR, image.phdr_addr),
        (AT_PHENT, image.phent),
        (AT_PHNUM, image.phnum),
        (AT_PAGESZ, PAGE_SIZE_U64),
        (AT_BASE, interp_base),
        (AT_FLAGS, 0),
        (AT_ENTRY, image.entry),
        (AT_UID, 0),
        (AT_EUID, 0),
        (AT_GID, 0),
        (AT_EGID, 0),
        (AT_SECURE, 0),
        (AT_RANDOM, random_addr),
        (AT_EXECFN, execfn_addr),
    ];
    // Linux passes AT_HWCAP2 on both machines: `create_elf_tables` emits it
    // wherever the architecture defines `ELF_HWCAP2`, and x86 and arm64 both
    // do.
    let extra: [(u64, u64); 4] = [
        (AT_PLATFORM, platform_addr),
        (AT_CLKTCK, 100),
        (AT_HWCAP, crate::hwcap::hwcap()),
        (AT_HWCAP2, crate::hwcap::hwcap2()),
    ];
    // The vector as the words that go on the stack, AT_NULL pair included.
    // The same words are kept for /proc/<pid>/auxv, so the file cannot say
    // anything the program was not told.
    let mut vector: Vec<u64> = Vec::with_capacity(2 * (auxv.len() + extra.len() + 1));
    for (key, value) in auxv.iter().chain(extra.iter()) {
        vector.push(*key);
        vector.push(*value);
    }
    vector.push(AT_NULL);
    vector.push(0);

    // Size of the pointer block, so the final rsp lands 16-byte aligned.
    let words = 1                       // argc
        + argv.len() + 1                // argv + NULL
        + envp.len() + 1                // envp + NULL
        + vector.len();                 // auxv pairs + AT_NULL
    let block = (words * 8) as u64;
    sp = (sp - block) & !0xF;

    let mut at = sp;
    let push_word = |value: u64, at: &mut u64| -> Result<(), Errno> {
        crate::uaccess::write_u64_in(task, *at, value)?;
        *at += 8;
        Ok(())
    };
    push_word(argv.len() as u64, &mut at)?;
    for addr in &argv_addrs {
        push_word(*addr, &mut at)?;
    }
    push_word(0, &mut at)?;
    for addr in &envp_addrs {
        push_word(*addr, &mut at)?;
    }
    push_word(0, &mut at)?;
    for word in &vector {
        push_word(*word, &mut at)?;
    }
    // Into the record of the address space the stack was just built in, which
    // exec made this task's before loading anything. An exec that fails puts
    // the old record back, and that still holds the old program's vector.
    task.with_mem(|mm| mm.saved_auxv = vector);

    Ok(sp)
}

/// Populate a task's trap frame so it starts at `entry` with stack `sp`.
pub fn set_user_entry(task: &Task, entry: u64, sp: u64) {
    let frame = task.trap_frame();
    unsafe { arch::start_user_at(&mut *frame, entry, sp) };
}

/// Open the standard descriptors on the console.
pub fn attach_console(task: &Task) -> Result<(), Errno> {
    let console = crate::fs::lookup("/dev/console")?;
    let stdin = OpenFile::from_node_at(console.clone(), O_RDONLY, "/dev/console");
    let stdout = OpenFile::from_node_at(console.clone(), O_WRONLY, "/dev/console");
    let stderr = OpenFile::from_node_at(console, O_WRONLY, "/dev/console");
    task.fds.insert_at(0, stdin, false);
    task.fds.insert_at(1, stdout, false);
    task.fds.insert_at(2, stderr, false);
    Ok(())
}

/// Read a file's contents, following a `#!` line if present.
/// Find the file exec should run: the named one, or the interpreter its `#!`
/// line names. The file itself is not read here; exec reads what it needs
/// straight out of the node, and the pages come from there afterwards.
pub fn read_executable(
    path: &str,
) -> Result<(crate::fs::NodeRef, Option<(String, Option<String>)>), Errno> {
    let node = crate::fs::lookup(path)?;
    if node.is_dir() {
        return Err(Errno::EACCES);
    }
    if node.mode() & 0o111 == 0 {
        return Err(Errno::EACCES);
    }
    let shebang = if node.kind == crate::fs::NodeKind::DataFile {
        // A file on the data volume holds nothing in its node, so its first
        // bytes are read from the card: as many as Linux's BINPRM_BUF_SIZE,
        // which is the longest `#!` line Linux reads.
        let mut head = [0u8; 256];
        let n = crate::fs::data::read(&node, crate::fs::Offset::START, &mut head)?;
        shebang_of(&head[..n])?
    } else {
        shebang_of(&node.inner.lock().data)?
    };
    if let Some((interp, arg)) = shebang {
        let interp_node = crate::fs::lookup(&interp)?;
        if interp_node.mode() & 0o111 == 0 {
            return Err(Errno::EACCES);
        }
        return Ok((interp_node, Some((interp, arg))));
    }
    Ok((node, None))
}

/// The interpreter a `#!` line at the start of `data` names, and the one
/// argument that may follow it.
fn shebang_of(data: &[u8]) -> Result<Option<(String, Option<String>)>, Errno> {
    if !data.starts_with(b"#!") {
        return Ok(None);
    }
    let line_end = data.iter().position(|&b| b == b'\n').unwrap_or(data.len());
    let line = core::str::from_utf8(&data[2..line_end]).map_err(|_| Errno::ENOEXEC)?;
    let line = line.trim();
    let mut parts = line.splitn(2, char::is_whitespace);
    let interp = parts.next().unwrap_or("").trim().to_string();
    let arg = parts.next().map(|a| a.trim().to_string()).filter(|a| !a.is_empty());
    if interp.is_empty() {
        return Err(Errno::ENOEXEC);
    }
    Ok(Some((interp, arg)))
}

/// Entry point for a task created by the kernel rather than by fork: load the
/// program it was created for, then drop into user mode.
pub extern "C" fn user_bootstrap() -> ! {
    let Some((path, argv, envp)) = crate::sched::current().take_pending_exec() else {
        crate::println!("[kernel] bootstrap task has no program");
        crate::sched::exit_current(1 << 8);
    };
    if let Err(err) = crate::syscall::proc::exec_into_current(&path, argv, envp) {
        crate::println!("[kernel] cannot exec {}: {:?}", path, err);
        crate::sched::exit_current(1 << 8);
    }
    let task = crate::sched::current();
    arch::set_kernel_entry_stack(task.kstack_top);
    unsafe { arch::return_to_user(task.trap_frame()) }
}

/// Build a task that will run `path`. Its pid is taken now; handing it to
/// `sched::register` is what starts it, and a task built here has to be, or
/// its kernel stack is never given back.
///
/// The two are apart so the pid order and the start order can differ: init
/// has to be pid 1, and at boot it is held back while the network task, which
/// is pid 2, asks for an address.
pub fn prepare(
    path: &str,
    argv: Vec<String>,
    envp: Vec<String>,
    parent_pid: u32,
) -> Result<alloc::boxed::Box<Task>, Errno> {
    // Nothing owns the kernel stack until the task is registered, so anything
    // that goes wrong before then hands it back here or it is held by nobody:
    // dropping the task does not free it. The task has no address space until
    // the exec its bootstrap runs gives it one.
    let name = path.rsplit('/').next().unwrap_or(path);
    let Some(mut task) = Task::new(name, None) else {
        return Err(Errno::ENOMEM);
    };
    task.ppid.set(parent_pid);
    task.pgid.set(task.pid);
    if let Err(err) = attach_console(&task) {
        task.free_kernel_stack();
        return Err(err);
    }
    task.set_pending_exec((path.to_string(), argv, envp));
    task.prepare_kernel_frame(user_bootstrap as extern "C" fn() -> ! as usize as u64);
    Ok(task)
}

/// An entry in the process table: a task `sched::register` took the box of.
///
/// The pointer is private. With a public one, any code could build an entry
/// naming anything, or keep reading one after `release` had freed the task
/// behind it, and `get` would hand out a reference to whatever was there.
pub struct TaskPtr(*mut Task);
unsafe impl Send for TaskPtr {}

impl TaskPtr {
    /// Take ownership of `task` as an entry. Nothing frees it until `release`
    /// turns the pointer back into the box.
    pub fn new(task: alloc::boxed::Box<Task>) -> TaskPtr {
        TaskPtr(alloc::boxed::Box::into_raw(task))
    }

    /// The task's address, for comparing entries and for handing the box back.
    pub fn as_ptr(&self) -> *mut Task {
        self.0
    }

    /// The task, borrowed for as long as the entry that names it.
    ///
    /// # Safety
    ///
    /// The entry must be in the process table, and the reference must not be
    /// used after the hold on the table it was read under is let go.
    /// `sched::release` frees a task once it has been taken out of the table
    /// under that lock, and nothing else keeps the task alive, so a reference
    /// kept past the hold can name freed memory. The hold also masks
    /// interrupts, which is what keeps the task's cells whole while they are
    /// read through a reference another task can hold too.
    pub unsafe fn get(&self) -> &Task {
        // SAFETY: the caller keeps the entry in the table for the borrow.
        unsafe { &*self.0 }
    }
}

/// Copy a slice of C strings from user memory.
pub fn read_string_array(mut addr: u64) -> Result<Vec<String>, Errno> {
    let mut out = Vec::new();
    if addr == 0 {
        return Ok(out);
    }
    loop {
        if out.len() > 4096 {
            return Err(Errno::E2BIG);
        }
        let ptr = crate::uaccess::read_u64(addr)?;
        if ptr == 0 {
            break;
        }
        out.push(crate::uaccess::read_cstr(ptr, 4096)?);
        addr += 8;
    }
    Ok(out)
}

pub fn share_arc<T>(value: &Arc<T>) -> Arc<T> {
    value.clone()
}
