//! Address spaces: one owner for each, and the reference a processor holds on
//! the one it is running on.
//!
//! - `Mm` owns a program's page tables and the record of what is in them, the
//!   regions and the program break, behind one lock. Tasks hold it as
//!   `Arc<Mm>`: threads made with `CLONE_VM` share the `Arc`, a fork makes a
//!   new `Mm` from a copy of the tables, and exec gives the task a new one.
//!   The last `Arc` to go drops the `PageTables` inside, which frees the
//!   space. Nothing decides by looking at other tasks whether a space is still
//!   in use; the count is the answer, and it cannot be asked twice.
//! - The processor holds an `Arc` to the `Mm` whose tables it has loaded
//!   (`switch_mm`), so an address space is not freed while the processor walks
//!   it. A kernel task has no `Mm` and runs on whatever is loaded.
//! - A last reference that goes while interrupts are masked does not free the
//!   space there: freeing walks every table the space has, which for a large
//!   program is long enough to hold the timer off. The tables wait in a list
//!   that `release_deferred` empties from a place interrupts are on.
//!
//! The page tables are behind the lock, but the changes to them are not made
//! under it yet: `tables_unlocked` hands them out for that, as the
//! `AddressSpace` value every task held used to. Moving those changes onto
//! `MmGuard` is the next step, and removes `tables_unlocked`.

use super::active::{Active, Deferred};
use crate::abi::{PROT_EXEC, PROT_WRITE};
use crate::arch::paging::{kernel_tables, Borrowed, PageTables, NO_EXECUTE, PRESENT, USER, WRITABLE};
use crate::mm::{page_align_up, USER_MMAP_BASE};
use crate::sync::{interrupts_enabled, NoInterrupts, SpinGuard, Spinlock};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::mem::ManuallyDrop;
use core::ops::{Deref, DerefMut};

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

/// The record of what is in an address space: the regions, the heap bounds and
/// the auxiliary vector.
#[derive(Clone)]
pub struct MemState {
    pub vmas: Vec<Vma>,
    pub brk_start: u64,
    pub brk: u64,
    pub mmap_top: u64,
    /// The auxiliary vector exec wrote onto the stack of the program running
    /// here, word for word, ending with the AT_NULL pair: what
    /// /proc/<pid>/auxv hands back. Linux keeps it in the mm as `saved_auxv`,
    /// and it goes the same way here: threads share it, a fork copies it, and
    /// exec starts a new one. Empty until the first exec, as a task with no mm
    /// has none to show.
    pub saved_auxv: Vec<u64>,
}

impl MemState {
    pub fn new() -> MemState {
        MemState {
            vmas: Vec::new(),
            brk_start: 0,
            brk: 0,
            mmap_top: USER_MMAP_BASE,
            saved_auxv: Vec::new(),
        }
    }

    /// The region that covers `addr`.
    pub fn find_vma(&self, addr: u64) -> Option<&Vma> {
        self.vmas.iter().find(|v| v.contains(addr))
    }

    /// True when no recorded region overlaps `[start, end)`.
    pub fn range_is_free(&self, start: u64, end: u64) -> bool {
        self.vmas.iter().all(|v| v.end <= start || v.start >= end)
    }

    /// Give every region overlapping `[start, end)` the new protection.
    pub fn set_vma_prot(&mut self, start: u64, end: u64, prot: u64) {
        for vma in self.vmas.iter_mut() {
            if vma.start < end && start < vma.end {
                vma.prot = prot;
            }
        }
    }

    /// Remove `[start, end)` from the recorded regions, splitting as needed.
    pub fn remove_vma_range(&mut self, start: u64, end: u64) {
        let mut out: Vec<Vma> = Vec::new();
        for vma in self.vmas.iter().cloned() {
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
        self.vmas = out;
    }

    /// Find a free span of `len` bytes in the mmap area.
    pub fn find_free_region(&mut self, len: u64) -> u64 {
        let len = page_align_up(len);
        let mut candidate = self.mmap_top;
        loop {
            let end = candidate + len;
            let clash =
                self.vmas.iter().find(|v| v.start < end && candidate < v.end).map(|v| v.end);
            match clash {
                Some(v) => candidate = v,
                None => {
                    self.mmap_top = end;
                    return candidate;
                }
            }
        }
    }

    /// Total size of every recorded region plus the heap, for /proc reporting.
    pub fn virtual_size(&self) -> u64 {
        let regions: u64 = self.vmas.iter().map(|v| v.end - v.start).sum();
        regions + self.brk.saturating_sub(self.brk_start)
    }
}

/// A program's address space: its page tables and the record of what is in
/// them, behind one lock.
pub struct Mm {
    /// The tables' `id`, kept outside the lock. It is what a switch compares
    /// and loads, what a futex key names, and what `tables_unlocked` reaches
    /// the tables by, none of which may wait for the lock or read through it
    /// while another holder has the record open. It does not change.
    root: u64,
    inner: Spinlock<Inner>,
}

struct Inner {
    /// Dropped by `Mm`'s own drop, which decides whether that happens now or
    /// once interrupts are on.
    tables: ManuallyDrop<PageTables>,
    mem: MemState,
}

/// The address space, locked. It reads and writes as the record of regions;
/// the page tables are `tables`.
///
/// The lock is a spinlock that masks interrupts, so this is correct on several
/// processors as well as against the timer on one.
pub struct MmGuard<'a> {
    inner: SpinGuard<'a, Inner>,
}

