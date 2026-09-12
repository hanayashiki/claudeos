//! Anonymous pipes.

use crate::abi::Errno;
use crate::sync::Spinlock;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicUsize, Ordering};

pub const PIPE_CAPACITY: usize = 64 * 1024;

struct Buffer {
    data: alloc::vec::Vec<u8>,
    head: usize,
}

pub struct Pipe {
    buffer: Spinlock<Buffer>,
    pub readers: AtomicUsize,
    pub writers: AtomicUsize,
    /// Tasks blocked because the pipe is empty, and because it is full.
    not_empty: crate::sched::WaitQueue,
    not_full: crate::sched::WaitQueue,
}

impl Pipe {
    pub fn new() -> Arc<Pipe> {
        Arc::new(Pipe {
            buffer: Spinlock::new(Buffer { data: alloc::vec::Vec::new(), head: 0 }),
            readers: AtomicUsize::new(0),
            writers: AtomicUsize::new(0),
            not_empty: crate::sched::WaitQueue::new(),
            not_full: crate::sched::WaitQueue::new(),
        })
    }

    pub fn available(&self) -> usize {
        let buffer = self.buffer.lock();
        buffer.data.len() - buffer.head
    }

    pub fn read(&self, buf: &mut [u8], nonblock: bool) -> Result<usize, Errno> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            {
                let mut buffer = self.buffer.lock();
                let available = buffer.data.len() - buffer.head;
                if available > 0 {
                    let n = available.min(buf.len());
                    let head = buffer.head;
                    buf[..n].copy_from_slice(&buffer.data[head..head + n]);
                    buffer.head += n;
                    if buffer.head == buffer.data.len() {
                        buffer.data.clear();
                        buffer.head = 0;
                    }
                    drop(buffer);
                    self.not_full.wake_all();
                    return Ok(n);
                }
            }
            // Empty: end of file once every writer has gone away.
            if self.writers.load(Ordering::Acquire) == 0 {
                return Ok(0);
            }
            if nonblock {
                return Err(Errno::EAGAIN);
            }
            if crate::sched::has_pending_signal() {
                return Err(Errno::EINTR);
            }
            self.not_empty.wait_until(|| {
                let buffer = self.buffer.lock();
                buffer.data.len() > buffer.head
                    || self.writers.load(Ordering::Acquire) == 0
                    || crate::sched::has_pending_signal()
            });
        }
    }

    pub fn write(&self, buf: &[u8], nonblock: bool) -> Result<usize, Errno> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            if self.readers.load(Ordering::Acquire) == 0 {
                // Writing to a pipe nobody is reading raises SIGPIPE, whose
                // default action ends the writer. Without it a producer such
                // as `yes` spins forever after its reader has gone.
                crate::sched::raise_on_current(crate::abi::SIGPIPE);
                return Err(Errno::EPIPE);
            }
            {
                let mut buffer = self.buffer.lock();
                let used = buffer.data.len() - buffer.head;
                if used < PIPE_CAPACITY {
                    let n = buf.len().min(PIPE_CAPACITY - used);
                    buffer.data.extend_from_slice(&buf[..n]);
                    drop(buffer);
                    self.not_empty.wake_all();
                    return Ok(n);
                }
            }
            if nonblock {
                return Err(Errno::EAGAIN);
            }
            if crate::sched::has_pending_signal() {
                return Err(Errno::EINTR);
            }
            self.not_full.wait_until(|| {
                let buffer = self.buffer.lock();
                buffer.data.len() - buffer.head < PIPE_CAPACITY
                    || self.readers.load(Ordering::Acquire) == 0
                    || crate::sched::has_pending_signal()
            });
        }
    }
}

/// Every named pipe that has been opened, by inode number. A FIFO's two ends
/// are separate `open` calls that have to meet at the same buffer, so the
/// buffer belongs to the file rather than to the descriptor.
static FIFOS: Spinlock<alloc::collections::BTreeMap<u64, Arc<Pipe>>> =
    Spinlock::new(alloc::collections::BTreeMap::new());

fn fifo_for(ino: u64) -> Arc<Pipe> {
    let mut fifos = FIFOS.lock();
    if let Some(pipe) = fifos.get(&ino) {
        return pipe.clone();
    }
    let pipe = Pipe::new();
    fifos.insert(ino, pipe.clone());
    pipe
}

/// Open one end of a named pipe.
///
/// Opening for reading waits for a writer and opening for writing waits for a
/// reader, which is what makes a FIFO a rendezvous. O_RDWR opens both ends at
/// once and never waits.
pub fn open_fifo(ino: u64, flags: u32, path: &str) -> Result<Arc<super::OpenFile>, Errno> {
    use crate::abi::{O_ACCMODE, O_NONBLOCK, O_RDWR, O_WRONLY};
    let pipe = fifo_for(ino);
    let access = flags & O_ACCMODE;
    let writing = access == O_WRONLY;
    let both = access == O_RDWR;

    if writing {
        if flags & O_NONBLOCK != 0 && pipe.readers.load(Ordering::Acquire) == 0 {
            return Err(Errno::ENXIO);
        }
        pipe.writers.fetch_add(1, Ordering::AcqRel);
    } else {
        pipe.readers.fetch_add(1, Ordering::AcqRel);
        if both {
            pipe.writers.fetch_add(1, Ordering::AcqRel);
        }
    }
    // The end that just arrived may be the one the other side was waiting for.
    pipe.not_empty.wake_all();
    pipe.not_full.wake_all();

    let file = Arc::new(super::OpenFile {
        backing: super::FileBacking::Pipe(pipe.clone(), writing),
        offset: Spinlock::new(0),
        flags: Spinlock::new(flags),
        path: alloc::string::String::from(path),
    });

    if !both && flags & O_NONBLOCK == 0 {
        let want = if writing { &pipe.readers } else { &pipe.writers };
        let queue = if writing { &pipe.not_full } else { &pipe.not_empty };
        loop {
            if want.load(Ordering::Acquire) > 0 {
                break;
            }
            if crate::sched::has_pending_signal() {
                return Err(Errno::EINTR);
            }
            queue.wait_until(|| {
                want.load(Ordering::Acquire) > 0 || crate::sched::has_pending_signal()
            });
        }
    }
    Ok(file)
}

/// Create a connected pair of open files: (read end, write end).
pub fn create_pair(flags: u32) -> (Arc<super::OpenFile>, Arc<super::OpenFile>) {
    use super::{FileBacking, OpenFile};
    let pipe = Pipe::new();
    pipe.readers.store(1, Ordering::Release);
    pipe.writers.store(1, Ordering::Release);

    let read_end = Arc::new(OpenFile {
        backing: FileBacking::Pipe(pipe.clone(), false),
        offset: Spinlock::new(0),
        flags: Spinlock::new(flags),
        path: alloc::string::String::from("pipe:"),
    });
    let write_end = Arc::new(OpenFile {
        backing: FileBacking::Pipe(pipe, true),
        offset: Spinlock::new(0),
        flags: Spinlock::new(flags),
        path: alloc::string::String::from("pipe:"),
    });
    (read_end, write_end)
}

impl Drop for super::OpenFile {
    fn drop(&mut self) {
        if let super::FileBacking::Pipe(pipe, is_write) = &self.backing {
            let counter = if *is_write { &pipe.writers } else { &pipe.readers };
            counter.fetch_sub(1, Ordering::AcqRel);
            // The other end has to notice that this one is gone.
            pipe.not_empty.wake_all();
            pipe.not_full.wake_all();
        }
    }
}
