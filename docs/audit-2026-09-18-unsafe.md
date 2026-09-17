# Kernel unsafe-code audit, 2026-09-18

Read-only audit of kernel/src at main 544cd55, aarch64 board build, against Rust's safety rules and the kernel's own invariants. Done by an agent for the lead; relayed in three messages and recorded here. Nothing below is proven to cause the cloudflared crashes (BACKLOG.md, "Incident 2026-09-16").

## Facts used throughout

- **Preemption:** one CPU, and kernel mode can be preempted. A fault or system call from EL0 runs with interrupts on (arch/aarch64/trap.rs:367-379). The timer tick switches tasks (trap.rs:44 → sched.rs:378).
- **GOMAXPROCS is 1:** sched_getaffinity reports one CPU (syscall/proc.rs:706-712), so cloudflared has one P. Two of its threads run Go code at once only after sysmon hands the P away from a thread slow inside a system call.
- **Page tables built with interrupts on:**
  - user page faults, sched.rs:436 → task.rs:794;
  - the pre-fault in uaccess::validate (uaccess.rs:50-55), from syscall/file.rs:103 (read) and syscall/net.rs:388 (recvfrom, recvmsg).
- **Run with interrupts off:** read_bytes_in, write_bytes_in, handle_cow.

## Ranked findings

### 1. Two threads creating the same page table: the second store overwrites the first
- **Where:** paging.rs:366-382 (entry_for reads the descriptor at 371, calls publish_table at 376); paging.rs:253-257 (alloc_zeroed at 254 unmasked, store at 255). Reached from task.rs:813, 819, 855 and uaccess.rs:54.
- **Rule broken:**
  - The descriptor is the only record of a table's reference (paging.rs:449-455, frame.rs:332-342), and it is overwritten without being taken back.
  - A valid table descriptor is replaced without invalidating its range (paging.rs:222-232).
- **Sequence:**
  1. M1's read(fd, buf) pre-faults buf, which is fresh memory in a 2 MiB range with no last-level table. entry_for reads "absent".
  2. The tick lands during allocation or zeroing (frame.rs:392-395), and M1 is switched out.
  3. sysmon hands the P to M2, which allocates in the same range, faults, creates table T_B, maps P1..Pn and writes objects.
  4. M1 resumes and stores T_A. T_B and P1..Pn are leaked. Only M1's page is flushed (paging.rs:472).
  5. After the next address-space switch (write_ttbr flushes all, paging.rs:93-102), M2's addresses fault again and get fresh zero pages. Everything M2 wrote there reads back as zero.
- **Fit (medium):**
  - The kernel side is in the code. The window is about 1 µs per new 2 MiB range, plus the P handoff, which fits hours between crashes.
  - The Go-side step is inferred, not observed: Go 1.22+ keeps pointer bitmaps for small objects at the end of the span. A zeroed bitmap page makes the collector free live objects, and the memory is reused (for example TLS buffers) while live objects still point into it.
  - The same race exists between two kernel heap growers (mm/heap.rs:193-203, 288-289, 234-238). It would end in a kernel page fault panic, which was not seen.
- **Fix:** allocate zeroed frames for the missing levels before masking. In one masked section: walk again, use a preallocated frame only where the descriptor is still absent, store the leaf and invalidate. Release unused frames afterwards.

### 2. map: presence check and store split by a preemption point
- **Where:** paging.rs:464-474 (check 467, store 470, flush 472); same callers as 1.
- **Sequence:**
  1. M1 finds page X absent and is preempted.
  2. M2 maps F2 at X and writes to it.
  3. M1 stores zero frame F1 over F2. F2 is leaked and X reads zero.
- **Fit:** medium-low; the window is a few instructions.
- **Fix:** as 1.

### 3. release can destroy one address space twice
- **Where:**
  - sched.rs:1083-1098 (remove_process 1087, space_in_use 1090, destroy 1091), called unmasked after entries leave the table: reap_dead_threads (1132-1148, release 1152) and reap_child (1062-1068, release 1071).
  - The same gap in exec: syscall/proc.rs:309-311.
- **Rule broken:** Frame::from_recorded must remove exactly one recorded reference (frame.rs:364-372); a second destroy frees the root twice (paging.rs:792-795). Root cause: AddressSpace is Copy, has a pub root, and has a safe destroy(self) (paging.rs:323-326, 792).
- **Sequence:**
  1. Thread T of P exits, then leader L exits and wakes parent K.
  2. Task X (an httpd child in exit_current, sched.rs:492, or any task at system-call exit, syscall/mod.rs:89) reaps T and is preempted inside remove_process (fs/procfs.rs:149-160), before the check.
  3. K reaps L and destroys P's space. The root R is freed and reallocated (frame.rs:161-163).
  4. X resumes, finds nothing using "the space", and destroys it again. free_user_memory (paging.rs:696-712) zeroes R's first 2048 bytes, treats words with bit 0 set as tables and frees and zeroes them (799-814), and frees R under its new owner.
  5. Those frames get second owners, so one owner's bytes appear in the other's page.
