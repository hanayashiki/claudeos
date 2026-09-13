/* AArch64 entry: down to EL1, page tables built by hand, then the higher half.
 *
 * The board hands control at whatever exception level it likes. The emulated
 * Pi 4 starts at EL3; a real board's firmware stub has already stepped down to
 * EL2 by the time it jumps here. So the entry code reads where it is and steps
 * down from there, configuring each level on the way out of it.
 *
 * x0 holds the firmware handoff, which on anything following the Linux
 * AArch64 boot protocol is the physical address of a flattened device tree.
 * It is moved out of the way at the first instruction, because the core
 * selection below needs x0 and only the first core gets any further.
 */

/* The sixty-four byte header an AArch64 Linux image carries, so that a loader
 * can tell what it has been handed. A loader that does not find the magic at
 * offset 56 does not believe this is a kernel and boots it some other way, or
 * not at all. The first two words are executable, because the header is also
 * the entry point: the loader jumps to offset zero and the branch steps over
 * the rest of it.
 */
.section .text.header, "ax"
.global _image_start
_image_start:
    b    _start                  /* code0 */
    .long 0                      /* code1 */
    .quad __phys_start           /* offset from the base of RAM, which is 0 */
    .quad __image_size           /* including the space .bss needs */
    .quad 2                      /* little-endian, 4 KiB pages, fixed place */
    .quad 0                      /* reserved */
    .quad 0
    .quad 0
    .long 0x644d5241             /* "ARM\x64" */
    .long 0                      /* reserved */

/* Which 2 MiB block of the fourth gigabyte is the first that is registers
 * rather than memory. The peripherals on this chip start at 0xFC000000, which
 * is what DEVICE_PHYS_BASE in mod.rs says as well; the two have to agree,
 * because the allocator hands out every frame below it. */
.equ FIRST_DEVICE_BLOCK, (0xFC000000 - 0xC0000000) / 0x200000

.section .text.boot, "ax"
.global _start
_start:
    mov  x19, x0

    mrs  x0, mpidr_el1
    and  x0, x0, #3
    cbz  x0, 1f
0:  wfe
    b    0b

1:  ldr  x0, =boot_stack_top
    mov  sp, x0

    mrs  x0, CurrentEL
    lsr  x0, x0, #2
    and  x0, x0, #3
    cmp  x0, #3
    b.eq 3f
    cmp  x0, #2
    b.eq 2f
    b    4f

    /* EL3: say that the level below runs 64-bit and is non-secure, then drop
     * into EL2 with interrupts masked. */
3:  mov  x0, #0x531              /* NS | IRQ | FIQ | EA | RW | HCE */
    msr  scr_el3, x0
    mov  x0, #0x3c9              /* EL2h, all four masks set */
    msr  spsr_el3, x0
    adr  x0, 2f
    msr  elr_el3, x0
    eret

    /* EL2: the level below runs 64-bit; let it read the timer directly and
     * start that timer from zero; then drop into EL1. */
2:  mov  x0, #(1 << 31)          /* RW: EL1 is AArch64 */
    msr  hcr_el2, x0
    mrs  x0, cnthctl_el2
    orr  x0, x0, #3              /* EL1 may use the physical counter and timer */
    msr  cnthctl_el2, x0
    msr  cntvoff_el2, xzr
    mov  x0, #0x3c5              /* EL1h, all four masks set */
    msr  spsr_el2, x0
    adr  x0, 4f
    msr  elr_el2, x0
    ldr  x0, =boot_stack_top
    msr  sp_el1, x0
    eret

    /* EL1. Give floating point to this level and the one below, build the
     * tables, turn translation on, and leave for the higher half. */
4:  ldr  x0, =boot_stack_top
    mov  sp, x0
    mov  x0, #(3 << 20)          /* FPEN: no trap on vector or floating point */
    msr  cpacr_el1, x0
    isb

    bl   build_page_tables
    bl   enable_mmu

    ldr  x0, =higher_half
    br   x0

/* The low four gigabytes, reachable three ways: where they physically are,
 * through the direct map, and through the kernel's own base. The first two
 * hold the same second-level table, because the direct map is the identity map
 * moved up; the third is a separate one so that the kernel's base can be an
 * alias of physical zero.
 */
build_page_tables:
    /* Zero the four tables. */
    ldr  x0, =level0
    mov  x1, #(4 * 4096 / 8)
