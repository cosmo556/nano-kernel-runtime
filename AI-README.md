# AI-README — NKR (Nano-Kernel Runtime) v0.1.0

> **Propósito de este archivo:** Documento de contexto para que cualquier agente de IA
> pueda entender la arquitectura, tomar decisiones informadas y continuar el desarrollo
> del proyecto sin ambigüedad. No es documentación de usuario.

---

## 1. Identidad del Proyecto

| Campo            | Valor                                                        |
|------------------|--------------------------------------------------------------|
| **Nombre**       | NKR — Nano-Kernel Runtime                                    |
| **Versión**      | 0.1.0 (fundacional)                                          |
| **Lenguaje**     | Rust (edition 2021)                                          |
| **Target**       | `x86_64-unknown-linux-gnu` (Ubuntu 22.04+)                   |
| **Licencia**     | MIT                                                          |
| **Binario**      | `nkr` — único ejecutable distribuible                        |
| **Dependencia**  | Solo `/dev/kvm` del kernel Linux (sin QEMU, sin Firecracker) |

## 2. Misión

Reemplazar Docker como runtime de ejecución para cargas de trabajo que requieren:
- **Latencia cero** en arranque (sin overhead de init, systemd, ni userspace Linux)
- **Aislamiento por hardware** (VM con EPT/NPT, no namespaces/cgroups)
- **Superficie de ataque mínima** (unikernel: un solo binario, sin shell, sin FS)
- **Distribución trivial** (un binario `nkr` + una imagen `.img` = deployment completo)

## 3. Arquitectura v0.1.0

```
┌──────────────────────────────────────────────────────────┐
│                    Host Linux (Ubuntu)                     │
│                                                           │
│  ┌─────────────────────────────────────────────────────┐  │
│  │                 NKR Process (nkr)                    │  │
│  │                                                     │  │
│  │  ┌───────────┐   ┌──────────────┐   ┌───────────┐  │  │
│  │  │  KVM API  │   │ GuestMemory  │   │ ELF Loader│  │  │
│  │  │ /dev/kvm  │   │  (mmap 512M) │   │  (linux-  │  │  │
│  │  │ kvm-ioctls│   │  vm-memory   │   │  loader)  │  │  │
│  │  └─────┬─────┘   └──────┬───────┘   └─────┬─────┘  │  │
│  │        │                │                  │        │  │
│  │        ▼                ▼                  ▼        │  │
│  │  ┌──────────────────────────────────────────────┐   │  │
│  │  │               vCPU 0 Run Loop                │   │  │
│  │  │  VMENTER → ejecutar guest → VMEXIT → handle  │   │  │
│  │  │                                              │   │  │
│  │  │  IoOut(0x3F8) ──► stdout (consola serial)    │   │  │
│  │  │  Hlt          ──► exit(0)                    │   │  │
│  │  │  Shutdown      ──► exit(1)                   │   │  │
│  │  └──────────────────────────────────────────────┘   │  │
│  └─────────────────────────────────────────────────────┘  │
│                                                           │
│  ┌─────────────────────────────────────────────────────┐  │
│  │              KVM (kernel module)                     │  │
│  │  EPT/NPT  │  VMCS  │  IRQ routing  │  MSR bitmap   │  │
│  └─────────────────────────────────────────────────────┘  │
│                                                           │
│  ┌─────────────────────────────────────────────────────┐  │
│  │              Hardware (Intel VT-x / AMD-V)          │  │
│  └─────────────────────────────────────────────────────┘  │
└──────────────────────────────────────────────────────────┘
```

## 4. Estructura de Archivos

```
nano-kernel-runtime/
├── Cargo.toml          # Manifiesto con dependencias rust-vmm fijadas
├── README.md           # Documentación de usuario (overview público)
├── AI-README.md        # ← ESTE ARCHIVO (contexto para IAs)
└── src/
    └── main.rs         # VMM completo: init → memoria → loader → vCPU → loop
```

## 5. Dependencias y Rol de Cada Crate

