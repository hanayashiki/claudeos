//! Tasks: kernel stacks, address spaces, and the process table.
//!
//! The kernel runs on a single CPU with interrupt-disabling locks, and a task
//! only ever mutates its own control block, so the current task is reached
//! through a raw pointer rather than a lock that a blocking syscall would have
//! to hold across a context switch.

use crate::abi::*;
use crate::arch::paging::{AddressSpace, NO_EXECUTE, PRESENT, USER, WRITABLE};
use crate::arch::{self, TaskContext, TrapFrame};
use crate::fs::{FdTable, OpenFile};
use crate::mm::{page_align_down, page_align_up, PAGE_SIZE_U64, USER_MMAP_BASE, USER_STACK_TOP};
use alloc::alloc::{alloc, dealloc, Layout};
use alloc::string::{String, ToString};
use crate::sync::Spinlock;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};

pub const KERNEL_STACK_SIZE: usize = 32 * 1024;

/// Total address range reserved for the main thread stack.
pub const STACK_RESERVE: u64 = 8 * 1024 * 1024;
/// How much of it is mapped up front; the rest faults in on demand.
pub const STACK_PREFAULT: u64 = 256 * 1024;
/// The most the arguments, the environment and the strings alongside them may
/// come to. They are written onto the new stack before the program starts, so
/// a block larger than the stack has nowhere to go; a quarter of the reserve
/// is the share Linux gives them and leaves the program the rest.
pub const MAX_ARG_BYTES: u64 = STACK_RESERVE / 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Runnable,
    /// Waiting for a wake-up: a tick deadline, a child, or I/O.
    Sleeping,
    /// Stopped by a job-control signal. Only SIGCONT makes it runnable again.
    Stopped,
    Zombie,
    Dead,
}

/// Where a region's contents come from, for a region backed by a file.
///
/// An executable is not copied into memory at exec: the pages are filled one
/// at a time from the file as the program reaches them, which is most of what
/// makes starting a program cheap.
#[derive(Clone)]
pub struct FileMap {
    pub node: crate::fs::NodeRef,
    /// Offset in the file of the region's first byte.
    pub offset: u64,
    /// Bytes from the start of the region that come from the file. Anything
    /// past this reads as zero, which is what .bss is.
    pub length: u64,
}

impl core::fmt::Debug for FileMap {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "FileMap {{ offset: {:#x}, length: {:#x} }}", self.offset, self.length)
    }
}

/// A region of the user address space, used to fault pages in on demand.
#[derive(Debug, Clone)]
pub struct Vma {
    pub start: u64,
    pub end: u64,
    pub prot: u64,
    pub flags: u64,
    pub file: Option<FileMap>,
}

impl Vma {
    pub fn contains(&self, addr: u64) -> bool {
        addr >= self.start && addr < self.end
    }

    /// Page table bits this region's pages should get.
    pub fn page_flags(&self) -> u64 {
        let mut bits = PRESENT | USER;
        if self.prot & PROT_WRITE != 0 {
            bits |= WRITABLE;
        }
        if self.prot & PROT_EXEC == 0 {
            bits |= NO_EXECUTE;
        }
        bits
    }
}

/// The parts of the address space bookkeeping that threads share. The page
/// tables themselves are shared through the identical `AddressSpace` value;
/// this holds the region list and heap bounds that go with them.
pub struct MemState {
    pub vmas: Vec<Vma>,
    pub brk_start: u64,
    pub brk: u64,
    pub mmap_top: u64,
}

impl MemState {
    pub fn new() -> MemState {
        MemState { vmas: Vec::new(), brk_start: 0, brk: 0, mmap_top: USER_MMAP_BASE }
    }
}

pub struct Task {
    pub pid: u32,
    pub tgid: u32,
    pub ppid: u32,
    pub pgid: u32,
    pub state: State,

    /// Saved kernel stack pointer between context switches.
    pub kernel_sp: u64,
    kstack: *mut u8,
    pub kstack_top: u64,

    pub space: AddressSpace,
    /// Shared with every thread running in the same address space.
    pub mm: Arc<Spinlock<MemState>>,
    /// Shared with every task that cloned with `CLONE_FILES`.
    pub fds: FdTable,
    /// Shared with every task that cloned with `CLONE_FS`, so that a directory
    /// one thread changes into is the one its siblings resolve against.
    pub cwd: Arc<Spinlock<String>>,
    pub name: String,
    /// Path of the running executable, reported through /proc/self/exe.
    pub exe_path: String,

