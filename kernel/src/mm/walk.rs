//! The walk of a four-level translation table, and every change to one.
//!
//! Both machines have four levels of five hundred and twelve descriptors, a
//! 4 KiB granule and the same questions to answer, and they disagree only
//! about which bits a descriptor is made of and how a cached translation is
//! thrown away. Everything that is the same is here, once: the descent, the
//! tables a descent creates, the tables an unmap gives back, the teardown and
//! the fork walk. `Machine` is what each machine supplies underneath, and
//! `crate::arch::paging` is where the two implementations are.
//!
//! Writing it twice is what let the two halves drift. The changes this module
//! makes are the ones the unsafe-code audit found races in
//! (docs/audit-2026-09-18-unsafe.md, findings 1, 2, 4, 5, 6), and a fix
//! written twice is a fix that holds on one machine.
//!
//! Nothing here decides *when* a change may be made. Every entry point takes
//! the caller's word for it that nothing else is looking at these tables,
//! which in the kernel is `mm::space::MmGuard` and its lock, and on the host
//! is a test that runs alone. A descriptor pointer never leaves this module,
//! so the address a caller holds across a point where another task can run is
//! a virtual address, which stays meaningful whatever happened to the tables
//! meanwhile.
//!
//! This file uses nothing of the kernel, so tools/mmtest compiles it for the
//! Mac and runs it against a `Vec` of memory.

use core::ptr;

/// Descriptors in a table.
pub const ENTRIES: usize = 512;
/// Levels a walk may have to create, which is every level below the top.
pub const DEPTH: usize = 3;
/// Address bits one page covers.
pub const PAGE_BITS: u32 = 12;
/// Address bits one level of the walk consumes.
pub const LEVEL_BITS: u32 = 9;
/// Bytes in a page.
pub const PAGE_BYTES: usize = 1 << PAGE_BITS;

/// The descriptor of `virt` at `level`: 3 in the top table, 0 at the last.
#[inline]
pub const fn index_of(virt: u64, level: u32) -> usize {
    ((virt >> (PAGE_BITS + LEVEL_BITS * level)) & 0x1FF) as usize
}

/// How much of the address space one descriptor at `level` covers.
#[inline]
pub const fn level_size(level: u32) -> u64 {
    1u64 << (PAGE_BITS + LEVEL_BITS * level)
}

/// What a machine's translation tables are made of, where its physical memory
/// is, where its frames come from, and what has to happen around a change to a
/// descriptor.
///
/// # Safety
///
/// Everything in this module rests on these:
///
/// - `table_ptr(phys)` names `ENTRIES` readable and writable `u64`s, for as
///   long as `phys` is a frame this hierarchy holds a reference on.
/// - `alloc_zeroed` hands over a frame nothing else owns, all zero, and the
///   caller holds its one reference until `free` gives it back. `free` is
///   called once per reference.
/// - `share` adds a reference and `references` counts them.
/// - `store_barrier` puts earlier stores in memory before later ones, as the
///   hardware walker reads them.
/// - `flush_page` and `flush_all` have taken effect by the time they return.
pub unsafe trait Machine {
    /// Descriptor bits for a last-level entry naming `phys` with `flags`.
    fn leaf(&self, phys: u64, flags: u64) -> u64;

    /// Descriptor bits for an entry naming the table at `phys`, on the way to
    /// a leaf that will ask for `under`.
    fn table(&self, phys: u64, under: u64) -> u64;

    /// True when the walker follows this descriptor at all.
    fn present(&self, bits: u64) -> bool;

    /// True when the walk stops here rather than descending: a block or large
    /// page. Never true at level 0, where it stops anyway.
    fn ends_walk(&self, bits: u64, level: u32) -> bool;

    /// The output address the descriptor names.
    fn addr(&self, bits: u64) -> u64;

    /// The flags as they were asked for, software bits included.
    fn flags(&self, bits: u64) -> u64;

    /// The flags a page gets when a second address space starts sharing it
    /// and the first write to either side must make a private copy, or `None`
    /// when the page is already read-only and needs no mark.
    ///
    /// The two machines do not agree on what this is. One spells write
    /// permission as a bit the hardware reads, so sharing has to take that bit
    /// away and the mark is what remembers it was there; the other keeps the
    /// permission as a software bit and derives read-only from the mark, so
    /// the permission stays as it was. A fork that wrote the first machine's
    /// answer on the second leaves both sides writing one frame.
    fn shared(&self, flags: u64) -> Option<u64>;