5:  str  xzr, [x0], #8
    subs x1, x1, #1
    b.ne 5b

    ldr  x0, =level0
    ldr  x1, =level1_low
    orr  x2, x1, #3              /* valid, and a table rather than a block */
    str  x2, [x0]                /* 0x0000000000000000: identity */
    str  x2, [x0, #(256 * 8)]    /* 0xFFFF800000000000: the direct map */
    ldr  x1, =level1_high
    orr  x2, x1, #3
    str  x2, [x0, #(511 * 8)]

    /* Gigabytes 0..2 are memory, one block each. */
    ldr  x0, =level1_low
    mov  x1, xzr                 /* physical address of the block */
    mov  x2, xzr                 /* index */
    mov  x4, #0x4000
    lsl  x4, x4, #16             /* one gigabyte, too wide for an immediate */
6:  mov  x3, #0x701              /* valid block | inner shareable | accessed */
    orr  x3, x3, x1
    str  x3, [x0, x2, lsl #3]
    add  x1, x1, x4
    add  x2, x2, #1
    cmp  x2, #3
    b.lo 6b

    /* Gigabyte 3 is both: memory up to the peripheral base and registers from
     * there to the top. A block per two megabytes is what lets the two halves
     * carry different attributes. One block for the whole gigabyte would make
     * the memory under the peripherals device memory as well, and a 4 GiB
     * board reports that memory as ordinary RAM, so the frame allocator would
     * hand out frames whose only kernel-side alias is device memory. */
    ldr  x1, =level2_dev
    orr  x3, x1, #3              /* valid, and a table rather than a block */
    str  x3, [x0, #(3 * 8)]

    mov  x0, x1
    mov  x1, #0xC000
    lsl  x1, x1, #16             /* three gigabytes: the first address covered */
    mov  x2, xzr
    mov  x4, #0x20
    lsl  x4, x4, #16             /* two megabytes */
7:  mov  x3, #0x701              /* valid block | inner shareable | accessed */
    cmp  x2, #FIRST_DEVICE_BLOCK
    b.lo 8f
    mov  x3, #0x405              /* valid block | accessed | device attributes */
    movk x3, #0x60, lsl #48      /* never execute, from either level */
8:  orr  x3, x3, x1
    str  x3, [x0, x2, lsl #3]
    add  x1, x1, x4
    add  x2, x2, #1
    cmp  x2, #512
    b.lo 7b

    /* The kernel's base is an alias of physical zero: index 510 of the table
     * under the topmost entry is where 0xFFFFFFFF80000000 lands. */
    ldr  x0, =level1_high
    mov  x3, #0x701
    str  x3, [x0, #(510 * 8)]
    ret

enable_mmu:
    /* Nothing the firmware left in the translation buffers or the instruction
     * cache is ours, and on the board it will not be what the emulator leaves.
     * Throw both away before the tables built above start being used.
     *
     * The first barrier is what makes those tables visible to the walkers.
     * They were written with translation off, which makes them ordinary
     * uncached stores, and the walkers are separate observers of memory. */
    dsb  ishst
    tlbi vmalle1
    ic   iallu
    dsb  nsh
    isb

    /* Attribute slot zero: ordinary memory, written back, allocating on both
     * read and write. Slot one: device, with no gathering, reordering or
     * early acknowledgement. */
    mov  x0, #0xFF
    msr  mair_el1, x0
    ldr  x0, =0x2B5103510        /* 48-bit through both bases, 4 KiB pages */
    msr  tcr_el1, x0
    ldr  x0, =level0
    msr  ttbr0_el1, x0
    msr  ttbr1_el1, x0
    isb
    mrs  x0, sctlr_el1
    orr  x0, x0, #(1 << 0)       /* translation on */
    orr  x0, x0, #(1 << 2)       /* data cache on */
    orr  x0, x0, #(1 << 12)      /* instruction cache on */
    msr  sctlr_el1, x0
    dsb  sy
    isb
    ret

.ltorg

.balign 4096
boot_stack_bottom:
    .space 16384
boot_stack_top:

.balign 4096
level0:
    .space 4096
level1_low:
    .space 4096
level1_high:
    .space 4096
level2_dev:
    .space 4096

.section .text, "ax"
higher_half:
    ldr  x0, =kernel_stack_top
    mov  sp, x0

    /* Zero .bss. The stack is inside it and nothing is on the stack yet. */
    ldr  x0, =__bss_start
    ldr  x1, =__bss_end
8:  cmp  x0, x1
    b.hs 9f
    str  xzr, [x0], #8
    b    8b

9:  mov  x0, x19
    mov  x29, xzr
    mov  x30, xzr
    bl   kmain
10: wfe
    b    10b

.ltorg

.section .bss, "aw", @nobits
.balign 4096
.global kernel_stack_bottom
kernel_stack_bottom:
    .space 65536
.global kernel_stack_top
kernel_stack_top:
