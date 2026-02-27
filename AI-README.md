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
| `kvm-ioctls`     | 0.19    | —                | Wrappers seguros sobre ioctls de /dev/kvm (Kvm, VmFd, VcpuFd)|
| `kvm-bindings`   | 0.10    | `fam-wrappers`   | Structs FFI del kernel (kvm_regs, kvm_sregs, cpuid2, etc.)    |
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
| `KVM_MAX_CPUID_ENTRIES`| 256   | Importado de kvm-bindings. Máximo del kernel Linux para CPUID.|

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

## 16. Documentación Técnica del Código (extraída de main.rs)

Esta sección contiene toda la documentación inline que originalmente vivía como comentarios
en `src/main.rs`. Se extrajo para mantener el código fuente limpio y la documentación
centralizada.

### 16.1 Descripción General del Archivo

NKR (Nano-Kernel Runtime) v0.1.0 — Virtual Machine Monitor Fundacional.
Hipervisor bare-metal que se comunica directamente con /dev/kvm para ejecutar
unikernels (.img) sin dependencias externas (sin QEMU, sin Firecracker).

Arquitectura: VMM single-process, single-vCPU, consola serial por port I/O.

Flujo de ejecución:
```
/dev/kvm → VM fd → GuestMemory (mmap) → ELF loader → vCPU (sregs+regs)
→ vcpu.run() loop → IoOut(0x3F8) → stdout del host
```

### 16.2 Constantes de Arquitectura x86_64

- **`GUEST_RAM_SIZE` (512 MiB)**: RAM del guest por defecto. Suficiente para la mayoría de unikernels. Configurable vía CLI en futuras versiones.

- **`COM1_PORT` (0x3F8)**: Puerto I/O del COM1 estándar (UART 16550). El unikernel escribe bytes aquí (`out 0x3F8, al`) para salida de consola. El VMM intercepta el VMEXIT tipo IoOut y redirige el byte a stdout del host.

- **Tablas de paginación (`PML4_ADDR=0x9000`, `PDPT_ADDR=0xA000`, `PD_ADDR=0xB000`)**: Direcciones físicas para las tablas de paginación identity-mapped. Se ubican en la zona baja de memoria (<64 KiB) para evitar colisión con el kernel que típicamente se carga en direcciones más altas (>1 MiB). Estructura de 4 niveles (obligatoria para modo largo x86_64): PML4 (0x9000) → PDPT (0xA000) → PD (0xB000) → páginas de 2 MiB.

- **`GDT_ADDR` (0x500)**: Dirección donde se escribe la Global Descriptor Table en memoria del guest. La GDT es requerida por el hardware x86_64 aunque la segmentación esté efectivamente flat en modo largo. Ubicada en zona baja para no colisionar.

- **`KVM_MAX_CPUID_ENTRIES`**: Se importa directamente de kvm-bindings. Es el máximo definido por el kernel Linux (256 en la mayoría de versiones). Se usa como tope para `get_supported_cpuid()`; valores mayores causan ENOMEM.

### 16.3 Función `run_vmm()`

Orquesta el ciclo de vida completo del VMM:
1. Apertura de /dev/kvm
2. Creación de la VM (espacio de direcciones aislado)
3. Asignación de memoria física del guest (mmap)
4. Carga del kernel ELF en la memoria del guest
5. Escritura de tablas de paginación y GDT
6. Creación y configuración de la vCPU 0
7. Ejecución del bucle vcpu.run()

**Paso 1 — Abrir /dev/kvm**: `Kvm::new()` ejecuta `open("/dev/kvm")` y verifica la versión de la API. Si falla, es porque KVM no está disponible en el host.

**Paso 2 — Crear VM**: `KVM_CREATE_VM` devuelve un file descriptor que representa una VM completa con su propio espacio de direcciones físicas (GPA space), IRQ routing, y conjunto de vCPUs. Cada VM es un sandbox de hardware aislado.

**Paso 3 — Memoria del guest**: `GuestMemoryMmap::from_ranges()` crea un `mmap(2)` anónimo de `GUEST_RAM_SIZE` bytes. Este bloque de memoria del host se convertirá en la "RAM física" que el guest percibe. KVM traduce GPA → HVA en hardware via EPT/NPT.

**Paso 4 — Registrar memoria en KVM**: Establece el shadow page table o configura EPT/NPT para que los accesos del guest a GPA [0, GUEST_RAM_SIZE) se traduzcan a la HVA del mmap.

