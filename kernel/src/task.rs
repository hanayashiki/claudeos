//! Tasks: kernel stacks, address spaces, and the process table.
//!
//! The kernel runs on a single CPU with interrupt-disabling locks, so the
//! current task is reached through a raw pointer rather than a lock that a
//! blocking syscall would have to hold across a context switch. What keeps a
//! change to a task whole is the interrupt mask, not a lock on the task.

use crate::abi::*;
use crate::arch::paging::{
    AddressSpace, FreshPage, MapError, NO_EXECUTE, PRESENT, USER, WRITABLE,
};
use crate::arch::{self, TaskContext, TrapFrame};
use crate::fs::{FdTable, OpenFile};
use crate::mm::{page_align_down, page_align_up, PAGE_SIZE_U64, USER_MMAP_BASE, USER_STACK_TOP};
use alloc::alloc::{alloc, dealloc, Layout};
use alloc::string::{String, ToString};
use crate::sync::{NoInterrupts, Spinlock};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::Cell;
use core::sync::atomic::{AtomicU32, Ordering};

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

/// Where a region's contents come from, for a region backed by a file.
///
/// An executable is not copied into memory at exec: the pages are filled one
/// at a time from the file as the program reaches them, which is most of what
/// makes starting a program cheap.
#[derive(Clone)]
pub struct FileMap {
    pub node: crate::fs::NodeRef,
    /// Offset in the file of the region's first byte.
    pub offset: u64,
    /// Bytes from the start of the region that come from the file. Anything
    /// past this reads as zero, which is what .bss is.
    pub length: u64,
}

impl core::fmt::Debug for FileMap {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "FileMap {{ offset: {:#x}, length: {:#x} }}", self.offset, self.length)
    }
}

/// A region of the user address space, used to fault pages in on demand.
#[derive(Debug, Clone)]
pub struct Vma {
    pub start: u64,
    pub end: u64,
    pub prot: u64,
    pub flags: u64,
    pub file: Option<FileMap>,
}

impl Vma {
    pub fn contains(&self, addr: u64) -> bool {
        addr >= self.start && addr < self.end
    }

    /// Page table bits this region's pages should get.
    pub fn page_flags(&self) -> u64 {
        let mut bits = PRESENT | USER;
        if self.prot & PROT_WRITE != 0 {
            bits |= WRITABLE;
        }
        if self.prot & PROT_EXEC == 0 {
            bits |= NO_EXECUTE;
        }
        bits
    }
}

/// The parts of the address space bookkeeping that threads share. The page
/// tables themselves are shared through the identical `AddressSpace` value;
/// this holds the region list and heap bounds that go with them.
pub struct MemState {
    pub vmas: Vec<Vma>,
    pub brk_start: u64,
    pub brk: u64,
    pub mmap_top: u64,
}

impl MemState {
    pub fn new() -> MemState {
        MemState { vmas: Vec::new(), brk_start: 0, brk: 0, mmap_top: USER_MMAP_BASE }
    }
}

