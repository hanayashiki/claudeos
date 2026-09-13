/* SYSCALL entry.
 *
 * On entry the CPU has put the return address in RCX and the caller's RFLAGS
 * in R11, and has left RSP pointing at the user stack. The frame built here
 * matches `TrapFrame`, so a syscall and an interrupt look the same to the
 * rest of the kernel and both can return through IRETQ.
 *
 * Every offset into the frame, every offset into the per-CPU block reached
 * through GS, and both user selectors come in as constants from the Rust that
 * defines them. None of them is written out here, so a field that moves or a
 * selector that changes moves the instruction that uses it.
 */
.section .text, "ax", @progbits
.code64
.global syscall_entry
.type syscall_entry, @function
syscall_entry:
    swapgs
    movq %rsp, %gs:{PER_CPU_USER_RSP}   /* stash the user stack pointer */
    movq %gs:{PER_CPU_KERNEL_RSP}, %rsp /* switch to this task's kernel stack */

    subq ${FRAME_SIZE}, %rsp      /* room for the whole TrapFrame */

    movq %rax, {OFF_RAX}(%rsp)
    movq %rbx, {OFF_RBX}(%rsp)
    movq %rcx, {OFF_RCX}(%rsp)
    movq %rdx, {OFF_RDX}(%rsp)
    movq %rsi, {OFF_RSI}(%rsp)
    movq %rdi, {OFF_RDI}(%rsp)
    movq %rbp, {OFF_RBP}(%rsp)
    movq %r8,  {OFF_R8}(%rsp)
    movq %r9,  {OFF_R9}(%rsp)
    movq %r10, {OFF_R10}(%rsp)
    movq %r11, {OFF_R11}(%rsp)
    movq %r12, {OFF_R12}(%rsp)
    movq %r13, {OFF_R13}(%rsp)
    movq %r14, {OFF_R14}(%rsp)
    movq %r15, {OFF_R15}(%rsp)

    movq ${VECTOR_SYSCALL}, {OFF_VECTOR}(%rsp)
    movq $0, {OFF_ERROR_CODE}(%rsp)
    movq %rcx, {OFF_RIP}(%rsp)
    movq ${USER_CS}, {OFF_CS}(%rsp)
    movq %r11, {OFF_RFLAGS}(%rsp)
    movq %gs:{PER_CPU_USER_RSP}, %rax
    movq %rax, {OFF_RSP}(%rsp)
    movq ${USER_SS}, {OFF_SS}(%rsp)

    cld
    /* SYSCALL masks IF on entry. The kernel stack and the full frame are in
     * place now, so interrupts can be taken again; without this a syscall that
     * waits for input would block the timer and the device interrupts it is
     * waiting on. */
    sti
    movq %rsp, %rdi
    call syscall_dispatch

    cli
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
    /* Drop the vector and the error code, which is everything between the last
     * register popped and what the CPU pushed. */
    addq $({OFF_RIP} - {OFF_VECTOR}), %rsp
    swapgs
    iretq