    pub exit_code: i32,
    /// Registers only this architecture has, carried across context switches.
    pub cpu: TaskContext,
    pub clear_child_tid: u64,
    pub set_child_tid: u64,
    pub robust_list: u64,

    pub pending_signals: u64,
    /// The scheduling nice value. Round robin does not act on it, but a
    /// program that sets it reads it back.
    pub nice: i32,
    /// The signal that stopped this task, and whether the stop and the
    /// following continue have been reported to whoever is waiting.
    pub stop_signal: i32,
    pub report_stop: bool,
    pub report_continue: bool,
    pub signal_actions: [crate::signal::SigAction; 64],
    pub signal_mask: u64,

    /// Tick count to wake at when sleeping, or zero.
    pub wake_at: u64,
    /// Pid this task is waiting for, if it is in wait4.
    pub waiting_for: Option<i32>,

    pub children: Vec<u32>,
    pub umask: u32,
    /// Set once the task has been switched away from at least once, so a
    /// freshly created task is not resumed from a stale frame.
    pub started: bool,
    /// Program a freshly created task should exec before reaching user mode.
    pub pending_exec: Option<(String, Vec<String>, Vec<String>)>,
    /// Parent blocked in vfork, to be woken when this task execs or exits.
    pub vfork_parent: Option<u32>,
}

unsafe impl Send for Task {}

static NEXT_PID: AtomicU32 = AtomicU32::new(1);

pub fn allocate_pid() -> u32 {
    NEXT_PID.fetch_add(1, Ordering::Relaxed)
}

/// Restart pid numbering, so the first user process is pid 1.
pub fn reset_pid_counter(next: u32) {
    NEXT_PID.store(next, Ordering::Relaxed);
}

fn kstack_layout() -> Layout {
    Layout::from_size_align(KERNEL_STACK_SIZE, 16).unwrap()
}

impl Task {
    /// Allocate a task with a fresh kernel stack and address space.
    pub fn new(name: &str, space: AddressSpace) -> Option<alloc::boxed::Box<Task>> {
        let kstack = unsafe { alloc(kstack_layout()) };
        if kstack.is_null() {
            return None;
        }
        let kstack_top = kstack as u64 + KERNEL_STACK_SIZE as u64;
        let pid = allocate_pid();
        Some(alloc::boxed::Box::new(Task {
            pid,
            tgid: pid,
            ppid: 0,
            pgid: pid,
            state: State::Runnable,
            kernel_sp: 0,
            kstack,
            kstack_top,
            space,
            mm: Arc::new(Spinlock::new(MemState::new())),
            fds: FdTable::new(),
            cwd: Arc::new(Spinlock::new(String::from("/"))),
            name: name.to_string(),
            exe_path: String::new(),
            exit_code: 0,
            cpu: TaskContext::new(),
            clear_child_tid: 0,
            set_child_tid: 0,
            robust_list: 0,
            pending_signals: 0,
            nice: 0,
            stop_signal: 0,
            report_stop: false,
            report_continue: false,
            signal_actions: [crate::signal::SigAction::default(); 64],
            signal_mask: 0,
            wake_at: 0,
            waiting_for: None,
            children: Vec::new(),
            umask: 0o022,
            started: false,
            pending_exec: None,
            vfork_parent: None,
        }))
    }

    /// The user register frame, which always sits at the top of the kernel
    /// stack because both entry paths start with RSP at `kstack_top`.
    pub fn trap_frame(&self) -> *mut TrapFrame {
        arch::trap_frame_at(self.kstack_top)
    }

    /// Lay out the kernel stack so the first context switch into this task
    /// lands in `entry`.
    pub fn prepare_kernel_frame(&mut self, entry: u64) {
        self.kernel_sp = arch::prepare_kernel_entry(self.kstack_top, entry);
    }

    pub fn cwd(&self) -> String {
        self.cwd.lock().clone()
    }

    pub fn set_cwd(&self, path: String) {
        *self.cwd.lock() = path;
    }

    pub fn brk_start(&self) -> u64 {
        self.mm.lock().brk_start
    }

    pub fn brk(&self) -> u64 {
        self.mm.lock().brk
    }

    pub fn set_brk(&self, value: u64) {
        self.mm.lock().brk = value;
    }

    pub fn set_heap_base(&self, value: u64) {
        let mut mm = self.mm.lock();
        mm.brk_start = value;
        mm.brk = value;
    }

