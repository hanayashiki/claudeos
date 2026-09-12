ENTRY(_start)

KERNEL_VMA = 0xFFFFFFFF80000000;

SECTIONS
{
    /* Where the firmware drops an AArch64 kernel image on this board. */
    . = 0x80000;
    __phys_start = .;

    /* Runs with translation off, so it is linked at the address it is loaded
     * at: the entry path, the boot stack and the boot page tables. */
    .boot : ALIGN(4K)
    {
        KEEP(*(.text.boot))
        KEEP(*(.boot))
        . = ALIGN(4K);
    }

    . = ALIGN(64K);
    __higher_half_load = .;
    . = . + KERNEL_VMA;

    .text : AT(ADDR(.text) - KERNEL_VMA) ALIGN(4K)
    {
        __text_start = .;
        *(.text .text.*)
        . = ALIGN(4K);
        __text_end = .;
    }

    .rodata : AT(ADDR(.rodata) - KERNEL_VMA) ALIGN(4K)
    {
        __rodata_start = .;
        *(.rodata .rodata.*)
        . = ALIGN(4K);
        __rodata_end = .;
    }

    .data : AT(ADDR(.data) - KERNEL_VMA) ALIGN(4K)
    {
        __data_start = .;
        *(.data .data.*)
        *(.got .got.plt)
        . = ALIGN(4K);
        __data_end = .;
    }

    .bss : AT(ADDR(.bss) - KERNEL_VMA) ALIGN(4K)
    {
        __bss_start = .;
        *(COMMON)
        *(.bss .bss.*)
        . = ALIGN(4K);
        __bss_end = .;
    }

    __kernel_end_virt = .;
    __kernel_end_phys = . - KERNEL_VMA;

    /DISCARD/ :
    {
        *(.eh_frame)
        *(.eh_frame_hdr)
        *(.note .note.*)
        *(.comment)
    }
}
