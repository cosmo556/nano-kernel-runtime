---
title: "Nano-Kernel Runtime (NKR): Un Hipervisor Bare-Metal de Micro-VMs para Cargas de Trabajo SaaS Multi-Tenant"
subtitle: "White Paper — Versión 1.3"
date: "Abril 2026"
lang: es
geometry: "margin=2.5cm"
fontsize: 11pt
linestretch: 1.25
mainfont: "DejaVu Serif"
sansfont: "DejaVu Sans"
monofont: "DejaVu Sans Mono"
colorlinks: true
linkcolor: "NavyBlue"
urlcolor: "NavyBlue"
toc: true
toc-depth: 2
numbersections: true
header-includes:
  - \usepackage{microtype}
  - \usepackage{fancyhdr}
  - \pagestyle{fancy}
  - \fancyhf{}
  - \fancyhead[L]{\small Nano-Kernel Runtime (NKR) — White Paper}
  - \fancyhead[R]{\small Abril 2026}
  - \fancyfoot[C]{\thepage}
  - \usepackage{booktabs}
  - \usepackage{longtable}
  - \renewcommand{\arraystretch}{1.3}
  - \usepackage{listings}
  - \lstset{basicstyle=\ttfamily\small, breaklines=true, frame=single, backgroundcolor=\color{gray!10}}
  - \usepackage{xcolor}
---

\newpage

> **Resumen.** El *Nano-Kernel Runtime* (NKR) es un hipervisor bare-metal de código abierto escrito en Rust que reemplaza los *runtimes* de contenedores como Docker por micro-VMs con aislamiento hardware, ejecutándose directamente sobre Linux KVM. NKR está diseñado para operadores que gestionan despliegues SaaS multi-tenant densos —especialmente Odoo ERP— sobre un único servidor con recursos limitados (16–32 GB RAM). Al eliminar la sobrecarga de QEMU, libvirt y el intercambio a nivel de contenedor, NKR consigue aislamiento hardware completo con un binario de tan solo 2–4 MB, arranque de VM en menos de un segundo, planificación exclusiva de CPU (modelo «chrs»), y un flujo de trabajo compatible con Docker para construir imágenes de disco. La versión 1.1 agregó seis capacidades clave: compartición de sistema de archivos en vivo via VirtIO-FS, desbordamiento controlado de CPU (*bursting*) mediante cgroupv2, aislamiento de red L2 con ebtables, límites de base de datos por inquilino, un exportador nativo de métricas Prometheus, y generación automática de ficheros compose multi-tenant. La versión 1.2 introduce cuatro optimizaciones adicionales para superar las 100 instancias Odoo en 32 GB RAM: VirtIO-PMEM + DAX (elimina ~150–200 MB de caché de páginas duplicada por instancia), E/S asíncrona con io_uring (reduce el coste de syscalls ~70% bajo alta concurrencia), carga de kernel ELF vmlinux sin comprimir (~20 ms de arranque más rápido) y un *jailer* Seccomp BPF. La versión 1.3 da un salto de rendimiento, densidad y operabilidad añadiendo: el **Sistema de Células** (stacks multi-VM con red L2/L3 aislada por célula), VirtIO-FS con DAX (reemplaza VirtIO-9P, 3–5× más rápido en I/O de ficheros), VirtIO-Balloon (recuperación de RAM ociosa), un canal VirtIO-Console (hvc0) para apagado coordinado en ~2s, y clonación de instancias (`nkr cell clone`). Este documento presenta la arquitectura completa, la implementación y el modelo de despliegue en producción de NKR v1.3.

---

\newpage

# Introducción y Motivación

## El Problema

Los proveedores de servicios que gestionan docenas de inquilinos SaaS sobre infraestructura compartida se enfrentan a una tensión fundamental entre **densidad** (maximizar el número de inquilinos por servidor) y **aislamiento** (evitar el efecto vecino ruidoso). Los contenedores Docker ofrecen alta densidad pero comparten el kernel del host, exponen una gran superficie de ataque y no garantizan CPU ni RAM. Las VMs tradicionales (QEMU/KVM con libvirt) proveen un aislamiento sólido, pero imponen una sobrecarga prohibitiva de memoria y disco para despliegues densos.

Considérese un escenario concreto: un operador que gestiona **50 instancias de Odoo 17 ERP** sobre un único servidor de 16–32 GB usando Docker:

| Problema | Impacto con Docker | Impacto con NKR |
|---|---|---|
| **Uso de disco** | 50 × 1,5 GB de imágenes ≈ **75 GB** | Base ext4 compartida + snapshots CoW |
| **Consumo de RAM** | 50 × ~1 GB ≈ **50 GB** | 50 × ~256 MB ≈ **12,5 GB** (exclusiva) |
| **Contención de CPU** | Planificador compartido, sin garantías | Cores pinados con el modelo «chrs» |
| **Latencia de reinicio** | ~3 minutos por reinicio de stack | ~2 segundos (apagado limpio vía hvc0) |
| **Ciclo de despliegue** | git pull → rebuild → restart | git pull → rsync → solo reiniciar Odoo |
| **Huella de infraestructura** | 50 Odoo + 50 PostgreSQL + 50 nginx | N Odoo + **1** PostgreSQL + **1** PgBouncer + **1** nginx |

NKR fue creado para eliminar estos compromisos, proporcionando aislamiento a nivel de VM con la simplicidad operacional de un contenedor.

## ¿Qué es NKR?

**Nano-Kernel Runtime (NKR)** es un hipervisor diseñado específicamente que:

- Ejecuta micro-VMs directamente sobre `/dev/kvm` sin QEMU, libvirt ni containerd
- Dota a cada «contenedor» de un kernel Linux real, un sistema de archivos ext4 y dispositivos VirtIO
- Compila a un **único binario de ~2–4 MB** (Rust, LTO, *stripped*)
- Ofrece una CLI compatible con Docker (`nkr run`, `nkr ps`, `nkr stop`, `nkr restart`, `nkr compose up`)
- Gestiona **Células**: grupos multi-VM con redes L2/L3 aisladas (`nkr cell create/up/down/clone`)
- Utiliza Docker **solo** en tiempo de construcción para generar imágenes de disco desde OCI/Dockerfiles

---

# Objetivos de Diseño

El diseño de NKR está guiado por cinco principios:

1. **Cero dependencias externas en tiempo de ejecución.** El binario `nkr` requiere únicamente un kernel Linux con soporte KVM. Sin QEMU, sin libvirt, sin *container runtime*.

2. **Aislamiento hardware con ergonomía de contenedor.** Cada carga de trabajo se ejecuta en una máquina virtual KVM completa —con su propio kernel, tablas de páginas y controlador de interrupciones— aunque los operadores interactúan con ella mediante comandos y archivos compose familiares, al estilo Docker.

3. **Asignación de recursos determinista.** La RAM se mapea exclusivamente a cada VM. Los ciclos de CPU se garantizan mediante *core pinning*. No existe *overcommit*.

4. **Huella mínima.** El binario del hipervisor pesa 2–4 MB. La sobrecarga del guest es acotada: una VM de 256 MB usa exactamente 256 MB de RAM del host.

5. **Listo para producción en SaaS multi-tenant.** Soporte de primera clase para despliegues multi-tenant de Odoo con PostgreSQL compartido (apoyado por PgBouncer), actualizaciones de módulos en caliente, aprovisionamiento automatizado y aislamiento de red por Célula para correr múltiples versiones de Odoo en paralelo.

---

# Visión General de la Arquitectura

```
┌──────────────────────────────────────────────────────────────────┐
│                    Servidor Host (Linux + KVM)                   │
│                                                                  │
│  Célula "nazcatex" (cell_id=1)   Célula "cafeteria" (cell_id=2) │
│  ┌─────┐ ┌─────┐ ┌─────┐         ┌─────┐ ┌─────┐ ┌─────┐       │
│  │ PG  │ │PgBnc│ │Odoo │  ...    │ PG  │ │PgBnc│ │Odoo │ ...   │
│  │2GB  │ │128M │ │256M │         │2GB  │ │128M │ │256M │       │
│  └──┬──┘ └──┬──┘ └──┬──┘         └──┬──┘ └──┬──┘ └──┬──┘       │
│     └───────┴───────┘               └───────┴───────┘           │
│  ┌──────────────────────┐    ┌──────────────────────┐           │
│  │ nkr-br1 10.0.1.0/24 │    │ nkr-br2 10.0.2.0/24 │           │
│  └──────────────────────┘    └──────────────────────┘           │
│          │                           │                           │
│  ┌───────┴───────────────────────────┴────────┐                 │
│  │   iptables: NAT / DNAT / MASQUERADE        │                 │
│  └─────────────────────────────────────────────┘                │
│  ┌─────────────────────────────────────────────┐                │
│  │  nginx (host) — proxy inverso + SSL         │                │
│  │  Mapa SNI → IP de célula:8069 / :8072       │                │
│  └─────────────────────────────────────────────┘                │
└──────────────────────────────────────────────────────────────────┘
```

Cada micro-VM es una máquina virtual completa con:

- Un kernel Linux (`nanolinux` ELF altamente optimizado o `bzImage` clásico, binario compartido entre todas las VMs de una célula)
- Un sistema de archivos raíz ext4 (creado desde imágenes OCI), opcionalmente expuesto vía VirtIO-PMEM + DAX
- Dispositivos VirtIO-MMIO para almacenamiento en bloque, red, VirtIO-FS con DAX, memoria persistente, balloon y consola
- Un initramfs que gestiona la carga de módulos, la configuración de red, el montaje VirtIO-FS y el pivotado del rootfs
- RAM exclusiva y *pinning* de CPU vía cgroupv2 + `sched_setaffinity`

