//! Interrupt-safe spinlocks.

use core::cell::UnsafeCell;
use core::marker::PhantomData;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, Ordering};

/// Turning interrupts off and back on is the machine's own instruction; these
/// are named here because every lock in the kernel is built on them.
pub use crate::arch::{disable_interrupts, enable_interrupts, interrupts_enabled};

/// Proof that interrupts are masked on the processor right now.
///
/// An operation that must not be split -- one that makes something visible and
/// then makes it usable, with a timer tick able to land in between -- takes one
/// of these as an argument. The only things that hand one out are the function
/// that masks interrupts and the lock guard that masks them on the way in, and
/// the token borrows from that scope, so it cannot be carried out of the
/// section it stands for. Asking for it is how such an operation refuses to
/// compile in a place where it could be interrupted halfway.
///
/// It holds nothing and is never read. The argument disappears in the machine
/// code; what is left is the call the compiler was going to make anyway.
#[derive(Clone, Copy)]
pub struct NoInterrupts<'a> {
    /// The borrow is what keeps the token inside the section. The raw pointer
    /// is what keeps it out of a `static` and off another processor: the mask
    /// is this processor's own state.
    scope: PhantomData<(&'a (), *const ())>,
}

impl NoInterrupts<'_> {
    /// Claim that interrupts are already masked here.
    ///
    /// For the paths the hardware masks on the way in, where there is no
    /// masking call to hand a token out. The caller asserts what the
    /// architecture guarantees about how the code was reached.
    pub unsafe fn assume() -> Self {
        NoInterrupts { scope: PhantomData }
    }
}

/// Run `f` with interrupts disabled, restoring the previous state after.
pub fn without_interrupts<T>(f: impl FnOnce(NoInterrupts) -> T) -> T {
    let was_enabled = interrupts_enabled();
    if was_enabled {
        disable_interrupts();
    }
    let result = f(NoInterrupts { scope: PhantomData });
    if was_enabled {
        enable_interrupts();
    }
    result
}

pub struct Spinlock<T> {
    locked: AtomicBool,
    data: UnsafeCell<T>,
}

unsafe impl<T: Send> Sync for Spinlock<T> {}
unsafe impl<T: Send> Send for Spinlock<T> {}

impl<T> Spinlock<T> {
    pub const fn new(data: T) -> Self {
        Spinlock { locked: AtomicBool::new(false), data: UnsafeCell::new(data) }
    }

    pub fn lock(&self) -> SpinGuard<'_, T> {
        let was_enabled = interrupts_enabled();
        disable_interrupts();
        while self
            .locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            while self.locked.load(Ordering::Relaxed) {
                core::hint::spin_loop();
            }
        }
        SpinGuard { lock: self, reenable: was_enabled }
    }

    /// Steal the contents without locking. Only for panic paths.
    pub unsafe fn force_unlock(&self) {
        self.locked.store(false, Ordering::Release);
    }

    pub fn get_mut(&mut self) -> &mut T {
        unsafe { &mut *self.data.get() }
    }
}

pub struct SpinGuard<'a, T> {
    lock: &'a Spinlock<T>,
    reenable: bool,
}

impl<T> SpinGuard<'_, T> {
    /// The lock masks interrupts on the way in and unmasks them when the guard
    /// is dropped, so everything done while it is held is done with them off.
    pub fn irq(&self) -> NoInterrupts<'_> {
        NoInterrupts { scope: PhantomData }
    }
}

impl<T> Deref for SpinGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.lock.data.get() }
    }
}

impl<T> DerefMut for SpinGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.lock.data.get() }
    }
}

impl<T> Drop for SpinGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.locked.store(false, Ordering::Release);
        if self.reenable {
            enable_interrupts();
        }
    }
}
