//! Anonymous pipes.

use crate::abi::Errno;
use crate::sync::Spinlock;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicUsize, Ordering};

pub const PIPE_CAPACITY: usize = 64 * 1024;

/// Which ends of a pipe one descriptor holds.
///
/// A FIFO opened O_RDWR holds both at once: it may be written and read, and
/// closing it has to give back a reader and a writer. A single "is this the
/// write end" flag cannot say that, and a descriptor it called a read end
/// refused writes and never reached end of file.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PipeEnd {
    Read,
    Write,
    Both,
}

impl PipeEnd {
    pub fn reads(self) -> bool {
        matches!(self, PipeEnd::Read | PipeEnd::Both)
    }

    pub fn writes(self) -> bool {
        matches!(self, PipeEnd::Write | PipeEnd::Both)
    }
}

struct Buffer {
    data: alloc::vec::Vec<u8>,
    head: usize,
}

pub struct Pipe {
    buffer: Spinlock<Buffer>,
    pub readers: AtomicUsize,
    pub writers: AtomicUsize,
    /// How many times each side has opened this pipe.
    ///
    /// A FIFO's open waits for the other side to turn up. Waiting for the
    /// count to be non-zero is not enough: the other side can open, write and
    /// close again before the waiter looks, and then the count is back to
    /// zero and it waits for something that has already happened. The count
    /// of opens only ever goes up, so a change in it is proof the rendezvous
    /// took place.
    pub reader_opens: AtomicUsize,
    pub writer_opens: AtomicUsize,
    /// Tasks blocked because the pipe is empty, and because it is full.
    pub(super) not_empty: crate::sched::WaitQueue,
    pub(super) not_full: crate::sched::WaitQueue,
}

impl Pipe {
    pub fn new() -> Arc<Pipe> {
        Arc::new(Pipe {
            buffer: Spinlock::new(Buffer { data: alloc::vec::Vec::new(), head: 0 }),
            readers: AtomicUsize::new(0),
            writers: AtomicUsize::new(0),
            reader_opens: AtomicUsize::new(0),
            writer_opens: AtomicUsize::new(0),
            not_empty: crate::sched::WaitQueue::new(),
            not_full: crate::sched::WaitQueue::new(),
        })
    }

    /// Let anyone waiting on this pipe look again.
    pub fn wake(&self) {
        self.not_empty.wake_all();
        self.not_full.wake_all();
        crate::sched::io_ready();
    }

    /// Room for at least one more byte.
    pub fn writable_now(&self) -> bool {
        let buffer = self.buffer.lock();
        buffer.data.len() - buffer.head < PIPE_CAPACITY
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
                    self.wake();
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
                    self.wake();
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
    let end = match flags & O_ACCMODE {
        O_WRONLY => PipeEnd::Write,
        O_RDWR => PipeEnd::Both,
        _ => PipeEnd::Read,
    };

    // Read the other side's open count before joining, so an open that
    // happens from here on is seen as a change even if it has finished by the
    // time this one looks.
    let seen = if end == PipeEnd::Write {
        pipe.reader_opens.load(Ordering::Acquire)
    } else {
        pipe.writer_opens.load(Ordering::Acquire)
    };

    let nonblock = flags & O_NONBLOCK != 0;
    if end == PipeEnd::Write && nonblock && pipe.readers.load(Ordering::Acquire) == 0 {
        return Err(Errno::ENXIO);
    }
    if end.reads() {
        pipe.readers.fetch_add(1, Ordering::AcqRel);
        pipe.reader_opens.fetch_add(1, Ordering::AcqRel);
    }
    if end.writes() {
        pipe.writers.fetch_add(1, Ordering::AcqRel);
        pipe.writer_opens.fetch_add(1, Ordering::AcqRel);
    }
    // The end that just arrived may be the one the other side was waiting for.
    pipe.wake();

    let file = Arc::new(super::OpenFile {
        backing: super::FileBacking::Pipe(pipe.clone(), end),
        offset: Spinlock::new(0),
        flags: Spinlock::new(flags),
        path: alloc::string::String::from(path),
    });

    if end != PipeEnd::Both && !nonblock {
        let writing = end == PipeEnd::Write;
        let want = if writing { &pipe.readers } else { &pipe.writers };
        let opens = if writing { &pipe.reader_opens } else { &pipe.writer_opens };
        let queue = if writing { &pipe.not_full } else { &pipe.not_empty };
        let arrived = || {
            want.load(Ordering::Acquire) > 0 || opens.load(Ordering::Acquire) != seen
        };
        loop {
            if arrived() {
                break;
            }
            if crate::sched::has_pending_signal() {
                return Err(Errno::EINTR);
            }
            queue.wait_until(|| arrived() || crate::sched::has_pending_signal());
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
        backing: FileBacking::Pipe(pipe.clone(), PipeEnd::Read),
        offset: Spinlock::new(0),
        flags: Spinlock::new(flags),
        path: alloc::string::String::from("pipe:"),
    });
    let write_end = Arc::new(OpenFile {
        backing: FileBacking::Pipe(pipe, PipeEnd::Write),
        offset: Spinlock::new(0),
        flags: Spinlock::new(flags),
        path: alloc::string::String::from("pipe:"),
    });
    (read_end, write_end)
}

impl Drop for super::OpenFile {
    fn drop(&mut self) {
        match &self.backing {
            super::FileBacking::Pipe(pipe, end) => {
                if end.reads() {
                    pipe.readers.fetch_sub(1, Ordering::AcqRel);
                }
                if end.writes() {
                    pipe.writers.fetch_sub(1, Ordering::AcqRel);
                }
                // The other end has to notice that this one is gone.
                pipe.wake();
            }
            // A socket end reads one pipe and writes the other, so closing it
            // takes a reader off one and a writer off the other.
            super::FileBacking::Socket(socket) => {
                if socket.owes_reader() {
                    socket.rx.readers.fetch_sub(1, Ordering::AcqRel);
                }
                if socket.owes_writer() {
                    socket.tx.writers.fetch_sub(1, Ordering::AcqRel);
                }
                socket.rx.wake();
                socket.tx.wake();
            }
            _ => {}
        }
    }
}
