# Memory-safety plan

Why: cloudflared on the Raspberry Pi crashed four times, each time because a Go runtime object held random data where the runtime keeps a pointer or nil (BACKLOG.md, "Incident 2026-09-16"). A read-only audit of the kernel's unsafe code against Rust's rules found the likely causes (docs/audit-2026-09-18-unsafe.md; its findings are summarised below). The goal of this plan is a kernel that does not corrupt user memory, with the rules that prevent it enforced by Rust's types rather than by care.

## What the audit found

- **Nothing owns a page table, nothing locks one, and no type says a descriptor holds a frame reference.**
  - `AddressSpace { pub root: u64 }` derives `Copy`, and every `Task` keeps a copy in `space: Cell<AddressSpace>`, so threads share page tables only by holding identical values.
  - Regions and heap bounds are shared as `Arc<Spinlock<MemState>>`, but the page tables are not behind that lock.
  - `Frame(u64)` releases on `Drop`, yet page tables store raw `u64` descriptors, and `Frame::from_recorded` turns them back into owners unchecked.
- **Page-table and region updates are check-then-store sequences run with interrupts on,** in a kernel that can be preempted in kernel mode. A sibling thread of the same process can run in the gap:
  1. Two threads creating the same page table: the second store overwrites the first, and the loser's pages read back as zero (paging.rs `entry_for`/`publish_table`).
  2. `map`: the presence check and the store are split by a preemption point.
  4. `reclaim_tables` frees a table while another task holds a pointer into it.
  5. `mprotect`/`set_flags` race an unmap or a copy-on-write break.
  6. The fork walk reads the parent's tables unmasked.
  8. A page fault racing `munmap` leaves a page outside any region.
  9. `mmap`'s range check and region insert are separate.
- **Other findings:**
  3. `release` can destroy one address space twice (and exec has the same gap).
  7. A fork-shared read-only page can become writable without copy-on-write.
  10. User-copy critical sections can sleep when a page comes from /data.
  11. Threads start with no signals blocked; Linux copies the parent's mask.
- **Rule violations with no wrong bytes today:**
  - two live `&mut TrapFrame`;
  - `Cell` fields shared across tasks through `unsafe impl Send`;
  - safe functions that forge or double-free address spaces;
  - 16 `static mut`;
  - 170 undocumented unsafe blocks.
- **Not checked by the audit:**
  - kernel stacks have no guard page;
  - the device tree's `/reserved-memory` is not parsed;
  - the logic of the FAT, SD card and SDHCI code, which now supplies programs' pages from /data.

## Rules and the Rust mechanism that enforces each

| Rule | Mechanism |
|---|---|
| A frame or page table has exactly one owner and is released exactly once | Ownership types: no `Copy`, private fields, `Drop` releases, APIs that consume or return ownership, `#[must_use]` |
| No page-table or region change interleaves with another's | Mutation methods exist only on the address space's lock guard |
| A frame is released only after its TLB entry is flushed | Type-state: unmapping returns `Stale<Frame>`, which becomes a droppable `Frame` only through the guard's flush |
| Nothing sleeps while interrupts are masked | Capability token: sleeping functions take `&mut Preempt`, which a masked guard borrows, so the borrow checker refuses the call. `might_sleep()` checks at run time where the token is not yet threaded. |
| Shared task state is really thread-safe | No `unsafe impl Send/Sync` without proof; shared parts are atomics or locks; per-thread state is reachable only through a `!Send` handle to the current task |
| One `&mut TrapFrame` at a time | The trap entry owns it and passes it down |
| `unsafe` is confined and justified | Lints as a gate: `unsafe_op_in_unsafe_fn`, `clippy::undocumented_unsafe_blocks`, `clippy::missing_safety_doc`; unsafe lives in small modules with private fields and sound safe APIs |

What Rust cannot check stays in a few small reviewed `unsafe` leaf modules: assembly, MMU and TLB semantics, device registers.

## Decisions

1. **Locks are real spinlocks that also mask interrupts,** so the design stays correct if the other three Cortex-A72 cores are brought up.
2. **The `Preempt` token goes first on the paths that matter:** uaccess, page faults, storage (`SleepLock`, `sleep_ms`), `schedule`. `might_sleep()` runtime checks cover everything else.
3. **A regression test that reproduces the races comes before the fixes.**
4. **Testing:** targeted boots during work; both full suites (`./scripts/test.sh`, `ARCH=aarch64 ./scripts/test.sh`) at the end of every phase.
5. **After the core fix (phase 2), the board runs for several days** to confirm the cloudflared crashes stop.

## Phases