    /// What a table descriptor has to become before a leaf asking for
    /// `leaf_flags` under it is reachable, or `None` when it already is.
    ///
    /// One machine forbids from above what the level below allows and the
    /// other says nothing about permission above the last level.
    fn widen(&self, table_bits: u64, leaf_flags: u64) -> Option<u64>;

    /// The descriptors of the table at `phys`.
    fn table_ptr(&self, phys: u64) -> *mut u64;

    /// A zeroed frame nothing else owns, or `None` when there is none.
    fn alloc_zeroed(&self) -> Option<u64>;

    /// Give a reference back.
    fn free(&self, phys: u64);

    /// Take another reference on a frame this hierarchy already holds one on.
    fn share(&self, phys: u64);

    /// How many references the frame at `phys` has.
    fn references(&self, phys: u64) -> u32;

    /// Put earlier stores in memory before the descriptor naming what they
    /// filled in.
    fn store_barrier(&self);

    /// Throw away whatever the walker has cached for `virt`.
    fn flush_page(&self, virt: u64);

    /// Throw away everything the walker has cached.
    fn flush_all(&self);
}

/// Translations that a change under way has left behind, and not thrown away
/// yet.
///
/// A translation the hardware has cached outlives the descriptor it was read
/// from, so a frame whose descriptor was cleared is still reachable until the
/// caches are told. Nothing may run between the two, which on this kernel is
/// what the address space's lock says, and the release of the frame comes
/// after: `mm::space::Stale` is the frame that cannot go back until this has
/// been acted on.
///
/// It collects rather than acting on each change, because a range is as long
/// as a program asks for and the machines invalidate one address at a time.
/// Past the first address the whole space goes at once, which is one operation
/// against as many as the range has pages. Linux draws the same line in
/// `flush_tlb_range`, from a per-address ceiling upwards.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tlb {
    /// Nothing has changed since the last flush.
    Clean,
    /// One address has, and no other.
    Page(u64),
    /// More than one has, or a table descriptor did, which covers every
    /// address under it and every unfinished walk the hardware kept.
    All,
}

impl Tlb {
    pub const fn new() -> Tlb {
        Tlb::Clean
    }

    /// Record that the descriptor for `virt` has changed.
    pub fn note(&mut self, virt: u64) {
        *self = match *self {
            Tlb::Clean => Tlb::Page(virt),
            Tlb::Page(already) if already == virt => Tlb::Page(virt),
            _ => Tlb::All,
        };
    }

    /// Record that a table descriptor has changed, or anything else whose
    /// reach is more than one address.
    pub fn note_all(&mut self) {
        *self = Tlb::All;
    }

    pub fn is_clean(&self) -> bool {
        matches!(self, Tlb::Clean)
    }

    /// Act on what was recorded, and start again.
    pub fn flush(&mut self, machine: &impl Machine) {
        match core::mem::replace(self, Tlb::Clean) {
            Tlb::Clean => {}
            Tlb::Page(virt) => machine.flush_page(virt),
            Tlb::All => machine.flush_all(),
        }
    }
}

impl Default for Tlb {
    fn default() -> Tlb {
        Tlb::new()
    }
}

/// Zeroed frames a walk may put missing tables in, taken before the lock.
///
/// A descent that finds a level missing cannot ask the allocator for a frame:
/// the allocator has a lock of its own, zeroing a frame is a page of stores,
/// and both are inside whatever section the descent is in. So the frames come
/// from here, and what is left over goes back afterwards, outside.
///
/// This owns nothing. `mm::space::Prepared` is what holds the frames and gives
/// back what was not used; the raw addresses are borrowed to the walk.
#[derive(Debug, Default)]
pub struct TableStock {
    frames: [u64; DEPTH],
    len: usize,
}

impl TableStock {
    pub const fn new() -> TableStock {
        TableStock { frames: [0; DEPTH], len: 0 }
    }

    /// How many frames are left.
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Add a frame. More than `DEPTH` is a mistake in the caller, since a
    /// walk cannot create more levels than there are.
    pub fn push(&mut self, phys: u64) {
        assert!(self.len < DEPTH, "more table frames than a walk can use");
        self.frames[self.len] = phys;
        self.len += 1;
    }

    /// Take a frame, or `None` when the stock is out.
    pub fn take(&mut self) -> Option<u64> {
        if self.len == 0 {
            return None;
        }
        self.len -= 1;
        Some(self.frames[self.len])
    }
}

