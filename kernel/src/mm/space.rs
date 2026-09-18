//! Address spaces: one owner for each, one lock over everything in one, and
//! the reference a processor holds on the one it is running on.
//!
//! - `Mm` owns a program's page tables and the record of what is in them, the
//!   regions and the program break, behind one lock. Tasks hold it as
//!   `Arc<Mm>`: threads made with `CLONE_VM` share the `Arc`, a fork makes a
//!   new `Mm` from a copy of the tables, and exec gives the task a new one.
//!   The last `Arc` to go drops the `PageTables` inside, which frees the
//!   space.
//! - Every change to either half is a method on `MmGuard`, the lock's guard,
//!   and there is no other way to reach the tables. A change to the tables and
//!   the change to the record that goes with it are therefore one operation as
//!   far as any other task is concerned, which is what the unsafe-code audit's
//!   findings 1, 2, 4, 5, 6, 8 and 9 each needed and none had.
//! - The processor holds an `Arc` to the `Mm` whose tables it has loaded
//!   (`switch_mm`), so an address space is not freed while the processor walks
//!   it. A kernel task has no `Mm` and runs on whatever is loaded.
//! - A last reference that goes while interrupts are masked does not free the
//!   space there: freeing walks every table the space has, which for a large
//!   program is long enough to hold the timer off. The tables wait in a list
//!   that `release_deferred` empties from a place interrupts are on.
//!
//! **Prepare outside, commit inside.** The lock masks interrupts, so nothing
//! that allocates a page, zeroes one, reads a file or waits on the card may
//! happen while it is held. Each of those produces a `Prepared` first, and the
//! guard then reads the record and the descriptor again and either publishes
//! it or hands it back to be released outside. Linux's fault path is the same
//! shape: it reads the page with no page-table lock held, takes the lock, and
//! checks the entry has not changed before it stores one (`do_anonymous_page`,
//! `filemap_fault` and `pte_same`).

use super::active::{Active, Deferred};
use super::tables::{Mach, PageTables, Prepared, Stale, MACH, USER_LIMIT};
use super::walk::{self, TableStock, Tables, Tlb};
use crate::abi::{PROT_EXEC, PROT_WRITE};
use crate::arch::paging::{is_user_addr, COW, NO_EXECUTE, PRESENT, USER, WRITABLE};
use crate::mm::frame::{self, Frame};
use crate::mm::tables::kernel_tables;
use crate::mm::{page_align_down, page_align_up, PAGE_SIZE, PAGE_SIZE_U64, USER_MMAP_BASE};
use crate::sync::{interrupts_enabled, NoInterrupts, SpinGuard, Spinlock};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::mem::ManuallyDrop;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicU32, Ordering};

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
    /// and loads and what a futex key names, neither of which may wait for the
    /// lock. It does not change.
    root: u64,
    inner: Spinlock<Inner>,
}

struct Inner {
    /// Dropped by `Mm`'s own drop, which decides whether that happens now or
    /// once interrupts are on.
    tables: ManuallyDrop<PageTables>,
    mem: MemState,
}

/// How many `MmGuard`s the processor holds. One processor, so one counter.
///
/// `might_sleep` reads it. Phase C of docs/memory-safety-plan.md replaces this
/// with a token the borrow checker refuses to let a sleeping function take
/// while a guard holds it; until then the check is at run time, which is where
/// the kernel finds out rather than the compiler.
static GUARDS: AtomicU32 = AtomicU32::new(0);

/// Panic if this is a place that may not sleep.
///
/// A guard masks interrupts and holds the address space's lock, so waiting for
/// the card there stops the timer and leaves every other thread of the process
/// spinning on a lock its holder is not running to release.
pub fn might_sleep(what: &str) {
    assert!(
        GUARDS.load(Ordering::Relaxed) == 0,
        "{} would sleep while an address space is locked",
        what,
    );
}

