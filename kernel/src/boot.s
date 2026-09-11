/* Multiboot1 header + 32-bit trampoline into 64-bit long mode.
 * Entered by the boot loader in 32-bit protected mode, paging disabled.
 * EAX = multiboot magic, EBX = physical pointer to multiboot info.
 */

.set MB_MAGIC,    0x1BADB002
.set MB_FLAGS,    0x00000003          /* align modules on 4K, provide mem map */
.set MB_CHECKSUM, -(MB_MAGIC + MB_FLAGS)

.section .multiboot_header, "a", @progbits
.align 8
mb_header:
    .long MB_MAGIC
    .long MB_FLAGS
    .long MB_CHECKSUM

.section .boot, "awx", @progbits
.code32
.global _start
.type _start, @function
_start:
    cli
    cld
    movl $boot_stack_top, %esp

    /* Stash the multiboot handoff registers; the page-table code needs them. */
    movl %eax, mb_magic
    movl %ebx, mb_info

    /* Zero the seven boot page tables (7 * 4096 bytes). */
    movl $pml4, %edi
    xorl %eax, %eax
    movl $((7 * 4096) / 4), %ecx
    rep stosl

    /* PML4[0]   -> pdpt_lo   (identity map of the low 4 GiB) */
    movl $pdpt_lo, %eax
    orl  $0x03, %eax
    movl %eax, pml4 + 0

    /* PML4[256] -> pdpt_lo   (HHDM: 0xFFFF800000000000 + phys) */
    movl $pdpt_lo, %eax
    orl  $0x03, %eax
    movl %eax, pml4 + 256 * 8

    /* PML4[511] -> pdpt_hi   (higher half, 0xFFFFFFFF80000000) */
    movl $pdpt_hi, %eax
    orl  $0x03, %eax
    movl %eax, pml4 + 511 * 8

    /* pdpt_lo[0..3] -> pd0..pd3 */
    movl $pd0, %eax
    orl  $0x03, %eax
    movl %eax, pdpt_lo + 0 * 8
    movl $pd1, %eax
    orl  $0x03, %eax
    movl %eax, pdpt_lo + 1 * 8
    movl $pd2, %eax
    orl  $0x03, %eax
    movl %eax, pdpt_lo + 2 * 8
    movl $pd3, %eax
    orl  $0x03, %eax
    movl %eax, pdpt_lo + 3 * 8

    /* pdpt_hi[510] -> pd0: kernel virtual base aliases physical 0. */
    movl $pd0, %eax
    orl  $0x03, %eax
    movl %eax, pdpt_hi + 510 * 8

    /* Fill pd0..pd3 with 2 MiB pages covering physical 0 .. 4 GiB. */
    movl $pd0, %edi
    xorl %eax, %eax
    xorl %esi, %esi
1:
    movl %eax, %edx
    orl  $0x83, %edx              /* present | writable | page size (2 MiB) */
    movl %edx, 0(%edi)
    movl $0, 4(%edi)
    addl $0x200000, %eax
    addl $8, %edi
    incl %esi
    cmpl $2048, %esi
    jb 1b

    /* CR3 = pml4 */
    movl $pml4, %eax
    movl %eax, %cr3

    /* CR4: PAE | PGE */
    movl %cr4, %eax
    orl  $((1 << 5) | (1 << 7)), %eax
    movl %eax, %cr4

    /* EFER: SCE | LME | NXE */
    movl $0xC0000080, %ecx
    rdmsr
    orl  $((1 << 0) | (1 << 8) | (1 << 11)), %eax
    wrmsr

    /* CR0: PE | WP | PG */
    movl %cr0, %eax
    orl  $((1 << 0) | (1 << 16) | (1 << 31)), %eax
    movl %eax, %cr0

    lgdt gdt64_ptr
    ljmp $0x08, $long_mode_entry

.code64
long_mode_entry:
    movw $0x10, %ax
    movw %ax, %ds
    movw %ax, %es
    movw %ax, %ss
    movw %ax, %fs
    movw %ax, %gs

    /* Still executing from the identity map, so RIP-relative loads reach
     * these low addresses. Put the multiboot handoff into the SysV argument
     * registers before leaving for the higher half. */
    movl mb_info(%rip), %edi
    movl mb_magic(%rip), %esi

    movabsq $higher_half, %rax
    jmp *%rax

.section .text, "ax", @progbits
.code64
higher_half:
    movabsq $kernel_stack_top, %rsp
    movq %rsp, %rbp

    /* Zero .bss defensively; do not clobber the handoff arguments. */
    movq %rdi, %r12
    movq %rsi, %r13
    movabsq $__bss_start, %rdi
    movabsq $__bss_end, %rcx
    subq %rdi, %rcx
    xorl %eax, %eax
    rep stosb
    movq %r12, %rdi
    movq %r13, %rsi

    /* Drop the identity mapping later from Rust; call the kernel now. */
    xorl %ebp, %ebp
    callq kmain

.Lhalt:
    cli
    hlt
    jmp .Lhalt

.section .boot, "awx", @progbits
.align 16
gdt64:
    .quad 0x0000000000000000      /* null */
    .quad 0x00AF9A000000FFFF      /* 0x08: 64-bit kernel code */
    .quad 0x00CF92000000FFFF      /* 0x10: kernel data */
gdt64_end:
gdt64_ptr:
    .word gdt64_end - gdt64 - 1
    .quad gdt64

.global mb_magic
mb_magic:
    .long 0
.global mb_info
mb_info:
    .long 0

.align 4096
boot_stack_bottom:
    .space 16384
boot_stack_top:

.align 4096
pml4:
    .space 4096
pdpt_lo:
    .space 4096
pdpt_hi:
    .space 4096
pd0:
    .space 4096
pd1:
    .space 4096
pd2:
    .space 4096
pd3:
    .space 4096

.section .bss, "aw", @nobits
.align 4096
.global kernel_stack_bottom
kernel_stack_bottom:
    .space 65536
.global kernel_stack_top
kernel_stack_top:
