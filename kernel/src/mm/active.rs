//! Which address space a processor is on, held as a reference that keeps it
//! alive, and the address spaces whose release waits until interrupts are on.
//!
//! Nothing here uses the rest of the kernel: `tools/mmtest` compiles this file
//! on the Mac and checks the order in which spaces are loaded and released,
//! against a space that records what happens to it.

use alloc::sync::Arc;
use alloc::vec::Vec;

/// The address space loaded on one processor, held.
///
/// A task that exits, or a process that execs, lets go of its reference to the
/// address space it ran in while the processor is still on those tables, and a
/// kernel task runs on whatever tables it finds. Holding the loaded space here
/// as well keeps it alive for as long as the processor stays on it, and it goes
/// when the processor moves off. Linux keeps the same reference as `active_mm`:
/// `context_switch` takes it for a task with no mm of its own, and
/// `finish_task_switch` drops the one left behind.
///
/// `None` is the kernel's own tables, which are never freed.
pub struct Active<S> {
    loaded: Option<Arc<S>>,
}

impl<S> Active<S> {
    /// The processor on the kernel's own tables.
    pub const fn new() -> Active<S> {
        Active { loaded: None }
    }

    /// True when `space` is the one loaded.
    pub fn is(&self, space: &Arc<S>) -> bool {
        self.loaded.as_ref().is_some_and(|loaded| Arc::ptr_eq(loaded, space))
    }

    /// Put the processor on `next`, unless it is on it already.
    ///
    /// `load` runs before the reference to the space being left goes, since in
    /// the other order that reference can be the last one and the tables are
    /// freed while the processor is still walking them. The reference is
    /// handed back rather than dropped here: dropping it can free the space,
    /// which walks every table the space has, and the caller is the one that
    /// knows whether interrupts are on.
    #[must_use = "dropping the space left behind can free it; the caller chooses where"]
    pub fn switch(&mut self, next: &Arc<S>, load: impl FnOnce(&S)) -> Option<Arc<S>> {
        if self.is(next) {
            return None;
        }
        load(next);
        self.loaded.replace(next.clone())
    }

    /// Put the processor on the kernel's own tables, unless it is on them
    /// already, and hand back the space it was on, for the reason `switch`
    /// gives.
    #[must_use = "dropping the space left behind can free it; the caller chooses where"]
    pub fn leave(&mut self, load_kernel: impl FnOnce()) -> Option<Arc<S>> {
        self.loaded.as_ref()?;
        load_kernel();
        self.loaded.take()
    }
}

/// Values whose drop is put off until a caller that may take the time takes
/// them.
pub struct Deferred<T> {
    waiting: Vec<T>,
}

impl<T> Deferred<T> {
    pub const fn new() -> Deferred<T> {
        Deferred { waiting: Vec::new() }
    }

    pub fn push(&mut self, value: T) {
        self.waiting.push(value);
    }

    /// Everything waiting, for the caller to drop outside whatever lock holds
    /// this.
    #[must_use = "the values are dropped by whoever holds what this returns"]
    pub fn take(&mut self) -> Vec<T> {
        core::mem::take(&mut self.waiting)
    }
}
