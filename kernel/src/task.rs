//! Tasks: kernel stacks, address spaces, and the process table.
//!
//! The kernel runs on a single CPU with interrupt-disabling locks, and a task
//! only ever mutates its own control block, so the current task is reached
//! through a raw pointer rather than a lock that a blocking syscall would have
//! to hold across a context switch.

use crate::abi::*;
use crate::cpu::idt::TrapFrame;
use crate::fs::{FdTable, OpenFile};
use crate::mm::paging::{AddressSpace, NO_EXECUTE, PRESENT, USER, WRITABLE};
use crate::mm::{page_align_down, page_align_up, PAGE_SIZE_U64, USER_MMAP_BASE, USER_STACK_TOP};
use alloc::alloc::{alloc, dealloc, Layout};
use alloc::string::{String, ToString};
use crate::sync::Spinlock;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};

pub const KERNEL_STACK_SIZE: usize = 32 * 1024;
pub const TRAP_FRAME_SIZE: usize = core::mem::size_of::<TrapFrame>();

/// Total address range reserved for the main thread stack.
pub const STACK_RESERVE: u64 = 8 * 1024 * 1024;
/// How much of it is mapped up front; the rest faults in on demand.
pub const STACK_PREFAULT: u64 = 256 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Runnable,
    /// Waiting for a wake-up: a tick deadline, a child, or I/O.
    Sleeping,
    Zombie,
    Dead,
}

/// A region of the user address space, used to fault pages in on demand.
#[derive(Debug, Clone, Copy)]
pub struct Vma {
    pub start: u64,
    pub end: u64,
    pub prot: u64,
    pub flags: u64,
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
    pub rsp: u64,
    kstack: *mut u8,
    pub kstack_top: u64,

    pub space: AddressSpace,
    /// Shared with every thread running in the same address space.
    pub mm: Arc<Spinlock<MemState>>,
    pub fds: FdTable,
    pub cwd: String,
    pub name: String,
    /// Path of the running executable, reported through /proc/self/exe.
    pub exe_path: String,

    pub exit_code: i32,
    pub fs_base: u64,
    pub gs_base: u64,
    pub clear_child_tid: u64,
    pub set_child_tid: u64,
    pub robust_list: u64,

