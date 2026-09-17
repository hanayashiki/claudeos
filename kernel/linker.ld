ENTRY(_start)

KERNEL_VMA = 0xFFFFFFFF80000000;

SECTIONS
{
    . = 1M;
    __phys_start = .;

    /* Identity-mapped boot section: 32-bit trampoline, page tables, GDT. */
    .boot : ALIGN(4K)
    {
        KEEP(*(.multiboot_header))
        KEEP(*(.boot))
        . = ALIGN(4K);
    }

    . = ALIGN(2M);
    __higher_half_load = .;
    . = . + KERNEL_VMA;

    .text : AT(ADDR(.text) - KERNEL_VMA) ALIGN(4K)
    {
        __text_start = .;
        /* The bytes the boot-time integrity check hashes run from here to
         * __integrity_end at the end of .rodata: the code and the read-only
         * data, which do not change once the kernel is loaded. The kernel
         * hashes them in memory through these two symbols, and
         * tools/distro/src/checksums.rs reads the same two symbols out of the
         * ELF to hash the same bytes at build time. */
        __integrity_start = .;
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
        __integrity_end = .;
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