    pub fn add_vma(&self, start: u64, end: u64, prot: u64, flags: u64) {
        self.mm.lock().vmas.push(Vma { start, end, prot, flags, file: None });
    }

    pub fn add_file_vma(&self, start: u64, end: u64, prot: u64, flags: u64, file: FileMap) {
        self.mm.lock().vmas.push(Vma { start, end, prot, flags, file: Some(file) });
    }

    pub fn find_vma(&self, addr: u64) -> Option<Vma> {
        self.mm.lock().vmas.iter().find(|v| v.contains(addr)).cloned()
    }

    /// True when no recorded region overlaps `[start, end)`.
    pub fn range_is_free(&self, start: u64, end: u64) -> bool {
        self.mm.lock().vmas.iter().all(|v| v.end <= start || v.start >= end)
    }

    pub fn clear_vmas(&self) {
        let mut mm = self.mm.lock();
        mm.vmas.clear();
        mm.mmap_top = USER_MMAP_BASE;
    }

    /// Total size of every recorded region plus the heap, for /proc reporting.
    pub fn virtual_size(&self) -> u64 {
        let mm = self.mm.lock();
        let regions: u64 = mm.vmas.iter().map(|v| v.end - v.start).sum();
        regions + mm.brk.saturating_sub(mm.brk_start)
    }

    /// Pages actually backed by memory right now.
    pub fn resident_pages(&self) -> u64 {
        let mm = self.mm.lock();
        let mut pages = 0u64;
        for vma in mm.vmas.iter() {
            let mut page = vma.start;
            while page < vma.end {
                if self.space.translate(page).is_some() {
                    pages += 1;
                }
                page += PAGE_SIZE_U64;
            }
        }
        let mut page = mm.brk_start;
        while page < mm.brk {
            if self.space.translate(page).is_some() {
                pages += 1;
            }
            page += PAGE_SIZE_U64;
        }
        pages
    }

    pub fn snapshot_vmas(&self) -> Vec<Vma> {
        self.mm.lock().vmas.clone()
    }

    /// Give every region overlapping `[start, end)` the new protection.
    pub fn set_vma_prot(&self, start: u64, end: u64, prot: u64) {
        let mut mm = self.mm.lock();
        for vma in mm.vmas.iter_mut() {
            if vma.start < end && start < vma.end {
                vma.prot = prot;
            }
        }
    }

    /// Remove `[start, end)` from the recorded regions, splitting as needed.
    pub fn remove_vma_range(&self, start: u64, end: u64) {
        let mut mm = self.mm.lock();
        let mut out: Vec<Vma> = Vec::new();
        for vma in mm.vmas.iter().cloned() {
            if vma.end <= start || vma.start >= end {
                out.push(vma);
                continue;
            }
            if vma.start < start {
                let mut head = vma.clone();
                head.end = start;
                if let Some(file) = &mut head.file {
                    file.length = file.length.min(start - vma.start);
                }
                out.push(head);
            }
            if vma.end > end {
                let mut tail = vma.clone();
                tail.start = end;
                // The tail begins further into the file than the whole did.
                if let Some(file) = &mut tail.file {
                    let skipped = end - vma.start;
                    file.offset += skipped;
                    file.length = file.length.saturating_sub(skipped);
                }
                out.push(tail);
            }
        }
        mm.vmas = out;
    }

    /// Find a free span of `len` bytes in the mmap area.
    pub fn find_free_region(&self, len: u64) -> u64 {
        let len = page_align_up(len);
        let mut mm = self.mm.lock();
        let mut candidate = mm.mmap_top;
        loop {
            let end = candidate + len;
            let clash = mm.vmas.iter().find(|v| v.start < end && candidate < v.end).map(|v| v.end);
            match clash {
                Some(v) => candidate = v,
                None => {
                    mm.mmap_top = end;
                    return candidate;
                }
            }
        }
    }