- **Fit (low-medium):** needs a cloudflared exit coinciding with httpd activity and K reaping, so it can't cause a boot's first crash. Severity high.
- **Fix:** decide each destroy under the same TASKS hold that removes the entries (also in exec), and make the address-space owner non-Copy.

### 4. reclaim_tables frees a table while another task holds a pointer into it
- **Where:** paging.rs:425-447 (take_table 443, free 444), racing the entry pointer held in map (466-470) or set_flags (670-674).
- **Sequence:** A holds an entry pointer into table T and is preempted. Sibling B unmaps T's last page (mem.rs:207 → paging.rs:626-627), and T is freed and reused. A's store then writes 8 bytes into someone's page, or maps A's frame into an unrelated table.
- **Fit:** low; needs munmap, MAP_FIXED, a brk shrink or an mremap shrink racing a map into the same table.
- **Fix:** 1's masking, also in set_flags.

### 5. mprotect/set_flags race an unmap or a copy-on-write break
- **Where:** syscall/mem.rs:229-244 (flags_of 235, set_flags 241, both unmasked); paging.rs:668-678 (frame read and stored at 674, flush 676).
- **Sequence A (unmap):** A reads frame F and is preempted. Sibling B munmaps; F is freed and reused. A stores encode(F, bits), so its process maps a frame it no longer owns, and bytes cross between owners.
- **Sequence B (after fork):** A reads a COW entry and is preempted. B's write copies the page to C and drops shared S from 2 to 1. A stores S back with COW, leaking C. The next write takes the last-owner path (task.rs:727-728), and two processes write S.
- **Fit:** low; Go makes no mprotect calls, and musl's RELRO mprotect runs before threads exist. Severity high.
- **Fix:** set_flags takes a NoInterrupts token; read, compute the COW-preserving bits and store in one section; drop the separate flags_of in mprotect.

### 6. clone_user_from walks the parent's tables with interrupts on
- **Where:** paging.rs:777-788, 818-840 (read 828, recursion 836 unmasked; share_page 748-775 masked).
- **Sequence:** a sibling's munmap frees a table mid-walk. share_page then treats reused bytes as descriptors, increments arbitrary frame counts, maps arbitrary frames into the child (761, 774), and may write a COW descriptor into the reused frame (771).
- **Fit:** low; needs a multithreaded process that forks. cloudflared does not; Go's os/exec uses CLONE_VM|CLONE_VFORK.
- **Fix:** a per-address-space lock held for the walk and taken by unmap, or masking per table while it is read.

### 7. A fork-shared read-only page can become writable without COW
- **Where:** paging.rs:763-773 (COW only when WRITABLE); mem.rs:236-241 (sets WRITABLE on any present page without COW); task.rs:728, 752 (WRITABLE granted unconditionally).
- **Sequence:** after a fork, mprotect(PROT_READ|PROT_WRITE) on an inherited read-only private page lets both processes write one frame. A COW page later made PROT_READ also becomes writable on its first write fault.
- **Fit:** low.
- **Fix:** set COW instead of WRITABLE when a frame's count is above 1, and take handle_cow's writable bit from the region's protection.

### 8. A page fault racing munmap leaves a page mapped outside any region
- **Where:** task.rs:806-819 (region looked up under the mm lock, mapped after dropping it, unmasked) vs mem.rs:199-209. The file-backed path task.rs:825-855 has a longer window.
- **Sequence:** the fault finds the region, a sibling removes and unmaps it, and the fault then publishes. A later hinted mmap (mem.rs:93-99), brk growth, or in-place mremap growth (mem.rs:273-275) exposes old bytes where zeros are expected.
- **Fit:** low.
- **Fix:** one per-mm lock across lookup plus publish, and across removal plus unmap.

### 9. mmap with a hint: range check and region insert are separate
- **Where:** mem.rs:93-101 vs 108; MAP_FIXED between mem.rs:83 and add_vma.
- **Sequence:** two threads claim overlapping ranges.
- **Fit:** low; Go's hinted arenas (0x7F40_0000_0000) and unhinted mappings from mmap_top don't meet.
- **Fix:** check, placement and add_vma in one with_mem.

