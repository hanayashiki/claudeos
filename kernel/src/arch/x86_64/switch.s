/* Kernel-mode context switch.
 *
 * switch_context(u64 *save_rsp, u64 new_rsp)
 *
 * Saves the callee-saved registers and flags of the outgoing task on its own
 * kernel stack, records the stack pointer, then resumes the incoming task
 * from the mirror-image frame on its stack.
 *
 * The frame is built by pushes, so it names no offsets; what it does have is a
 * word count, which task.rs lays the same frame out by hand from. The check
 * below is what ties the two together.
 */
.section .text, "ax", @progbits
.code64

.if {FRAME_WORDS} != 8
.error "switch.s saves seven words and returns to an eighth; task.rs disagrees"
.endif
.global switch_context
.type switch_context, @function
switch_context:
    pushq %rbp
    pushq %rbx
    pushq %r12
    pushq %r13
    pushq %r14
    pushq %r15
    pushfq
    movq %rsp, (%rdi)
    movq %rsi, %rsp
    popfq
    popq %r15
    popq %r14
    popq %r13
    popq %r12
    popq %rbx
    popq %rbp
    ret