/// How many pages one turn of the lock takes away, and how many a fork shares
/// in one.
///
/// A range a program asks to be rid of is as long as the program says, and a
/// fork copies as much as the program has; the lock masks interrupts, so one
/// guard for the whole of either holds the timer off for that long. Measured
/// on this kernel under emulation, a 64 MiB range taken away under one guard
/// was eight milliseconds and one two-megabyte block of a fork was three, both
/// long enough to stop the tick and the card polling with it. At this many the
/// longest either holds is under two hundred microseconds there, and a tenth
/// of that on the board.
const UNMAP_CHUNK: u64 = 128;

/// How many times a publish goes back for more table frames before it gives
/// up. Each turn means another task took a level away in the window between
/// the count and the lock, so one is nearly always enough.
const TOP_UP_TRIES: usize = 4;

/// The most frames one guard holds back before it invalidates and releases
/// them. A range taken away is as long as a program asks for, so it cannot be
/// all of them; past the first address the invalidation is of the whole space
/// whatever the count, so there is nothing to gain from a larger one.
const RETIRED_MAX: usize = 32;

/// The address space, locked: the record of regions, and the page tables.
///
/// The lock is a spinlock that masks interrupts, so this is correct on several
/// processors as well as against the timer on one. It reads and writes as the
/// record of regions through `Deref`; the page tables are reached only by the
/// methods below, so every change to them is made here, under it.
pub struct MmGuard<'a> {
    inner: SpinGuard<'a, Inner>,
    /// Translations the changes made here have left behind.
    tlb: Tlb,
    /// Frames the changes have taken out of the tables, which go back once
    /// those translations have.
    retired: [u64; RETIRED_MAX],
    retired_len: usize,
}

/// A publish the guard refused, and why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    /// Something is mapped at the address already. Whoever put it there
    /// finished before this call started, so what is there is complete.
    Occupied,
    /// The walk wanted more tables than the page brought. Another task took
    /// the levels away between their being counted and the lock being taken.
    ShortOfTables,
}