---

# Motor VMM: De KVM al Arranque

El motor VMM (`vmm.rs`, ~1.600 líneas) implementa el ciclo de vida completo de una micro-VM usando KVM ioctls directos a través del ecosistema de crates `rust-vmm` — la misma base que usa AWS Firecracker e Intel Cloud Hypervisor.

## Inicialización KVM

```
1. Abrir /dev/kvm
2. KVM_CREATE_VM       → descriptor de fichero de VM
3. KVM_CREATE_IRQCHIP  → PIC + IOAPIC en kernel
4. KVM_CREATE_PIT2     → Temporizador de Intervalo Programable
5. Mapear memoria guest → GuestMemoryMmap (dos regiones RAM; slot PMEM opcional)
6. KVM_CREATE_VCPU     → vCPU único (id=0)
7. Configurar CPUID, SREGs, Registros Generales
```

## Mapa de Memoria del Guest (x86_64)

NKR usa un modelo de memoria de dos regiones compatible con el protocolo de arranque de Linux:

| Dirección | Contenido | Tamaño |
|---|---|---|
| `0x0000–0x9FFFF` | RAM base (convencional) | 640 KB |
| `0x0500` | GDT (*Global Descriptor Table*) | 32 bytes |
| `0x7000` | *Zero Page* (parámetros de arranque) | 4 KB |
| `0x9000` | PML4 (*Page Map Level 4*) | 4 KB |
| `0xA000` | PDPTE (*Page Directory Pointer*) | 4 KB |
| `0xB000` | PDE (*Page Directory*, páginas de 2 MB) | 4 KB |
| `0x20000` | Línea de comandos del kernel | variable |
| `0x100000` | Dirección de carga del bzImage | ~10 MB |
| `0x800_0000` | Initramfs | variable |
| `0x1_0000_0000` | Slot VirtIO-PMEM (si `--pmem`) | = tamaño del disco |
| `0x2_0000_0000` | Ventana DAX de VirtIO-FS (si `--share`) | 4 GB |

## Protocolo de Arranque

NKR soporta formatos de kernel detectados automáticamente por los bytes mágicos del fichero:

- **ELF nanolinux** (por defecto): Detectado por el magic `\x7fELF`. Cargado vía `linux-loader::Elf::load()`. El vCPU arranca directamente en modo largo de 64 bits (`EFER=0xD01, CR0=0x80050033, CR4=PAE, CS.l=1`). Elimina por completo la descompresión gzip en el guest, acelerando drásticamente el arranque.
- **bzImage** (v1.0 clásico): Protocolo de arranque Linux de 32 bits. Kernel cargado en `0x100000` mediante `linux-loader::BzImage::load()`. El vCPU arranca en modo protegido de 32 bits.

Secuencia de arranque (compartida):

1. **Carga del kernel** — ELF (en bloques) o bzImage cargado en `0x100000`
2. **Carga del initramfs** — Se copia en `0x800_0000` de la memoria del guest
3. **Configuración de la *zero page*** — Parámetros de arranque en `0x7000`
4. **Escritura de las tablas de páginas** — Páginas de 2 MB con mapeo de identidad vía PML4 → PDPT → PD
5. **Escritura de la GDT** — Tabla de 4 entradas: null, código64, datos, null
6. **Configuración del vCPU** — RIP = punto de entrada del kernel; sregs configurados para 64-bit (ELF) o 32-bit (bzImage)

La línea de comandos configura todos los dispositivos VirtIO en línea:

```
console=ttyS0 panic=1 pci=off noapic nolapic clocksource=jiffies tsc=nowatchdog
virtio_mmio.device=4K@0xd0000000:5     # red
virtio_mmio.device=4K@0xd0001000:6     # disco 0
virtio_mmio.device=4K@0xd0002000:7     # disco 1 (si existe)
virtio_mmio.device=4K@0xd0010000:8     # share VirtIO-FS 0 (si --share)
virtio_mmio.device=4K@0xd0020000:16    # PMEM (si --pmem)
virtio_mmio.device=4K@0xd0030000:17    # Balloon
virtio_mmio.device=4K@0xd0040000:18    # VirtIO-Console (hvc0)
root=/dev/vda rw init=/sbin/init nkr.ip=10.0.{cell_id}.{vm_id+1}
# Con --pmem: root=/dev/pmem0 rootflags=dax
```

## Gestión del Tiempo y Reloj

Las micro-VMs pueden sufrir desfase de reloj (*clock drift*) en entornos de alta densidad. NKR lo resuelve con dos mecanismos:

1. **PIT2 (Programmable Interval Timer):** Se instancia explícitamente (`KVM_CREATE_PIT2`), proporcionando la interrupción base de reloj del sistema vital para el planificador del *guest*.
2. **Fuentes de Reloj de Kernel:** El parámetro `clocksource=jiffies tsc=nowatchdog` fuerza al kernel guest a basarse en interrupciones temporizadas (jiffies) y desactiva el watchdog del TSC, evitando cuelgues por inconsistencias del TSC bajo alta contención de CPU.

## Bucle del vCPU

El bucle principal ejecuta `KVM_RUN` en ciclo continuo, gestionando cuatro tipos de salida:

| Tipo de salida | Manejador |
|---|---|
| `IoOut` (escritura en puerto E/S) | Salida de consola serie (COM1 `0x3F8`) |
| `IoIn` (lectura de puerto E/S) | Registros de estado serie |
| `MmioWrite` | Escrituras en registros VirtIO-MMIO (configuración de dispositivo, colas, notificaciones) |
| `MmioRead` | Lecturas de registros VirtIO-MMIO (funcionalidades, estado, espacio de configuración) |
| `Hlt` / `Shutdown` | Salida limpia |

## Apagado Robusto y Reinicio — *Nuevo en v1.3*

**Canal de control VirtIO-Console (hvc0)** (`console.rs`): El proceso init del guest bloquea en `read -r cmd < /dev/hvc0`. Al recibir `"SHUTDOWN\n"`, el init ejecuta un apagado ordenado (SIGTERM a los servicios, espera al postmaster de PostgreSQL hasta 25s, llama a `poweroff`).

**Flujo SIGTERM en el VMM** (`vmm.rs:1916–1944`):

1. `SIGTERM` recibido → AtomicBool `SHUTDOWN_REQUESTED` activado
2. Primera iteración del loop (phase=0):
   - Almacena timestamp `SHUTDOWN_STARTED_MS`
   - Inyecta `"SHUTDOWN\n"` en la receiveq del hvc0
   - Arma `SIGALRM` cada 1s (`setitimer`) para romper `vcpu.run()` si el guest está en HLT
   - Avanza a phase=1
3. Iteraciones siguientes (phase=1):
   - Re-inyecta `"SHUTDOWN\n"` cada 2s — mitiga carreras donde la inicialización del driver virtio-console retrasó la lectura de la primera inyección
   - Tras timeout de 60s: break forzado del loop vCPU
4. Tras el break: extracción de volúmenes RW, desmontaje del TAP, eliminación de reglas iptables, desregistro del estado

**Detección de zombies** (`state.rs:249–256`): `is_pid_alive()` combina `kill(pid, 0)` con la lectura de `/proc/{pid}/status` para tratar procesos zombie (`State: Z`) como muertos. Esto evita que `nkr stop`/`nkr restart` espere 90s en vano cuando el proceso de compose padre no ha llamado a `wait()`.

**`nkr restart`** (`main.rs:126–208`):

1. Lee `/proc/{pid}/cmdline` — captura el argv original de `nkr run ...`
2. Detiene la VM vía SIGTERM (timeout 90s con degradación a SIGKILL)
3. Espera 500ms — permite que se complete la limpieza del TAP/bridge
4. Relanza con `setsid()` (desvinculado del terminal), stdout/stderr redirigidos a `/tmp/nkr-restart-{vm_id}.log`

Resultado: ciclo de reinicio típico en ~2s con el canal hvc0, frente a 60s de timeout si el canal no está disponible.

---

# Modelo de Dispositivos VirtIO

NKR implementa el transporte VirtIO-MMIO (*Memory-Mapped I/O*), no PCI, para máxima simplicidad. El parámetro de arranque del kernel `pci=off` deshabilita completamente la enumeración PCI.

## Mapa de Direcciones MMIO

| Dirección | Dispositivo | IRQ | Desde |
|---|---|---|---|
| `0xD000_0000` | VirtIO-Net (red) | 5 | v1.0 |
| `0xD000_1000` | VirtIO-Block disco 0 (rootfs) | 6 | v1.0 |
| `0xD000_2000` | VirtIO-Block disco 1 | 7 | v1.0 |
| `0xD000_3000+` | Discos adicionales (+0x1000 c/u) | 8+ | v1.0 |
| `0xD001_0000+` | VirtIO-FS shares (+0x1000 c/u, DAX) | 8+ | **v1.3** |
| `0xD002_0000` | VirtIO-PMEM (memoria persistente, DAX) | 16 | v1.2 |
| `0xD003_0000` | VirtIO-Balloon | 17 | **v1.3** |
| `0xD004_0000` | VirtIO-Console (hvc0) | 18 | **v1.3** |

El rango `0xD001_0000+` garantiza que no haya colisión con la zona de bloques (crece hasta `0xD000_9000` con 9 discos). PMEM, Balloon y Console son reservados estáticamente.

