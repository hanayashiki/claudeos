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
}

impl Pipe {
    pub fn new() -> Arc<Pipe> {
        Arc::new(Pipe {
            buffer: Spinlock::new(Buffer { data: alloc::vec::Vec::new(), head: 0 }),
            readers: AtomicUsize::new(0),
            writers: AtomicUsize::new(0),
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
            crate::sched::yield_now();
        }
    }

    pub fn write(&self, buf: &[u8], nonblock: bool) -> Result<usize, Errno> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            if self.readers.load(Ordering::Acquire) == 0 {
                return Err(Errno::EPIPE);
            }
            {
                let mut buffer = self.buffer.lock();
                let used = buffer.data.len() - buffer.head;
                if used < PIPE_CAPACITY {
                    let n = buf.len().min(PIPE_CAPACITY - used);
                    buffer.data.extend_from_slice(&buf[..n]);
                    return Ok(n);
                }
            }
            if nonblock {
                return Err(Errno::EAGAIN);
            }
            crate::sched::yield_now();
        }
    }
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
        }
    }
}