/// A change refused, with nothing written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    /// The address already has something mapped at it.
    Occupied,
    /// The walk needs a table the stock did not have.
    ShortOfTables,
    /// A block or large page covers the address, so there is no last-level
    /// descriptor to write.
    Blocked,
}

/// A hierarchy of translation tables, walked with `machine`.
///
/// This is the raw layer: it says nothing about who may call it or when. In
/// the kernel the caller is `mm::space::MmGuard`, which holds the address
/// space's lock for the whole of a change; in the tests it is a test.
pub struct Tables<'a, M: Machine> {
    machine: &'a M,
    root: u64,
}

impl<'a, M: Machine> Tables<'a, M> {
    /// The hierarchy whose top table is the frame at `root`.
    ///
    /// `'a` is the machine's, which is a constant in the kernel; the tables
    /// are named by a number and nothing here borrows their owner.
    ///
    /// # Safety
    ///
    /// `root` must be the top table of a hierarchy the caller owns, and it
    /// must not be freed for as long as this value is used. Naming somebody
    /// else's is how an address space came to be freed twice before there was
    /// one owner for each.
    pub unsafe fn new(machine: &'a M, root: u64) -> Tables<'a, M> {
        Tables { machine, root }
    }

    pub fn machine(&self) -> &'a M {
        self.machine
    }

    pub fn root(&self) -> u64 {
        self.root
    }

    /// Read a descriptor.
    #[inline]
    fn read(&self, table: u64, index: usize) -> u64 {
        // SAFETY: `table` is a frame this hierarchy holds a reference on, and
        // `index` is below `ENTRIES`, so `Machine::table_ptr` promises the
        // word is there.
        unsafe { ptr::read(self.machine.table_ptr(table).add(index)) }
    }

    /// Write a descriptor, with whatever it makes reachable already in memory.
    ///
    /// The barrier is what puts the zeroed table or the filled page there
    /// before the descriptor naming it is. Without it the walker can reach the
    /// descriptor and read what was at that address before. This is the only
    /// place a descriptor is written, so it cannot be left out.
    #[inline]
    fn store(&self, table: u64, index: usize, bits: u64) {
        self.machine.store_barrier();
        // SAFETY: as `read`.
        unsafe { ptr::write(self.machine.table_ptr(table).add(index), bits) }
    }

    /// The table one level down from `table` on the way to `virt`, or why the
    /// walk stops.
    #[inline]
    fn step(&self, table: u64, virt: u64, level: u32) -> Option<u64> {
        let bits = self.read(table, index_of(virt, level));
        if !self.machine.present(bits) || self.machine.ends_walk(bits, level) {
            return None;
        }
        Some(self.machine.addr(bits))
    }

    /// The last-level table for `virt`, walking what is there and creating
    /// nothing.
    fn leaf_table(&self, virt: u64) -> Option<u64> {
        let mut table = self.root;
        for level in (1..4).rev() {
            table = self.step(table, virt, level)?;
        }
        Some(table)
    }

    /// The last-level table for `virt`, putting the stock's frames in where a
    /// level is missing.
    ///
    /// Each level is read again at the moment it is used, so a level another
    /// task created since the stock was counted is found rather than written
    /// over: the frame stays in the stock and goes back unused. That check and
    /// the store are one step here because the whole descent is inside the
    /// caller's section. Held apart they were finding 1 of the audit: two
    /// threads faulting in one fresh two-megabyte range both read the level as
    /// absent, and the second store left the first's table and every page
    /// under it unreachable and unowned.
    fn leaf_table_creating(
        &self,
        virt: u64,
        under: u64,
        stock: &mut TableStock,
    ) -> Result<u64, Refused> {
        let mut table = self.root;
        for level in (1..4).rev() {
            let index = index_of(virt, level);
            let bits = self.read(table, index);
            if !self.machine.present(bits) {
                let fresh = stock.take().ok_or(Refused::ShortOfTables)?;
                self.store(table, index, self.machine.table(fresh, under));
                table = fresh;
                continue;
            }
            if self.machine.ends_walk(bits, level) {
                return Err(Refused::Blocked);
            }
            // A machine that forbids from above what the last level allows
            // needs the way down opened as well.
            if let Some(wider) = self.machine.widen(bits, under) {
                self.store(table, index, wider);
            }
            table = self.machine.addr(bits);
        }
        Ok(table)
    }