## Dispositivo de Bloque VirtIO

**Implementación:** `block.rs` — ~310 líneas

- **Cola:** Virtqueue única, 256 descriptores
- **Tamaño de sector:** 512 bytes
- **Operaciones:** Lectura (`type=0`), Escritura (`type=1`)
- **Cadena de descriptores:** Cabecera (16 bytes: tipo + sector) → Buffer de datos → Byte de estado
- **Interrupciones:** Inyección de IRQ vía `irqfd` tras procesar cada lote de completaciones
- **Negociación de funcionalidades:** `VIRTIO_F_VERSION_1` (bit 32)
- **E/S asíncrona (v1.2):** Usa `io_uring` (profundidad 128) para lecturas/escrituras no bloqueantes. Cada operación se envía como `opcode::Read` o `opcode::Write` al SQ; las completaciones se drenan al inicio de cada iteración del bucle vCPU mediante `poll_completions()`. Degradación silenciosa a `pread64/pwrite64` síncronos si `io_uring` no está disponible (kernel < 5.1).

## Dispositivo de Red VirtIO

**Implementación:** `net.rs` — ~310 líneas

- **Colas:** Dos virtqueues (RX=0, TX=1), 256 descriptores cada una
- **Backend:** Dispositivo TAP Linux (`/dev/net/tun`, `TUNSETIFF`)
- **Cabecera:** 12 bytes de cabecera VirtIO-net (eliminada/añadida en TX/RX)
- **Ruta RX:** Un hilo de fondo dedicado realiza lecturas bloqueantes del fd TAP, inyecta paquetes en la cola RX y señala la IRQ
- **Ruta TX (v1.2):** `opcode::Write` SQE al fd TAP vía `io_uring` (profundidad 64). El payload `Vec<u8>` se guarda en `tx_pending` hasta drenar la CQE, evitando desalocación prematura. Cae a `tap.write_all()` si el ring no está disponible.
- **Funcionalidades:** `VIRTIO_NET_F_MAC` | `VIRTIO_NET_F_STATUS` | `VIRTIO_F_VERSION_1`

## Dispositivo VirtIO-FS (*Compartición de Ficheros*) — **Nuevo en v1.3**

**Implementación:** `virtio_fs.rs`

NKR v1.3 reemplaza el servidor VirtIO-9P por VirtIO-FS con DAX, entregando acceso al sistema de archivos 3–5× más rápido para la carga de bibliotecas Python y actualizaciones en caliente de módulos Odoo.

- **Protocolo:** Socket vhost-user conectando con el demonio externo `virtiofsd`
- **Ventana DAX:** 4 GB montada en la dirección física guest `0x2_0000_0000` (slot KVM 3). Las lecturas del guest acceden directamente a la *page cache* del host sin copia adicional.
- **Device ID:** 26 sobre VirtIO-MMIO
- **Semántica:** Compatibilidad POSIX completa (`fcntl`, `O_DIRECT`, `flock`)
- **CLI:** `--share host_path:guest_path` (repetible; primer share en `0xD001_0000`, cada adicional +0x1000)
- **Rendimiento:** El arranque en frío de 30 micro-VMs compartiendo un rootfs Odoo común baja de ~90s (9P) a ~25s (VirtIO-FS DAX)

En el guest, el initramfs monta automáticamente las shares declaradas en el cmdline del kernel:

```bash
nkr run --disk odoo.ext4 --share /opt/modules:/mnt/extra-addons
# guest: mount -t virtiofs virtiofs0 /mnt/extra-addons
```

## Dispositivo VirtIO-PMEM + DAX — **Nuevo en v1.2**

**Implementación:** `pmem.rs` — ~200 líneas

VirtIO-PMEM (device ID 27) mapea el disco raíz del guest en la RAM del host mediante `mmap(MAP_SHARED)` y lo registra como un slot de memoria KVM en la dirección física guest `0x1_0000_0000` (4 GB). El kernel guest (con `CONFIG_VIRTIO_PMEM=y` y `CONFIG_FS_DAX=y`) lo expone como `/dev/pmem0` y monta el rootfs con la opción `dax`, eliminando por completo la caché de páginas del guest.

- **MMIO:** `0xD002_0000`, IRQ 16, device ID 27
- **Config space (offset 0x100):** `[u64 start_phys_addr][u64 size]`
- **Slot KVM:** Slot 2 (los slots 0 y 1 usan las dos regiones de RAM)
- **mmap en host:** `MAP_SHARED | PROT_READ | PROT_WRITE` + hint `MADV_HUGEPAGE` para reducir presión TLB
- **Requests FLUSH:** El guest envía `VIRTIO_PMEM_REQ_TYPE_FLUSH`; NKR responde con `msync(MS_ASYNC)` sin bloquear el vCPU
- **Entrada E820:** Tipo 12 (*persistent memory*) para `[4 GB, 4 GB + disk_size]`
- **Cambio en cmdline:** `root=/dev/pmem0 rootflags=dax` sustituye a `root=/dev/vda rw`
- **Degradación:** Silenciosa a VirtIO-Block si el disco no se puede `mmap`
- **Requisito kernel guest:** `CONFIG_VIRTIO_PMEM=y`, `CONFIG_FS_DAX=y`
- **CLI:** flag `--pmem` en `nkr run`

**Mecanismo de ahorro de memoria:** Con DAX, las lecturas del guest acceden directamente a la caché de páginas del host — no existe una segunda copia de los datos en la RAM del guest. Para una instancia Odoo típica con un *working set* activo de ~300 MB en lectura, esto elimina 150–200 MB de caché de páginas duplicada por VM.

## Dispositivo VirtIO-Balloon — **Nuevo en v1.3**

**Implementación:** `balloon.rs` — ~150 líneas

VirtIO-Balloon (device ID 5) recupera la memoria no utilizada de VMs ociosas y la devuelve al kernel del host mediante `madvise(MADV_DONTNEED)`.

- **MMIO:** `0xD003_0000`, IRQ 17
- **Operación:** El VMM escribe el objetivo del balloon (en páginas) en el espacio de configuración del dispositivo; el driver del guest infla/desinfla asignando/liberando páginas
- **CLI:** `--balloon-mb N` en `nkr run` pre-infla el balloon en N MB al arrancar
- **Efecto combinado:** Una VM de 700 MB con `--balloon-mb 300` ocupa efectivamente solo ~400 MB de RAM del host
- **Compose:** `balloon_mb: 200` en la especificación de servicio

Al combinar PMEM+DAX (elimina duplicación de page cache), KSM (deduplica páginas idénticas entre VMs) y VirtIO-Balloon (recupera RAM ociosa), NKR permite ubicar 103+ instancias de Odoo concurrentes en 32 GB de RAM.

## Dispositivo VirtIO-Console (hvc0) — **Nuevo en v1.3**

**Implementación:** `console.rs`

VirtIO-Console proporciona un canal de control bidireccional entre el VMM y el proceso init del guest, usado exclusivamente para el apagado coordinado.

- **Device ID:** 3 sobre VirtIO-MMIO en `0xD004_0000`, IRQ 18
- **Lado guest:** El init bloquea en `read -r cmd < /dev/hvc0`. Al recibir `"SHUTDOWN\n"`: ejecuta apagado ordenado (SIGTERM a servicios, espera postmaster PG, `poweroff`)
- **Lado host:** `try_inject(b"SHUTDOWN\n")` escribe en la receiveq y eleva la IRQ. `poll_pending()` reintenta si la cola estaba llena
- **Mitigación de carreras:** El VMM re-inyecta cada 2s durante la ventana de apagado por si la primera inyección se perdió antes de que el driver hvc0 estuviera inicializado

---

# Red (*Networking*)

## Topología del Bridge

NKR soporta dos modos de red:

**Modo legado** (`cell_id=0`): Bridge único `nkr0`, subnet `10.0.0.0/24`. Todas las VMs comparten un dominio L2.

**Modo célula** (`cell_id=1..254`): Bridge por célula `nkr-br{N}`, subnet `10.0.{N}.0/24`. Cada célula es un dominio L2/L3 aislado con su propio NAT.

```
Legado (cell_id=0):               Célula 1 (nazcatex):   Célula 2 (cafeteria):
nkr0  10.0.0.0/24                 nkr-br1 10.0.1.0/24    nkr-br2 10.0.2.0/24
nkr-tap1  → VM 10.0.0.2           nkr-c1-tap1 10.0.1.2   nkr-c2-tap1 10.0.2.2
nkr-tap2  → VM 10.0.0.3           nkr-c1-tap2 10.0.1.3   nkr-c2-tap2 10.0.2.3
```

## Fórmula Determinista

Definida en `src/registry.rs:216`:

```
IP  = 10.0.{cell_id}.{vm_id + 1}
MAC = 52:54:00:{cell_id}:34:{vm_id}
TAP = nkr-c{cell_id}-tap{vm_id}   (cell_id>0)
    = nkr-tap{vm_id}               (cell_id=0, legado)
```

Asignaciones convencionales por célula: `pg=vm_id 1`, `pgbouncer=vm_id 2`, `odoo-NN=vm_id 3..N`.

Ejemplo cell_id=1: db→`10.0.1.2`, pgbouncer→`10.0.1.3`, odoo-01→`10.0.1.4`.

## Configuración Automática

Para cada VM, el VMM (`vmm.rs:956–974`):

