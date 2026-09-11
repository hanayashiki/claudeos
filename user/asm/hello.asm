; Smallest possible Linux program: write a line and exit.
bits 64
default rel

section .text
global _start
_start:
    mov     rax, 1              ; SYS_write
    mov     rdi, 1              ; stdout
    lea     rsi, [msg]
    mov     rdx, msg_len
    syscall

    mov     rax, 39             ; SYS_getpid
    syscall

    mov     rax, 60             ; SYS_exit
    mov     rdi, 7
    syscall

section .rodata
msg:     db "hello from ring 3", 10
msg_len: equ $ - msg