    pub pending_signals: u64,
    pub signal_handlers: [u64; 64],
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
            rsp: 0,
            kstack,
            kstack_top,
            space,
            mm: Arc::new(Spinlock::new(MemState::new())),
            fds: FdTable::new(),
            cwd: String::from("/"),
            name: name.to_string(),
            exe_path: String::new(),
            exit_code: 0,
            fs_base: 0,
            gs_base: 0,
            clear_child_tid: 0,
            set_child_tid: 0,
            robust_list: 0,
            pending_signals: 0,
            signal_handlers: [0; 64],
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
        (self.kstack_top - TRAP_FRAME_SIZE as u64) as *mut TrapFrame
    }

    /// Lay out the kernel stack so the first context switch into this task
    /// lands in `entry`.
    pub fn prepare_kernel_frame(&mut self, entry: u64) {
        // Leave the trap frame area untouched and build the switch frame below it.
        let base = self.kstack_top - TRAP_FRAME_SIZE as u64;
        let frame = (base - 8 * 8) as *mut u64;
        unsafe {
            // Mirrors what switch_context pops: rflags, r15, r14, r13, r12,
            // rbx, rbp, then the address it returns to.
            *frame.add(0) = 0x0000_0002; // rflags with interrupts off
            *frame.add(1) = 0; // r15
            *frame.add(2) = 0; // r14
            *frame.add(3) = 0; // r13
            *frame.add(4) = 0; // r12
            *frame.add(5) = 0; // rbx
            *frame.add(6) = 0; // rbp
            *frame.add(7) = entry;
        }
        self.rsp = frame as u64;
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
        self.mm.lock().vmas.push(Vma { start, end, prot, flags });
    }

    pub fn find_vma(&self, addr: u64) -> Option<Vma> {
        self.mm.lock().vmas.iter().find(|v| v.contains(addr)).copied()
    }

    pub fn clear_vmas(&self) {
        let mut mm = self.mm.lock();
        mm.vmas.clear();
        mm.mmap_top = USER_MMAP_BASE;
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
        for vma in mm.vmas.iter().copied() {
            if vma.end <= start || vma.start >= end {
                out.push(vma);
                continue;
            }
            if vma.start < start {
                out.push(Vma { end: start, ..vma });
            }
            if vma.end > end {
                out.push(Vma { start: end, ..vma });
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
            let clash = mm.vmas.iter().find(|v| v.start < end && candidate < v.end).copied();
            match clash {
                Some(v) => candidate = v.end,
                None => {
                    mm.mmap_top = end;
                    return candidate;
                }
            }
        }
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
                mm.vmas.iter().find(|v| v.contains(page)).copied(),
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
        self.space.map_new(page, vma.page_flags()).is_ok()
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
) -> Result<u64, Errno> {
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

    // Strings first, from the very top down.
    let mut push_bytes = |sp: &mut u64, bytes: &[u8]| -> u64 {
        *sp -= bytes.len() as u64 + 1;
        unsafe {
            core::ptr::copy_nonoverlapping(bytes.as_ptr(), *sp as *mut u8, bytes.len());
            *(( *sp + bytes.len() as u64) as *mut u8) = 0;
        }
        *sp
    };

    let mut envp_addrs = Vec::with_capacity(envp.len());
    for value in envp.iter().rev() {
        envp_addrs.push(push_bytes(&mut sp, value.as_bytes()));
    }
    envp_addrs.reverse();

    let mut argv_addrs = Vec::with_capacity(argv.len());
    for value in argv.iter().rev() {
        argv_addrs.push(push_bytes(&mut sp, value.as_bytes()));
    }
    argv_addrs.reverse();

    let platform_addr = push_bytes(&mut sp, b"x86_64");
    let execfn_addr = push_bytes(&mut sp, exec_path.as_bytes());

    // 16 bytes of randomness for AT_RANDOM (stack guard, pointer mangling).
    sp -= 16;
    sp &= !0xF;
    let random_addr = sp;
    unsafe {
        let mut bytes = [0u8; 16];
        crate::fs::dev::fill_random(&mut bytes);
        core::ptr::copy_nonoverlapping(bytes.as_ptr(), random_addr as *mut u8, 16);
    }

    let auxv: [(u64, u64); 14] = [
        (AT_PHDR, image.phdr_addr),
        (AT_PHENT, image.phent),
        (AT_PHNUM, image.phnum),
        (AT_PAGESZ, PAGE_SIZE_U64),
        (AT_BASE, 0),
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

    unsafe {
        let mut p = sp as *mut u64;
        *p = argv.len() as u64;
        p = p.add(1);
        for addr in &argv_addrs {
            *p = *addr;
            p = p.add(1);
        }
        *p = 0;
        p = p.add(1);
        for addr in &envp_addrs {
            *p = *addr;
            p = p.add(1);
        }
        *p = 0;
        p = p.add(1);
        for (key, value) in auxv.iter().chain(extra.iter()) {
            *p = *key;
            p = p.add(1);
            *p = *value;
            p = p.add(1);
        }
        *p = AT_NULL;
        p = p.add(1);
        *p = 0;
    }

    Ok(sp)
}

/// Populate a task's trap frame so it starts at `entry` with stack `sp`.
pub fn set_user_entry(task: &mut Task, entry: u64, sp: u64) {
    let frame = task.trap_frame();
    unsafe {
        core::ptr::write_bytes(frame as *mut u8, 0, TRAP_FRAME_SIZE);
        (*frame).rip = entry;
        (*frame).cs = crate::cpu::gdt::USER_CODE as u64;
        (*frame).rflags = 0x202; // interrupts enabled
        (*frame).rsp = sp;
        (*frame).ss = crate::cpu::gdt::USER_DATA as u64;
        (*frame).vector = 0x100;
    }
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
pub fn read_executable(path: &str) -> Result<(Vec<u8>, Option<(String, Option<String>)>), Errno> {
    let node = crate::fs::lookup(path)?;
    if node.is_dir() {
        return Err(Errno::EACCES);
    }
    let data = node.inner.lock().data.clone();
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
        let interp_node = crate::fs::lookup(&interp)?;
        let interp_data = interp_node.inner.lock().data.clone();
        return Ok((interp_data, Some((interp, arg))));
    }
    Ok((data, None))
}

/// Entry point for a task created by the kernel rather than by fork: load the
/// program it was created for, then drop into user mode.
pub extern "C" fn user_bootstrap() -> ! {
    let task = crate::sched::current();
    let Some((path, argv, envp)) = task.pending_exec.take() else {
        crate::println!("[kernel] bootstrap task has no program");
        crate::sched::exit_current(1 << 8);
    };
    if let Err(err) = crate::syscall::proc::exec_into_current(&path, argv, envp) {
        crate::println!("[kernel] cannot exec {}: {:?}", path, err);
        crate::sched::exit_current(1 << 8);
    }
    let task = crate::sched::current();
    crate::cpu::gdt::set_kernel_stack(task.kstack_top);
    crate::cpu::per_cpu().kernel_rsp = task.kstack_top;
    unsafe { crate::cpu::idt::enter_user_mode(task.trap_frame()) }
}

/// Create a task that will run `path` once scheduled.
pub fn spawn(
    path: &str,
    argv: Vec<String>,
    envp: Vec<String>,
    parent_pid: u32,
) -> Result<u32, Errno> {
    let space = crate::mm::paging::AddressSpace::new_user().ok_or(Errno::ENOMEM)?;
    let name = path.rsplit('/').next().unwrap_or(path);
    let mut task = Task::new(name, space).ok_or(Errno::ENOMEM)?;
    task.ppid = parent_pid;
    task.pgid = task.pid;
    attach_console(&mut task)?;
    task.pending_exec = Some((path.to_string(), argv, envp));
    task.prepare_kernel_frame(user_bootstrap as usize as u64);
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