1. Crea el dispositivo TAP `nkr-c{cell_id}-tap{vm_id}` (o `nkr-tap{vm_id}` en legado)
2. Lo conecta al bridge `nkr-br{cell_id}` (o `nkr0`)
3. Asigna la MAC `52:54:00:{cell_id}:34:{vm_id}`
4. Pasa la IP al guest vía cmdline del kernel (`nkr.ip=`)
5. Configura reglas iptables:
   - `POSTROUTING MASQUERADE` para acceso a internet
   - `FORWARD ACCEPT` para tráfico inter-VM dentro de la célula
   - `PREROUTING DNAT` + `OUTPUT DNAT` para reenvío de puertos (si `--port`)

Las reglas se verifican con `iptables -C` antes de añadir (idempotente) y se eliminan al apagar la VM.

## Reenvío de Puertos

```bash
nkr run --disk odoo.ext4 --port 8069:8069 --id 2
# Crea: host:8069 → 10.0.0.3:8069 (DNAT + MASQUERADE)
```

## Aislamiento L2 con ebtables — **Nuevo en v1.1**

Tres reglas ebtables por TAP confinan el tráfico a la MAC+IP asignada por el hipervisor:

```
ebtables -A INPUT -i nkr-c1-tapN -p ARP --arp-mac-src 52:54:00:01:34:N -j ACCEPT
ebtables -A INPUT -i nkr-c1-tapN -p IPv4 --ip-src 10.0.1.(N+1) -s 52:54:00:01:34:N -j ACCEPT
ebtables -A INPUT -i nkr-c1-tapN -j DROP
```

Estas reglas impiden que una VM comprometida envíe paquetes con MAC/IP distintas. Las reglas se eliminan en el cleanup mediante `teardown_tap_isolation()`. Si `ebtables` no está instalado, NKR emite un aviso y continúa sin aislamiento L2 (degradación silenciosa).

---

# Sistema de Células — **Nuevo en v1.3**

El Sistema de Células es la respuesta de NKR para correr **múltiples stacks independientes de Odoo** (ej. Odoo 15, 17 y 19 para distintos tipos de clientes) en el mismo host sin conflictos de IP/red.

## ¿Qué es una Célula?

Una *célula* es un grupo nombrado de micro-VMs con:
- Un bridge Linux dedicado y subnet (`10.0.{cell_id}.0/24`)
- Su propio `nkr-compose.yml` orquestando el stack completo (PG + PgBouncer + N Odoos)
- Directorios de instancias aislados bajo `/mnt/nkr/cells/<nombre>/instances/`
- Registry de VMs cell-scoped (IDs 2–254 por célula, independientes entre células)

Hasta 254 células pueden coexistir en un solo host (cell_ids 1–254). `cell_id=0` es el modo legado plano.

## Estructura de Directorios

```
/mnt/nkr/
├── cell-registry.json              # cell_name → cell_id
├── registry.json                   # "cell_name/vm_name" → vm_id (con scope)
└── cells/
    └── nazcatex/                   # célula "nazcatex" (cell_id=1)
        ├── cell.yml                # { name, cell_id, odoo_version }
        ├── nkr-compose.yml         # Compose del stack completo
        └── instances/
            ├── nazcatex-odoo-01/
            │   ├── config/odoo.conf
            │   ├── addons/
            │   ├── filestore/
            │   └── logs/
            └── nazcatex-odoo-02/
                └── ...
```

## Sistema de Registry

**`cell-registry.json`** — mapea `cell_name → cell_id` (entero, 1–254). Persistido en `/mnt/nkr/cell-registry.json`.

**`registry.json`** — mapea `"cell_name/vm_name" → vm_id` (entero, 2–254, con scope por célula). Persistido en `/mnt/nkr/registry.json`.

El formato de clave con scope significa que `nazcatex/odoo-01` y `cafeteria/odoo-01` pueden tener ambas `vm_id=3` sin conflicto — viven en subnets distintas (`10.0.1.4` vs `10.0.2.4`).

`resolve_id_scoped(cell_name, vm_name)` en `registry.rs:106` asigna el siguiente ID libre dentro del scope de la célula, o devuelve el existente si ya está registrado. `register_explicit_scoped()` registra un ID específico y verifica que no esté tomado dentro del mismo scope de célula.

## Ciclo de Vida de una Célula

```bash
# Crear una célula — registra cell_id, crea bridge + estructura de directorios
sudo nkr cell create nazcatex --odoo-version 17.0

# Generar compose (script externo o manual) y arrancar el stack completo:
sudo nkr cell up nazcatex -d        # compose up en modo daemon

# Estado
sudo nkr cell ls                    # tabla de todas las células
sudo nkr cell ps nazcatex           # VMs activas en esta célula

# Apagado
sudo nkr cell down nazcatex         # detiene todas las VMs
sudo nkr cell destroy nazcatex      # elimina del registry (datos preservados)
```

## Formato del Compose de una Célula

Los ficheros compose de célula incluyen `cell_id` y `nkr_name` por servicio:

```yaml
services:
  pg:
    nkr_name: "nazcatex-pg"
    id: 1
    disks: [/mnt/nkr/images/postgres.ext4]
    ram: 2048
    chrs: 5
    cell_id: 1
    ports: ["5432:5432"]

  pgbouncer:
    nkr_name: "nazcatex-pgbouncer"
    id: 2
    ram: 128
    chrs: 1
    cell_id: 1

  odoo-01:
    nkr_name: "nazcatex-odoo-01"
    id: 3
    ram: 512
    chrs: 2
    cell_id: 1
    environment:
      DB_HOST: "10.0.1.2"        # IP PG: 10.0.{cell_id}.{pg_vm_id+1}
      PGB_HOST: "10.0.1.3"       # IP PgBouncer
      DB_NAME: "db-nazcatex-odoo-01"
```

Las IPs en `environment:` son **literales** calculados desde `cell_id + vm_id` en tiempo de generación del compose.

## Clonación de Instancias — `nkr cell clone`

`clone_instance()` en `cell.rs:659` proporciona clonación atómica de una instancia Odoo dentro de una célula — el flujo principal para crear entornos de test/staging desde producción.

**Algoritmo:**

1. Escanea `cells/*/instances/<src>/` para localizar la célula propietaria
2. Rechaza si `dst` ya existe
3. Avisa si la VM `src` está activa (las sesiones PG se interrumpirán brevemente)
4. Registra `dst_vm_id` vía `resolve_id_scoped` (siguiente ID libre en el scope de la célula)
5. `cp -a --reflink=auto <src_dir> <dst_dir>` — O(1) en btrfs/XFS (CoW), copia física en ext4
6. Limpia los logs del destino
7. `rewrite_odoo_conf()` — sustituye todas las ocurrencias de `src_nkr` → `dst_nkr` en `odoo.conf` (db_name, dbfilter, rutas)
8. `clone_database()` — clonación atómica PostgreSQL:
   - `ALTER DATABASE "{src}" WITH ALLOW_CONNECTIONS false`
   - `SELECT pg_terminate_backend(...)` — desconecta sesiones activas
   - `CREATE DATABASE "{dst}" WITH TEMPLATE "{src}" OWNER odoo`
   - `ALTER DATABASE "{src}" WITH ALLOW_CONNECTIONS true`
   - Conectividad verificada con `pg_isready` antes de intentar; rollback en fallo
9. `append_compose_block()` — edición de texto del YAML (preserva comentarios y formato original):
   - Localiza el bloque del servicio src por `nkr_name:` exacto
   - Clona el bloque con nuevo header, nuevo `id:`, todas las sustituciones `src_nkr` → `dst_nkr`
   - Crea backup con timestamp (`nkr-compose.yml.bak.{unix_ts}`)

**Flags:**
- `--no-db` — salta la clonación de la base de datos
- `--no-compose` — salta la modificación del compose

```bash
# Clonación completa (ficheros + DB + compose)
sudo nkr cell clone nazcatex-odoo-01 nazcatex-odoo-04

# Smoke test seguro (solo ficheros, sin DB ni compose)
sudo nkr cell clone nazcatex-odoo-01 nazcatex-odoo-04 --no-db --no-compose
```

---

# Ciclo de Vida del Disco: De OCI a ext4

NKR usa Docker exclusivamente como **herramienta de construcción** para transformar imágenes OCI en sistemas de archivos ext4 en bruto. Docker no es necesario en tiempo de ejecución.

## Pipeline de Construcción

```
┌──────────────┐    ┌──────────────┐    ┌──────────────┐
│  Docker Hub  │    │   Nkrfile    │    │ Imagen Local │
│ (imagen OCI) │    │ (Dockerfile) │    │              │
└──────┬───────┘    └──────┬───────┘    └──────┬───────┘
       │ nkr pull          │ nkr build          │
       ▼                   ▼                    ▼
  docker create       docker build         docker create
       │                   │                    │
       ▼                   ▼                    ▼
  docker export ───────────────────────► filesystem.tar
                                              │
                                              ▼
                                   truncate + mkfs.ext4
                                              │
                                              ▼
                                   mount -o loop + tar -xf
                                              │
                                              ▼
                                         disco.ext4
                                    (listo para nkr run)
```

## Formato Nkrfile

Los Nkrfiles son Dockerfiles estándar. NKR proporciona plantillas para servicios comunes:

```dockerfile
# Nkrfile.pg — PostgreSQL 15
FROM postgres:15
ENV POSTGRES_USER=odoo
ENV POSTGRES_PASSWORD=odoo
```

```dockerfile
# Nkrfile.odoo — Odoo 17
FROM odoo:17.0
USER root
COPY deploy/config/odoo.conf /etc/odoo/odoo.conf
RUN mkdir -p /mnt/extra-addons && chown odoo:odoo /mnt/extra-addons
USER odoo
```