/// A task control block.
///
/// Everything a task changes after it is in the process table sits in a cell,
/// so that changing it asks for a shared reference rather than an exclusive
/// one. That is not what makes the change whole: a single processor with
/// interrupts masked is, and a cell's load-modify-store is the same
/// instructions the plain field was.
///
/// The fields that are not in cells are the ones fixed before the task is
/// admitted, while it is a box one caller owns and nothing else can reach.
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

    /// The page tables the task runs on, and the record of what is in them.
    /// Private for the same reason the scheduling state is: the two have to
    /// change together, and `run_on_space` is where that happens.
    space: Cell<AddressSpace>,
    /// Shared with every thread running in the same address space. Exec gives
    /// the task a different one and leaves the old to whatever still runs on
    /// it, so the share itself is what changes, not only its contents.
    mm: Spinlock<Arc<Spinlock<MemState>>>,
    /// Shared with every task that cloned with `CLONE_FILES`.
    pub fds: FdTable,
    /// Shared with every task that cloned with `CLONE_FS`, so that a directory
    /// one thread changes into is the one its siblings resolve against.
    pub cwd: Arc<Spinlock<String>>,
    name: Spinlock<String>,
    /// Path of the running executable, reported through /proc/self/exe.
    exe_path: Spinlock<String>,

    pub exit_code: Cell<i32>,
    /// Registers only this architecture has, carried across context switches.
    cpu: Cell<TaskContext>,
    pub clear_child_tid: Cell<u64>,
    pub set_child_tid: Cell<u64>,
    pub robust_list: Cell<u64>,

    pub pending_signals: Cell<u64>,
    /// The scheduling nice value. Round robin does not act on it, but a
    /// program that sets it reads it back.
    pub nice: Cell<i32>,
    /// The signal that stopped this task, and whether the stop and the
    /// following continue have been reported to whoever is waiting.
    pub stop_signal: Cell<i32>,
    pub report_stop: Cell<bool>,
    pub report_continue: Cell<bool>,
    signal_actions: [Cell<crate::signal::SigAction>; 64],
    pub signal_mask: Cell<u64>,

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
    /// Allocate a task with a fresh kernel stack and address space.
    pub fn new(name: &str, space: AddressSpace) -> Option<alloc::boxed::Box<Task>> {
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
            space: Cell::new(space),
            mm: Spinlock::new(Arc::new(Spinlock::new(MemState::new()))),
            fds: FdTable::new(),
            cwd: Arc::new(Spinlock::new(String::from("/"))),
            name: Spinlock::new(name.to_string()),
            exe_path: Spinlock::new(String::new()),
            exit_code: Cell::new(0),
            cpu: Cell::new(TaskContext::new()),
            clear_child_tid: Cell::new(0),
            set_child_tid: Cell::new(0),
            robust_list: Cell::new(0),
            pending_signals: Cell::new(0),
            nice: Cell::new(0),
            stop_signal: Cell::new(0),
            report_stop: Cell::new(false),
            report_continue: Cell::new(false),
            signal_actions: core::array::from_fn(|_| {
                Cell::new(crate::signal::SigAction::default())
            }),
            signal_mask: Cell::new(0),
            wake_at: Cell::new(0),
            waiting_for: Cell::new(None),
            umask: Cell::new(0o022),
            started: Cell::new(false),
            pending_exec: Spinlock::new(None),
            vfork_parent: Cell::new(None),
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

    /// The disposition of one signal, and the way to change it.
    pub fn action(&self, signal: usize) -> crate::signal::SigAction {
        self.signal_actions[signal].get()
    }

    pub fn set_action(&self, signal: usize, action: crate::signal::SigAction) {
        self.signal_actions[signal].set(action);
    }

    /// Take another task's dispositions, which is what a fork gives the child.
    pub fn copy_actions_from(&self, other: &Task) {
        for (mine, theirs) in self.signal_actions.iter().zip(other.signal_actions.iter()) {
            mine.set(theirs.get());
        }
    }

    /// Handlers do not survive exec, but ignored signals stay ignored.
    pub fn reset_actions_for_exec(&self) {
        for action in self.signal_actions.iter() {
            if action.get().handler != crate::signal::SIG_IGN {
                action.set(crate::signal::SigAction::default());
            }
        }
    }

    /// Add and remove signals from the pending set. Every caller is a read,
    /// an or or an and, and a write, which is what the plain field was.
    pub fn add_pending(&self, signals: u64) {
        self.pending_signals.set(self.pending_signals.get() | signals);
    }

    pub fn drop_pending(&self, signals: u64) {
        self.pending_signals.set(self.pending_signals.get() & !signals);
    }

    /// Run `f` on the registers a context switch carries by hand.
    ///
    /// Reached through the cell's own pointer rather than by copying the value
    /// out and back: on one of the two machines this is half a kilobyte of
    /// vector state, and it is saved and restored at every switch. `as_ptr` is
    /// what a shared reference to a cell yields for exactly this, and nothing
    /// holds a reference into this one -- the field is private and this is the
    /// only way to reach it.
    pub fn with_cpu<R>(&self, f: impl FnOnce(&mut TaskContext) -> R) -> R {
        f(unsafe { &mut *self.cpu.as_ptr() })
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
    /// call that asked for it runs. Two locks: the outer one holds the share
    /// still, because exec replaces it with a different one, and the inner one
    /// is the record's own.
    fn with_mem<R>(&self, f: impl FnOnce(&mut MemState) -> R) -> R {
        let mm = self.mm.lock();
        let mut state = mm.lock();
        f(&mut state)
    }

    pub fn cwd(&self) -> String {
        self.cwd.lock().clone()
    }

    pub fn set_cwd(&self, path: String) {
        *self.cwd.lock() = path;
    }

    pub fn brk_start(&self) -> u64 {
        self.with_mem(|mm| mm.brk_start)
    }

    pub fn brk(&self) -> u64 {
        self.with_mem(|mm| mm.brk)
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
        self.with_mem(|mm| mm.vmas.iter().find(|v| v.contains(addr)).cloned())
    }

    /// True when no recorded region overlaps `[start, end)`.
    pub fn range_is_free(&self, start: u64, end: u64) -> bool {
        self.with_mem(|mm| mm.vmas.iter().all(|v| v.end <= start || v.start >= end))
    }

    pub fn clear_vmas(&self) {
        self.with_mem(|mm| {
            mm.vmas.clear();
            mm.mmap_top = USER_MMAP_BASE;
        });
    }

    /// Total size of every recorded region plus the heap, for /proc reporting.
    pub fn virtual_size(&self) -> u64 {
        self.with_mem(|mm| {
            let regions: u64 = mm.vmas.iter().map(|v| v.end - v.start).sum();
            regions + mm.brk.saturating_sub(mm.brk_start)
        })
    }

    /// Pages actually backed by memory right now.
    pub fn resident_pages(&self) -> u64 {
        let space = self.space.get();
        self.with_mem(|mm| {
            let mut pages = 0u64;
            for vma in mm.vmas.iter() {
                let mut page = vma.start;
                while page < vma.end {
                    if space.translate(page).is_some() {
                        pages += 1;
                    }
                    page += PAGE_SIZE_U64;
                }
            }
            let mut page = mm.brk_start;
            while page < mm.brk {
                if space.translate(page).is_some() {
                    pages += 1;
                }
                page += PAGE_SIZE_U64;
            }
            pages
        })
    }

    pub fn snapshot_vmas(&self) -> Vec<Vma> {
        self.with_mem(|mm| mm.vmas.clone())
    }

    /// Give every region overlapping `[start, end)` the new protection.
    pub fn set_vma_prot(&self, start: u64, end: u64, prot: u64) {
        self.with_mem(|mm| {
            for vma in mm.vmas.iter_mut() {
                if vma.start < end && start < vma.end {
                    vma.prot = prot;
                }
            }
        });
    }

    /// Remove `[start, end)` from the recorded regions, splitting as needed.
    pub fn remove_vma_range(&self, start: u64, end: u64) {
        self.with_mem(|mm| {
        let mut out: Vec<Vma> = Vec::new();
        for vma in mm.vmas.iter().cloned() {
            if vma.end <= start || vma.start >= end {
                out.push(vma);
                continue;
            }
            if vma.start < start {
                let mut head = vma.clone();
                head.end = start;
                if let Some(file) = &mut head.file {
                    file.length = file.length.min(start - vma.start);
                }
                out.push(head);
            }
            if vma.end > end {
                let mut tail = vma.clone();
                tail.start = end;
                // The tail begins further into the file than the whole did.
                if let Some(file) = &mut tail.file {
                    let skipped = end - vma.start;
                    file.offset += skipped;
                    file.length = file.length.saturating_sub(skipped);
                }
                out.push(tail);
            }
        }
        mm.vmas = out;
        });
    }

    /// Find a free span of `len` bytes in the mmap area.
    pub fn find_free_region(&self, len: u64) -> u64 {
        let len = page_align_up(len);
        self.with_mem(|mm| {
            let mut candidate = mm.mmap_top;
            loop {
                let end = candidate + len;
                let clash =
                    mm.vmas.iter().find(|v| v.start < end && candidate < v.end).map(|v| v.end);
                match clash {
                    Some(v) => candidate = v,
                    None => {
                        mm.mmap_top = end;
                        return candidate;
                    }
                }
            }
        })
    }

    /// Give this task a private copy of a shared page it is trying to write.
    /// Returns false when the fault was not a copy-on-write fault.
    ///
    /// Nothing in the task itself changes: the page tables are reached through
    /// a value the task holds by copy, and the region list through a lock. A
    /// shared reference is what the validating path can hand over, and asking
    /// for an exclusive one there would mean a second one to a task the caller
    /// already holds.
    ///
    /// The whole of it is one step. What the entry says, what it points at,
    /// how many address spaces that frame is in and what finally goes in the
    /// entry have to be one account of the page: a sibling thread that runs in
    /// the middle of it is looking at the same entry, and what it does there
    /// is decided by a state that only exists halfway through this. The token
    /// is the proof that it cannot. On aarch64 a fault from user mode is
    /// handled with interrupts in the state the faulting code was in, so a
    /// program's own fault arrives here with them on, and `read`, `recvfrom`
    /// and `mremap` reach here from a system call with them on whichever
    /// machine it is.
    pub fn handle_cow(&self, addr: u64, irq: NoInterrupts) -> bool {
        use crate::arch::paging::COW;
        let page = page_align_down(addr);
        let space = self.space.get();
        let Some(flags) = space.flags_of(page) else {
            return false;
        };
        if flags & COW == 0 {
            return false;
        }
        let Some(phys) = space.translate(page).map(page_align_down) else {
            return false;
        };

        // The last owner can simply take the page back.
        if crate::mm::frame::frame_references(phys) <= 1 {
            return space.set_flags(page, (flags & !COW) | WRITABLE).is_some();
        }

        let Some(copy) = crate::mm::frame::alloc() else {
            return false;
        };
        unsafe {
            core::ptr::copy_nonoverlapping(
                crate::mm::phys_to_virt(phys) as *const u8,
                crate::mm::phys_to_virt(copy.addr()) as *mut u8,
                crate::mm::PAGE_SIZE,
            );
        }
        // A page the program may execute has just been written through a
        // different address than the one it will be fetched from.
        if flags & NO_EXECUTE == 0 {
            crate::arch::sync_instruction_cache(
                crate::mm::phys_to_virt(copy.addr()),
                crate::mm::PAGE_SIZE,
            );
        }
        // The copy goes in over the shared page in one store, which hands back
        // the reference this table held on the frame it was sharing. Dropping
        // it is the release.
        let shared = space.replace(page, copy, (flags & !COW) | WRITABLE, irq);
        let replaced = shared.is_some();
        drop(shared);
        replaced
    }

    /// Back `addr`'s page with memory if the heap or a region covers it.
    ///
    /// True means the address has memory at it now, not that this call is what
    /// put it there. Threads share an address space, so two of them reaching
    /// one page of a program's text at the same moment is ordinary, and the
    /// one that arrives second has nothing to do and nothing to report. What
    /// it finds is finished, because a page is published with its contents
    /// already in it.
    pub fn fault_in(&self, addr: u64) -> bool {
        let page = page_align_down(addr);
        let space = self.space.get();
        if space.translate(page).is_some() {
            // The hardware found nothing at this address and the tables have
            // something at it: two readings of one entry either side of a
            // sibling's store. The faulting instruction can run again. A fault
            // the tables really do refuse does not arrive here, because a
            // present page's fault is a protection violation and that is
            // decided before this is called.
            return true;
        }
        let (in_heap, vma) = self.with_mem(|mm| {
            (
                page >= mm.brk_start && page < mm.brk,
                mm.vmas.iter().find(|v| v.contains(page)).cloned(),
            )
        });
        if in_heap {
            return served(space.map_new(page, PRESENT | WRITABLE | USER | NO_EXECUTE));
        }
        let Some(vma) = vma else {
            return false;
        };
        let Some(file) = &vma.file else {
            return served(space.map_new(page, vma.page_flags()));
        };

        // A page of an executable is filled before it is published, and goes
        // in once with the protection its segment asked for. A fresh frame is
        // already zero, so the part past the file's contents needs nothing.
        let Some(mut fresh) = FreshPage::new() else {
            return false;
        };
        let flags = vma.page_flags();
        let into = page - vma.start;
        if into < file.length {
            let want = (file.length - into).min(PAGE_SIZE_U64) as usize;
            let from = (file.offset + into) as usize;
            let filled = {
                let data = file.node.inner.lock();
                let available = data.data.len().saturating_sub(from).min(want);
                fresh.bytes()[..available].copy_from_slice(&data.data[from..from + available]);
                available
            };
            // A text page arrives this way, and these bytes have just been
            // written through a different address than the one they will be
            // fetched from.
            if filled > 0 && flags & NO_EXECUTE == 0 {
                crate::arch::sync_instruction_cache(fresh.bytes().as_ptr() as u64, filled);
            }
        }
        served(space.publish(page, fresh, flags))
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

    /// The page tables the task runs on.
    pub fn space(&self) -> AddressSpace {
        self.space.get()
    }

    /// The record of what is in them, shared with this task's threads.
    pub fn mm(&self) -> Arc<Spinlock<MemState>> {
        self.mm.lock().clone()
    }

    /// How many tasks hold that record: the threads running in this address
    /// space, this one among them. The last one out is the only task that may
    /// tear the address space down, and this is how it knows it is the last.
    pub fn mm_shares(&self) -> usize {
        Arc::strong_count(&self.mm.lock())
    }

    /// Take a copy of `other`'s region list, which is what a fork that does
    /// not share the address space gives the child.
    pub fn copy_mem_from(&self, other: &Task) {
        let source = other.mm();
        let source = source.lock();
        self.with_mem(|target| {
            target.vmas = source.vmas.clone();
            target.brk_start = source.brk_start;
            target.brk = source.brk;
            target.mmap_top = source.mmap_top;
        });
    }

    /// Run in the address space `other` runs in, sharing its region list.
    ///
    /// Only for a task that has not been admitted to the scheduler yet, which
    /// is why it asks for no proof of anything: nothing can see this task to
    /// be confused by a half-done change. A task that is already running
    /// changes address space through `run_on_space`.
    pub fn share_space_of(&self, other: &Task) {
        self.space.set(other.space.get());
        *self.mm.lock() = other.mm();
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
    pub fn stop(&self, signal: i32, table: &crate::sched::Held) {
        self.stop_signal.set(signal);
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
    /// address space open, and, for a process rather than one of the threads
    /// inside one, the parent that may be in wait4. A thread's exit is not a
    /// child exit; whoever joins it is woken through its cleared tid word.
    pub fn become_zombie(&self, table: &crate::sched::Held) {
        self.state.set(State::Zombie);
        if let Some(parent_pid) = self.vfork_parent.take() {
            if let Some(parent) = table.find(parent_pid) {
                parent.wake(table.irq());
            }
        }
        if self.pid == self.tgid {
            table.notify_parent(self.ppid.get());
        }
    }

    /// Record that a child of this task changed state, and put it back on the
    /// run queue if it was asleep.
    ///
    /// Any sleep, not only a wait for a child. This is a signal, and every
    /// other signal returns a sleeping task to the run queue; a shell blocked
    /// reading its terminal has a handler for this one and would otherwise not
    /// learn that a background job had finished until the next key was
    /// pressed.
    pub fn child_changed_state(&self, irq: NoInterrupts) {
        self.add_pending(1u64 << (SIGCHLD as u64 & 63));
        self.wake(irq);
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

    /// Run on `space` from here on, with `mm` as the record of what is in it.
    ///
    /// A context switch reloads the page table root only when the two tasks'
    /// recorded spaces differ, so between the record changing and the CPU
    /// following it the two disagree, and a sibling thread recorded on the
    /// other one is resumed with no reload. The record and the register move
    /// together or not at all.
    pub fn run_on_space(
        &self,
        space: AddressSpace,
        mm: Arc<Spinlock<MemState>>,
        _irq: NoInterrupts,
    ) {
        self.space.set(space);
        *self.mm.lock() = mm;
        unsafe { space.switch_to() };
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
    let mut page = prefault_from;
    while page < USER_STACK_TOP {
        task.space()
            .map_new(page, PRESENT | WRITABLE | USER | NO_EXECUTE)
            .map_err(|_| Errno::ENOMEM)?;
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
    let extra: [(u64, u64); 3] = [
        (AT_PLATFORM, platform_addr),
        (AT_CLKTCK, 100),
        (AT_HWCAP, 0),
    ];

    // Size of the pointer block, so the final rsp lands 16-byte aligned.
    let words = 1                       // argc
        + argv.len() + 1                // argv + NULL
        + envp.len() + 1                // envp + NULL
        + 2 * (auxv.len() + extra.len() + 1); // auxv pairs + AT_NULL
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
    for (key, value) in auxv.iter().chain(extra.iter()) {
        push_word(*key, &mut at)?;
        push_word(*value, &mut at)?;
    }
    push_word(AT_NULL, &mut at)?;
    push_word(0, &mut at)?;

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
    let shebang = {
        let inner = node.inner.lock();
        let data = &inner.data;
        if data.starts_with(b"#!") {
            let line_end = data.iter().position(|&b| b == b'\n').unwrap_or(data.len());
            let line = core::str::from_utf8(&data[2..line_end]).map_err(|_| Errno::ENOEXEC)?;
            let line = line.trim();
            let mut parts = line.splitn(2, char::is_whitespace);
            let interp = parts.next().unwrap_or("").trim().to_string();
            let arg = parts.next().map(|a| a.trim().to_string()).filter(|a| !a.is_empty());
            if interp.is_empty() {
                return Err(Errno::ENOEXEC);
            }
            Some((interp, arg))
        } else {
            None
        }
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

/// Create a task that will run `path` once scheduled.
pub fn spawn(
    path: &str,
    argv: Vec<String>,
    envp: Vec<String>,
    parent_pid: u32,
) -> Result<u32, Errno> {
    // Nothing owns the address space or the kernel stack until the task is
    // registered, so anything that goes wrong before then hands them back here
    // or they are held by nobody: dropping the task frees neither.
    let space = crate::arch::paging::AddressSpace::new_user().ok_or(Errno::ENOMEM)?;
    let name = path.rsplit('/').next().unwrap_or(path);
    let Some(mut task) = Task::new(name, space) else {
        space.destroy();
        return Err(Errno::ENOMEM);
    };
    task.ppid.set(parent_pid);
    task.pgid.set(task.pid);
    if let Err(err) = attach_console(&task) {
        task.free_kernel_stack();
        space.destroy();
        return Err(err);
    }
    task.set_pending_exec((path.to_string(), argv, envp));
    task.prepare_kernel_frame(user_bootstrap as extern "C" fn() -> ! as usize as u64);
    Ok(crate::sched::register(task))
}

pub struct TaskPtr(pub *mut Task);
unsafe impl Send for TaskPtr {}

impl TaskPtr {
    pub fn get(&self) -> &'static mut Task {
        unsafe { &mut *self.0 }
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