Each phase is its own branch, merged after its gates pass.

### A. Reproduce first
- **Debug kernel option** (for example `preempt=stress`): force a task switch at every frame allocation, page-table publish, user fault and uaccess validate, to hit the audit's windows on purpose.
- **Rust suite check** (`init=/bin/rtest`): several threads concurrently map, fault, write and verify patterns, `munmap`, `mprotect`, and exit and reap.
- **Gate:** the check fails on today's main with the option on (showing findings 1–3 are real) and passes with the option off. It becomes the acceptance test for phase B.

### B1. One owner per address space (finding 3; foundation)
- **`Mm`:** owns the page tables and today's `MemState` behind one lock. Tasks hold `Arc<Mm>`, replacing `space: Cell<AddressSpace>` and `mm`.
- **`PageTables`:** has private fields, is not `Copy`, and its `Drop` frees user memory and the root. The last `Arc<Mm>` drop destroys it, which removes `release`'s `space_in_use` scan and the exec gap.
- **Active mm:** the CPU keeps an `Arc` to the `Mm` loaded in TTBR0, so it cannot be freed while loaded.
- **Close the unsound safe APIs:** `destroy`, `free_user_memory` and `map_fixed` are private; `TaskPtr`'s field is private and `get` is `unsafe`.

### B2. Every page-table and region change through the guard (findings 1, 2, 4, 5, 6, 8, 9)
- **Guard-only API:** `mm.lock()` returns `MmGuard`. `map`, `unmap`, `protect`, region lookup, insert and remove, and the fork walk are guard methods only.
- **Typed entries:**
  - `set_leaf(va, Frame)` returns `Err(Occupied(frame))` rather than overwrite;
  - unmapping and copy-on-write replacement return `#[must_use] Stale<Frame>`;
  - the guard flushes before a stale frame can be dropped.
- **Prepare outside, commit inside:**
  - Allocation, zeroing and file reads happen before locking, producing `PreparedPage` and reserved table frames.
  - `guard.commit_fault(va, prepared)` re-checks the region and presence, then stores or hands the page back.
  - Masked sections are pointer walks and stores only.
- **Atomic system calls:** `mmap` (check and insert), `munmap`, `mremap`, `brk` and `mprotect` each run under one guard.
- **Host tests and Miri:** physical memory access goes behind a `PhysMem` trait (the direct map in the kernel, a `Vec` on the host), so page tables, frames and `Mm` run in `cargo test` and under Miri.
- **Gate:** phase A's check passes with `preempt=stress`, and both full suites pass.

### C. No sleeping while masked (finding 10)
- **uaccess:** faults pages in before masking. Inside the masked section it only checks; a missing page returns to fault in again.
- **`Preempt` token:** created at syscall and fault entry, required by `SleepLock::lock`, `sleep_ms` and `schedule`, and borrowed by masked guards.
- **Runtime checks:** `might_sleep()` in every sleeping function, and an "interrupts masked" assertion in page-table and frame internals.

### D. Shared versus per-thread task state (Send/Sync, TrapFrame, finding 11)
- **Split `Task`:**
  - shared parts are atomics, locks, `Arc<Mm>`, and `Arc<Spinlock<SigHand>>` for signal dispositions, shared under `CLONE_SIGHAND`;
  - per-thread parts are reachable only through `CurrentTask<'_>`.
  - Remove `unsafe impl Send for Task`.
- **TrapFrame:** only the trap entry holds `&mut TrapFrame`; it passes it into `check_signals` and exec.
- **Signal mask:** `fork` and `clone` copy the blocked set.

### E. Logic fixes and defences
- **Copy-on-write writability:** decided from the region's protection and the frame's reference count, under the guard (finding 7).
- **File mappings on /data:** hold an open file, not a path. Writes to a running executable return `ETXTBSY`.
- **Kernel stacks:** mapped with an unmapped guard page, so an overflow faults instead of overwriting the heap.
- **Reserved memory:** `/reserved-memory` is parsed from the device tree and reserved.
- Independent of B, so it can run beside it.

### F. Lints to deny
- **Lints:** the three lints become errors, with a `// SAFETY:` comment on every remaining unsafe block and `# Safety` docs on every unsafe function.
- **`static mut`:** all replaced.
- **Inventory:** the unsafe blocks remaining, by module, are recorded here.

## Order

1. **A and B1** in parallel. B1 merges A once A lands.
2. **B2**, then **C**, then **D**. **E's** reserved-memory and stack guard items can run beside B.
3. **F** last.
4. **After B2:** rewrite the card's boot partition and watch the board for several days.