## Snapshots Copy-on-Write

Para despliegues multi-tenant, NKR crea snapshots CoW a partir de un disco base:

```bash
cp --reflink=auto odoo-base.ext4 cliente1.ext4
```

En sistemas de archivos que soportan reflinks (btrfs, XFS con reflink), esta operación es instantánea y no consume espacio adicional. En otros, NKR recurre a `cp --sparse=always`.

## Volúmenes

NKR proporciona un sistema de volúmenes para inyectar configuración y persistir datos:

- **Inyección pre-boot:** El disco raíz se monta en modo bucle y los ficheros se copian del host a las rutas del guest
- **Extracción post-apagado:** Los volúmenes marcados con `:rw` se copian de vuelta del guest al host
- **Formato:** `ruta_host:ruta_guest` (solo lectura) o `ruta_host:ruta_guest:rw`

## Variables de Entorno

Las variables de entorno se escriben en `/etc/nkr-env` dentro del disco raíz antes del arranque:

```bash
nkr run --disk pg.ext4 --env POSTGRES_USER=odoo --env POSTGRES_PASSWORD=secreto
```

El initramfs carga este fichero durante el arranque, poniendo las variables a disposición del proceso init del guest.

---

# El Modelo de CPU: «Chrs»

NKR introduce una unidad de asignación de CPU denominada **chr** (pronunciado «cor»):

| Valor | Significado |
|---|---|
| 1 chr | 20% de un core físico |
| 5 chrs | 1 core físico completo |
| 10 chrs | 2 cores físicos |

## Implementación

La asignación de CPU se aplica mediante `sched_setaffinity()`:

```rust
let cores_needed = ((chrs as f32) / 5.0).ceil() as u32;
let cores_to_use = cores_needed.min(num_cpus);
// Pinear el hilo vCPU a los cores [0..cores_to_use]
sched_setaffinity(0, &cpuset);
```

Los chrs son **exclusivos** — el proceso de la VM se pinea a cores físicos dedicados, evitando la contención con otras VMs.

## CPU Bursting con cgroupv2 — **Nuevo en v1.1**

NKR v1.1 añade desbordamiento controlado de CPU mediante el controlador `cpu.max` de cgroupv2. La garantía mínima sigue siendo `1 chr = 20% de un core`, pero la VM puede absorber ciclos ociosos del host sin impactar a otros inquilinos.

```
Configuración cgroupv2 para N chrs:
  cpu.max        →  "{N×20000} 100000"   (N×20% de cuota en cada periodo de 100 ms)
  cpu.max.burst  →  "{N×5000}"           (crédito extra acumulable — kernel ≥ 5.15)
```

La jerarquía se crea en `/sys/fs/cgroup/nkr/{nombre-vm}/` y se elimina al apagar la VM mediante `teardown_cgroup()`.

## `nkr nitro` — Desbloqueo Temporal de CPU

```bash
nkr nitro nazcatex-odoo-01 --duration 10m
```

Escribe `max 100000` en el `cpu.max` de la VM, dándole CPU sin límite durante la duración especificada (por defecto 10m). Un `sh -c "sleep N; echo quota > cpu.max"` desvinculado (detached con `setsid()`) restaura el throttle. Útil para instalar módulos pesados de Odoo (`-i account`, `mrp`, `website`).

## Nitro Dinámico durante el Arrange de Compose

Durante `compose up`, cada servicio con `healthcheck:` pasa por un ciclo automático de CPU:

1. **`nitro_relax_cgroup()`** — establece `cpu.max = max 100000` al arrancar la VM (CPU completa durante el boot)
2. **Health check TCP** — espera a que el puerto del servicio acepte conexiones
3. **`run_warmup()`** — emite GETs HTTP a `/web/assets/debug/*.{css,js}` y `/web/login` para forzar la compilación de assets QWeb antes del primer cliente real
4. **Periodo de gracia de 30s** — mantiene la CPU al máximo para la primera solicitud del backend
5. **`nitro_throttle_cgroup()`** — restaura la cuota de `chrs` configurada

Logs: `[NKR-WARMUP] ✅ X compilado (Ts, N bytes)` por cada asset compilado.

## Limitación de E/S de Disco con cgroupv2 — **Nuevo en v1.1**

La misma jerarquía de cgroupv2 aplica límites de tasa de E/S por dispositivo de bloque:

```
io.max  →  "MAJ:MIN rbps=209715200 wbps=104857600"   (200 MB/s lectura, 100 MB/s escritura)
```

Los números de dispositivo (mayor:menor) se obtienen con `libc::stat()` sobre la ruta del disco. El *enforcement* lo realiza el planificador `blk-mq` del kernel, sin consumo de CPU adicional en el hipervisor.

**Ejemplo de despliegue (servidor de 8 cores):**

| Servicio | Chrs | Cores Usados |
|---|---|---|
| PostgreSQL | 10 (2 cores) | Cores 0–1 |
| Odoo #1 | 5 (1 core) | Core 2 |
| Odoo #2 | 5 (1 core) | Core 3 |
| Odoo #3–#8 | 1 c/u | Cores 4–7 (compartidos) |

---

# Generación de Initramfs

NKR incluye un generador automático de initramfs (`initramfs.rs`, ~410 líneas) que crea entornos de arranque adaptados a cada servicio.

## Secuencia de Arranque

```
El initramfs arranca (PID 1)
    │
    ├─ Montar /proc, /sys, /dev
    ├─ Cargar módulos del kernel:
    │   crc32c → libcrc32c → crc16 → mbcache → jbd2 → ext4
    │   virtio_blk → failover → net_failover → virtio_net
    │   fuse → virtiofs  (si hay shares VirtIO-FS declaradas — v1.3)
    │   virtio_pmem → nd_btt → dax  (si --pmem activo — v1.2)
    │
    ├─ Esperar /dev/vda o /dev/pmem0 (hasta 3 segundos)
    ├─ Parsear nkr.ip= de /proc/cmdline
    ├─ Configurar eth0: IP/24, ruta por defecto → 10.0.{cell_id}.1
    │
    ├─ Montar /dev/vda (o /dev/pmem0 con dax) → /newroot (ext4)
    ├─ Montar discos adicionales /dev/vdb..vde → /newroot/mnt/disk0..3
    ├─ Montar unidades VirtIO-FS (si están en cmdline — v1.3):
    │   mkdir -p /newroot${NKR_FS0_MNT}
    │   mount -t virtiofs virtiofs0 /newroot$mnt
    ├─ Bind-mount de /proc, /sys, /dev en /newroot
    │
    ├─ Escribir /etc/nkr-net.sh (script de configuración de red)
    ├─ Escribir /etc/resolv.conf (DNS: 8.8.8.8, 8.8.4.4)
    ├─ Configurar red vía chroot
    │
    ├─ Detectar init: /sbin/init → systemd → entrypoint Docker
    ├─ Crear wrapper /sbin/nkr-init:
    │   - Cargar /etc/nkr-env (variables de entorno NKR)
    │   - Iniciar watcher hvc0: read -r cmd < /dev/hvc0 (bloqueante)
    │   - Ejecutar el init detectado
    │
    └─ exec switch_root /newroot /sbin/nkr-init
```

## Detección Automática del Entrypoint

Cuando se construye con `nkr pull` o `nkr build`, NKR:

1. Extrae `ENTRYPOINT` + `CMD` de los metadatos de la imagen Docker
2. Monta el disco en modo solo lectura y busca scripts de entrypoint conocidos (`/entrypoint.sh`, `/docker-entrypoint.sh`, etc.)
3. Genera un script init personalizado que carga las variables de entorno NKR y lanza el entrypoint correcto

Esto permite a NKR arrancar imágenes Docker no modificadas — PostgreSQL, PgBouncer, nginx, Redis, Odoo — como micro-VMs sin ninguna modificación de la imagen.

---

# Orquestación con Compose

NKR proporciona un sistema de compose (`compose.rs`, ~1.400 líneas) modelado sobre Docker Compose pero diseñado para la orquestación de VMs.

## Formato del Fichero Compose

```yaml
services:
  db:
    nkr_name: "nazcatex-pg"
    id: 1
    cell_id: 1
    disks: [/mnt/nkr/images/postgres.ext4]
    ram: 2048
    chrs: 5
    ports: ["5432:5432"]
    shares: ["/mnt/nkr/cells/nazcatex/pg/data:/var/lib/postgresql/data:rw"]
    healthcheck:
      port: 5432
      initial_delay: 15
      interval: 5
      retries: 12

  odoo-01:
    nkr_name: "nazcatex-odoo-01"
    id: 3
    cell_id: 1
    disks: [/mnt/nkr/images/odoo.ext4]
    ram: 512
    chrs: 2
    ports: ["8069:8069", "8072:8072"]
    shares:
      - "/mnt/nkr/cells/nazcatex/instances/nazcatex-odoo-01/filestore:/var/lib/odoo:rw"
      - "/mnt/nkr/cells/nazcatex/instances/nazcatex-odoo-01/addons:/mnt/extra-addons"
    environment:
      DB_HOST: "10.0.1.2"
      DB_NAME: "db-nazcatex-odoo-01"
    healthcheck:
      port: 8069
      initial_delay: 20
      interval: 5
      retries: 24
```

## Orden de Arranque Secuencial

Compose arranca los servicios en orden de dependencia:

1. `db` — PostgreSQL, espera sonda TCP en `:5432`
2. `pgbouncer` — espera sonda TCP en `:6432`
3. Todos los servicios `odoo-*` — lanzados en paralelo una vez PgBouncer está sano