| Crate            | Versión | Feature          | Rol en NKR                                                    |
|------------------|---------|------------------|---------------------------------------------------------------|
| `kvm-ioctls`     | 0.16    | —                | Wrappers seguros sobre ioctls de /dev/kvm (Kvm, VmFd, VcpuFd)|
| `kvm-bindings`   | 0.7     | `fam-wrappers`   | Structs FFI del kernel (kvm_regs, kvm_sregs, cpuid2, etc.)    |
| `vm-memory`      | 0.14    | `backend-mmap`   | GuestMemoryMmap: RAM del guest como mmap anónimo              |
| `linux-loader`   | 0.11    | `elf`, `bzimage` | Carga de imágenes ELF/bzImage en GuestMemory                  |

### Grafo de dependencias relevante:
```
nkr
├── kvm-ioctls ──► kvm-bindings (re-exporta structs)
├── kvm-bindings
├── vm-memory
└── linux-loader ──► vm-memory (GuestMemory trait)
```

## 6. Flujo de Ejecución Detallado

```
main()
  └─► run_vmm()
        ├─► [1] Kvm::new()                    // open("/dev/kvm")
        ├─► [2] kvm.create_vm()               // KVM_CREATE_VM → VmFd
        ├─► [3] GuestMemoryMmap::from_ranges() // mmap(512 MiB)
        ├─► [4] register_guest_memory()        // KVM_SET_USER_MEMORY_REGION
        ├─► [5] load_elf_kernel()              // linux-loader: ELF → GuestMemory
        ├─► [6] write_page_tables()            // Identity map 1 GiB (2 MiB pages)
        ├─► [7] write_gdt()                    // Null + Code64 + Data
        ├─► [8] vm.create_vcpu(0)              // KVM_CREATE_VCPU
        ├─► [9] vcpu.set_cpuid2()              // Passthrough CPUID del host
        ├─► [10] configure_sregs()             // CR0/CR3/CR4/EFER + CS/DS + GDTR
        ├─► [11] configure_regs()              // RIP=entry_point, RFLAGS=0x2
        └─► [12] run_vcpu_loop()               // VMENTER/VMEXIT loop
                  ├─ IoOut(0x3F8) → stdout
                  ├─ IoIn(0x3F8)  → fill(0)
                  ├─ Hlt          → break OK
                  ├─ Shutdown     → break OK
                  └─ other        → Err
```

## 7. Mapa de Memoria del Guest (GPA Space)

```
0x0000_0000 ┌──────────────────────────────┐
            │  Zona reservada (IVT legacy) │  ← No usada en modo largo
0x0000_0500 ├──────────────────────────────┤
            │  GDT (4 entries × 8 bytes)   │  ← GDTR.base apunta aquí
0x0000_0520 ├──────────────────────────────┤
            │  (libre)                     │
0x0000_9000 ├──────────────────────────────┤
            │  PML4 Table (4 KiB)          │  ← CR3 apunta aquí
0x0000_A000 ├──────────────────────────────┤
            │  PDPT Table (4 KiB)          │
0x0000_B000 ├──────────────────────────────┤
            │  PD Table (4 KiB, 512 × 2M)  │
0x0000_C000 ├──────────────────────────────┤
            │  (libre hasta kernel load)   │
            │  ...                         │
0x0010_0000 ├──────────────────────────────┤  ← 1 MiB: zona típica de carga ELF
            │  Kernel ELF segments         │
            │  (PT_LOAD 0, 1, ...)         │
            │  ...                         │
            │  Entry point (e_entry) ──────│──► RIP inicial de la vCPU
            │  ...                         │
0x2000_0000 ├──────────────────────────────┤  ← 512 MiB: fin de la RAM del guest
            │  (no mapeado — MMIO exits)   │
            └──────────────────────────────┘
```

## 8. Constantes Clave y Justificación

| Constante         | Valor       | Justificación                                              |
|-------------------|-------------|-------------------------------------------------------------|
| `GUEST_RAM_SIZE`  | 512 MiB     | Suficiente para unikernels típicos (<100 MiB). Configurable.|
| `COM1_PORT`       | 0x3F8       | Puerto serial estándar x86. Más simple que virtio-console.  |
| `PML4_ADDR`       | 0x9000      | Zona baja libre. No colisiona con BDA (0x400) ni kernel.    |
| `PDPT_ADDR`       | 0xA000      | Contigua a PML4 para localidad de caché.                    |
| `PD_ADDR`         | 0xB000      | 512 entries × 2 MiB = 1 GiB de identity mapping.           |
| `GDT_ADDR`        | 0x500       | Justo después de BDA (0x400-0x4FF) legacy.                  |
| `CPUID_MAX_ENTRIES`| 256        | Cubre todas las hojas estándar + extended + hypervisor.     |