impl MmGuard<'_> {
    /// The page tables, for reading them while the record is held still.
    pub fn tables(&self) -> &PageTables {
        &self.inner.tables
    }
}

impl Deref for MmGuard<'_> {
    type Target = MemState;

    fn deref(&self) -> &MemState {
        &self.inner.mem
    }
}

impl DerefMut for MmGuard<'_> {
    fn deref_mut(&mut self) -> &mut MemState {
        &mut self.inner.mem
    }
}

impl Mm {
    /// A fresh address space with nothing in its user half.
    pub fn new_user() -> Option<Arc<Mm>> {
        let tables = PageTables::new_user()?;
        Some(Arc::new(Mm {
            root: tables.id(),
            inner: Spinlock::new(Inner { tables: ManuallyDrop::new(tables), mem: MemState::new() }),
        }))
    }

    /// The address space a forked child starts with: tables that share every
    /// page of this one copy-on-write, and a copy of the record.
    ///
    /// A copy that fails partway is dropped here, and its drop gives back what
    /// it had collected.
    pub fn fork(&self) -> Option<Arc<Mm>> {
        let child = Mm::new_user()?;
        child.tables_unlocked().clone_user_from(&self.tables_unlocked()).ok()?;
        let mem = self.lock().clone();
        *child.lock() = mem;
        Some(child)
    }

    pub fn lock(&self) -> MmGuard<'_> {
        MmGuard { inner: self.inner.lock() }
    }

    /// A number that tells this address space apart from every other one alive
    /// at the same moment.
    pub fn id(&self) -> u64 {
        self.root
    }

    /// The page tables, without the lock, for the changes to them that are not
    /// made under it yet: demand paging, unmapping, protection changes, exec's
    /// loader and fork's copy. Each of those orders itself against the others
    /// the way it did before there was a lock.
    pub fn tables_unlocked(&self) -> Borrowed<'_> {
        // SAFETY: `root` is the id of the tables in `inner`, which are dropped
        // only by this `Mm`'s drop, and the handle borrows `self`.
        unsafe { PageTables::borrow(self.root) }
    }

    /// Put the processor on this address space's tables.
    ///
    /// # Safety
    ///
    /// As `PageTables::load`: interrupts off, and a reference to this `Mm`
    /// kept for as long as its tables are loaded.
    unsafe fn load(&self) {
        // SAFETY: passed on to the caller.
        unsafe { self.tables_unlocked().load() };
    }
}

/// Address spaces whose last reference went with interrupts masked.
static DEFERRED: Spinlock<Deferred<PageTables>> = Spinlock::new(Deferred::new());

impl Drop for Mm {
    /// Free the tables, or leave them for `release_deferred` when interrupts
    /// are masked.
    ///
    /// The last reference can go anywhere a reference is held: at a context
    /// switch, where the processor lets go of the space it is leaving, or in
    /// an exit reached from a fault taken with interrupts off. Freeing a large
    /// program's tables there holds the timer off for as long as the walk of
    /// every table takes. Linux puts the same work off the same way for the
    /// switch: `finish_task_switch` drops the reference it left behind after
    /// interrupts are back on.
    fn drop(&mut self) {
        let inner = self.inner.get_mut();
        // SAFETY: taken once, here, in the owner's drop, and nothing reads the
        // field afterwards.
        let tables = unsafe { ManuallyDrop::take(&mut inner.tables) };
        if interrupts_enabled() {
            drop(tables);
            return;
        }
        DEFERRED.lock().push(tables);
        // The way out of the next system call takes it from there.
        crate::sched::tasks_to_release();
    }
}

/// Free the address spaces whose last reference went while interrupts were
/// masked. Does nothing when they are masked here too; the next caller with
/// them on takes the list.
pub fn release_deferred() {
    if !interrupts_enabled() {
        return;
    }
    let waiting = DEFERRED.lock().take();
    drop(waiting);
}

/// The address space the processor is on. One processor.
static ACTIVE: Spinlock<Active<Mm>> = Spinlock::new(Active::new());

/// Put the processor on `mm`'s tables, unless it is on them already, and hand
/// back the reference to the address space it was on.
///
/// The token is the caller's section: the task that is to run in `mm` has to
/// be recorded there in the same one, or a switch in between compares the
/// record with the tables and finds them agreeing when they do not.
#[must_use = "dropping the space left behind can free it; the caller chooses where"]
pub fn switch_mm(mm: &Arc<Mm>, _irq: NoInterrupts) -> Option<Arc<Mm>> {
    // SAFETY: interrupts are off, and `ACTIVE` keeps a reference to `mm` for
    // as long as its tables are loaded.
    ACTIVE.lock().switch(mm, |mm| unsafe { mm.load() })
}

/// Put the processor on the kernel's own tables, and hand back the reference
/// to the address space it was on.
#[must_use = "dropping the space left behind can free it; the caller chooses where"]
pub fn switch_to_kernel(_irq: NoInterrupts) -> Option<Arc<Mm>> {
    // SAFETY: interrupts are off, and the reference to the space being left
    // is given up only after this returns.
    ACTIVE.lock().leave(|| unsafe { kernel_tables().load() })
}

/// The references a task let go of when it moved to another address space:
/// its own to the one it ran in, and the processor's to the one it was on.
///
/// Either can be the last. Dropped where interrupts are on, that frees the
/// space there; dropped where they are masked, the free waits for
/// `release_deferred`.
#[must_use = "dropping these can free an address space; the caller chooses where"]
pub struct Displaced {
    pub task: Option<Arc<Mm>>,
    pub processor: Option<Arc<Mm>>,
}