    /// How many tables a mapping at `virt` would have to create.
    ///
    /// The answer is a count rather than a set because the levels a walk is
    /// missing are always the bottom ones: a descriptor exists only where
    /// everything above it does.
    pub fn missing_tables(&self, virt: u64) -> usize {
        let mut table = self.root;
        for level in (1..4).rev() {
            match self.step(table, virt, level) {
                Some(next) => table = next,
                // This level and every one below it.
                None => return level as usize,
            }
        }
        0
    }

    /// The descriptor of `virt` at the last level, as it stands.
    pub fn leaf(&self, virt: u64) -> Option<u64> {
        let table = self.leaf_table(virt)?;
        let bits = self.read(table, index_of(virt, 0));
        if self.machine.present(bits) {
            Some(bits)
        } else {
            None
        }
    }

    /// The flags on the mapping of `virt`, as they were asked for.
    pub fn flags_of(&self, virt: u64) -> Option<u64> {
        self.leaf(virt).map(|bits| self.machine.flags(bits))
    }

    /// The physical address `virt` reaches, including its offset in the page.
    pub fn translate(&self, virt: u64) -> Option<u64> {
        let mut table = self.root;
        for level in (1..4).rev() {
            let bits = self.read(table, index_of(virt, level));
            if !self.machine.present(bits) {
                return None;
            }
            if self.machine.ends_walk(bits, level) {
                let size = level_size(level);
                return Some((self.machine.addr(bits) & !(size - 1)) | (virt & (size - 1)));
            }
            table = self.machine.addr(bits);
        }
        let bits = self.read(table, index_of(virt, 0));
        if !self.machine.present(bits) {
            return None;
        }
        Some(self.machine.addr(bits) | (virt & (level_size(0) - 1)))
    }

    /// Put the frame at `phys` in at `virt`.
    ///
    /// An address that already has a mapping is refused rather than written
    /// over: the descriptor is the only record of the reference the frame it
    /// names holds, so overwriting it would leave that frame with no owner and
    /// no way back. The caller gets its frame back to release.
    pub fn publish(
        &self,
        virt: u64,
        phys: u64,
        flags: u64,
        stock: &mut TableStock,
        tlb: &mut Tlb,
    ) -> Result<(), Refused> {
        let table = self.leaf_table_creating(virt, flags, stock)?;
        let index = index_of(virt, 0);
        if self.machine.present(self.read(table, index)) {
            return Err(Refused::Occupied);
        }
        self.store(table, index, self.machine.leaf(phys, flags));
        // A descriptor that was absent leaves nothing cached on the machines
        // this kernel runs on, but a table created on the way down does: the
        // walker is allowed to keep the fact that the address translated to
        // nothing. Noting it costs one invalidation the caller was making
        // anyway for the whole change.
        tlb.note(virt);
        Ok(())
    }

    /// Put the frame at `phys` in at `virt` over whatever was there, handing
    /// back what the descriptor held.
    ///
    /// One store, because the two-step form is wrong and reads as if it were
    /// not: taking the old mapping away and putting the new one in leaves the
    /// address with nothing at it in between, and a sibling on this address
    /// space that touches it there does not find a page on its way back, it
    /// finds one that was never there and is served a fresh page of zeroes
    /// over the top.
    ///
    /// `None` when nothing was mapped at `virt`, with nothing written: putting
    /// the frame in would be creating a mapping rather than replacing one.
    #[must_use = "the frame handed back is a reference nothing else holds"]
    pub fn replace(&self, virt: u64, phys: u64, flags: u64, tlb: &mut Tlb) -> Option<u64> {
        let table = self.leaf_table(virt)?;
        let index = index_of(virt, 0);
        let old = self.read(table, index);
        if !self.machine.present(old) {
            return None;
        }
        self.store(table, index, self.machine.leaf(phys, flags));
        tlb.note(virt);
        Some(self.machine.addr(old))
    }

    /// Change the flags on the mapping of `virt`, leaving the frame alone.
    /// The frame's address, or `None` when nothing is mapped there.
    pub fn protect(&self, virt: u64, flags: u64, tlb: &mut Tlb) -> Option<u64> {
        let table = self.leaf_table(virt)?;
        let index = index_of(virt, 0);
        let old = self.read(table, index);
        if !self.machine.present(old) {
            return None;
        }
        let phys = self.machine.addr(old);
        self.store(table, index, self.machine.leaf(phys, flags));
        tlb.note(virt);
        Some(phys)
    }

