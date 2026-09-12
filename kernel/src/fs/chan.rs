//! Descriptors that are not files: event counters, socket pairs, and epoll
//! sets.
//!
//! These are the three things a program written against Linux reaches for when
//! it waits on more than one thing at a time.

use crate::abi::Errno;
use crate::sched::WaitQueue;
use crate::sync::Spinlock;
use alloc::sync::Arc;
use alloc::vec::Vec;

/// A counter a program can wait on: writing adds, reading takes the whole
/// value and leaves zero behind.
pub struct EventFd {
    count: Spinlock<u64>,
    /// True for EFD_SEMAPHORE: a read takes one rather than all of it.
    semaphore: bool,
    ready: WaitQueue,
}

impl EventFd {
    pub fn new(initial: u32, semaphore: bool) -> Arc<EventFd> {
        Arc::new(EventFd {
            count: Spinlock::new(initial as u64),
            semaphore,
            ready: WaitQueue::new(),
        })
    }

    pub fn readable(&self) -> bool {
        *self.count.lock() > 0
    }

    pub fn read(&self, buf: &mut [u8], nonblock: bool) -> Result<usize, Errno> {
        if buf.len() < 8 {
            return Err(Errno::EINVAL);
        }
        loop {
            {
                let mut count = self.count.lock();
                if *count > 0 {
                    let value = if self.semaphore { 1 } else { *count };
                    *count -= value;
                    drop(count);
                    buf[..8].copy_from_slice(&value.to_le_bytes());
                    self.ready.wake_all();
                    crate::sched::io_ready();
                    return Ok(8);
                }
            }
            if nonblock {
                return Err(Errno::EAGAIN);
            }
            if crate::sched::has_pending_signal() {
                return Err(Errno::EINTR);
            }
            self.ready.wait_until(|| {
                *self.count.lock() > 0 || crate::sched::has_pending_signal()
            });
        }
    }

    pub fn write(&self, buf: &[u8], nonblock: bool) -> Result<usize, Errno> {
        if buf.len() < 8 {
            return Err(Errno::EINVAL);
        }
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(&buf[..8]);
        let value = u64::from_le_bytes(bytes);
        // The all-ones value is reserved: it can never leave the counter in a
        // state a read could report.
        if value == u64::MAX {
            return Err(Errno::EINVAL);
        }
        loop {
            {
                let mut count = self.count.lock();
                if let Some(sum) = count.checked_add(value) {
                    if sum < u64::MAX {
                        *count = sum;
                        drop(count);
                        self.ready.wake_all();
                        crate::sched::io_ready();
                        return Ok(8);
                    }
                }
            }
            if nonblock {
                return Err(Errno::EAGAIN);
            }
            if crate::sched::has_pending_signal() {
                return Err(Errno::EINTR);
            }
            self.ready.wait_until(|| {
                *self.count.lock() < u64::MAX - value || crate::sched::has_pending_signal()
            });
        }
    }
}

/// One end of a connected pair. Each end reads what the other writes, so the
/// two directions are two pipes pointed opposite ways.
pub struct Socket {
    pub rx: Arc<super::pipe::Pipe>,
    pub tx: Arc<super::pipe::Pipe>,
    write_shut: core::sync::atomic::AtomicBool,
    read_shut: core::sync::atomic::AtomicBool,
}

impl Socket {
    /// Two connected endpoints.
    pub fn pair() -> (Arc<Socket>, Arc<Socket>) {
        use core::sync::atomic::Ordering;
        let a = super::pipe::Pipe::new();
        let b = super::pipe::Pipe::new();
        // Each pipe has exactly one reader and one writer: opposite ends.
        for pipe in [&a, &b] {
            pipe.readers.store(1, Ordering::Release);
            pipe.writers.store(1, Ordering::Release);
        }
        let one = Arc::new(Socket {
            rx: a.clone(),
            tx: b.clone(),
            write_shut: core::sync::atomic::AtomicBool::new(false),
            read_shut: core::sync::atomic::AtomicBool::new(false),
        });
        let two = Arc::new(Socket {
            rx: b,
            tx: a,
            write_shut: core::sync::atomic::AtomicBool::new(false),
            read_shut: core::sync::atomic::AtomicBool::new(false),
        });
        (one, two)
    }

    pub fn read(&self, buf: &mut [u8], nonblock: bool) -> Result<usize, Errno> {
        self.rx.read(buf, nonblock)
    }

    pub fn write(&self, buf: &[u8], nonblock: bool) -> Result<usize, Errno> {
        self.tx.write(buf, nonblock)
    }

    pub fn readable(&self) -> bool {
        use core::sync::atomic::Ordering;
        self.rx.available() > 0 || self.rx.writers.load(Ordering::Acquire) == 0
    }

    /// Let the other end see end of file without closing this descriptor.
    /// Done once: the count is the descriptor's, and closing takes it again.
    pub fn shutdown_write(&self) {
        use core::sync::atomic::Ordering;
        if !self.write_shut.swap(true, Ordering::AcqRel) {
            self.tx.writers.fetch_sub(1, Ordering::AcqRel);
            self.tx.wake();
        }
    }

    pub fn shutdown_read(&self) {
        use core::sync::atomic::Ordering;
        if !self.read_shut.swap(true, Ordering::AcqRel) {
            self.rx.readers.fetch_sub(1, Ordering::AcqRel);
            self.rx.wake();
        }
    }

    /// Which of the two counts closing this end still owes.
    pub fn owes_writer(&self) -> bool {
        !self.write_shut.load(core::sync::atomic::Ordering::Acquire)
    }

    pub fn owes_reader(&self) -> bool {
        !self.read_shut.load(core::sync::atomic::Ordering::Acquire)
    }
}

/// One descriptor an epoll set is watching.
#[derive(Clone, Copy)]
pub struct Watch {
    pub fd: i32,
    pub events: u32,
    pub data: u64,
}

/// A set of descriptors to wait on.
///
/// Readiness is worked out when the set is waited on rather than pushed in
/// from each descriptor, so this is level-triggered: a descriptor that is
/// still ready is reported again on the next wait.
pub struct Epoll {
    pub watches: Spinlock<Vec<Watch>>,
}

impl Epoll {
    pub fn new() -> Arc<Epoll> {
        Arc::new(Epoll { watches: Spinlock::new(Vec::new()) })
    }

    pub fn add(&self, watch: Watch) -> Result<(), Errno> {
        let mut watches = self.watches.lock();
        if watches.iter().any(|w| w.fd == watch.fd) {
            return Err(Errno::EEXIST);
        }
        watches.push(watch);
        Ok(())
    }

    pub fn modify(&self, watch: Watch) -> Result<(), Errno> {
        let mut watches = self.watches.lock();
        match watches.iter_mut().find(|w| w.fd == watch.fd) {
            Some(existing) => {
                *existing = watch;
                Ok(())
            }
            None => Err(Errno::ENOENT),
        }
    }

    pub fn remove(&self, fd: i32) -> Result<(), Errno> {
        let mut watches = self.watches.lock();
        let before = watches.len();
        watches.retain(|w| w.fd != fd);
        if watches.len() == before {
            return Err(Errno::ENOENT);
        }
        Ok(())
    }
}