**Paso 5 — Preparar entorno x86_64**: Antes de arrancar la vCPU, se escriben en la "RAM" del guest las estructuras de datos que el procesador consulta al arrancar:
  - Tablas de paginación identity-mapped: GVA == GPA para el primer GiB. Sin paginación activa, no se puede entrar en modo largo.
  - GDT: el hardware requiere al menos un descriptor de código (CS) y uno de datos. En modo largo la segmentación es flat, pero la GDT no puede estar ausente.

**Paso 6 — Crear vCPU**: `KVM_CREATE_VCPU` devuelve un fd que representa un hilo de ejecución del procesador virtual. En v0.1.0 usamos una sola vCPU (ID=0). Se pasan las hojas CPUID del host al guest — sin esto, el kernel podría intentar usar instrucciones no soportadas o detectar un CPU inválido.

### 16.4 Función `register_guest_memory()`

Registra todas las regiones de `GuestMemoryMmap` en el file descriptor de la VM. Por cada región, emite el ioctl `KVM_SET_USER_MEMORY_REGION` que instruye a KVM a mapear un rango de Guest Physical Addresses (GPA) a la Host Virtual Address (HVA) del mmap anónimo correspondiente.

En v0.1.0 solo tenemos una región contigua [0, 512 MiB), pero el código está preparado para múltiples regiones (e.g., separar low-mem de high-mem, o crear huecos para MMIO en futuras versiones).

**SAFETY**: La región de memoria está respaldada por un mmap válido que permanece vivo durante toda la ejecución de la VM. El mmap no se comparte con otros procesos ni se libera prematuramente. `as_ptr()` devuelve la HVA del inicio del mmap de esta región; KVM usa esta dirección para configurar EPT/NPT.

### 16.5 Función `load_elf_kernel()`

Carga un binario ELF desde `path` en la memoria física del guest. Utiliza `linux-loader` que:
1. Parsea el ELF header (`e_ident`, `e_entry`, `e_phoff`)
2. Itera sobre los Program Headers tipo `PT_LOAD`
3. Copia cada segmento a la GPA especificada en `p_paddr`
4. Retorna el entry point (`e_entry`) como `kernel_load`

El entry point es la dirección donde el unikernel espera que la CPU comience a ejecutar instrucciones (primer byte del `_start` o `main`).

**Parámetros de carga**:
- `kernel_start = None`: sin offset adicional, usar las GPAs del ELF tal cual. Los unikernels típicamente se linkean a direcciones absolutas que coinciden con su posición deseada en la memoria física del guest.
- `highmem_start_address = None`: sin restricción de zona alta. Todo el espacio de direcciones del guest es válido para carga.
- `kernel_load` contiene `e_entry` del ELF: la dirección virtual donde el procesador debe comenzar la ejecución. Con identity mapping, GVA == GPA.

### 16.6 Función `write_page_tables()`

Escribe las tablas de paginación de 4 niveles para identity mapping del primer GiB de memoria física.

```
┌─────────────────────────────────────────────────────────────┐
│ CR3 ──► PML4 (0x9000)                                      │
│           └─ [0] ──► PDPT (0xA000)                         │
│                        └─ [0] ──► PD (0xB000)              │
│                                    ├─ [0]   → 0x00000000   │
│                                    ├─ [1]   → 0x00200000   │
│                                    ├─ ...                   │
│                                    └─ [511] → 0x3FE00000   │
│                                                             │
│  512 entradas × 2 MiB = 1 GiB identity-mapped              │
└─────────────────────────────────────────────────────────────┘
```

Cada entrada PDE usa el bit PS (Page Size) para indicar páginas de 2 MiB, eliminando la necesidad de un cuarto nivel (Page Table). Esto simplifica el setup y es suficiente para el rango de memoria que usamos.

Identity mapping (GVA == GPA) es necesario porque el kernel recién cargado no tiene su propio MMU setup y espera que las direcciones virtuales coincidan con las físicas al menos durante el bootstrap.

**Bits de las entradas**:
- PML4[0]: apunta a la PDPT. Present (bit 0) + Read/Write (bit 1) = 0x3
- PDPT[0]: apunta al Page Directory. Mismos bits: Present + Read/Write.
- PD[0..511]: cada entrada mapea 2 MiB de memoria física. bit 0 (P) = Present, bit 1 (RW) = Read/Write, bit 7 (PS) = Page Size (indica 2 MiB, no hay nivel PT). 0x83 = P | RW | PS. La dirección física base de cada página es i × 2 MiB. Con identity mapping, GVA 0x00400000 → GPA 0x00400000.