## Resolución de Recursos

NKR compose resuelve los recursos de forma inteligente, siguiendo una cadena de prioridad:

| Recurso | Orden de Resolución |
|---|---|
| **Disco** | Ruta YAML → `<dir_yaml>/<nombre>` → `/mnt/nkr/images/<nombre>` |
| **Kernel** | Explícita → `<dir_yaml>/nanolinux` → `/mnt/nkr/kernel/nanolinux` → junto al binario `nkr` |
| **Initramfs** | Explícita → por nombre de servicio → por nombre de disco → heurística → auto-generación |

## Funcionalidades

- **Auto-build:** Si un servicio tiene una sección `build:` y el disco no existe, se construye automáticamente
- **Health checks:** Monitorización TCP con retardo, intervalo y reintentos configurables
- **Modo daemon:** `nkr compose up -d` ejecuta en segundo plano con rotación de logs (máx. 10 MB, 3 rotados)
- **Snapshots CoW:** Creación automática de snapshots cuando un disco base ya está en uso por otra VM
- **IDs deterministas:** Los servicios usan `nkr_name` + `id:` opcional; IDs con scope de célula en `registry.json`
- **Warmup + Nitro dinámico:** Relajación automática de CPU durante el boot, pre-compilación de assets QWeb, periodo de gracia de 30s

## Directorio de Datos NKR

```
/mnt/nkr/                          # Default (variable NKR_DATA_DIR)
├── images/                         # Imágenes de disco ext4 base
├── initramfs/                      # Ficheros .cpio.gz por servicio
│   ├── base/                       # busybox + módulos del kernel (compartido)
│   ├── pg.cpio.gz
│   └── odoo.cpio.gz
├── kernel/                         # nanolinux ELF / bzImage compartido
├── snapshots/                      # Snapshots CoW por stack
├── cell-registry.json              # cell_name → cell_id
├── registry.json                   # "cell/vm" → vm_id (con scope)
└── cells/                          # Directorios de instancias por célula
    └── nazcatex/
        ├── cell.yml
        ├── nkr-compose.yml
        └── instances/
```

---

# Despliegue Multi-Tenant

NKR incluye un conjunto de herramientas de despliegue completo para Odoo 17 multi-tenant.

## Registro de Clientes

Los clientes se definen en `deploy/clients.yml`:

```yaml
global:
  pg_ram: 2048
  odoo_ram: 256
  odoo_chrs: 1
  base_disk: /mnt/nkr/images/odoo-base.ext4
  db_statement_timeout: 60000   # ms — duración máxima de query por tenant (v1.1)
  db_conn_limit: 10             # conexiones simultáneas máx por base de datos (v1.1)

clients:
  - name: acme
    domain: acme.ejemplo.com
    db_name: acme_prod
  - name: globex
    domain: globex.ejemplo.com
    db_name: globex_prod
    ram: 512        # override
    chrs: 2         # override
    db_conn_limit: 20  # override — cliente con mayor carga
```

## Pipeline de Aprovisionamiento

```
mt-provision.sh <nombre-cliente>
    │
    ├── Crear disco CoW:    cp --reflink=auto base.ext4 → <cliente>.ext4
    ├── Generar config Odoo: odoo.conf con db_name, dbfilter, workers=2+
    ├── Generar config nginx: <dominio> → 10.0.{cell_id}.<ip_vm>:8069/8072
    ├── Activar sitio nginx:  ln -s sites-available → sites-enabled
    ├── Recargar nginx:       nginx -s reload
    └── Límites PostgreSQL (v1.1):
        ├── ALTER DATABASE "<db>" SET statement_timeout = '<N>ms';
        └── ALTER DATABASE "<db>" CONNECTION LIMIT <N>;
```

## Odoo Multi-Worker

Cada instancia Odoo usa `workers = 2+` (abandona el modo werkzeug single-thread):
- `:8069` — workers HTTP síncronos
- `:8072` — gevent para long-polling y WebSockets

## Actualización de Módulos en Caliente (*Hot Module Update*)

El script `deploy/update.sh` proporciona actualizaciones de módulos con tiempo de inactividad casi nulo:

| Modo | Comando | Tiempo inactivo |
|---|---|---|
| **Producción** | `update.sh` | ~2s (apagado limpio vía hvc0 + reinicio) |
| **Test** | `update.sh --test` | 0 (ejecuta en puerto 8070) |
| **Rollback** | `update.sh --rollback` | ~2s |
| **Actualizar BD** | `update.sh --update-db` | ~30 segundos |

**Flujo de actualización:**

1. Copia de seguridad automática de módulos actuales (conserva las últimas 5)
2. Detener VM Odoo vía `nkr stop` (SIGTERM → hvc0 SHUTDOWN → salida limpia ~2s; PostgreSQL sigue ejecutándose)
3. Montar disco, rsync de módulos con `__manifest__.py`
4. Reiniciar VM Odoo vía `nkr restart` o compose

## Arquitectura Objetivo

```
Servidor (16–32 GB RAM), 5 células × (1 PG + 1 PgBouncer + 20 Odoos)
│
├── Célula 1 "nazcatex" — nkr-br1, 10.0.1.0/24
│   ├── VM nazcatex-pg          (id=1, 10.0.1.2, 2GB RAM)
│   ├── VM nazcatex-pgbouncer   (id=2, 10.0.1.3, 128MB RAM)
│   ├── VM nazcatex-odoo-01     (id=3, 10.0.1.4, 256MB RAM)
│   └── ... nazcatex-odoo-20   (id=22, 10.0.1.23, 256MB RAM)
│
├── Célula 2 "cafeteria" — nkr-br2, 10.0.2.0/24
│   └── ... (misma estructura)
│
├── nginx (en el host)   — Mapa SNI → IP de célula:8069/8072
└── Puertos expuestos: 80, 443, SSH
    Todo lo demás: interno en el bridge por célula
```

**Escalabilidad de densidad (servidor 32 GB):**

| Escenario | RAM/Instancia | Instancias en 32 GB |
|---|---|---|
| v1.1 base | ~640 MB | 50 |
| v1.2 + PMEM | ~440 MB | 72 |
| v1.2 + PMEM + KSM | ~330 MB | 96 |
| v1.3 todas las features + KSM + Balloon | ~310 MB | **103+** |

*Con KSM activado, la RAM efectiva por instancia Odoo baja a ~330 MB (ahorro adicional del 25–30% por compartición de páginas idénticas de Python/librerías entre VMs de la misma célula).*

---

# Observabilidad y Métricas

NKR incorpora un sistema de telemetría de bajo nivel implementado en el propio hipervisor que mide y expone los recursos utilizados en tiempo real por cada micro-VM, evitando la necesidad de desplegar agentes adicionales dentro de los *guests*.

El motor de métricas extrae información mediante sondas ligeras desde `procfs` y del subsistema de red del host:

- **CPU%**: Ventana de muestreo síncrona de 200 ms analizando `/proc/{pid}/stat`. El intervalo se comparte globalmente si se verifican múltiples VMs simultáneas.
- **RAM (VmRSS)**: RSS físico desde `/proc/{pid}/status`. Muestra la memoria real del host ocupada frente a la RAM pre-asignada a la VM.
- **Disco (E/S)**: Bytes acumulados de lecturas y escrituras (`/proc/{pid}/io`).
- **Red (TAP)**: Transferencia y recepción volumétrica de la interfaz TAP usando `/proc/net/dev`.
- **Estado KSM**: MB ahorrados globalmente por el deduplicador de páginas, leído de `/sys/kernel/mm/ksm/`.

```bash
sudo nkr stats                        # todas las VMs
sudo nkr stats nazcatex-odoo-01       # filtrado por nombre/hash/id
```

## Exportador Prometheus Nativo — **Nuevo en v1.1**

```bash
sudo nkr serve --port 9090
# Expone: http://host:9090/metrics
```

Implementado con solo `std::net::TcpListener` (~30 líneas). Sin crates adicionales.

**Métricas expuestas:**

| Métrica | Tipo | Descripción |
|---|---|---|
| `nkr_cpu_pct{vm="..."}` | Gauge | Porcentaje de CPU consumido (ventana 50 ms) |
| `nkr_rss_mb{vm="..."}` | Gauge | RAM física real (RSS) en MB |
| `nkr_io_read_bytes{vm="..."}` | Counter | Bytes leídos del disco (acumulado) |
| `nkr_io_write_bytes{vm="..."}` | Counter | Bytes escritos al disco (acumulado) |
| `nkr_net_rx_bytes{vm="..."}` | Counter | Bytes recibidos por el TAP (acumulado) |
| `nkr_net_tx_bytes{vm="..."}` | Counter | Bytes enviados por el TAP (acumulado) |
| `nkr_ksm_savings_mb` | Gauge | MB ahorrados globalmente por KSM |

---

# Comparación con Soluciones Existentes

## NKR vs Docker

