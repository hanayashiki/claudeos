//! The kernel's translation tables: who owns a hierarchy, where its frames and
//! its physical memory come from, and the three values that make the changes
//! to one spell out what they cost.
//!
//! - `PageTables` owns a hierarchy. It is not `Copy`, its root is private, and
//!   the only way the tables go back is dropping it.
//! - `Prepared` is a page with its contents already in it and the frames for
//!   the tables a walk may have to create: everything a publish needs, taken
//!   before the address space is locked.
//! - `Stale` is a frame a table no longer names. It is not a `Frame`, so it
//!   cannot go back to the allocator, until the guard that took it has thrown
//!   the translation away.
//!
//! The walk is `mm::walk`, and `Mach` is what this kernel supplies it with.

use crate::arch::paging;
use crate::mm::frame::{self, Frame};
use crate::mm::walk::{self, Machine, TableStock, Tables, Tlb};
use crate::mm::{phys_to_virt, PAGE_SIZE};
use crate::sync::Spinlock;
use core::mem::ManuallyDrop;

/// This machine, for the walk: the descriptor format `arch::paging` spells,
/// physical memory through the direct map, and frames from the allocator.
pub struct Mach;

/// One value, because it holds nothing: every method reaches a global.
pub const MACH: Mach = Mach;

// SAFETY:
// - `table_ptr` is the direct map, which covers every frame the allocator can
//   hand out and is mapped for as long as the kernel runs.
// - `alloc_zeroed` hands over a frame with one reference recorded in the
//   descriptor that will name it, `free` gives one back, and `share` takes
//   another, which is the accounting `frame::Frame` does by hand here because
//   a descriptor is where the reference lives.
// - the barrier and the two invalidations are `arch::paging`'s, and each has
//   taken effect when it returns.
unsafe impl Machine for Mach {
    #[inline]
    fn leaf(&self, phys: u64, flags: u64) -> u64 {
        paging::leaf(phys, flags)
    }

    #[inline]
    fn table(&self, phys: u64, under: u64) -> u64 {
        paging::table(phys, under)
    }

    #[inline]
    fn present(&self, bits: u64) -> bool {
        paging::present(bits)
    }

    #[inline]
    fn ends_walk(&self, bits: u64, level: u32) -> bool {
        paging::ends_walk(bits, level)
    }

    #[inline]
    fn addr(&self, bits: u64) -> u64 {
        paging::addr(bits)
    }

    #[inline]
    fn flags(&self, bits: u64) -> u64 {
        paging::flags(bits)
    }

    #[inline]
    fn widen(&self, table_bits: u64, leaf_flags: u64) -> Option<u64> {
        paging::widen(table_bits, leaf_flags)
    }

    #[inline]
    fn table_ptr(&self, phys: u64) -> *mut u64 {
        phys_to_virt(phys) as *mut u64
    }

    #[inline]
    fn alloc_zeroed(&self) -> Option<u64> {
        frame::alloc_zeroed().map(Frame::into_recorded)
    }

    #[inline]
    fn free(&self, phys: u64) {
        // SAFETY: every caller here took the reference out of a descriptor
        // that named the frame, and the descriptor is gone.
        drop(unsafe { Frame::from_recorded(phys) });
    }

    #[inline]
    fn share(&self, phys: u64) {
        // SAFETY: the caller holds a reference through a descriptor that names
        // the frame, and records the new one in another descriptor.
        unsafe { frame::share_recorded(phys) }.into_recorded();
    }

    #[inline]
    fn references(&self, phys: u64) -> u32 {
        frame::frame_references(phys) as u32
    }

    #[inline]
    fn store_barrier(&self) {
        paging::store_barrier();
    }

    #[inline]
    fn flush_page(&self, virt: u64) {
        paging::flush_tlb(virt);
    }