impl MmGuard<'_> {
    /// Proof that interrupts are masked, which the lock does on the way in.
    pub fn irq(&self) -> NoInterrupts<'_> {
        self.inner.irq()
    }

    /// The walk of these tables. Private: a descriptor pointer must not leave
    /// the lock, and this is what hands one out.
    fn tables(&self) -> Tables<'static, Mach> {
        self.inner.tables.walk()
    }

    /// The physical address `virt` reaches, including its offset in the page.
    pub fn translate(&self, virt: u64) -> Option<u64> {
        self.tables().translate(virt)
    }

    /// The flags on the mapping of `virt`, as they were asked for.
    pub fn flags_of(&self, virt: u64) -> Option<u64> {
        self.tables().flags_of(virt)
    }

    /// How many tables a mapping at `virt` would have to create, which is how
    /// many frames a `Prepared` for it has to bring.
    pub fn missing_tables(&self, virt: u64) -> usize {
        self.tables().missing_tables(virt)
    }

    /// Print the walk of `virt`, for a fault report.
    pub fn dump_walk(&self, virt: u64) {
        super::tables::dump_walk(&self.tables(), virt);
    }

    /// Throw away the translations the changes made here have left behind, and
    /// give back the frames that were waiting on that.
    ///
    /// The order is the whole point: a frame released before the invalidation
    /// is one the allocator can give to another address space while the old
    /// one still reaches it.
    pub fn flush(&mut self) {
        self.tlb.flush(&MACH);
        for phys in self.retired.iter().take(self.retired_len) {
            // SAFETY: each was taken out of a descriptor by a method below,
            // nothing names it now, and the invalidation above has happened.
            drop(unsafe { Frame::from_recorded(*phys) });
        }
        self.retired_len = 0;
    }

    /// Hold `stale`'s frames until the invalidation, and give them back then.
    pub fn retire(&mut self, stale: Stale) {
        let (frames, len) = stale.take();
        if self.retired_len + len > RETIRED_MAX {
            self.flush();
        }
        for phys in frames.iter().take(len) {
            self.retired[self.retired_len] = *phys;
            self.retired_len += 1;
        }
    }

    /// Put a prepared page in at `virt`, which must have nothing at it.
    ///
    /// The page's reference moves into the descriptor. A refusal leaves the
    /// page in the `Prepared`, which the caller drops outside the lock: that
    /// is the only place a frame the guard did not use goes back.
    pub fn publish(
        &mut self,
        virt: u64,
        prepared: &mut Prepared,
        flags: u64,
    ) -> Result<u64, Refused> {
        let tables = self.tables();
        let (page, stock) = prepared.commit();
        match tables.publish(virt, page, flags, stock, &mut self.tlb) {
            Ok(()) => Ok(page),
            Err(refused) => {
                prepared.uncommit(page);
                Err(match refused {
                    walk::Refused::ShortOfTables => Refused::ShortOfTables,
                    // A block covering a user address is not something this
                    // kernel maps, so it can only be read as occupied.
                    walk::Refused::Occupied | walk::Refused::Blocked => Refused::Occupied,
                })
            }
        }
    }

    /// Put `frame` in at `virt` in place of what is mapped there, handing back
    /// what the descriptor held.
    ///
    /// One store: taking the old mapping away and putting the new one in as
    /// two calls leaves the address with nothing at it in between, and a
    /// sibling that touches it there is served a fresh page of zeroes over the
    /// top.
    ///
    /// `Err` when nothing was mapped at `virt`, with nothing written and the
    /// frame handed back: putting it in would be creating a mapping rather
    /// than replacing one.
    pub fn replace(&mut self, virt: u64, frame: Frame, flags: u64) -> Result<Stale, Frame> {
        let tables = self.tables();
        let phys = frame.addr();
        match tables.replace(virt, phys, flags, &mut self.tlb) {
            Some(old) => {
                core::mem::forget(frame);
                let mut stale = Stale::new();
                stale.push(old);
                Ok(stale)
            }
            None => Err(frame),
        }
    }

    /// Change the flags on the mapping of `virt`, leaving the frame alone.
    /// The frame's address, or `None` when nothing is mapped there.
    ///
    /// Reading what the descriptor says and writing the new one are one step
    /// here. Apart, a sibling's unmap between them left this storing a
    /// descriptor for a frame the address space no longer owned, and a
    /// sibling's copy-on-write break left it storing the shared frame back
    /// over the private copy.
    pub fn protect(&mut self, virt: u64, flags: u64) -> Option<u64> {
        self.tables().protect(virt, flags, &mut self.tlb)
    }

    /// Take the mapping at `virt` away, with the tables it emptied.
    ///
    /// Nothing is released: what comes back is `Stale`, which `retire` turns
    /// into frames the allocator can have once the invalidation has happened.
    #[must_use = "the frames a mapping held go back only through the guard"]
    pub fn unmap(&mut self, virt: u64) -> Option<Stale> {
        assert!(is_user_addr(virt), "an address space unmapping a kernel address");
        let tables = self.tables();
        let mut reclaimed = TableStock::new();
        let page = tables.unmap(virt, &mut self.tlb, &mut reclaimed)?;
        let mut stale = Stale::new();
        stale.push(page);
        while let Some(table) = reclaimed.take() {
            stale.push(table);
        }
        Some(stale)
    }

    /// Take every mapping in `[start, end)` away.
    pub fn unmap_range(&mut self, start: u64, end: u64) {
        let mut page = start;
        while page < end {
            if let Some(stale) = self.unmap(page) {
                self.retire(stale);
            }
            page += PAGE_SIZE_U64;
        }
    }

    /// Whether `page` is inside the program break.
    pub fn in_heap(&self, page: u64) -> bool {
        page >= self.inner.mem.brk_start && page < self.inner.mem.brk
    }

    /// What the record says should be at `page`, and how many tables a mapping
    /// there would have to create. `None` when nothing is recorded at it.
    ///
    /// This is what a fault decides from, with the record held still. The page
    /// is then filled outside the lock and `commit` asks the same question
    /// again, because everything it read can have changed meanwhile.
    pub fn plan(&self, page: u64) -> Option<Plan> {
        let tables = self.missing_tables(page);
        if self.in_heap(page) {
            return Some(Plan {
                flags: PRESENT | WRITABLE | USER | NO_EXECUTE,
                source: Source::Zero,
                tables,
            });
        }
        let vma = self.inner.mem.find_vma(page)?;
        let flags = vma.page_flags();
        let source = match &vma.file {
            None => Source::Zero,
            Some(file) => {
                let into = page - vma.start;
                if into >= file.length {
                    // Past the file's contents: this is .bss, which is zeroes.
                    Source::Zero
                } else {
                    Source::File {
                        node: file.node.clone(),
                        offset: file.offset + into,
                        want: (file.length - into).min(PAGE_SIZE_U64) as usize,
                    }
                }
            }
        };
        Some(Plan { flags, source, tables })
    }

    /// Put a page prepared for `plan` in at `page`, having checked that the
    /// record still says the same thing and that nothing has been mapped there
    /// meanwhile.
    pub fn commit(&mut self, page: u64, plan: &Plan, prepared: &mut Prepared) -> Commit {
        if self.translate(page).is_some() {
            // A sibling thread reaching the same page of a program's text at
            // the same moment is ordinary, and what it published is finished,
            // because a page is published with its contents already in it.
            return Commit::Already;
        }
        match self.plan(page) {
            // The region went away, or changed under the fill: findings 8 and
            // 5. Publishing now would leave a page mapped outside any region,
            // or with the protection the region used to have.
            None => Commit::Changed,
            Some(now) if !now.same_page_as(plan) => Commit::Changed,
            Some(_) => match self.publish(page, prepared, plan.flags) {
                Ok(_) => Commit::Done,
                Err(Refused::Occupied) => Commit::Already,
                Err(Refused::ShortOfTables) => Commit::ShortOfTables,
            },
        }
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

impl Drop for MmGuard<'_> {
    /// Nothing leaves the lock half done: whatever the changes here left in
    /// the translation caches goes, and the frames waiting on that go back.
    fn drop(&mut self) {
        self.flush();
        GUARDS.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Where a fault's page comes from.
#[derive(Clone)]
pub enum Source {
    /// Zeroes: the heap, an anonymous region, or a region's .bss tail.
    Zero,
    /// `want` bytes of `node` at `offset`, and zeroes past them.
    File { node: crate::fs::NodeRef, offset: u64, want: usize },
}

/// What the record says a fault should put at an address.
///
/// It is read under the guard, acted on outside it, and compared with the
/// record again under the guard before anything is published. Linux carries
/// the same thing in `struct vm_fault` and checks `pte_same` before it stores.
pub struct Plan {
    pub flags: u64,
    pub source: Source,
    /// How many tables the walk to the address would have had to create at the
    /// moment this was read. A count is enough: a descriptor exists only where
    /// everything above it does, so what is missing is always the bottom
    /// levels.
    pub tables: usize,
}

impl Plan {
    /// True when the two would put the same bytes at the address with the same
    /// permissions. The table count is no part of it: a level another task
    /// created since is one fewer to create, not a different page.
    fn same_page_as(&self, other: &Plan) -> bool {
        if self.flags != other.flags {
            return false;
        }
        match (&self.source, &other.source) {
            (Source::Zero, Source::Zero) => true,
            (
                Source::File { node, offset, want },
                Source::File { node: other_node, offset: other_offset, want: other_want },
            ) => Arc::ptr_eq(node, other_node) && offset == other_offset && want == other_want,
            _ => false,
        }
    }
}

/// What came of a commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Commit {
    /// The page is in.
    Done,
    /// Something is at the address already, finished, so the fault is served.
    Already,
    /// The record no longer says what the page was filled for. Nothing is
    /// written; the caller decides again.
    Changed,
    /// The walk wanted more tables than the page brought. Nothing is written.
    ShortOfTables,
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

    pub fn lock(&self) -> MmGuard<'_> {
        let inner = self.inner.lock();
        GUARDS.fetch_add(1, Ordering::Relaxed);
        MmGuard { inner, tlb: Tlb::new(), retired: [0; RETIRED_MAX], retired_len: 0 }
    }

    /// A number that tells this address space apart from every other one alive
    /// at the same moment.
    pub fn id(&self) -> u64 {
        self.root
    }

    /// The address space a forked child starts with: tables that share every
    /// page of this one copy-on-write, and a copy of the record.
    ///
    /// The walk is driven by address rather than by a pointer into the
    /// parent's tables, and the guard is taken again for each block of two
    /// megabytes. What a preemption between blocks can do is take a table
    /// away, and the next block is looked for from the top, so there is no
    /// pointer left to follow into a freed table: that was finding 6. Holding
    /// one guard for the whole walk instead would mask interrupts for as long
    /// as copying the tables of the largest program takes, which for a Go
    /// runtime is tens of thousands of pages.
    ///
    /// A copy that fails partway is dropped here, and its drop gives back what
    /// it had collected.
    pub fn fork(&self) -> Option<Arc<Mm>> {
        let child = Mm::new_user()?;
        let mut at = 0u64;
        while at < USER_LIMIT {
            let Some(block) = self.lock().tables().next_populated(at, USER_LIMIT) else {
                break;
            };
            // Outside the lock: the frames the child's tables for this block
            // may need. One block is one last-level table, so three levels at
            // most, whatever the block holds.
            let mut stock = TableStock::new();
            for _ in 0..walk::DEPTH {
                stock.push(frame::alloc_zeroed()?.into_recorded());
            }
            // The block goes in pieces, with the locks let go between them.
            // Only the first piece creates tables, and the frames it did not
            // use stay in the stock for the next block, so no piece allocates.
            let mut done = Ok(());
            let mut piece = block;
            while piece < block + walk::level_size(1) {
                let count = UNMAP_CHUNK as usize;
                done = {
                    let mut parent = self.lock();
                    // The child was made here and no other task can reach it,
                    // so taking its lock inside the parent's cannot wait on
                    // anyone.
                    let into = child.lock();
                    let from = parent.tables();
                    let to = into.tables();
                    // One accounting of the translations for both sides: this
                    // kernel uses no address space identifiers, so an
                    // invalidation by address covers the parent's cached
                    // translation and the child's alike, and the parent is the
                    // space the processor is on.
                    to.share_range(&from, piece, count, &mut stock, &mut parent.tlb)
                };
                if done.is_err() {
                    break;
                }
                piece += count as u64 * PAGE_SIZE_U64;
            }
            // What the block did not use goes back out here, with the locks
            // let go and interrupts back on.
            while let Some(unused) = stock.take() {
                // SAFETY: taken from the allocator above, and no descriptor
                // names it: the walk takes what it uses out of the stock.
                drop(unsafe { Frame::from_recorded(unused) });
            }
            done.ok()?;
            at = block.checked_add(walk::level_size(1))?;
        }
        *child.lock() = self.lock().clone();
        Some(child)
    }

    /// Back `page` with memory if the heap or a region covers it. True means
    /// the address has memory at it now, not that this call is what put it
    /// there.
    ///
    /// Three steps, and the lock is held for the first and the third: read the
    /// record, fill a page from whatever it named, and publish it if the
    /// record still says the same. The middle step reads a file, which on this
    /// machine waits for the card, so it cannot be under the lock; everything
    /// it read is therefore asked about again in the third.
    pub fn fault_in(&self, addr: u64) -> bool {
        let page = page_align_down(addr);
        // A plan that no longer holds is tried again: a sibling that mapped
        // the tables away, or changed the region, leaves this with a page
        // filled for the wrong address. Each turn either publishes, finds the
        // page already there, or finds a different record, and a program that
        // kept changing the record could otherwise keep one thread here.
        const TRIES: usize = 8;
        for _ in 0..TRIES {
            let plan = {
                let guard = self.lock();
                if guard.translate(page).is_some() {
                    // The hardware found nothing at this address and the tables
                    // have something at it: two readings of one descriptor
                    // either side of a sibling's store. The instruction can run
                    // again.
                    return true;
                }
                match guard.plan(page) {
                    Some(plan) => plan,
                    None => return false,
                }
            };
            let Some(mut prepared) = Prepared::new(plan.tables) else {
                return false;
            };
            if !fill(&plan, &mut prepared) {
                return false;
            }
            let outcome = self.lock().commit(page, &plan, &mut prepared);
            // The page a refused commit did not take goes back here, with the
            // lock let go and interrupts back on.
            drop(prepared);
            match outcome {
                Commit::Done | Commit::Already => return true,
                Commit::Changed | Commit::ShortOfTables => continue,
            }
        }
        false
    }

    /// Take the recorded regions covering `[start, end)` away, and then every
    /// page in it.
    ///
    /// The regions go first, under one guard. A thread sharing the address
    /// space that touches this range while the pages are being taken away
    /// faults, and a fault inside a region that is still recorded is served a
    /// fresh page of zeroes: one this call has already walked past and so
    /// leaves behind, at an address the program was told nothing is at. With
    /// the regions gone first there is nothing here to fault into, which is
    /// what an unmapped range is, and nothing can publish into the range
    /// behind the sweep however long the sweep takes.
    pub fn unmap_recorded(&self, start: u64, end: u64) {
        self.lock().remove_vma_range(start, end);
        self.unmap_pages(start, end);
    }

    /// Take every page in `[start, end)` away, a table's worth per turn of the
    /// lock.
    ///
    /// The caller has already made the range one nothing can fault into: the
    /// regions are gone, or the break is below it. That is what lets the lock
    /// be let go between chunks.
    pub fn unmap_pages(&self, start: u64, end: u64) {
        let mut page = start;
        while page < end {
            let stop = end.min(page + UNMAP_CHUNK * PAGE_SIZE_U64);
            self.lock().unmap_range(page, stop);
            page = stop;
        }
    }

    /// Put a prepared page in at `virt`, going back outside the lock for the
    /// table frames the walk turns out to need.
    ///
    /// A caller that has filled a page does not know how many levels are
    /// missing until it looks, and it may not look and then allocate under the
    /// lock. So it asks, brings that many, and asks again: a level taken away
    /// in between is one more turn rather than a failure. This is what
    /// everything that builds an image -- exec's loader, the signal
    /// trampoline, a file-backed mmap, the first pages of a stack -- publishes
    /// through.
    pub fn publish_page(
        &self,
        virt: u64,
        prepared: &mut Prepared,
        flags: u64,
    ) -> Result<u64, Refused> {
        for _ in 0..TOP_UP_TRIES {
            let missing = self.lock().missing_tables(virt);
            if missing > prepared.tables_left() && !prepared.top_up(missing) {
                return Err(Refused::ShortOfTables);
            }
            match self.lock().publish(virt, prepared, flags) {
                Err(Refused::ShortOfTables) => continue,
                done => return done,
            }
        }
        Err(Refused::ShortOfTables)
    }

    /// Give the address space a private copy of the shared page at `addr`,
    /// which a write to it needs. Whether a write may be made now, which is
    /// false for a page no write can complete on.
    ///
    /// The whole of it is one step under the guard: what the descriptor says,
    /// what it points at, how many address spaces that frame is in and what
    /// finally goes in the descriptor have to be one account of the page, and
    /// a sibling that ran in the middle would be deciding from a state that
    /// only exists halfway through.
    ///
    /// The copy is a page of stores inside that step. Linux takes its page
    /// table lock, holds a reference on the source page, drops the lock for
    /// the copy and takes it again to check the entry has not changed
    /// (`wp_page_copy`). Here the guard is also what keeps the source frame
    /// from being released under the copy, and a page of stores is shorter
    /// than the two extra lock round trips and the reference it would cost to
    /// do it the other way.
    pub fn break_cow(&self, addr: u64) -> bool {
        let page = page_align_down(addr);
        // Outside: the frame the copy may need. A page that turns out not to
        // need one gives it straight back.
        let spare = frame::alloc();
        let mut guard = self.lock();
        let Some(flags) = guard.flags_of(page) else {
            return false;
        };
        if flags & COW == 0 {
            // Not shared. A page that was never shared and a page a sibling
            // has already taken the copy of look exactly alike from here, and
            // the permissions are what tell them apart: one that user code may
            // write is repaired, whoever repaired it, and the store can be
            // made again.
            return flags & WRITABLE != 0 && flags & USER != 0;
        }
        let Some(phys) = guard.translate(page).map(page_align_down) else {
            return false;
        };
        // The last owner can simply take the page back.
        if frame::frame_references(phys) <= 1 {
            return guard.protect(page, (flags & !COW) | WRITABLE).is_some();
        }
        let Some(copy) = spare else {
            return false;
        };
        let to = copy.addr();
        // SAFETY: `phys` is named by a descriptor of these tables, which the
        // guard holds still, and `copy` is a frame nothing else holds. Both
        // are reached through the direct map, and they are different frames
        // because the allocator did not hand out one that is mapped.
        unsafe {
            core::ptr::copy_nonoverlapping(
                crate::mm::phys_to_virt(phys) as *const u8,
                crate::mm::phys_to_virt(to) as *mut u8,
                PAGE_SIZE,
            );
        }
        // A page the program may execute has just been written through a
        // different address than the one it will be fetched from.
        if flags & NO_EXECUTE == 0 {
            crate::arch::sync_instruction_cache(crate::mm::phys_to_virt(to), PAGE_SIZE);
        }
        match guard.replace(page, copy, (flags & !COW) | WRITABLE) {
            Ok(stale) => {
                guard.retire(stale);
                true
            }
            Err(_) => false,
        }
    }

    /// Print the walk of `virt` through these tables, without the lock.
    ///
    /// A fault report only reads, and the fault it is reporting can be one
    /// taken while this very lock was held: waiting for it would hang the
    /// machine instead of printing why it stopped.
    pub fn dump_walk_unlocked(&self, virt: u64) {
        // SAFETY: the caller holds a reference to this `Mm`, so its tables are
        // not freed under the walk, and nothing here writes.
        let tables = unsafe { Tables::new(&MACH, self.root) };
        super::tables::dump_walk(&tables, virt);
    }

    /// Put the processor on this address space's tables.
    ///
    /// # Safety
    ///
    /// As `PageTables::load`: interrupts off, and a reference to this `Mm`
    /// kept for as long as its tables are loaded.
    unsafe fn load(&self) {
        // SAFETY: passed on to the caller, and `root` is the id of the tables
        // this `Mm` owns, which the caller's reference keeps alive.
        unsafe { super::tables::load_id(self.root) };
    }
}