    /// Take the mapping at `virt` away, handing back the frame the descriptor
    /// held, and put any table the removal emptied into `reclaimed`.
    ///
    /// Nothing is released here. Everything handed back is a frame whose
    /// translation the hardware may still have, and the caller releases it
    /// only after acting on `tlb`.
    #[must_use = "the frame handed back is a reference nothing else holds"]
    pub fn unmap(&self, virt: u64, tlb: &mut Tlb, reclaimed: &mut TableStock) -> Option<u64> {
        let table = self.leaf_table(virt)?;
        let index = index_of(virt, 0);
        let old = self.read(table, index);
        if !self.machine.present(old) {
            return None;
        }
        self.store(table, index, 0);
        tlb.note(virt);
        self.reclaim(virt, tlb, reclaimed);
        Some(self.machine.addr(old))
    }

    /// Hand back the tables the unmap of `virt` has left with nothing in them,
    /// from the last level upwards, stopping at the first that still holds
    /// something. The top table is not among them: it belongs to the address
    /// space and goes back when the space does.
    ///
    /// A last-level table covers two megabytes. Without this, a program that
    /// maps one page, touches it and takes it away again at one fresh
    /// two-megabyte-aligned address after another gets every page back and
    /// leaves a table behind each time.
    ///
    /// A table handed back must be named by nothing else, which its reference
    /// count is the answer to: a fork builds the child's tables with mappings
    /// of its own rather than pointing at the parent's, so a live table's
    /// count is one.
    fn reclaim(&self, virt: u64, tlb: &mut Tlb, reclaimed: &mut TableStock) {
        for level in 1..4 {
            // Walked again from the top each time rather than from a pointer
            // kept across the level below: the table this is about to look in
            // may be the one the previous turn handed back.
            let mut parent = self.root;
            let mut reached = true;
            for above in (level + 1..4).rev() {
                match self.step(parent, virt, above) {
                    Some(next) => parent = next,
                    None => {
                        reached = false;
                        break;
                    }
                }
            }
            if !reached {
                return;
            }
            let index = index_of(virt, level);
            let bits = self.read(parent, index);
            if !self.machine.present(bits) || self.machine.ends_walk(bits, level) {
                return;
            }
            let table = self.machine.addr(bits);
            if !self.table_is_empty(table, index_of(virt, level - 1))
                || self.machine.references(table) != 1
            {
                return;
            }
            self.store(parent, index, 0);
            // A table descriptor covered every address under it, and the
            // walker keeps unfinished walks as well as finished translations.
            tlb.note_all();
            reclaimed.push(table);
        }
    }

    /// True when nothing in the table at `phys` is present.
    ///
    /// The sweep starts one past `cleared`, the descriptor an unmap has just
    /// emptied, because a range taken away a page at a time still has the next
    /// page in place there and the sweep stops on its first look. Only the
    /// last page of a table costs all five hundred and twelve.
    fn table_is_empty(&self, phys: u64, cleared: usize) -> bool {
        for step in 1..=ENTRIES {
            if self.machine.present(self.read(phys, (cleared + step) % ENTRIES)) {
                return false;
            }
        }
        true
    }

