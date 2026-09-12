//! Interrupt-safe spinlocks.

use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, Ordering};

/// Turning interrupts off and back on is the machine's own instruction; these
/// are named here because every lock in the kernel is built on them.
pub use crate::arch::{disable_interrupts, enable_interrupts, interrupts_enabled};

/// Run `f` with interrupts disabled, restoring the previous state after.
pub fn without_interrupts<T>(f: impl FnOnce() -> T) -> T {
    let was_enabled = interrupts_enabled();
    if was_enabled {
        disable_interrupts();
    }
    let result = f();
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