    #[inline]
    fn flush_all(&self) {
        paging::flush_tlb_all();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MapError {
    OutOfMemory,
    AlreadyMapped,
}

/// Where the half a program owns ends, which is what a teardown frees and what
/// `is_user_addr` answers about.
pub const USER_LIMIT: u64 = 0x0000_8000_0000_0000;

/// A page on its way in: a frame the allocator has just handed over, zeroed,
/// with the contents it will hold already written into it, and the frames for
/// the tables the walk to it may have to create.
///
/// This is the only thing a publish takes, and it consumes it. Everything that
/// allocates, zeroes or reads from a file happens in making one, which is
/// before the address space is locked; what happens under the lock is a walk
/// and a store. The other order -- map the page wide enough to write through,
/// write, then narrow it to what it should have been -- has no spelling, and
/// it is the order that leaves a page a sibling thread can read blank, or run,
/// before it holds anything.
///
/// Only the allocator makes one, so this is not a way to move a frame another
/// mapping is holding.
pub struct Prepared {
    /// Taken by the publish that succeeds; what is left when this is dropped
    /// goes back to the allocator.
    page: Option<Frame>,
    tables: TableStock,
}

impl Prepared {
    /// A zeroed page, and `tables` zeroed frames for the levels a walk to it
    /// would have to create. `None` when there are not enough frames.
    pub fn new(tables: usize) -> Option<Prepared> {
        let page = frame::alloc_zeroed()?;
        let mut stock = TableStock::new();
        for _ in 0..tables.min(walk::DEPTH) {
            match frame::alloc_zeroed() {
                Some(frame) => stock.push(frame.into_recorded()),
                // What was taken goes back with the value.
                None => {
                    drop(Prepared { page: Some(page), tables: stock });
                    return None;
                }
            }
        }
        Some(Prepared { page: Some(page), tables: stock })
    }

    /// The page's bytes, through the direct map. The address is also what a
    /// cache maintenance operation on these bytes has to name, since it is the
    /// one they were written through.
    pub fn bytes(&mut self) -> &mut [u8] {
        let phys = self.page.as_ref().expect("the page of a Prepared that was published").addr();
        // SAFETY: a frame the allocator handed over and nothing else holds, and
        // the direct map covers it.
        unsafe { core::slice::from_raw_parts_mut(phys_to_virt(phys) as *mut u8, PAGE_SIZE) }
    }

    /// Where the page is.
    pub fn addr(&self) -> u64 {
        self.page.as_ref().expect("the page of a Prepared that was published").addr()
    }

    /// Frames for how many more levels this brought than the walk used.
    pub fn tables_left(&self) -> usize {
        self.tables.len()
    }

    /// Take enough more zeroed frames that the walk can create `want` levels.
    /// False when there are not enough to be had.
    ///
    /// A publish asks how many levels are missing before it takes the lock,
    /// and that answer can be short by the time the lock is held: another task
    /// took a level away in between. This is how the caller goes back for the
    /// rest, outside the lock, and tries again.
    pub fn top_up(&mut self, want: usize) -> bool {
        while self.tables.len() < want.min(walk::DEPTH) {
            match frame::alloc_zeroed() {
                Some(frame) => self.tables.push(frame.into_recorded()),
                None => return false,
            }
        }
        true
    }

    /// Hand the page's reference to the descriptor about to name it, and lend
    /// the table frames to the walk.
    pub(crate) fn commit(&mut self) -> (u64, &mut TableStock) {
        let frame = self.page.take().expect("a Prepared published twice");
        (frame.into_recorded(), &mut self.tables)
    }

    /// Take the page's reference back, for a publish that was refused.
    pub(crate) fn uncommit(&mut self, phys: u64) {
        // SAFETY: `commit` recorded this reference a moment ago and nothing
        // stored a descriptor naming it.
        self.page = Some(unsafe { Frame::from_recorded(phys) });
    }

    fn give_back(&mut self) {
        drop(self.page.take());
        while let Some(phys) = self.tables.take() {
            // SAFETY: `new` recorded each of these and no descriptor names it.
            drop(unsafe { Frame::from_recorded(phys) });
        }
    }
}

impl Drop for Prepared {
    /// Whatever the walk did not use goes back. A publish that was refused is
    /// dropped by its caller, which the guard's callers do outside the lock.
    fn drop(&mut self) {
        self.give_back();
    }
}

/// The most frames one change can take out of the tables: the page, and the
/// three tables emptying it can leave behind.
const STALE_MAX: usize = 1 + walk::DEPTH;

/// Frames a table no longer names.
///
/// A translation the hardware has cached outlives the descriptor it was read
/// from, so these are still reachable. They are not `Frame`s -- there is no
/// way to release one from here -- until `MmGuard::retire` has taken them,
/// and that is what orders the invalidation before the release. Dropping one
/// instead is a frame handed to somebody else while the old owner can still
/// write through it, so it is a panic rather than a leak.
#[must_use = "a frame a table no longer names goes back only through the guard"]
pub struct Stale {
    frames: [u64; STALE_MAX],
    len: usize,
}

impl Stale {
    pub(crate) fn new() -> Stale {
        Stale { frames: [0; STALE_MAX], len: 0 }
    }

    pub(crate) fn push(&mut self, phys: u64) {
        assert!(self.len < STALE_MAX, "more frames than one change can take out");
        self.frames[self.len] = phys;
        self.len += 1;
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// What is waiting, for a check that wants to see it. Reading is all this
    /// allows: releasing one still goes through the guard.
    pub fn frames(&self) -> &[u64] {
        &self.frames[..self.len]
    }

    /// The frames, with this value's drop skipped. Only `MmGuard::retire`
    /// calls it, and only after the invalidation.
    pub(crate) fn take(self) -> ([u64; STALE_MAX], usize) {
        let me = ManuallyDrop::new(self);
        (me.frames, me.len)
    }
}

impl Drop for Stale {
    fn drop(&mut self) {
        assert!(
            self.len == 0,
            "a frame a page table no longer names was dropped without being invalidated",
        );
    }
}

/// A hierarchy of translation tables for one address space: its top table, and
/// every table and frame the user half of it reaches.
///
/// It has one owner. It is not `Copy` or `Clone`, the root is private, and the
/// only way the tables go back is dropping the owner, so a second value naming
/// the same tables cannot be written outside this file and cannot free them
/// twice. Threads that share an address space share this owner through
/// `mm::space::Mm`, and the last reference to that is what drops it.
pub struct PageTables {
    root: u64,
}

impl PageTables {
    /// A fresh address space that shares the kernel half.
    pub fn new_user() -> Option<PageTables> {
        // The owner holds the top table's reference; its drop gives it back.
        let root = frame::alloc_zeroed()?.into_recorded();
        let fresh = PageTables { root };
        // Entries 256..512 cover the kernel: direct map, heap, image. They are
        // copied rather than referenced, so nothing below the top level is
        // owned here and a teardown frees none of it.
        fresh.walk().adopt_top(&kernel_tables().0.walk(), 256, walk::ENTRIES);
        Some(fresh)
    }

    /// A number that tells these tables apart from every other set alive at the
    /// same moment, for anything that has to key on which one it is.
    pub fn id(&self) -> u64 {
        self.root
    }

    /// The walk of these tables.
    ///
    /// Everything that changes them goes through `mm::space::MmGuard`, which
    /// holds the address space's lock for the whole of a change; this is how
    /// it reaches them.
    pub(crate) fn walk(&self) -> Tables<'static, Mach> {
        // SAFETY: `self` owns the hierarchy at `root` and the borrow of `self`
        // the caller holds is what keeps it alive. The `'static` is the
        // machine's, which is a constant.
        unsafe { Tables::new(&MACH, self.root) }
    }

    /// Put the processor on these tables.
    ///
    /// # Safety
    ///
    /// Interrupts must be off, and the tables must stay alive for as long as
    /// they are loaded: the caller keeps a reference to their owner until the
    /// processor has been put on other tables.
    pub unsafe fn load(&self) {
        // SAFETY: the kernel half is copied into every address space, so the
        // kernel runs on as it did; the caller keeps the tables alive.
        unsafe { load_root(self.root) };
    }
}

impl Drop for PageTables {
    /// Release every user mapping, every table that held one, and the top
    /// table. The kernel half is shared, so it is not freed.
    ///
    /// The processor must not be on these tables, since it walks them for the
    /// kernel's own addresses as well. `mm::space` holds a reference to the
    /// address space that is loaded and gives it up only after another has
    /// been, so the last owner is never dropped while its tables are loaded.
    /// The check turns a mistake there into a panic rather than a page table
    /// freed under the processor.
    fn drop(&mut self) {
        assert_ne!(self.root, paging::live_root(), "freeing the tables the processor is on");
        self.walk().free_below(USER_LIMIT);
        // SAFETY: `new_user` recorded the top table's reference in this owner,
        // and this is the owner's one drop.
        drop(unsafe { Frame::from_recorded(self.root) });
    }
}

/// Put the processor on the top table at `root`.
///
/// # Safety
///
/// As `PageTables::load`.
#[inline]
unsafe fn load_root(root: u64) {
    #[cfg(target_arch = "aarch64")]
    // SAFETY: passed on to the caller.
    unsafe {
        paging::write_ttbr(root)
    };
    #[cfg(target_arch = "x86_64")]
    // SAFETY: passed on to the caller.
    unsafe {
        paging::write_cr3(root)
    };
}

/// The kernel half of every address space, through the kernel's own top table.
///
/// Below the top level the kernel half is one set of tables that every address
/// space names, because `new_user` copies the top table's upper half, so a page
/// mapped there through one top table is reached through all of them. The
/// kernel's own is the one this walks.
pub struct KernelTables(ManuallyDrop<PageTables>);

/// How many times a publish goes back for more table frames before it gives
/// up. Each turn means another task took a level away in the window between
/// the count and the lock, so one is nearly always enough.
const TOP_UP_TRIES: usize = 4;

/// The kernel half has no address space of its own to be locked, and two
/// growers of the kernel heap are the same race two threads faulting in one
/// program are: both read a level as absent and the second store leaves the
/// first's table unreachable and unowned. This is what stands in for the
/// address space's lock there.
static KERNEL_CHANGES: Spinlock<()> = Spinlock::new(());

/// The kernel's own tables, which `adopt_boot_tables` recorded.
pub fn kernel_tables() -> KernelTables {
    KernelTables(ManuallyDrop::new(PageTables { root: paging::kernel_root() }))
}

impl KernelTables {
    /// Map a zeroed page at `virt`, an address in the kernel half: the heap.
    ///
    /// The frames are taken before the lock, so what happens under it is a
    /// walk and a store. An address another grower reached first is an error
    /// rather than a second page over the top of the first.
    pub fn map_new(&self, virt: u64, flags: u64) -> Result<u64, MapError> {
        assert!(!paging::is_user_addr(virt), "a kernel mapping at a program's address");
        let tables = self.0.walk();
        // How many levels are missing, asked before the lock and re-asked
        // under it: bringing three frames every time would zero twelve
        // kilobytes for each page of a heap that grows in thousands.
        let missing = {
            let _held = KERNEL_CHANGES.lock();
            tables.missing_tables(virt)
        };
        let mut prepared = Prepared::new(missing).ok_or(MapError::OutOfMemory)?;
        let phys = prepared.addr();
        let mut outcome = Err(walk::Refused::ShortOfTables);
        for _ in 0..TOP_UP_TRIES {
            let mut tlb = Tlb::new();
            outcome = {
                let _held = KERNEL_CHANGES.lock();
                let (page, stock) = prepared.commit();
                let done = match tables.publish(virt, page, flags, stock, &mut tlb) {
                    Ok(()) => Ok(phys),
                    Err(refused) => {
                        prepared.uncommit(page);
                        Err(refused)
                    }
                };
                tlb.flush(&MACH);
                done
            };
            // Another grower took a level away between the count and the
            // lock. Go back for the frames out here and try again.
            if outcome == Err(walk::Refused::ShortOfTables) && prepared.top_up(walk::DEPTH) {
                continue;
            }
            break;
        }
        // Outside the lock: what the walk did not use goes back here.
        drop(prepared);
        outcome.map_err(|refused| match refused {
            walk::Refused::Occupied | walk::Refused::Blocked => MapError::AlreadyMapped,
            walk::Refused::ShortOfTables => MapError::OutOfMemory,
        })
    }

    /// Map `phys` at `virt`, an address in the kernel half, without taking a
    /// reference on it: in practice a device's registers.
    ///
    /// # Safety
    ///
    /// `phys` must not be memory the frame allocator can hand out for as long
    /// as the mapping stays. Nothing takes this mapping away when a frame is
    /// released, so a frame given to a program afterwards would be reachable
    /// through it.
    pub unsafe fn map_fixed(&self, virt: u64, phys: u64, flags: u64) -> Result<(), MapError> {
        assert!(!paging::is_user_addr(virt), "a kernel mapping at a program's address");
        let tables = self.0.walk();
        let mut stock = TableStock::new();
        for _ in 0..walk::DEPTH {
            stock.push(frame::alloc_zeroed().ok_or(MapError::OutOfMemory)?.into_recorded());
        }
        let mut tlb = Tlb::new();
        let outcome = {
            let _held = KERNEL_CHANGES.lock();
            let done = match tables.publish(virt, phys, flags, &mut stock, &mut tlb) {
                // Two devices whose registers share a page ask for the same
                // mapping twice, and the second is not a clash. Anything else
                // at the address is, and is refused rather than written over.
                Err(walk::Refused::Occupied)
                    if tables.leaf(virt) == Some(MACH.leaf(phys, flags)) =>
                {
                    Ok(())
                }
                done => done,
            };
            tlb.flush(&MACH);
            done
        };
        while let Some(unused) = stock.take() {
            // SAFETY: recorded a moment ago and named by no descriptor.
            drop(unsafe { Frame::from_recorded(unused) });
        }
        outcome.map_err(|refused| match refused {
            walk::Refused::Occupied | walk::Refused::Blocked => MapError::AlreadyMapped,
            walk::Refused::ShortOfTables => MapError::OutOfMemory,
        })
    }

    /// Put the processor on the kernel's own tables.
    ///
    /// # Safety
    ///
    /// Interrupts must be off, and the caller must give up whatever kept the
    /// tables it is leaving alive only after this returns.
    pub unsafe fn load(&self) {
        // SAFETY: the kernel's tables map the kernel half, which is all that
        // runs with no address space of a program's.
        unsafe { load_root(self.0.root) };
    }
}

/// Print the walk of `virt` through `tables`, descriptor by descriptor.
///
/// A fault report that says a page is present and writable is a summary of the
/// last descriptor only. When that descriptor looks right and the access
/// faulted anyway, what is wanted is every descriptor the walker actually
/// reads.
pub fn dump_walk(tables: &Tables<'_, Mach>, virt: u64) {
    crate::println!("  walk of {:#018x} through {:#x}:", virt, tables.root());
    let mut read = [(0u32, 0usize, 0u64); 4];
    let found = tables.walk_of(virt, &mut read);
    for (level, index, bits) in read.iter().take(found) {
        if !paging::present(*bits) {
            crate::println!("    level {} index {:3} = {:#018x} absent", level + 1, index, bits);
            return;
        }
        crate::println!(
            "    level {} index {:3} = {:#018x} {}",
            level + 1,
            index,
            bits,
            paging::describe(*bits, *level),
        );
    }
}

/// Print the walk of `virt` through the tables the processor is on, which in a
/// fault report may not be the ones the running task is recorded on.
pub fn dump_live_walk(virt: u64) {
    // SAFETY: the tables the processor is on are the kernel's own or an
    // address space `mm::space` holds a reference to while it is loaded, so
    // they are not freed under the walk. Nothing here changes or frees them.
    let tables = unsafe { Tables::new(&MACH, paging::live_root()) };
    dump_walk(&tables, virt);
}

/// Put the processor on the tables whose id is `root`.
///
/// The switch may not wait for the address space's lock, because the task it
/// is switching away from can be the one holding it. What it needs is the
/// root, which does not change for as long as the `Mm` is alive.
///
/// # Safety
///
/// As `PageTables::load`, and `root` must be the `id()` of a live
/// `PageTables` the caller holds a reference to.
pub(crate) unsafe fn load_id(root: u64) {
    // SAFETY: passed on to the caller.
    unsafe { load_root(root) }
}
