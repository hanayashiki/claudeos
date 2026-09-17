//! Futex wait queues.
//!
//! A waiter registers before it re-reads the word it is waiting on, so a wake
//! that arrives in between is seen as a flag on the registration rather than
//! being lost into a sleep nobody will end.

use crate::sched;
use crate::sync::Spinlock;
use alloc::vec::Vec;

/// What identifies a futex: the address space it lives in and the address
/// within it. Threads share an address space, so they meet on the same key;
/// two processes at the same address do not.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Key {
    space: u64,
    address: u64,
}

pub fn futex_key(address: u64) -> Key {
    Key { space: sched::current().mm_id(), address }
}

struct Waiter {
    pid: u32,
    key: Key,
    woken: bool,
}

static WAITERS: Spinlock<Vec<Waiter>> = Spinlock::new(Vec::new());

pub fn register(key: Key) {
    let pid = sched::current().pid;
    WAITERS.lock().push(Waiter { pid, key, woken: false });
}

pub fn unregister(key: Key) {
    let pid = sched::current().pid;
    WAITERS.lock().retain(|waiter| !(waiter.pid == pid && waiter.key == key));
}

fn woken(key: Key) -> bool {
    let pid = sched::current().pid;
    WAITERS.lock().iter().any(|w| w.pid == pid && w.key == key && w.woken)
}

/// Wake up to `count` tasks waiting on `key`. Returns how many were waiting.
pub fn wake(key: Key, count: u32) -> u32 {
    let mut pids = Vec::new();
    {
        let mut waiters = WAITERS.lock();
        for waiter in waiters.iter_mut() {
            if pids.len() as u32 >= count {
                break;
            }
            if waiter.key == key && !waiter.woken {
                waiter.woken = true;
                pids.push(waiter.pid);
            }
        }
    }
    // The task list has its own lock, so it is taken after the waiter list is
    // released rather than inside it.
    let woken = pids.len() as u32;
    sched::with_tasks(|table| {
        for pid in pids {
            if let Some(task) = table.find(pid) {
                task.wake(table.irq());
            }
        }
    });
    woken
}

/// Sleep until woken on `key` or until `deadline` passes. Returns false when
/// the deadline had already passed, which is the caller's timeout.
pub fn sleep_until(key: Key, deadline: u64) -> bool {
    // Registering, re-reading and parking have to be one step: a wake that
    // lands between the last check and the sleep finds a runnable task, wakes
    // nothing, and the sleep then has no deadline to end it.
    let parked = crate::sync::without_interrupts(|irq| {
        if woken(key) {
            return None;
        }
        if crate::trap::ticks() >= deadline {
            return Some(false);
        }
        sched::current().sleep(if deadline == u64::MAX { 0 } else { deadline }, irq);
        Some(true)
    });
    match parked {
        None => return true,
        Some(false) => return false,
        Some(true) => {}
    }

    // Nothing returns a sleeping task to the run queue without clearing its
    // deadline, so there is none left to clear here.
    sched::schedule();
    true
}