### 10. User-copy critical sections can sleep
- **Where:** uaccess.rs:101-107, 136-140 → validate_in → task.rs:833-841 (fs::data::read) → storage/vfs.rs:63-67 (SleepLock) or mmc/delay.rs:34-37 (sleep_ms) → schedule (sched.rs:1245-1264 or 397).
- **Rule broken:** a NoInterrupts section stays open across a context switch. A sibling can unmap or remap between the check and the copy.
- **Fit: low** (re-rated after the lead's correction that since 2026-09-17 cloudflared, busybox, busybox-extras and ld-musl run from /data/usr).
  - Reachable when a system call copies from or into a never-touched page of a /data image, for example a path in rodata passed to openat. Rare after start-up.
  - The copy itself stays correct: validate_in re-checks the page after fault_in returns (uaccess.rs:57-64), so a sibling's unmap during the sleep gives EFAULT, not a stale copy.
  - What it does break:
    - the section's documented guarantee (uaccess.rs:6-10);
    - it widens finding 8;
    - card polling (spin_us, sdhci.rs:542-553, and the PIO loop) runs masked for the whole read, stalling ticks and the WiFi task.
- **Fix:** fault pages in before masking, and only check them inside the section.
- **User faults on /data images** (sched.rs:436 → task.rs:825-855 → fs::data::read → SleepLock → card I/O) run unmasked, so they aren't finding 10. They widen finding 8's gap from microseconds to milliseconds.
- **A sleep inside a masked section does not make another task's section preemptible.** Masking is per-task processor state, saved and restored by schedule() (sched.rs:327-336). Findings 1, 2 and 5 are in code with no mask at all, so they are unaffected, apart from one indirect effect: system calls that sleep on the card let Go's sysmon hand the P to another thread more often.

### /data-only mechanism (safe Rust, not audited for logic)
- **Lazy faults re-resolve the file by path:** every fault on a /data image looks the file up again (vfs.rs:131-137, 249-253).
- **No ETXTBSY:** it's defined in abi.rs:41 but never returned, so a running program can be rewritten.
- **Consequence:** rewriting /data/usr/bin/cloudflared while it runs (a copy, `mkcard.sh --usr`) makes later faults load the new file's bytes. A FAT or EMMC2 read returning the wrong block would also go straight into its pages.
- **Fit: low.** The 09-16 crash ran cloudflared from the initramfs at the same pc, so no /data-only mechanism is the common cause. A corrupted static page would also tend to repeat the same bad value, not a different random address.
- **Could not check:** the logic of storage/fat, storage/card.rs, mmc/sdhci.rs and vfs block/path handling, which now supply cloudflared's pages. Nor whether cloudflared's file was rewritten during the 09-18 uptime.

### 11. Threads start with no signals blocked
- **Where:** task.rs:337 (blocked = 0); syscall/proc.rs:14-156 does not copy the parent's mask (for fork and CLONE_THREAD).
- **Sequence:** a signal taken between clone and Go's minit runs on the wrong stack with x28 naming the parent's g.
- **Fit:** low; Go would print "handler not on signal stack", which was not seen.
- **Fix:** copy the blocked set on fork and clone.

## Rule violations with no wrong user bytes today

- **Aliasing &mut TrapFrame:** sched.rs:944, 966 (task.trap_frame() while syscall_dispatch's, the timer's or exception_entry's frame is live); task.rs:1217-1218 and syscall/proc.rs:340-341 under execve's frame. Fix: pass the caller's &mut into check_signals and exec_into_current.
- **Cell shared across tasks:** Task is !Sync (task.rs:156-269), but TaskPtr::get (1342-1344, pub field 1335, safe) and `unsafe impl Send` (271, 1336) share &Task. set_action writes 32-byte SigActions unmasked (task.rs:387-389 from proc.rs:989) while other tasks read them (sched.rs:132, 713); a torn read gives a wrong SIGCHLD or stop decision. Fix: dispositions behind a lock shared by the thread group.
- **Unsound safe APIs:**
  - AddressSpace: pub root, Copy, safe destroy, free_user_memory and map_fixed (paging.rs:323-326, 478-488, 696, 792).
  - TaskPtr(pub *mut Task) with a safe get().
  - Nested with_cpu gives two &mut TaskContext (task.rs:497-499).
  - current() before sched::init dereferences null (sched.rs:33-38, debug_assert only).
  - map_new, publish, set_flags, flags_of, translate and clone_user_from are safe but assume nothing else changes the tables.
- **Frame allocator:** u16 reference counts saturate silently (frame.rs:166-172). Holes have their bitmap bit set with count 0 (frame.rs:253-254, 279), so a stray free of a hole address would hand out non-RAM.

## Checked and sound

- **FP/SIMD:** disassembly of build/kernel-aarch64.elf shows only schedule, rt_sigreturn, with_cpu, check_signals and fork touch q registers. FpuState offsets, bounds and the vectors() cast are correct (arch/aarch64/task.rs:71-165). The kernel is soft-float and the WPA crates emit no NEON.
- **Trap and context switch:** SAVE_STATE/LOAD_STATE (vectors.s:24-100) cover every register plus ELR, SPSR and SP_EL0, masking before ELR/SPSR restore. switch.s, prepare_kernel_entry and start_user_at are correct. TPIDR_EL0 is per task, and the EL0 trap frame is at kstack_top - FRAME_SIZE.
- **Signal frame:** matches Linux arm64 (uc_mcontext at 176, reserved area at 288, FPSIMD record, terminator). Alternate-stack placement and read-back are correct (signal_frame.rs:189-322). rt_sigreturn keeps only NZCV and masks FPCR/FPSR.
- **uaccess:** copies one page at a time through TTBR0 with interrupts off (uaccess.rs:91-247), apart from finding 10. read_struct targets are plain integers.
- **Syscall output sizes match Go:** stat 128, statfs 120, sigaction 32, stack_t 24, epoll_event 16, rusage 144, rlimit 16, utsname 390, plus msghdr, pollfd and pipe2.
- **Receive path:** never writes more than the user length (syscall/file.rs:96-108, syscall/net.rs:371-404). tcp.rs, ip.rs, udp.rs, socket.rs, syscall/net.rs, fs/pipe.rs, fs/chan.rs and the WiFi protocol and SDIO code contain no unsafe.
- **Zeroing:** every user frame is zeroed or fully overwritten (FreshPage::new → alloc_zeroed; publish_table; new_user; handle_cow copies 4096 bytes). The kernel image and .bss, the DTB, memreserve entries, the initramfs and physical 0..0x80000 are reserved.
- **TLB:** unmap, replace, take_table, free_user_memory and share_page invalidate before release. write_ttbr flushes all on every switch; no ASIDs are used.
- **Reference counts on single-threaded paths:** handle_cow, share_page, free_table and the fork error paths are consistent. exit_current frees user memory only when mm_shares() == 1.
- **Task lifetime:** release never frees CURRENT or idle. switch_to is masked.
- **Other unsafe checked:** Spinlock; SleepLock (CAS on `held`); the heap HoleList under its lock; the frame bitmap; MMIO for the GIC, UART, SDHCI, mailbox, GPIO and EMMC2; the mailbox page; GENET buffers (not probed with net=wifi); FDT/ATAGS reads; the initramfs and integrity slices; rng::wipe; hwcap reads; HANDLERS, OUTSTANDING and TIMER_* statics; CURRENT/IDLE writes.
- **madvise no-op:** Go decides zeroing from its own record of used arena bytes, so the no-op alone does not put non-zero bytes into fresh objects.
- **Inventory:** clippy (undocumented_unsafe_blocks, unsafe_op_in_unsafe_fn; aarch64, kernel-dev) reports 170 undocumented unsafe blocks, 8 unsafe impls and 119 unsafe operations inside unsafe fns. All were read.

## Could not check

- The Go-side step from zeroed pages to random bytes (findings 1 and 2); the Go 1.26.8 runtime source was not available.
- **Kernel stack depth:** stacks are 32 KiB heap blocks with no guard page (task.rs:28, 302). An overflow would silently overwrite neighbouring heap memory.
- **The board's /reserved-memory node:** the FDT parser reads only memreserve and memory nodes (fdt.rs:103-200).
- Whether firmware leaves a bus-master device active (the PCIe VL805 USB controller, or GENET), since neither is reset with net=wifi.
- How often the windows in findings 1, 2, 4 and 5 are hit on the board.
- arch/x86_64/paging.rs line by line.
- Logic errors in safe-Rust network, filesystem and driver code.

## Miri follow-up (not set up)

- **Runs now:** `cargo +nightly miri test -p fatdisk` covers kernel/src/storage/fat through tools/fatdisk/src/fat.
- **Needs Vec-backed host wrappers:** the heap HoleList, the frame BitmapAllocator, signal-frame encode/decode with FpuState::vectors, and the ELF, cpio and FDT parsers.
- **Catches there:** aliasing under Stacked/Tree Borrows, out-of-bounds access, uninitialised reads, invalid values.
- **Can't reach:** asm!, the page-table code, MMIO or interrupt preemption, so none of findings 1-10.
