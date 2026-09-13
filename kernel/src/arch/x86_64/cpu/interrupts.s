/* Interrupt stubs.
 *
 * Every vector gets a stub that normalises the CPU's frame (pushing a zero
 * error code where the CPU does not push one) and appends the vector number,
 * then falls into a common path that saves the general-purpose registers.
 * The resulting layout is `TrapFrame` in idt.rs, whose offsets come in here as
 * constants rather than being written out: the saved code selector in
 * particular decides whether GS is swapped, and reading the wrong word leaves
 * the swap happening an odd number of times, after which the kernel runs on
 * the user's GS base and the next system call takes its stack pointer from
 * memory the program chose.
 */

.altmacro

.section .text, "ax", @progbits
.code64

.macro ISR_STUB num
.global isr_stub_\num
.type isr_stub_\num, @function
isr_stub_\num:
    .if (\num == 8 || \num == 10 || \num == 11 || \num == 12 || \num == 13 || \num == 14 || \num == 17 || \num == 21 || \num == 29 || \num == 30)
    /* The CPU has already pushed an error code. */
    .else
    pushq $0
    .endif
    pushq $\num
    jmp isr_common
.endm

.set vec, 0
.rept 256
    ISR_STUB %vec
    .set vec, vec + 1
.endr

.macro TABLE_ENTRY num
    .quad isr_stub_\num
.endm

.section .rodata, "a", @progbits
.align 16
.global isr_stub_table
isr_stub_table:
.set vec, 0
.rept 256
    TABLE_ENTRY %vec
    .set vec, vec + 1
.endr

.section .text, "ax", @progbits
.code64
.type isr_common, @function
isr_common:
    pushq %r15
    pushq %r14
    pushq %r13
    pushq %r12
    pushq %r11
    pushq %r10
    pushq %r9
    pushq %r8
    pushq %rbp
    pushq %rdi
    pushq %rsi
    pushq %rdx
    pushq %rcx
    pushq %rbx
    pushq %rax

    /* Ring 3 in the saved code selector means we must swap in kernel GS. */
    movq {OFF_CS}(%rsp), %rax
    andq $3, %rax
    jz 1f
    swapgs
1:
    cld
    movq %rsp, %rdi
    call interrupt_dispatch

    movq {OFF_CS}(%rsp), %rax
    andq $3, %rax
    jz 2f
    swapgs
2:
    popq %rax
    popq %rbx
    popq %rcx
    popq %rdx
    popq %rsi
    popq %rdi
    popq %rbp
    popq %r8
    popq %r9
    popq %r10
    popq %r11
    popq %r12
    popq %r13
    popq %r14
    popq %r15

    /* Discard the vector and the error code, which is everything between the
     * last register popped and what the CPU pushed. */
    addq $({OFF_RIP} - {OFF_VECTOR}), %rsp
    iretq

/* Enter user mode with a fully populated TrapFrame in RDI. Never returns. */
.global enter_user_mode
.type enter_user_mode, @function
enter_user_mode:
    movq %rdi, %rsp
    swapgs
    popq %rax
    popq %rbx
    popq %rcx
    popq %rdx
    popq %rsi
    popq %rdi
    popq %rbp
    popq %r8
    popq %r9
    popq %r10
    popq %r11
    popq %r12
    popq %r13
    popq %r14
    popq %r15
    addq $({OFF_RIP} - {OFF_VECTOR}), %rsp
    iretq
