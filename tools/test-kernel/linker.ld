/*
 * Linker script para el test kernel del NKR.
 *
 * Ubica el código en GPA 0x100000 (1 MiB), por encima de la zona baja
 * usada por el VMM para page tables (0x9000-0xBFFF) y GDT (0x500).
 * El entry point es _start.
 */

ENTRY(_start)

SECTIONS
{
    . = 0x100000;

    .text : {
        *(.text)
    }

    .rodata : {
        *(.rodata)
    }

    /DISCARD/ : {
        *(.comment)
        *(.note*)
        *(.eh_frame*)
    }
}
