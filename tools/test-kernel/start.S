/*
 * NKR Test Kernel — micro-kernel de prueba para validar el VMM.
 *
 * Imprime "Hello from NKR!\n" por COM1 (puerto 0x3F8) y ejecuta HLT.
 * Debe compilarse como ELF64 estático con entry point en 0x100000 (1 MiB).
 *
 * Convenciones:
 *   - Se ejecuta en modo largo (64 bits), identity-mapped por el VMM.
 *   - No hay stack ni heap — solo instrucciones inline.
 *   - La salida serial la intercepta el VMM como VcpuExit::IoOut(0x3F8).
 */

.code64
.section .text
.globl _start

_start:
    /* Puntero al mensaje */
    lea     msg(%rip), %rsi
    /* Longitud del mensaje */
    movq    $msg_len, %rcx
    /* Puerto COM1 */
    movw    $0x3F8, %dx

.Lprint_loop:
    /* Cargar byte actual */
    lodsb                       /* AL = *RSI++  */
    /* Enviar byte al puerto COM1 */
    outb    %al, %dx            /* out DX, AL   */
    dec     %rcx
    jnz     .Lprint_loop

    /* Terminación limpia — el VMM intercepta HLT */
    hlt

    /* Safety: si por alguna razón HLT retorna, loop infinito */
.Lhang:
    hlt
    jmp     .Lhang

.section .rodata
msg:
    .ascii  "Hello from NKR!\n"
msg_len = . - msg