### 16.7 Función `write_gdt()`

Escribe una GDT mínima en la memoria del guest. En modo largo x86_64, la segmentación está efectivamente deshabilitada (todo es flat, base=0, limit=max), pero el hardware REQUIERE una GDT válida con al menos un descriptor de código para CS.

**Layout de la GDT**:

| Offset | Descriptor                                       |
|--------|--------------------------------------------------|
| 0x00   | Null (requerido por arquitectura Intel/AMD)      |
| 0x08   | Code64: L=1, D=0, P=1, DPL=0, Execute/Read      |
| 0x10   | Data: G=1, DB=1, P=1, DPL=0, Read/Write         |
| 0x18   | Reservado (padding para alineación)               |

El selector de CS (0x08) referencia el descriptor Code64 con L=1, que le dice al procesador que ejecute en modo 64-bit completo.

**Codificación de descriptores** (Referencia: Intel SDM Vol. 3A, Sección 3.4.5):
- `[0x00] 0x0000_0000_0000_0000` — Null descriptor, obligatorio, el procesador lo ignora.
- `[0x08] 0x00AF_9A00_0000_FFFF` — Code segment 64-bit: Base=0, Limit=0xFFFFF (con G=1 → 4 GiB efectivos), P=1 (present), DPL=0 (ring 0), S=1 (code/data), Type=0xA (exec/read), L=1 (long mode), D=0 (debe ser 0 cuando L=1).
- `[0x10] 0x00CF_9200_0000_FFFF` — Data segment: Base=0, Limit=0xFFFFF (con G=1 → 4 GiB), P=1, DPL=0, S=1, Type=0x2 (read/write), DB=1 (operaciones de 32-bit), G=1 (granularidad 4 KiB).
- `[0x18] 0x0000_0000_0000_0000` — Reservado, padding para alineación a 32 bytes.

### 16.8 Función `configure_sregs()`

Configura los Special Registers (sregs) para arrancar en modo largo x86_64.

El procesador x86_64 tiene múltiples "puertas" que deben abrirse en orden para transicionar de Real Mode → Protected Mode → Long Mode:

1. `CR0.PE = 1` → Activar Protected Mode
2. `CR4.PAE = 1` → Habilitar Physical Address Extension (>4 GiB)
3. `CR3 = &PML4` → Apuntar a las tablas de paginación
4. `EFER.LME = 1` → Habilitar Long Mode Enable
5. `CR0.PG = 1` → Activar paginación (esto activa LMA automáticamente)
6. `CS.L = 1, CS.D=0` → Ejecutar en modo 64-bit

KVM permite saltar directamente al estado final sin pasar por la transición secuencial, ya que configuramos todos los registros antes del primer `vcpu.run()`.

**Control Registers**:
- **CR0**: bit 0 (PE) = Protected Mode Enable, bit 31 (PG) = Paging Enable. Sin estos bits, el procesador está en Real Mode (16-bit).
- **CR3**: dirección física de la tabla PML4 (raíz del árbol de paginación). El hardware la consulta en cada acceso a memoria para traducir GVA → GPA.
- **CR4**: bit 5 (PAE) = Physical Address Extension — obligatorio para modo largo. Sin PAE, la paginación opera en modo legacy 32-bit (2 niveles).
- **EFER** (Extended Feature Enable Register, MSR 0xC0000080): bit 8 (LME) = Long Mode Enable — solicita transición a modo largo. bit 10 (LMA) = Long Mode Active — confirmación de que estamos en 64-bit. LMA se activa automáticamente por el hardware cuando PG=1 y LME=1, pero KVM requiere que lo configuremos explícitamente.

**Code Segment (selector 0x08)**: CS define el modo de ejecución. En modo largo: L=1, D=0 → modo 64-bit completo (operandos de 64-bit, RIP de 64-bit). L=1, D=1 → comportamiento indefinido (PROHIBIDO). L=0, D=1 → modo compatibilidad 32-bit dentro de long mode.

**Data Segments (selector 0x10)**: En modo largo, DS/ES/FS/GS/SS son efectivamente ignorados por el hardware (base siempre tratada como 0, excepto FS y GS que pueden tener base != 0 vía MSRs). Pero KVM valida que tengan descriptores presentes y válidos.