| Dimensión | Docker | NKR |
|---|---|---|
| **Aislamiento** | Kernel compartido (namespaces + cgroups) | VM completa (KVM, kernel separado) |
| **Vulnerabilidad de kernel** | Afecta a todos los contenedores | Afecta solo a la VM con ese kernel |
| **Garantía de CPU** | *Shares* de cgroups (límite suave) | Core pinning + cgroupv2 (límite duro) |
| **RAM** | *Overcommit* por defecto | Exclusiva, sin *overcommit* |
| **Tamaño del binario** | dockerd ~100 MB + containerd + runc | ~2–4 MB binario único |
| **Tiempo de arranque** | ~1–3 segundos (inicio de proceso) | ~1–2 segundos (arranque de VM) |
| **Tiempo de reinicio** | ~3–5 segundos | ~2 segundos (apagado limpio hvc0) |
| **Formato de disco** | *Overlay filesystem* por capas | ext4 en bruto (snapshots CoW) |
| **Red** | veth + bridge | TAP + bridge por célula + iptables |
| **Multi-stack** | Compose manual por stack | `nkr cell` con subnets aisladas |

## NKR vs Firecracker

| Dimensión | Firecracker | NKR |
|---|---|---|
| **Lenguaje** | Rust | Rust |
| **Interfaz KVM** | Directa (`kvm-ioctls`) | Directa (`kvm-ioctls`) |
| **VirtIO** | MMIO | MMIO |
| **Enfoque** | *Serverless* (AWS Lambda) | SaaS multi-tenant (Odoo) |
| **Gestión de discos** | Externa | Integrada (`nkr pull/build`, OCI→ext4) |
| **Orquestación** | Ninguna (externa: containerd) | Integrada (`nkr compose`, `nkr cell`) |
| **Herramientas MT** | Ninguna | Completas (Cell System, clonación de instancias) |
| **Inyección de volúmenes** | Externa | Integrada (montaje pre-boot + VirtIO-FS) |
| **Modelo de CPU** | vCPU estándar | «Chrs» (granularidad del 20% con pinning) |
| **Apagado** | Matar proceso | VirtIO-Console coordinado (~2s) |
| **Líneas de código** | ~70.000+ | ~7.900 (alcance enfocado) |

## NKR vs QEMU/KVM

| Dimensión | QEMU/KVM | NKR |
|---|---|---|
| **Tamaño del binario** | ~20–50 MB | 2–4 MB |
| **Modelo de dispositivo** | Emulación x86 completa (PCI, USB, ACPI...) | Solo VirtIO-MMIO mínimo |
| **Configuración** | CLI compleja / XML libvirt | Flags CLI simples / YAML |
| **Tiempo de arranque** | ~3–10 segundos | ~1–2 segundos |
| **Dependencias** | libvirt, qemu, virt-manager | Ninguna (solo `/dev/kvm`) |
| **Superficie de ataque** | Grande (emulación completa) | Mínima (6 tipos de dispositivo MMIO) |

---

# Modelo de Seguridad

## Fronteras de Aislamiento

| Capa | Mecanismo |
|---|---|
| **CPU** | Virtualización hardware KVM (VT-x/AMD-V). El guest ejecuta en ring 0 de un espacio de direcciones separado. |
| **Memoria** | `GuestMemoryMmap` crea regiones de memoria dedicadas. Sin memoria compartida entre VMs. |
| **Disco** | Cada VM tiene su propio fichero ext4. Sin *overlay filesystem* compartido. |
| **Red** | Dispositivo TAP separado por VM. Bridge L2 por célula. Reglas iptables por VM. Reglas ebtables L2 (v1.1): solo la MAC+IP asignada puede emitir tráfico. |
| **Proceso** | Cada VM se ejecuta como un proceso host separado. SIGTERM → hvc0 → apagado limpio. Estado zombie detectado vía `/proc/pid/status`. |
| **Syscalls** | *Jailer* Seccomp BPF (v1.2) restringe el proceso vCPU a ≤31 syscalls permitidas tras la inicialización. |

## Superficie de Ataque

La superficie de ataque de NKR es significativamente menor que la de Docker y QEMU:

- **Sin emulación de dispositivos en espacio de usuario** (vs QEMU): solo manejadores MMIO nativos (net, block, VirtIO-FS, Balloon, PMEM, Console + serie)
- **Sin kernel compartido** (vs Docker): un exploit del kernel en el guest no afecta al host
- **Sin rutas de escape de contenedor**: sin namespaces, cgroups ni compartición de procfs
- **Interacción mínima con el host**: solo E/S de ficheros (disco/mmap), lectura/escritura TAP (red) y salida serie
- **Aislamiento L2** (v1.1): reglas ebtables previenen IP/MAC spoofing entre VMs de inquilinos en el bridge
- **Aislamiento L3 por célula** (v1.3): subnets por célula; el routing inter-célula no está habilitado por defecto

## *Jailer* Seccomp BPF — **Nuevo en v1.2**

**Implementación:** `seccomp.rs` — ~170 líneas

Antes de entrar en el bucle de ejecución del vCPU, NKR instala un programa `SECCOMP_MODE_FILTER` construido en tiempo de ejecución a partir de una allowlist estática de 31 syscalls. El filtro usa `libc::prctl` directamente, sin dependencias adicionales.

- **Preámbulo:** `prctl(PR_SET_NO_NEW_PRIVS, 1)` (requerido por el kernel antes de instalar el filtro)
- **Política:** `SECCOMP_RET_KILL_PROCESS` para cualquier syscall fuera de la allowlist
- **Allowlist incluye:** `read`, `write`, `ioctl` (KVM ioctls), `mmap`, `madvise`, `clone` (thread::spawn), `futex`, `io_uring_*`, `epoll_*`, `eventfd2`, `openat`, `pread64/pwrite64`, `clock_gettime`, `exit_group` y esenciales de stdlib
- **Timing:** Se instala *después* de `VirtioNetDevice::new()` (que hace spawn del hilo RX)
- **Degradación:** Si `prctl` falla (kernel < 3.17 o permisos denegados), NKR emite un aviso y continúa sin el filtro

## Seguridad Operacional

- Solo los puertos 80, 443 y SSH (configurable) están expuestos externamente
- Todo el tráfico inter-VM está confinado al bridge por célula
- Requiere acceso root para KVM/TAP/iptables (intencionado — sin modo sin root)
- El filtro Seccomp (v1.2) restringe el proceso vCPU a la huella mínima de syscalls

---

# Limitaciones y Trabajo Futuro

## Limitaciones Actuales

| Limitación | Impacto | Resolución Planificada |
|---|---|---|
| **vCPU único por VM** | No se puede usar SMP en los guests | Soporte multi-vCPU (prioridad media) |
| **Solo VirtIO-MMIO** | Sin paso a través de PCI | Suficiente para las cargas de trabajo objetivo |
| **VirtIO-FS atado a vhost-user** | Necesita demonio externo `virtiofsd` | Automatización de setup en versión futura |
| **PMEM requiere soporte en kernel guest** | `CONFIG_VIRTIO_PMEM=y` + `CONFIG_FS_DAX=y` necesarios | Documentado; degradación silenciosa a VirtIO-Block |
| **Sin migración en vivo** | Hay que detener la VM para moverla entre hosts | Trabajo futuro |
| **Sin snapshots en caliente** | Hay que detener la VM para hacer snapshot del disco | Trabajo futuro |
| **Sin pruebas automatizadas** | Solo pruebas manuales | Suite de pruebas unitarias e integración |
| **Solo host Linux** | Requiere Linux con KVM | Por diseño |
| **ebtables opcional** | Aislamiento L2 solo si ebtables instalado | Migración a nftables bridge en versión futura |
| **IPs del compose son literales** | Cambiar topología de célula requiere regenerar compose | Sintaxis de placeholders (`${PG_IP}`) planificada |

## Hoja de Ruta

**Implementado en v1.1:**
- `mt-compose-gen.sh` genera `nkr-compose.yml` automáticamente ✓
- VirtIO-FS para compartición de directorios con DAX ✓
- Exportador Prometheus (`nkr serve`) ✓
- Aislamiento L2 ebtables ✓
- `statement_timeout` + `conn_limit` por tenant ✓
- cgroupv2 `cpu.max` + `cpu.max.burst` bursting ✓

**Implementado en v1.2:**
- Cargador ELF vmlinux (–20 ms de arranque) ✓
- E/S asíncrona io_uring (~70% reducción de syscalls) ✓
- VirtIO-PMEM + DAX (–150–200 MB/VM de page cache) ✓
- *Jailer* Seccomp BPF ✓

**Implementado en v1.3:**
- Sistema de Células (multi-stack con L2/L3 por célula) ✓
- VirtIO-FS con DAX reemplazando VirtIO-9P (3–5× más rápido) ✓
- VirtIO-Balloon (recuperación de RAM ociosa) ✓
- VirtIO-Console hvc0 (apagado coordinado ~2s) ✓
- `nkr cell clone` (duplicación atómica de instancias con DB) ✓
- `nkr restart` (relanzamiento desvinculado preservando argv original) ✓
- Detección de zombies en `is_pid_alive()` (sin esperas de 90s en vano) ✓
- Flujo de Nitro dinámico durante el arrange del compose ✓

**Alta prioridad:**
- Validación end-to-end con 5 células × 20 Odoos
- Let's Encrypt automatizado vía certbot
- Migración a nftables bridge (reemplazar ebtables)
- IPs placeholder en compose (`${PG_IP}`, `${PGB_IP}`)

**Prioridad media:**
- Soporte de múltiples vCPUs
- Mejoras en VirtIO-FS para mayor estabilidad asíncrona vhost-user
- Copia de seguridad automatizada de PostgreSQL por inquilino

**Prioridad baja:**
- Migración en vivo entre servidores
- Snapshots en caliente sin detener la VM
- Interfaz de gestión web

---

# Conclusión

NKR demuestra que es posible conseguir **densidad y simplicidad operacional a nivel de contenedor** con **aislamiento y garantías de recursos a nivel de VM**, en menos de 7.900 líneas de Rust, compilando a un binario de 2–4 MB sin dependencias en tiempo de ejecución.