    /// Give this task a private copy of a shared page it is trying to write.
    /// Returns false when the fault was not a copy-on-write fault.
    pub fn handle_cow(&mut self, addr: u64) -> bool {
        use crate::arch::paging::COW;
        let page = page_align_down(addr);
        let Some(flags) = self.space.flags_of(page) else {
            return false;
        };
        if flags & COW == 0 {
            return false;
        }
        let Some(phys) = self.space.translate(page).map(page_align_down) else {
            return false;
        };

        // The last owner can simply take the page back.
        if crate::mm::frame::frame_references(phys) <= 1 {
            return self
                .space
                .set_flags(page, (flags & !COW) | WRITABLE)
                .is_some();
        }

        let Some(copy) = crate::mm::frame::alloc() else {
            return false;
        };
        unsafe {
            core::ptr::copy_nonoverlapping(
                crate::mm::phys_to_virt(phys) as *const u8,
                crate::mm::phys_to_virt(copy.addr()) as *mut u8,
                crate::mm::PAGE_SIZE,
            );
        }
        // A page the program may execute has just been written through a
        // different address than the one it will be fetched from.
        if flags & NO_EXECUTE == 0 {
            crate::arch::sync_instruction_cache(
                crate::mm::phys_to_virt(copy.addr()),
                crate::mm::PAGE_SIZE,
            );
        }
        // Taking the old mapping away hands back the reference this table
        // held on the shared frame; the copy takes its place.
        let shared = self.space.unmap(page);
        let mapped = self.space.map(page, copy, (flags & !COW) | WRITABLE).is_ok();
        drop(shared);
        mapped
    }

    /// Back `addr`'s page with memory if the heap or a region covers it.
    pub fn fault_in(&mut self, addr: u64) -> bool {
        let page = page_align_down(addr);
        if self.space.translate(page).is_some() {
            // Already present: the fault was a protection violation.
            return false;
        }
        let (in_heap, vma) = {
            let mm = self.mm.lock();
            (
                page >= mm.brk_start && page < mm.brk,
                mm.vmas.iter().find(|v| v.contains(page)).cloned(),
            )
        };
        if in_heap {
            return self
                .space
                .map_new(page, PRESENT | WRITABLE | USER | NO_EXECUTE)
                .is_ok();
        }
        let Some(vma) = vma else {
            return false;
        };
        let Some(file) = &vma.file else {
            return self.space.map_new(page, vma.page_flags()).is_ok();
        };

        // A page of an executable: map it writable, fill it from the file,
        // then give it the protection the segment asked for. A fresh frame is
        // already zero, so the part past the file's contents needs nothing.
        if self.space.map_new(page, PRESENT | WRITABLE | USER).is_err() {
            return false;
        }
        let into = page - vma.start;
        if into < file.length {
            let want = (file.length - into).min(PAGE_SIZE_U64) as usize;
            let from = (file.offset + into) as usize;
            let data = file.node.inner.lock();
            let available = data.data.len().saturating_sub(from).min(want);
            if available > 0 {
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        data.data.as_ptr().add(from),
                        page as *mut u8,
                        available,
                    );
                }
                // A text page arrives this way, so these bytes may be the
                // next thing the program executes.
                if vma.page_flags() & NO_EXECUTE == 0 {
                    crate::arch::sync_instruction_cache(page, available);
                }
            }
        }
        self.space.set_flags(page, vma.page_flags());
        true
    }

    pub fn free_kernel_stack(&mut self) {
        if !self.kstack.is_null() {
            unsafe { dealloc(self.kstack, kstack_layout()) };
            self.kstack = core::ptr::null_mut();
        }
    }
}