    /// The lowest address at or above `from` and below `limit` whose
    /// last-level table exists, rounded down to what that table covers.
    ///
    /// This is how a walk of a whole address space is driven from outside
    /// without holding a descriptor pointer: the caller gets an address, lets
    /// go of whatever it was holding, and comes back with the address.
    pub fn next_populated(&self, from: u64, limit: u64) -> Option<u64> {
        let mut virt = from & !(level_size(1) - 1);
        'blocks: while virt < limit {
            let mut table = self.root;
            for level in (1..4).rev() {
                match self.step(table, virt, level) {
                    Some(next) => table = next,
                    None => {
                        // Nothing under this descriptor: on to the next one at
                        // this level, which skips everything it covered.
                        let size = level_size(level);
                        match (virt & !(size - 1)).checked_add(size) {
                            Some(next) => virt = next,
                            None => return None,
                        }
                        continue 'blocks;
                    }
                }
            }
            return Some(virt);
        }
        None
    }

    /// Share every page of the last-level table covering `block` out of
    /// `parent` into this hierarchy, as `share_from` does one.
    ///
    /// A block is one table, so three levels at most have to be created
    /// whatever it holds, which is what makes the stock a fixed size.
    pub fn share_block(
        &self,
        parent: &Tables<'a, M>,
        block: u64,
        stock: &mut TableStock,
        tlb: &mut Tlb,
    ) -> Result<(), Refused> {
        for index in 0..ENTRIES {
            let virt = block + (index as u64) * level_size(0);
            self.share_from(parent, virt, stock, tlb)?;
        }
        Ok(())
    }

    /// Share the page mapped at `virt` in `parent` into this hierarchy,
    /// leaving both sides marked so that the first write makes a private copy.
    ///
    /// One call, because the two halves are one account of the page. Taking
    /// the parent's write permission away and taking the child's reference on
    /// the frame were two calls with a window between them, and a sibling
    /// thread that faulted on the page there read a reference count of one,
    /// took the last-owner path and cleared the mark, so the parent kept write
    /// access to a frame the child was about to share.
    ///
    /// `Machine::shared` is what turns the flags a page has into the flags
    /// both sides get.
    ///
    /// The reference for the child is taken first, so the count is never lower
    /// than the number of mappings that are going to hold it.
    pub fn share_from(
        &self,
        parent: &Tables<'a, M>,
        virt: u64,
        stock: &mut TableStock,
        tlb: &mut Tlb,
    ) -> Result<(), Refused> {
        let Some(table) = parent.leaf_table(virt) else {
            return Ok(());
        };
        let index = index_of(virt, 0);
        let bits = parent.read(table, index);
        if !self.machine.present(bits) {
            // Gone since the caller looked, so there is nothing here to share.
            return Ok(());
        }
        let phys = self.machine.addr(bits);
        self.machine.share(phys);
        let flags = match self.machine.shared(self.machine.flags(bits)) {
            Some(shared) => {
                // The page the parent is still running on has to lose write
                // permission too, or its writes would be seen by the child.
                parent.store(table, index, self.machine.leaf(phys, shared));
                tlb.note(virt);
                shared
            }
            None => self.machine.flags(bits),
        };
        match self.publish(virt, phys, flags, stock, tlb) {
            Ok(()) => Ok(()),
            Err(err) => {
                // The child took no mapping, so it holds no reference.
                self.machine.free(phys);
                Err(err)
            }
        }
    }

    /// Release every mapping below `limit`, every table that held one, and
    /// nothing above it.
    ///
    /// The whole of the range is detached from the top table first and the
    /// detachment made visible before anything under it is handed back. A
    /// frame released while a mapping to it still exists can be given to
    /// another address space and written through the old one, and the walker
    /// is allowed to act on such a descriptor speculatively, without any
    /// instruction naming that address.
    pub fn free_below(&self, limit: u64) {
        let tops = (limit >> (PAGE_BITS + LEVEL_BITS * 3)) as usize;
        let mut detached = [0u64; ENTRIES];
        for index in 0..tops.min(ENTRIES) {
            detached[index] = self.read(self.root, index);
            if self.machine.present(detached[index]) {
                self.store(self.root, index, 0);
            }
        }
        self.machine.flush_all();
        for bits in detached.iter().take(tops.min(ENTRIES)) {
            if self.machine.present(*bits) && !self.machine.ends_walk(*bits, 3) {
                self.free_table(self.machine.addr(*bits), 3);
            }
        }
    }

    /// Release every frame the table at `phys` reaches, then the table.
    fn free_table(&self, phys: u64, level: u32) {
        for index in 0..ENTRIES {
            let bits = self.read(phys, index);
            if !self.machine.present(bits) {
                continue;
            }
            if level == 1 || self.machine.ends_walk(bits, level) {
                self.machine.free(self.machine.addr(bits));
            } else {
                self.free_table(self.machine.addr(bits), level - 1);
            }
            self.store(phys, index, 0);
        }
        self.machine.free(phys);
    }

    /// Copy the descriptors `[from, to)` of the top table out of `src`,
    /// without taking a reference on what they name.
    ///
    /// This is how the kernel half gets into a fresh address space: one set of
    /// tables below the top that every space names, never torn down, so an
    /// address in that half is reached through any of them.
    pub fn adopt_top(&self, src: &Tables<'a, M>, from: usize, to: usize) {
        for index in from..to {
            let bits = src.read(src.root, index);
            if bits != 0 {
                self.store(self.root, index, bits);
            }
        }
    }

    /// The descriptors the walk of `virt` reads, top first, for a fault
    /// report. Stops at the first that is absent or ends the walk.
    pub fn walk_of(&self, virt: u64, out: &mut [(u32, usize, u64); 4]) -> usize {
        let mut table = self.root;
        let mut found = 0;
        for level in (0..4).rev() {
            let index = index_of(virt, level);
            let bits = self.read(table, index);
            out[found] = (level, index, bits);
            found += 1;
            if !self.machine.present(bits) || level == 0 || self.machine.ends_walk(bits, level) {
                return found;
            }
            table = self.machine.addr(bits);
        }
        found
    }
}
