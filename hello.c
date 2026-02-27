#include <sys/io.h>
#include <unistd.h>

int main() {
    // Pedir permiso al Kernel de Linux para tocar el hardware directamente
    if (ioperm(0x3F8, 8, 1) != 0) {
        return 1; 
    }

    char *msg = "\n\n=================================================\n"
                "  [NKR] BARE-METAL C CORRIENDO EN RING-3 \n"
                "=================================================\n";
    
    // Escribir byte por byte directamente al puerto de Rust
    while (*msg) {
        outb(*msg++, 0x3F8);
    }
    
    // Bucle infinito para que la máquina no haga panic
    while(1) {
        sleep(2);
        char *beat = "[NKR] Latido directo a I/O...\n";
        while(*beat) {
            outb(*beat++, 0x3F8);
        }
    }
    return 0;
}