/// Build the initial user stack: argv, envp and the auxiliary vector, laid out
/// the way a Linux process expects to find them.
pub fn build_user_stack(
    task: &mut Task,
    image: &crate::elf::LoadedImage,
    argv: &[String],
    envp: &[String],
    exec_path: &str,
    interp_base: u64,
) -> Result<u64, Errno> {
    // The strings go on the stack before the program exists to grow it, so
    // what they come to has to be known to fit before any of it is written.
    // A quarter of the reserve is the share Linux gives them out of the stack
    // limit, and it leaves the program the rest to run in.
    let text: u64 = argv
        .iter()
        .chain(envp.iter())
        .map(|value| value.len() as u64 + 1)
        .sum::<u64>()
        + crate::arch::MACHINE.len() as u64
        + 1
        + exec_path.len() as u64
        + 1;
    if text > MAX_ARG_BYTES {
        return Err(Errno::E2BIG);
    }

    let stack_low = USER_STACK_TOP - STACK_RESERVE;
    task.add_vma(stack_low, USER_STACK_TOP, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS);

    // Map the top of the stack eagerly; the rest grows in on demand.
    let prefault_from = USER_STACK_TOP - STACK_PREFAULT;
    let mut page = prefault_from;
    while page < USER_STACK_TOP {
        task.space
            .map_new(page, PRESENT | WRITABLE | USER | NO_EXECUTE)
            .map_err(|_| Errno::ENOMEM)?;
        page += PAGE_SIZE_U64;
    }

    let mut sp = USER_STACK_TOP;

    // Strings first, from the very top down. These go through the checked
    // path: only the top of the stack is mapped at this point, and a block
    // longer than that lands on a page nothing has faulted in, which in the
    // kernel is fatal rather than a fault the handler can serve.
    let push_bytes = |sp: &mut u64, bytes: &[u8]| -> Result<u64, Errno> {
        *sp -= bytes.len() as u64 + 1;
        crate::uaccess::write_bytes(*sp, bytes)?;
        crate::uaccess::write_bytes(*sp + bytes.len() as u64, &[0])?;
        Ok(*sp)
    };

    let mut envp_addrs = Vec::with_capacity(envp.len());
    for value in envp.iter().rev() {
        envp_addrs.push(push_bytes(&mut sp, value.as_bytes())?);
    }
    envp_addrs.reverse();

    let mut argv_addrs = Vec::with_capacity(argv.len());
    for value in argv.iter().rev() {
        argv_addrs.push(push_bytes(&mut sp, value.as_bytes())?);
    }
    argv_addrs.reverse();

    let platform_addr = push_bytes(&mut sp, crate::arch::MACHINE.as_bytes())?;
    let execfn_addr = push_bytes(&mut sp, exec_path.as_bytes())?;

    // 16 bytes of randomness for AT_RANDOM (stack guard, pointer mangling).
    sp -= 16;
    sp &= !0xF;
    let random_addr = sp;
    let mut bytes = [0u8; 16];
    crate::fs::dev::fill_random(&mut bytes);
    crate::uaccess::write_bytes(random_addr, &bytes)?;

    let auxv: [(u64, u64); 14] = [
        (AT_PHDR, image.phdr_addr),
        (AT_PHENT, image.phent),
        (AT_PHNUM, image.phnum),
        (AT_PAGESZ, PAGE_SIZE_U64),
        (AT_BASE, interp_base),
        (AT_FLAGS, 0),
        (AT_ENTRY, image.entry),
        (AT_UID, 0),
        (AT_EUID, 0),
        (AT_GID, 0),
        (AT_EGID, 0),
        (AT_SECURE, 0),
        (AT_RANDOM, random_addr),
        (AT_EXECFN, execfn_addr),
    ];
    let extra: [(u64, u64); 3] = [
        (AT_PLATFORM, platform_addr),
        (AT_CLKTCK, 100),
        (AT_HWCAP, 0),
    ];

    // Size of the pointer block, so the final rsp lands 16-byte aligned.
    let words = 1                       // argc
        + argv.len() + 1                // argv + NULL
        + envp.len() + 1                // envp + NULL
        + 2 * (auxv.len() + extra.len() + 1); // auxv pairs + AT_NULL
    let block = (words * 8) as u64;
    sp = (sp - block) & !0xF;

    let mut at = sp;
    let push_word = |value: u64, at: &mut u64| -> Result<(), Errno> {
        crate::uaccess::write_u64(*at, value)?;
        *at += 8;
        Ok(())
    };
    push_word(argv.len() as u64, &mut at)?;
    for addr in &argv_addrs {
        push_word(*addr, &mut at)?;
    }
    push_word(0, &mut at)?;
    for addr in &envp_addrs {
        push_word(*addr, &mut at)?;
    }
    push_word(0, &mut at)?;
    for (key, value) in auxv.iter().chain(extra.iter()) {
        push_word(*key, &mut at)?;
        push_word(*value, &mut at)?;
    }
    push_word(AT_NULL, &mut at)?;
    push_word(0, &mut at)?;

    Ok(sp)
}

/// Populate a task's trap frame so it starts at `entry` with stack `sp`.
pub fn set_user_entry(task: &mut Task, entry: u64, sp: u64) {
    let frame = task.trap_frame();
    unsafe { arch::start_user_at(&mut *frame, entry, sp) };
}