## 9. Requisitos del Unikernel (Guest)

Para que el unikernel funcione con NKR v0.1.0, debe:

1. **Formato**: ELF estático x86_64 (no PIE, no dinámico)
2. **Entry point**: definido en `e_entry` del ELF header
3. **Memoria**: segmentos PT_LOAD con `p_paddr` dentro de [0, 512 MiB)
4. **Sin colisión**: no mapear en 0x0-0xFFFF (zona de page tables y GDT)
5. **Consola**: escribir en puerto I/O 0x3F8 (`out dx, al` con DX=0x3F8)
6. **Terminación**: ejecutar `hlt` para salida limpia
7. **Stack**: configurar RSP en su propio `_start` (NKR no lo inicializa)
8. **Sin interrupciones**: no depender de PIC/APIC (no hay IDT ni IRQ chip)

### Ejemplo mínimo de unikernel (nasm):
```nasm
; test-kernel.asm — Unikernel mínimo para NKR
; Compilar: nasm -f elf64 test-kernel.asm -o test-kernel.o
;           ld -o test-kernel.img -Ttext 0x100000 --oformat elf64-x86-64 test-kernel.o
BITS 64
SECTION .text
global _start

_start:
    ; Escribir "Hello from NKR!\n" byte a byte en COM1
    mov rsi, message
    mov rcx, message_len
.loop:
    lodsb               ; AL = [RSI], RSI++
    out 0x3F8, al       ; Escribir byte en COM1
    loop .loop           ; CX--, si CX != 0 → .loop
    hlt                  ; Señalar al VMM que terminamos

SECTION .rodata
message: db "Hello from NKR!", 10  ; 10 = '\n'
message_len equ $ - message
```

## 10. Compilación y Ejecución

```bash
# Compilar NKR (modo release para máximo rendimiento)
cargo build --release

# Ejecutar con kernel por defecto (test-kernel.img en directorio actual)
sudo ./target/release/nkr

# Ejecutar con ruta explícita al unikernel
sudo ./target/release/nkr /path/to/my-unikernel.img

# Nota: `sudo` necesario por permisos de /dev/kvm.
# Alternativa sin sudo: agregar usuario al grupo kvm:
#   sudo usermod -aG kvm $USER && newgrp kvm
```

## 11. Diagnóstico de Errores Comunes

| Error                                    | Causa probable                          | Solución                                      |
|------------------------------------------|-----------------------------------------|-----------------------------------------------|
| `Fallo al abrir /dev/kvm`               | KVM no habilitado o sin permisos        | `modprobe kvm_intel` + `chmod 666 /dev/kvm`   |
| `Fallo KVM_CREATE_VM`                    | Límite de FDs alcanzado                 | `ulimit -n 65536`                             |
| `Fallo al cargar ELF`                    | Imagen no es ELF x86_64 o es PIE       | Verificar con `readelf -h imagen.img`         |
| `vCPU shutdown (triple fault)`           | Excepción en guest sin IDT              | Verificar que el kernel no genera excepciones |
| `vcpu.run() falló`                       | Registros mal configurados              | Verificar CR0/CR3/CR4/EFER y segmentos        |
| `MMIO write no manejado`                 | Guest accede fuera de RAM mapeada       | Verificar que PT_LOAD cabe en 512 MiB         |

## 12. Roadmap de Desarrollo

### v0.2.0 — Dispositivos Virtuales Básicos
- [ ] Emulación completa de UART 16550 (TX + RX + interrupts)
- [ ] IRQ chip (KVM_CREATE_IRQCHIP) para interrupciones de hardware
- [ ] PIT (KVM_CREATE_PIT2) para timer ticks
- [ ] Soporte para bzImage (kernel Linux estándar) además de ELF
- [ ] Boot params / zero page para kernels Linux

