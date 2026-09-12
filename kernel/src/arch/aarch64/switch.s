/* Kernel-mode context switch.
 *
 * switch_context(u64 *save_sp, u64 new_sp)
 *
 * Saves the callee-saved registers of the outgoing task on its own kernel
 * stack, records the stack pointer, then resumes the incoming task from the
 * mirror-image frame on its stack. The processor state is not saved: it is
 * the caller's business, and both sides run with interrupts off.
 */
.section .text, "ax"
.global switch_context
switch_context:
    sub  sp, sp, #96
    stp  x19, x20, [sp, #0]
    stp  x21, x22, [sp, #16]
    stp  x23, x24, [sp, #32]
    stp  x25, x26, [sp, #48]
    stp  x27, x28, [sp, #64]
    stp  x29, x30, [sp, #80]
    mov  x9, sp
    str  x9, [x0]
    mov  sp, x1
    ldp  x19, x20, [sp, #0]
    ldp  x21, x22, [sp, #16]
    ldp  x23, x24, [sp, #32]
    ldp  x25, x26, [sp, #48]
    ldp  x27, x28, [sp, #64]
    ldp  x29, x30, [sp, #80]
    add  sp, sp, #96
    ret