/// Open the standard descriptors on the console.
pub fn attach_console(task: &mut Task) -> Result<(), Errno> {
    let console = crate::fs::lookup("/dev/console")?;
    let stdin = OpenFile::from_node_at(console.clone(), O_RDONLY, "/dev/console");
    let stdout = OpenFile::from_node_at(console.clone(), O_WRONLY, "/dev/console");
    let stderr = OpenFile::from_node_at(console, O_WRONLY, "/dev/console");
    task.fds.insert_at(0, stdin, false);
    task.fds.insert_at(1, stdout, false);
    task.fds.insert_at(2, stderr, false);
    Ok(())
}

/// Read a file's contents, following a `#!` line if present.
/// Find the file exec should run: the named one, or the interpreter its `#!`
/// line names. The file itself is not read here; exec reads what it needs
/// straight out of the node, and the pages come from there afterwards.
pub fn read_executable(
    path: &str,
) -> Result<(crate::fs::NodeRef, Option<(String, Option<String>)>), Errno> {
    let node = crate::fs::lookup(path)?;
    if node.is_dir() {
        return Err(Errno::EACCES);
    }
    if node.mode() & 0o111 == 0 {
        return Err(Errno::EACCES);
    }
    let shebang = {
        let inner = node.inner.lock();
        let data = &inner.data;
        if data.starts_with(b"#!") {
            let line_end = data.iter().position(|&b| b == b'\n').unwrap_or(data.len());
            let line = core::str::from_utf8(&data[2..line_end]).map_err(|_| Errno::ENOEXEC)?;
            let line = line.trim();
            let mut parts = line.splitn(2, char::is_whitespace);
            let interp = parts.next().unwrap_or("").trim().to_string();
            let arg = parts.next().map(|a| a.trim().to_string()).filter(|a| !a.is_empty());
            if interp.is_empty() {
                return Err(Errno::ENOEXEC);
            }
            Some((interp, arg))
        } else {
            None
        }
    };
    if let Some((interp, arg)) = shebang {
        let interp_node = crate::fs::lookup(&interp)?;
        if interp_node.mode() & 0o111 == 0 {
            return Err(Errno::EACCES);
        }
        return Ok((interp_node, Some((interp, arg))));
    }
    Ok((node, None))
}

/// Entry point for a task created by the kernel rather than by fork: load the
/// program it was created for, then drop into user mode.
pub extern "C" fn user_bootstrap() -> ! {
    let mut task = crate::sched::current();
    let Some((path, argv, envp)) = task.pending_exec.take() else {
        crate::println!("[kernel] bootstrap task has no program");
        crate::sched::exit_current(1 << 8);
    };
    if let Err(err) = crate::syscall::proc::exec_into_current(&path, argv, envp) {
        crate::println!("[kernel] cannot exec {}: {:?}", path, err);
        crate::sched::exit_current(1 << 8);
    }
    let task = crate::sched::current();
    arch::set_kernel_entry_stack(task.kstack_top);
    unsafe { arch::return_to_user(task.trap_frame()) }
}

/// Create a task that will run `path` once scheduled.
pub fn spawn(
    path: &str,
    argv: Vec<String>,
    envp: Vec<String>,
    parent_pid: u32,
) -> Result<u32, Errno> {
    let space = crate::arch::paging::AddressSpace::new_user().ok_or(Errno::ENOMEM)?;
    let name = path.rsplit('/').next().unwrap_or(path);
    let mut task = Task::new(name, space).ok_or(Errno::ENOMEM)?;
    task.ppid = parent_pid;
    task.pgid = task.pid;
    attach_console(&mut task)?;
    task.pending_exec = Some((path.to_string(), argv, envp));
    task.prepare_kernel_frame(user_bootstrap as extern "C" fn() -> ! as usize as u64);
    Ok(crate::sched::register(task))
}

pub struct TaskPtr(pub *mut Task);
unsafe impl Send for TaskPtr {}

impl TaskPtr {
    pub fn get(&self) -> &'static mut Task {
        unsafe { &mut *self.0 }
    }
}

/// Copy a slice of C strings from user memory.
pub fn read_string_array(mut addr: u64) -> Result<Vec<String>, Errno> {
    let mut out = Vec::new();
    if addr == 0 {
        return Ok(out);
    }
    loop {
        if out.len() > 4096 {
            return Err(Errno::E2BIG);
        }
        let ptr = crate::uaccess::read_u64(addr)?;
        if ptr == 0 {
            break;
        }
        out.push(crate::uaccess::read_cstr(ptr, 4096)?);
        addr += 8;
    }
    Ok(out)
}

pub fn share_arc<T>(value: &Arc<T>) -> Arc<T> {
    value.clone()
}