### v0.3.0 — Block Device & Networking
- [ ] virtio-blk para montar filesystem del unikernel (read-only)
- [ ] virtio-net con TAP backend para networking
- [ ] MMIO transport para dispositivos virtio (sin PCI)
- [ ] Configuración vía archivo TOML o flags CLI

### v0.4.0 — Multi-vCPU & Memory Hotplug
- [ ] Soporte para N vCPUs (SMP)
- [ ] APIC routing para distribución de interrupts
- [ ] Memory hotplug: ajustar RAM del guest dinámicamente
- [ ] Huge pages (2M/1G) para reducir TLB misses

### v0.5.0 — API & Orquestación
- [ ] API REST/gRPC para gestión de VMs (`nkr start/stop/status`)
- [ ] Snapshots: guardar/restaurar estado de VM (live migration base)
- [ ] Métricas: latencia de VMEXIT, throughput de I/O
- [ ] Rate limiting y QoS para dispositivos virtio

### v1.0.0 — Producción
- [ ] Seccomp filters para reducir superficie de ataque del VMM
- [ ] Jailer: sandbox del proceso nkr con namespaces/cgroups
- [ ] Compatibilidad OCI: ejecutar imágenes de contenedor como unikernels
- [ ] Integración con Kubernetes (CRI runtime)
- [ ] Documentación completa + benchmarks vs Docker/Firecracker

## 13. Decisiones de Diseño Fundamentales

### ¿Por qué NO QEMU?
QEMU es un emulador generalista con >2M líneas de código. NKR necesita sub-milisegundo
de boot time y superficie de ataque mínima. Cada línea de código de QEMU es un potencial CVE.

### ¿Por qué rust-vmm y no bindings directos a KVM?
rust-vmm provee abstracciones zero-cost que eliminan errores comunes (wrong ioctl number,
buffer overflows en CPUID, memory region misalignment). Las mismas crates las usa Firecracker
en producción en AWS Lambda — están battle-tested.

### ¿Por qué identity mapping y no un page table completo?
En v0.1.0, el unikernel corre en ring 0 con GVA == GPA. Esto simplifica radicalmente
el debugging (las direcciones que ves en los dumps son las mismas que en el guest).
Futuras versiones pueden implementar ASLR y separación user/kernel si se necesita.

### ¿Por qué no configurar RSP?
Cada unikernel tiene su propio layout de memoria. Configurar RSP desde el VMM requeriría
conocer el linker script del guest. Es más limpio que _start configure su propio stack.

### ¿Por qué serial (port I/O 0x3F8) y no virtio-console?
Port I/O es el mecanismo más simple posible (una instrucción `out`, un VMEXIT, print).
No requiere descriptores, virt queues, ni drivers en el guest. Perfecto para v0.1.0.

## 14. Convenciones de Código

- **Idioma de comentarios**: Español (el equipo es hispanohablante)
- **Manejo de errores**: `Result<T, Box<dyn Error>>` con `.map_err()` descriptivo
- **Sin `unwrap()` excesivo**: usar `.map_err()` o `.expect("mensaje técnico")`
- **Logs**: `eprintln!("[NKR] ...")` para logs del VMM, `stdout` exclusivo para el guest
- **Constantes**: SCREAMING_SNAKE_CASE con docstring explicando la justificación
- **Funciones**: una función por responsabilidad, documentada con `///`

## 15. Contexto para Continuación por IA

Cuando continúes el desarrollo de NKR, ten en cuenta:

1. **Compilación cruzada**: el proyecto SOLO compila en Linux x86_64 (depende de /dev/kvm)
2. **Permisos**: se necesita acceso a /dev/kvm (root o grupo kvm)
3. **Testing**: para probar, necesitas un unikernel ELF x86_64 (ver ejemplo en sección 9)
4. **Prioridad**: el siguiente paso lógico es v0.2.0 (UART 16550 completa + IRQ chip)
5. **No romper**: el bucle de vcpu.run() es el hot path — cualquier overhead ahí es crítico
6. **Single binary**: NKR debe seguir siendo un único binario sin dependencias de runtime
7. **Compatibilidad de crates**: al actualizar versiones, verificar cross-compatibility
   del ecosistema rust-vmm (especialmente kvm-ioctls ↔ kvm-bindings)