La versión 1.3 eleva el techo de densidad a 103+ instancias Odoo en un servidor de 32 GB mediante la combinación de seis optimizaciones: VirtIO-FS + DAX entrega acceso 3–5× más rápido al sistema de archivos eliminando copias entre host y guest. VirtIO-Balloon recupera hasta 300 MB de VMs ociosas sin reiniciarlas. VirtIO-Console hvc0 permite reinicios limpios en ~2s coordinando el apagado con el init del guest en lugar de matar el proceso. El Sistema de Células habilita el despliegue paralelo de Odoo 15, 17 y 19 en el mismo host, cada uno en su propia red L2/L3 aislada sin ningún conflicto de configuración. `nkr cell clone` automatiza la creación de entornos de test desde producción en segundos.

Para los operadores que gestionan docenas de inquilinos SaaS en un único servidor, NKR ofrece un equilibrio fundamentalmente distinto al de Docker o las VMs tradicionales:

- **Cada inquilino obtiene aislamiento hardware**, no solo separación de namespaces
- **Cada inquilino obtiene recursos garantizados**, no pools compartidos de CPU y memoria  
- **El operador mantiene los flujos de trabajo de Docker**, con patrones familiares de build, run y compose
- **La infraestructura se consolida**: 1 PostgreSQL + 1 PgBouncer por célula + N Odoos + 1 nginx en el host; en vez de N stacks completos solapados

NKR es software con propósito. En lugar de intentar ser un hipervisor de propósito general como QEMU o una plataforma de contenedores de propósito general como Kubernetes, NKR se enfoca en un patrón de carga de trabajo específico y de alto valor: **SaaS multi-tenant denso sobre bare metal**. Este enfoque le permite ser lo suficientemente simple como para comprenderse completamente, lo suficientemente pequeño como para auditarse línea a línea, y lo suficientemente rápido como para arrancar en segundos.

---

\newpage

# Apéndice A: Stack Tecnológico

| Componente | Tecnología | Versión | Desde |
|---|---|---|---|
| Lenguaje | Rust | Edition 2021 | v1.0 |
| Interfaz KVM | `kvm-ioctls` | 0.19 | v1.0 |
| Bindings KVM | `kvm-bindings` | 0.10 | v1.0 |
| Memoria del guest | `vm-memory` (GuestMemoryMmap) | 0.14 | v1.0 |
| Cargador de kernel | `linux-loader` (bzImage + ELF) | 0.11 | v1.0 / v1.2 |
| Colas VirtIO | `virtio-queue` | 0.12 | v1.0 |
| CLI | `clap` (derive) | 4.x | v1.0 |
| Serialización | `serde` + `serde_yaml` + `serde_json` | 1.x / 0.9 / 1.x | v1.0 |
| Utilidades del sistema | `vmm-sys-util` | 0.12 | v1.0 |
| E/S asíncrona | `io-uring` | 0.6 | v1.2 |
| Kernel del guest | Linux vmlinux ELF / bzImage | 6.6.117-0-virt | v1.0 |

# Apéndice B: Métricas del Código Fuente

| Módulo | Fichero | Líneas | Responsabilidad |
|---|---|---|---|
| Motor VMM | `vmm.rs` | ~1.600 | Init KVM, PIT2, cargador ELF/bzImage, MMIO, cgroups, ebtables, slot PMEM, seccomp, apagado hvc0 |
| Compose | `compose.rs` | ~1.400 | YAML, orquestación, health checks, modo daemon, flujo Nitro/warmup |
| Sistema de Células | `cell.rs` | ~730 | Registry de células, gestión de bridges, directorios de instancias, `clone_instance` |
| Initramfs | `initramfs.rs` | ~410 | Entornos de boot, carga de módulos FS/PMEM/virtiofs |
| Métricas | `metrics.rs` | ~420 | Telemetría /proc, KSM, exportador Prometheus |
| Red | `net.rs` | ~310 | VirtIO-net, backend TAP, hilos RX/TX, io_uring TX |
| Bloque | `block.rs` | ~310 | VirtIO-block, E/S asíncrona io_uring + fallback síncrono |
| Compartir FS | `virtio_fs.rs` | ~200 | VirtIO-FS (DAX, vhost-user) reemplazando 9P |
| PMEM | `pmem.rs` | ~200 | VirtIO-PMEM + DAX, mmap(MAP_SHARED), manejador FLUSH |
| Pull | `pull.rs` | 201 | Pipeline Docker Hub → ext4 |
| Build | `build.rs` | 192 | Pipeline Nkrfile → ext4 |
| Registry | `registry.rs` | 219 | Asignación persistente nombre → ID con scope de célula |
| Estado | `state.rs` | 272 | Registro de VMs, tracking ciclo de vida, detección de zombies, `nkr ps` |
| Balloon | `balloon.rs` | ~150 | VirtIO-Balloon, evicción MADV_DONTNEED de páginas ociosas |
| Consola | `console.rs` | ~120 | VirtIO-Console (hvc0), inyección SHUTDOWN, poll_pending |
| Seccomp | `seccomp.rs` | ~170 | Construcción filtro BPF + instalación vía prctl |
| CLI | `cli.rs` | ~330 | CLI completa: run/ps/stop/restart/nitro/compose/pull/build/stats/ksm/serve/cell |
| Main | `main.rs` | ~480 | Punto de entrada, dispatch completo incluyendo Cell/Clone |
| **Total** | | **~7.900** | (+~2.200 líneas respecto a v1.2) |

# Apéndice C: Inicio Rápido

```bash
# Compilar NKR desde el código fuente
cargo build --release
# Binario: target/release/nkr (~2–4 MB)

# ── Pull y Build ──────────────────────────────────────────────────────────────
# Descargar imagen PostgreSQL y crear disco
sudo ./target/release/nkr pull postgres:15 postgres.ext4 --size-mb 2048

# Construir desde Nkrfile
sudo ./target/release/nkr build -f Nkrfile.odoo -o odoo.ext4 --size-mb 4096

# ── Ejecución básica ──────────────────────────────────────────────────────────
# Ejecutar una micro-VM
sudo ./target/release/nkr run \
  --disk postgres.ext4 --ram 512 --chrs 1 --id 1 --port 5432:5432

# Ejecutar con compartición VirtIO-FS en vivo
sudo ./target/release/nkr run \
  --disk odoo.ext4 --ram 256 --chrs 1 --id 2 \
  --share /opt/modules:/mnt/extra-addons \
  --share /mnt/nkr/cells/nazcatex/instances/nazcatex-odoo-01/config:/etc/odoo

# Ejecutar con VirtIO-PMEM + DAX (~150–200 MB ahorro de RAM)
sudo ./target/release/nkr run \
  --disk odoo.ext4 --ram 256 --chrs 1 --id 3 --pmem

# Ejecutar con VirtIO-Balloon (recuperar 200 MB de VM ociosa)
sudo ./target/release/nkr run \
  --disk odoo.ext4 --ram 512 --chrs 1 --id 4 --balloon-mb 200

# ── Ciclo de vida ─────────────────────────────────────────────────────────────
sudo ./target/release/nkr ps                           # listar VMs activas
sudo ./target/release/nkr stats                        # CPU/RAM/IO/RED en vivo
sudo ./target/release/nkr stop nazcatex-odoo-01        # apagado limpio vía hvc0
sudo ./target/release/nkr restart nazcatex-odoo-01     # detener + relanzar desvinculado

# ── Nitro (desbloqueo temporal de CPU) ────────────────────────────────────────
sudo ./target/release/nkr nitro nazcatex-odoo-01 --duration 10m

# ── KSM (deduplicación de páginas) ────────────────────────────────────────────
sudo ./target/release/nkr ksm on
sudo ./target/release/nkr ksm status

# ── Métricas Prometheus ───────────────────────────────────────────────────────
sudo ./target/release/nkr serve --port 9090
curl http://localhost:9090/metrics

# ── Sistema de Células ────────────────────────────────────────────────────────
# Crear una célula (registra cell_id, crea bridge nkr-br1, directorios)
sudo ./target/release/nkr cell create nazcatex --odoo-version 17.0

# Listar todas las células
sudo ./target/release/nkr cell ls

# Arrancar el stack completo (requiere nkr-compose.yml en el directorio de la célula)
sudo ./target/release/nkr cell up nazcatex -d

# Ver VMs activas en una célula
sudo ./target/release/nkr cell ps nazcatex

# Detener todas las VMs de una célula
sudo ./target/release/nkr cell down nazcatex

# Eliminar célula del registry (datos preservados)
sudo ./target/release/nkr cell destroy nazcatex

# ── Clonación de Instancias ───────────────────────────────────────────────────
# Clonación completa: ficheros + DB + bloque en compose
sudo ./target/release/nkr cell clone nazcatex-odoo-01 nazcatex-odoo-04

# Smoke test seguro: solo ficheros, sin DB ni modificación del compose
sudo ./target/release/nkr cell clone nazcatex-odoo-01 nazcatex-odoo-04 \
  --no-db --no-compose

# ── Compose ───────────────────────────────────────────────────────────────────
sudo ./target/release/nkr compose up -f nkr-compose.yml -d
sudo ./target/release/nkr compose down -f nkr-compose.yml
sudo ./target/release/nkr compose ps
```

---

*NKR es software de código abierto. Las contribuciones y comentarios son bienvenidos.*

*© 2026 NKR Contributors. Licencia MIT.*