**GDT Register**: GDTR apunta al inicio de la GDT en la RAM del guest. limit = (4 entradas × 8 bytes) - 1 = 31.

**IDT Register**: En v0.1.0 no configuramos una IDT. Si el unikernel genera una excepción sin IDT válida, se producirá un triple fault → `VcpuExit::Shutdown`. Futuras versiones deben configurar al menos vectores de excepción básicos.

### 16.9 Función `configure_regs()`

Configura los registros generales de la vCPU.

- **RIP**: Entry point del kernel ELF (donde comienza la ejecución). Debe coincidir con `e_entry` del ELF + cualquier offset aplicado.
- **RFLAGS**: bit 1 es reservado y SIEMPRE debe estar en 1 (Intel SDM Vol. 1). Todos los demás flags en 0: IF=0 (interrupciones deshabilitadas, no tenemos IDT ni PIC/APIC), DF=0 (dirección de strings hacia adelante), TF=0 (sin single-step tracing).
- **RSP**: Deliberadamente no se configura. Razones: (1) El unikernel puede tener su propio linker script que define `_stack_top`. (2) Asignar un stack arbitrario podría colisionar con el kernel cargado. (3) Muchos unikernels configuran RSP como primera instrucción en `_start`. Si necesitas un stack pre-configurado (e.g., para kernels Linux): `regs.rsp = GUEST_RAM_SIZE as u64 - 0x1000` (Tope de RAM - guard page).

### 16.10 Función `run_vcpu_loop()`

Bucle de ejecución de la vCPU — corazón del VMM.

Cada iteración:
1. `vcpu.run()` ejecuta VMLAUNCH/VMRESUME en hardware
2. La CPU ejecuta instrucciones del guest a velocidad nativa
3. Cuando ocurre un evento que requiere intervención del host (VMEXIT), KVM retorna al userspace con un `VcpuExit` que describe el motivo
4. El VMM maneja el evento y decide si continuar o terminar

**VMEXITs manejados en v0.1.0**:

| Exit            | Causa                              | Acción                    |
|-----------------|------------------------------------|--------------------------|
| IoOut(0x3F8)    | `out 0x3F8, al` en el guest        | Byte → stdout del host    |
| IoIn(0x3F8)     | `in al, 0x3F8` en el guest         | Retorna 0x0 (sin input)   |
| Hlt             | Instrucción `hlt`                  | Terminación limpia        |
| Shutdown        | Triple fault o shutdown explícito  | Terminación con warning   |
| MmioRead/Write  | Acceso a GPA sin memoria mapeada   | Log warning, continuar    |

**Lock de stdout**: Se adquiere UNA VEZ fuera del loop para evitar el overhead de lock/unlock en cada byte de salida serial. Esto es seguro porque: (1) Somos single-threaded (una sola vCPU). (2) Los mensajes diagnósticos usan `eprintln!` (stderr, lock separado).

**Detalles por exit**:
- **IoOut (COM1)**: La instrucción `out dx, al` con DX=0x3F8 causa un VMEXIT tipo IoOut. El array `data` contiene el/los byte(s) escritos. Se redirigen directamente a stdout sin buffering. Otros puertos (0x3F9-0x3FF para control de UART, 0x60/0x64 para teclado PS/2) se ignoran silenciosamente. En v0.2.0 se puede agregar un bus de I/O con dispatch.
- **IoIn (COM1)**: `in al, dx` con DX=0x3F8. El guest está leyendo el Receive Buffer Register o el Line Status Register del UART. Sin emulación completa de 16550, retornamos 0x00 (sin datos disponibles). Un UART real retornaría el Line Status en puerto +5.
- **Hlt**: El guest ejecutó `hlt`. En un unikernel, esto típicamente significa "trabajo completado, no hay más que hacer".
- **Shutdown (Triple fault)**: El guest generó una excepción, no había IDT, la excepción de "no hay handler" generó otra excepción, y la tercera excepción anidada causa shutdown del procesador.
- **MMIO**: El guest intentó acceder a una dirección física que no tiene memoria RAM mapeada. En un VMM completo, estas direcciones se usan para dispositivos virtuales (virtio-net, etc.).
- **Error en vcpu.run()**: Condición irrecuperable. Causas típicas: registros configurados de forma inconsistente (e.g., PG=1 sin CR3 válido), memoria del guest no cubre las direcciones que el kernel intenta acceder, bug en KVM o el kernel del host.
