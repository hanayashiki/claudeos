/* SYSCALL entry.
 *
 * On entry the CPU has put the return address in RCX and the caller's RFLAGS
 * in R11, and has left RSP pointing at the user stack. The frame built here
 * matches `TrapFrame`, so a syscall and an interrupt look the same to the
 * rest of the kernel and both can return through IRETQ.
 */
.section .text, "ax", @progbits
.code64
.global syscall_entry
.type syscall_entry, @function
syscall_entry:
    swapgs
    movq %rsp, %gs:8              /* stash the user stack pointer */
    movq %gs:0, %rsp              /* switch to this task's kernel stack */

    subq $176, %rsp               /* room for the whole TrapFrame */

    movq %rax, 0(%rsp)
    movq %rbx, 8(%rsp)
    movq %rcx, 16(%rsp)
    movq %rdx, 24(%rsp)
    movq %rsi, 32(%rsp)
    movq %rdi, 40(%rsp)
    movq %rbp, 48(%rsp)
    movq %r8,  56(%rsp)
    movq %r9,  64(%rsp)
    movq %r10, 72(%rsp)
    movq %r11, 80(%rsp)
    movq %r12, 88(%rsp)
    movq %r13, 96(%rsp)
    movq %r14, 104(%rsp)
    movq %r15, 112(%rsp)

    movq $0x100, 120(%rsp)        /* synthetic vector: "syscall" */
    movq $0, 128(%rsp)            /* no error code */
    movq %rcx, 136(%rsp)          /* rip */
    movq $0x23, 144(%rsp)         /* user cs */
    movq %r11, 152(%rsp)          /* rflags */
    movq %gs:8, %rax
    movq %rax, 160(%rsp)          /* user rsp */
    movq $0x1b, 168(%rsp)         /* user ss */

    cld
    movq %rsp, %rdi
    call syscall_dispatch

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
    addq $16, %rsp                /* drop vector and error code */
    swapgs
    iretq
