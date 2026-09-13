/* Exception vectors.
 *
 * Sixteen slots of 128 bytes: four kinds of exception for each of four
 * origins. A slot is not room for saving the register state, so each one holds
 * a single branch to a body placed after the table. Overrunning a slot does
 * not fail loudly: it pushes every later entry out of place, and exceptions
 * quietly arrive at the wrong handler.
 *
 * Every body saves the whole register state before touching anything, because
 * the handler is ordinary compiled code and will use those registers for its
 * own purposes. Returning without putting them back leaves the interrupted
 * program running on someone else's values.
 */

/* The saved state, in the order a `TrapFrame` reads it: the thirty-one general
 * registers, the stack pointer of the level below, where to carry on and with
 * what processor state, what the exception was and what address it was about,
 * and last the two words the dispatcher fills in.
 */
.macro SAVE_STATE
    sub  sp, sp, #304
    stp  x0,  x1,  [sp, #16 * 0]
    stp  x2,  x3,  [sp, #16 * 1]
    stp  x4,  x5,  [sp, #16 * 2]
    stp  x6,  x7,  [sp, #16 * 3]
    stp  x8,  x9,  [sp, #16 * 4]
    stp  x10, x11, [sp, #16 * 5]
    stp  x12, x13, [sp, #16 * 6]
    stp  x14, x15, [sp, #16 * 7]
    stp  x16, x17, [sp, #16 * 8]
    stp  x18, x19, [sp, #16 * 9]
    stp  x20, x21, [sp, #16 * 10]
    stp  x22, x23, [sp, #16 * 11]
    stp  x24, x25, [sp, #16 * 12]
    stp  x26, x27, [sp, #16 * 13]
    stp  x28, x29, [sp, #16 * 14]
    mrs  x22, elr_el1
    mrs  x23, spsr_el1
    /* Where the stack pointer was when the exception was taken. An exception
     * from EL0 left it in SP_EL0; one from EL1 was taken on this stack, so it
     * is the address this frame ends at. Saving SP_EL0 for those names the
     * current task's user stack, which has nothing to do with a kernel fault,
     * and the fault report then prints it and dumps four lines of user memory
     * at an unrelated address -- on a board with nothing but a serial cable
     * that report is the whole of the evidence, and a kernel stack overflow
     * is exactly what it is needed for. */
    mrs  x21, sp_el0
    add  x24, sp, #304
    tst  x23, #0xf
    csel x21, x21, x24, eq
    stp  x30, x21, [sp, #16 * 15]
    stp  x22, x23, [sp, #16 * 16]
    mrs  x22, esr_el1
    mrs  x23, far_el1
    stp  x22, x23, [sp, #16 * 17]
.endm

.macro LOAD_STATE
    /* From here until the ERET reads them, where to carry on and with what
     * processor state live in ELR_EL1 and SPSR_EL1, and any exception taken in
     * between overwrites both with its own. The kernel is preemptible, so
     * without this mask a timer tick landing in this sequence leaves the ERET
     * returning to the instruction after the tick, at this level, with the
     * stack pointer already stepped past the frame -- and the next ERET does
     * it again, walking the stack pointer up through memory until it leaves
     * what is mapped. The ERET takes the processor state from SPSR_EL1, so
     * the mask does not outlive the return. */
    msr  daifset, #0xf
    ldp  x22, x23, [sp, #16 * 16]
    msr  elr_el1, x22
    msr  spsr_el1, x23
    ldp  x30, x21, [sp, #16 * 15]
    /* SP_EL0 belongs to the level below. A return to EL1 carries on with the
     * stack this frame sits on, and what the frame holds for it is that
     * stack's own address rather than something to install. */
    tst  x23, #0xf
    b.ne 1f
    msr  sp_el0, x21
1:
    ldp  x0,  x1,  [sp, #16 * 0]
    ldp  x2,  x3,  [sp, #16 * 1]
    ldp  x4,  x5,  [sp, #16 * 2]
    ldp  x6,  x7,  [sp, #16 * 3]
    ldp  x8,  x9,  [sp, #16 * 4]
    ldp  x10, x11, [sp, #16 * 5]
    ldp  x12, x13, [sp, #16 * 6]
    ldp  x14, x15, [sp, #16 * 7]
    ldp  x16, x17, [sp, #16 * 8]
    ldp  x18, x19, [sp, #16 * 9]
    ldp  x20, x21, [sp, #16 * 10]
    ldp  x22, x23, [sp, #16 * 11]
    ldp  x24, x25, [sp, #16 * 12]
    ldp  x26, x27, [sp, #16 * 13]
    ldp  x28, x29, [sp, #16 * 14]
    add  sp, sp, #304
.endm

.macro ENTRY n
.balign 128
    b    body_\n
.endm

.macro BODY n
body_\n:
    SAVE_STATE
    mov  x0, #\n
    str  x0, [sp, #(16 * 18 + 8)]
    str  xzr, [sp, #(16 * 18)]
    mov  x0, sp
    bl   exception_entry
    LOAD_STATE
    eret
.endm

.section .text, "ax"
.balign 2048
.global exception_vectors
exception_vectors:
    /* Current level on the low stack: synchronous, interrupt, fast interrupt,
     * system error. Then the same four for the current level on its own stack,
     * for the level below in 64-bit, and in 32-bit. */
    ENTRY 0
    ENTRY 1
    ENTRY 2
    ENTRY 3
    ENTRY 4
    ENTRY 5
    ENTRY 6
    ENTRY 7
    ENTRY 8
    ENTRY 9
    ENTRY 10
    ENTRY 11
    ENTRY 12
    ENTRY 13
    ENTRY 14
    ENTRY 15

    BODY 0
    BODY 1
    BODY 2
    BODY 3
    BODY 4
    BODY 5
    BODY 6
    BODY 7
    BODY 8
    BODY 9
    BODY 10
    BODY 11
    BODY 12
    BODY 13
    BODY 14
    BODY 15

/* Leave the kernel for user mode with the registers in the frame `x0` points
 * at. The frame is the topmost thing on the task's kernel stack, so moving the
 * stack pointer to it and unwinding is what a return through an exception
 * would have done anyway.
 */
.global enter_user_mode
enter_user_mode:
    mov  sp, x0
    LOAD_STATE
    eret