/// Fill `prepared` with what `plan` says goes at the address. False when the
/// source could not give it, which is a fault the program is not served.
///
/// Outside the lock: this waits for the card for a program kept on the data
/// volume, and takes the file's lock for one kept in the initramfs.
fn fill(plan: &Plan, prepared: &mut Prepared) -> bool {
    let Source::File { node, offset, want } = &plan.source else {
        // A fresh frame is already zero.
        return true;
    };
    let want = *want;
    let filled = if node.kind == crate::fs::NodeKind::DataFile {
        // A program kept on the data volume: the page comes from the card,
        // through the volume's sleeping lock, as a read would. A page the card
        // cannot give is a fault the program is not served, the same as an
        // address with nothing mapped.
        match crate::fs::data::read(node, crate::fs::Offset::new(*offset), &mut prepared.bytes()[..want]) {
            Ok(n) => n,
            Err(_) => return false,
        }
    } else {
        let data = node.inner.lock();
        let from = *offset as usize;
        let available = data.data.len().saturating_sub(from).min(want);
        prepared.bytes()[..available].copy_from_slice(&data.data[from..from + available]);
        available
    };
    // A text page arrives this way, and these bytes have just been written
    // through a different address than the one they will be fetched from.
    if filled > 0 && plan.flags & NO_EXECUTE == 0 {
        crate::arch::sync_instruction_cache(prepared.bytes().as_ptr() as u64, filled);
    }
    true
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
