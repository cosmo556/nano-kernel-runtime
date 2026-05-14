---
title: "Nano-Kernel Runtime (NKR): Un Hipervisor Bare-Metal de Micro-VMs para Cargas de Trabajo SaaS Multi-Tenant"
subtitle: "White Paper — Versión 1.6.4"
date: "Mayo 2026"
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
  - \fancyhead[R]{\small Mayo 2026}
  - \fancyfoot[C]{\thepage}
  - \usepackage{booktabs}
  - \usepackage{longtable}
  - \renewcommand{\arraystretch}{1.3}
  - \usepackage{listings}
  - \lstset{basicstyle=\ttfamily\small, breaklines=true, frame=single, backgroundcolor=\color{gray!10}}
  - \usepackage{xcolor}
---

\newpage

> **Resumen.** El *Nano-Kernel Runtime* (NKR) es un hipervisor bare-metal de código abierto escrito en Rust que reemplaza los *runtimes* de contenedores como Docker por micro-VMs con aislamiento hardware, ejecutándose directamente sobre Linux KVM. NKR está diseñado para operadores que gestionan despliegues SaaS multi-tenant densos —especialmente Odoo ERP— sobre un único servidor con recursos limitados (16–32 GB RAM). Al eliminar la sobrecarga de QEMU, libvirt y el intercambio a nivel de contenedor, NKR consigue aislamiento hardware completo con un binario de tan solo 2–4 MB, arranque de VM en menos de un segundo, planificación exclusiva de CPU (modelo «chrs»), y un flujo de trabajo compatible con Docker para construir imágenes de disco. La versión 1.1 agregó seis capacidades clave: compartición de sistema de archivos en vivo via VirtIO-FS, desbordamiento controlado de CPU (*bursting*) mediante cgroupv2, aislamiento de red L2 con ebtables, límites de base de datos por inquilino, un exportador nativo de métricas Prometheus, y generación automática de ficheros compose multi-tenant. La versión 1.2 introduce cuatro optimizaciones adicionales para superar las 100 instancias Odoo en 32 GB RAM: VirtIO-PMEM + DAX (elimina ~150–200 MB de caché de páginas duplicada por instancia), E/S asíncrona con io_uring (reduce el coste de syscalls ~70% bajo alta concurrencia), carga de kernel ELF vmlinux sin comprimir (~20 ms de arranque más rápido) y un *jailer* Seccomp BPF. La versión 1.3 da un salto de rendimiento, densidad y operabilidad añadiendo: el **Sistema de Células** (stacks multi-VM con red L2/L3 aislada por célula), VirtIO-FS con DAX (reemplaza VirtIO-9P, 3–5× más rápido en I/O de ficheros), VirtIO-Balloon (recuperación de RAM ociosa), un canal VirtIO-Console (hvc0) para apagado coordinado en ~2s, y clonación de instancias (`nkr cell clone`). La versión 1.4 estabiliza la operación multi-tenant: VirtIO-PMEM activo por *default*, *skip warmup* en clones, *filestore rename* dentro del guest (sin loop-mounts en el host), serialización de operaciones netlink (`flock` + `iptables -w`), y *hardening* de validación en todos los bordes del API. La versión 1.5 introduce **separación de privilegios**: el daemon `nkr` corre como root con un socket UDS (`/var/run/nkr.sock`) y todo el frontend HTTP queda en `nkr-api-server`, un proceso *unprivileged* (user `nkr-api`, sin capabilities) que sólo traduce HTTP↔IPC. Un RCE en el parser HTTP no compromete KVM/cgroups/iptables. La versión 1.6 cierra el bucle multi-tenant con un API HTTP completo dirigido por panel externo: clonación de tenants Odoo desde un *template seed* canónico (creado vía `/web/database/create`, con español Latinoamérica precargado), `edition` opt-in per-instancia para activar/desactivar el share enterprise, *admin user password* aplicada vía JSON-RPC al boot del tenant (cierra la ventana de `admin/admin` heredada), explote automático de repos OCA multi-módulo a `addons/<modulo>/`, *cache de estáticos en nginx* (`/web/static/*` 24h, `/web/assets/<hash>/*` 30d) con endpoint `POST /admin/cache/purge` para invalidación explícita, *rate limit* en `/web/login` y `444` (TCP close) sobre paths CMS/legacy. Todos los cambios viven en un único binario sin daemons accesorios y son configurables vía cuatro endpoints REST documentados (`/instances`, `/dns`, `/addons/git`, `/config`). La versión 1.6.1 introduce el **sistema de tiers** (`production` / `staging` / `dev`), un canal HVC0 *REL_OD* para recargar workers Odoo en ~3s sin reiniciar la VM (resolviendo la limitación virtio-fs+inotify), y el modo *edge dual* en nginx (Cloudflare proxied + DNS-only transparentes vía `set_real_ip_from`). La versión 1.6.2 cierra la doctrina de densidad sobre 32 GB con tres piezas: el **ballooning dinámico IDLE/ACTIVE** con decay automático (la VM arranca con squeeze máximo y se desinfla en deflate de ~2s al primer SIGUSR2 del panel), la **Higiene Doble** sobre `POST /addons/git` (`git clean -ffdx` recursivo + wipe completo de `addons/` antes de re-poblar — el tenant queda como espejo determinista del meta-repo) con validación **422 estricta** sobre submódulos, y el **ciclo `chattr +i`** sobre los rootfs maestros (`nkr build` aplica `-i → build → +i` automáticamente), eliminando una clase entera de fallos por mutación accidental del *master inmutable*. La versión 1.6.3 agrega un **watchdog** (sonda TCP de `:8069` por tenant en ejecución; auto-restart tras 60s sin respuesta — hoy se distribuye *deshabilitado* por env var mientras el panel pushea cambios activamente), corrige la doctrina de tiers **vaciando `dev_mode`** en DEV/STAGING (`reload` agota los `inotify` watches del guest bajo virtio-fs → ENOSPC → loop de respawn; `qweb,xml` activa el recompile interno de templates de Odoo en cada request → picos de CPU → cuelgues; el camino canónico de reload es `REL_OD` por HVC0, no `dev_mode`), y sube el perfil DEV a 1300 MB (soft/hard 800/1000) tras observar `Server memory limit reached` ciclando con Odoo 19 + ~31 módulos custom en modo threaded. La versión 1.6.4 cierra un sprint de seguridad/operabilidad: **SSO firmado por HMAC** (`/nkr-sso?u=…&exp=…&sig=…` — el panel emite una URL con TTL de 30s firmada con una clave de 256 bits por-tenant en `odoo.conf` `[nkr_sso] secret`; un módulo Odoo la verifica en tiempo constante y crea una sesión sudo sin exponer jamás el password del usuario al host), **`systemouts-addons`** (un directorio de addons a nivel célula, read-only, montado *antes* del `addons/` del tenant en `addons_path` — los módulos internos como `nkr_sso` viven ahí, invisibles al cliente e intocables por `POST /addons/git`, distribuidos una vez por célula y heredados por los clones vía la DB del template), **`POST /instances` asíncrono** (valida sincrónicamente, despacha el clone en background, devuelve `202` + un endpoint de polling `create-status` — elimina el `504` falso en los arranques lentos de PROD prefork), un **tmpfs sobre `/mnt` en el initramfs** (cualquier share virtio-fs nuevo bajo `/mnt/*` "just works" sin reconstruir el rootfs maestro), el **fix de `POST /reload` para `workers=0`** (el watcher HVC0 detecta threaded vs prefork desde `odoo.conf` → `pkill -TERM` + respawn del supervisor vs `pkill -HUP` al master), el **fix del balloon dinámico para `tier=dev`** (el dispositivo MMIO del balloon se anuncia al guest aunque `balloon_mb=0` al boot, así la inflación del decay a IDLE realmente ocurre; `nkr_balloon_mb` ahora refleja el target runtime), un **log de consola de boot por instancia**, y un **fix de truncación del cmdline del kernel** (omite los params redundantes `nkr.fs0/fsm0/fsr0` del rootfs cuando hay muchos shares virtio-fs). Este documento presenta la arquitectura completa, la implementación y el modelo de despliegue en producción de NKR v1.6.4.

---

\newpage

# Introducción y Motivación

## El Problema

Los proveedores de servicios que gestionan docenas de inquilinos SaaS sobre infraestructura compartida se enfrentan a una tensión fundamental entre **densidad** (maximizar el número de inquilinos por servidor) y **aislamiento** (evitar el efecto vecino ruidoso). Los contenedores Docker ofrecen alta densidad pero comparten el kernel del host, exponen una gran superficie de ataque y no garantizan CPU ni RAM. Las VMs tradicionales (QEMU/KVM con libvirt) proveen un aislamiento sólido, pero imponen una sobrecarga prohibitiva de memoria y disco para despliegues densos.

Considérese un escenario concreto: un operador que gestiona **50 instancias de Odoo 17 ERP** sobre un único servidor de 16–32 GB usando Docker:

| Problema | Impacto con Docker / Odoo.sh | Impacto con NKR |
|---|---|---|
| **Aislamiento** | Compartido (Container / PaaS) | A nivel hardware (Micro-VM KVM) |
| **Garantía de CPU** | Pools compartidos (sin pinning estricto) | Determinista (chrs pinados) |
| **Uso de RAM** | Redundante (page cache duplicada) | Optimizado (VirtIO-PMEM + DAX) |
| **Latencia de reinicio** | ~3 minutos por stack | ~2 segundos (vía hvc0) |
| **Huella de infraestructura** | N Odoo + N PG + N nginx | N Odoo + **1** PG + **1** PgBouncer + **1** nginx |

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
| `0xD001_0000+` | VirtIO-FS shares (+0x1000 c/u, DAX) | 8, 9, 10, … | **v1.3** |
| `0xD002_0000` | VirtIO-PMEM (memoria persistente, DAX) | 7 | v1.2 |
| `0xD004_0000` | VirtIO-Balloon | 10 | **v1.3** |
| `0xD005_0000` | VirtIO-Console (hvc0) | 11 | **v1.3** |

El rango `0xD001_0000+` garantiza que no haya colisión con la zona de bloques (crece hasta `0xD000_9000` con 9 discos). PMEM, Balloon y Console son reservados estáticamente. El guest es PIC-only (16 IRQs legacy, sin APIC), así que las IRQs por encima de las primeras se *comparten* entre dispositivos virtio-mmio — el registro de interrupt-status por-dispositivo de virtio-mmio desambigua cuál disparó, así que compartir es seguro (verificado: todos los dispositivos registran `DRIVER_OK` independientemente del IRQ compartido).

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

Al combinar VirtIO-FS+DAX (deduplica binarios, libs y `.pyc` entre VMs leyendo del mismo backing file del host), VirtIO-PMEM+DAX (elimina la page cache duplicada del rootfs RO en cada guest) y VirtIO-Balloon (recupera RAM ociosa), NKR permite ubicar densidades altas en 32 GB de RAM. **KSM no contribuye a ese ahorro en v1.4+**: el VMM mapea la memoria del guest con `memfd_create + MAP_SHARED` (requisito del protocolo `vhost-user SET_MEM_TABLE` para virtiofsd), y el kernel rechaza `madvise(MADV_MERGEABLE)` sobre VMAs con `VM_SHARED` retornando `EINVAL` silencioso. Por eso `nkr stats` reporta `[KSM] estado=detenido | compartidas=0 | ahorro≈0MB`. La densidad real viene de DAX, no de KSM.

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

# Separación de Privilegios y API HTTP — **Nuevo en v1.5/v1.6 (extendido hasta v1.6.4)**

A partir de v1.5, NKR opera como dos procesos cooperativos con responsabilidades separadas. El objetivo es **mover toda la superficie de ataque expuesta a la red fuera del proceso root**: un fallo en el parser HTTP, en la deserialización JSON, o en la validación de input no debe escalar a control sobre KVM, cgroups o iptables.

## Arquitectura de los dos procesos

```
┌─────────────┐  HTTPS (nginx)   ┌────────────────────┐  UDS framed JSON  ┌──────────────┐
│  Panel de   │ ────────────────▶│  nkr-api-server    │ ─────────────────▶│  nkr (root)  │
│  control    │                  │  127.0.0.1:9090    │  /var/run/nkr.sock│   daemon     │
└─────────────┘                  │  user=nkr-api      │                   └──────────────┘
                                 │  sin capabilities  │                          │
                                 └────────────────────┘                          ▼
                                                                          KVM, cells, PG,
                                                                          iptables, cgroups
```

- **`nkr` (root daemon)** — `src/main.rs serve`. Único proceso con privilegios de hipervisor. Escucha exclusivamente en un *Unix Domain Socket* en `/var/run/nkr.sock` con permisos `0660 root:nkr-api`. No habla HTTP ni TCP. Despacha cada `IpcRequest` a un handler tipado en `src/api.rs`.
- **`nkr-api-server` (proxy unprivileged)** — `src/bin/nkr_api_server.rs`. Corre como user `nkr-api` (uid creado al instalar), sin capabilities, dentro de un sandbox systemd con `NoNewPrivileges`, `ProtectSystem=strict`, `ProtectHome`, `PrivateTmp`, `PrivateDevices`, `RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6`, `MemoryDenyWriteExecute`, y `SystemCallFilter` que excluye `@privileged @resources @mount @cpu-emulation @debug @obsolete`. El proxy traduce HTTP REST → IPC y vuelve. No linkea kvm-ioctls, vmm, cell ni state — el binario pesa ~580 KB vs ~2.2 MB del daemon (verificado).

## Wire IPC

Cada request es un *frame* en el UDS: 4 bytes *length-prefix* en *little-endian* + cuerpo JSON, máximo 8 MiB. La conexión se cierra después de un response — sin multiplexing, sin estado conversacional. El timeout de lectura/escritura por conexión es 120 s para cubrir la operación más larga (`POST /instances` con clone de DB ~30–40 s).

```rust
pub enum IpcRequest {
    Health,
    ListCells,
    RenderMetrics,
    CreateInstance { cell_hint: Option<String>, body_json: String },
    GetInfo { nkr_name: String },
    DeleteInstance { nkr_name: String, drop_db: bool },
    Action { nkr_name: String, action: String },
    GetLogs { nkr_name: String, tail: Option<usize>,
              from_offset: Option<u64>, max_lines: Option<usize>,
              wait_ms: Option<u64> },
    ModulesAction { nkr_name: String, op: String,
                    modules: Vec<String>,
                    admin_login: String, admin_password: String },
    CreateDns { nkr_name: String, dns: String, enable_websocket: bool },
    DeleteDns { nkr_name: String, delete_cert: bool },
    InitDb { /* ... */ },
    PatchConfig { nkr_name: String, body_json: String },
    Psql { nkr_name: String, query: String, max_rows: usize },
    PurgeCache,
}
```

## Catálogo de endpoints HTTP

Todos los endpoints (excepto `/metrics` y `/api/v1/health`) requieren `Authorization: Bearer $NKR_API_TOKEN` con comparación *constant-time* (`netlock::ct_eq`) para evitar *timing attacks*. La validación de identificadores y paths se hace en el proxy y se **re-valida** en el daemon — defensa en profundidad: incluso un atacante con acceso al UDS no puede inyectar shell metacaracteres ni saltar al filesystem fuera de las cells.

| Método | Ruta | Función |
|---|---|---|
| `GET` | `/api/v1/health` | health check (sin auth) |
| `GET` | `/metrics` | Prometheus text exposition 0.0.4 (sin auth) |
| `GET` | `/api/v1/cells` | lista cells con `odoo_version`, `used_odoos`, `free_slots`, `max_odoos` |
| `POST` | `/api/v1/instances` | crea tenant (auto-selección de cell por `odoo_version`) |
| `POST` | `/api/v1/cells/{cell}/instances` | crea tenant forzando cell explícita |
| `GET` | `/api/v1/cells/{cell}/instances/{name}` | info + `nkr_status` (running, pid, ram_mb, uptime_s, port_8069_up) |
| `DELETE` | `/api/v1/cells/{cell}/instances/{name}?drop_db=1` | borra tenant |
| `POST` | `/api/v1/cells/{cell}/instances/{name}/actions` | `{"action":"start\|stop\|restart"}` |
| `GET` | `/api/v1/cells/{cell}/instances/{name}/logs?tail=N` | tail / cursor-paginated logs |
| `GET` | `/api/v1/cells/{cell}/instances/{name}/logs/download` | descarga `odoo.log` completo |
| `POST` | `/api/v1/cells/{cell}/instances/{name}/dns` | provisiona/regenera vhost nginx + Let's Encrypt cert |
| `DELETE` | `/api/v1/cells/{cell}/instances/{name}/dns` | quita vhost (idempotente) |
| `POST` | `/api/v1/cells/{cell}/instances/{name}/init-db` | crea DB inicial (asíncrono, polling vía `nkr_status.db_present`) |
| `PATCH` | `/api/v1/cells/{cell}/instances/{name}/config` | upsert workers/SMTP en `odoo.conf` |
| `POST` | `/api/v1/cells/{cell}/instances/{name}/psql` | SQL sandbox: `-d db-<tenant>` fijo, blacklist de DDL, audit log |
| `POST` | `/api/v1/cells/{cell}/instances/{name}/modules/{op}` | install/upgrade/uninstall vía JSON-RPC |
| `POST` | `/api/v1/cells/{cell}/instances/{name}/addons/git` | clona repo y explota módulos |
| `POST` | `/api/v1/cells/{cell}/enterprise/git` | clona repo enterprise (shared per-cell) |
| `PUT` | `/api/v1/cells/{cell}/instances/{name}/pylibs` | `pip install --target=<pylibs/lib>` |
| `POST` | `/api/v1/admin/cache/purge` | vacía cache nginx server-side |

## Hardening adicional aplicado al proxy

- **Bind default `127.0.0.1`** — el proxy no escucha en la red por default. nginx/Caddy al frente para TLS + ACL de IPs del panel. Override `NKR_BIND_ADDR=0.0.0.0` (emite WARN; sólo para labs aislados).
- **Body size limits** — `POST /instances` 64 KB, `POST /actions` 1 KB, `POST /addons/git` 64 KB, `PUT /pylibs` 256 KB. 413 si se excede.
- **Bounded log reader** — `read_tail_lines` hace seek inverso por chunks de 64 KiB con cap de 4 MiB en RAM. Rechaza DoS con `odoo.log` multi-GB.
- **Thread pool bounded** — `MAX_INFLIGHT=64` handlers HTTP concurrentes; excedentes reciben 503 con `Retry-After: 1`. `InflightGuard` RAII decrementa incluso si el handler entra en `panic!`.
- **Info disclosure mínima** — errores genéricos al cliente (`{"error":"not_found"}`), detalle sólo a stderr del operador.
- **Panic protection** — `read_request` usa `?` en vez de `unwrap()`; request malformado → 400, no thread crash.
- **`verify_dependencies()`** al boot — `nkr serve` chequea 11 binarios críticos (ip, iptables, mount, mkfs.ext4, e2fsck, losetup, chattr, psql, pg_isready, virtiofsd, umount) y loguea los faltantes antes de aceptar tráfico.

---

# Operación multi-tenant: el flujo completo — **Nuevo en v1.6 (extendido hasta v1.6.4)**

NKR v1.6 cierra el bucle entre el panel externo y el daemon: aprovisionar un cliente Odoo nuevo (con su DNS, su DB clonada, su usuario admin con password real, su rate limit, sus addons OCA, su edition correcta) es una secuencia de 4 calls REST. v1.6.1 añade el **tier** como ciudadano de primera clase (`tier=production|staging|dev`) con perfiles fijos para los tiers no-prod, y `POST /reload` para recargar workers Odoo sin reiniciar la VM. v1.6.2 añade `POST /balloon` (renueva el estado ACTIVE del ballooning dinámico) y refina `POST /addons/git` con la Higiene Doble + 422 estricta (espejo determinista del meta-repo). v1.6.3 vacía `dev_mode` en DEV/STAGING (el reload `REL_OD`/HVC0 lo reemplaza) y agrega el watchdog. v1.6.4 agrega el SSO firmado por HMAC + el path de addons interno `systemouts-addons`, `POST /instances` asíncrono (`202` + polling `create-status`), `POST /diag` (forensics host-side), y el tmpfs sobre `/mnt` en el initramfs. Esta sección describe cada pieza y los invariantes que NKR garantiza.

## El template DB seed canónico

Cada cell tiene una instancia reservada llamada `<cell>-odoo-template` con `disabled: true` en el compose. Esta VM **nunca arranca durante operación** — solo es el **origen** de los clones. La DB asociada (`db-<cell>-odoo-template`) se crea **una sola vez** vía el endpoint `/web/database/create` del propio Odoo, no vía `pg_restore` ni dump parcial:

```bash
# Procedimiento (manual, una vez por cell):
# 1. nkr compose up -d (con disabled:false temporalmente)
# 2. Esperar TCP :8069
# 3. POST /web/database/create con master_pwd=admin, lang=es_419,
#    country_code=PE, login=admin, password=admin, demo=False
# 4. pg_dump db-<cell>-odoo-template → /mnt/nkr/templates/<cell>-base-<ts>.sql.gz
# 5. nkr stop + disabled:true
```

**Por qué esto importa**: si el template DB se importa desde un dump parcial, queda con `ir_module_module` poblada pero `ir.asset` vacío y sin algunos hooks `_register_hook` ejecutados. El primer clone del template, al recibir `/web/login`, intenta compilar `web.assets_frontend` con un asset registry incoherente, falla con `Undefined variable: $black` (orden SCSS roto), y serializa el mensaje de error como CSS válido que se cachea en `ir_attachment` — todos los requests siguientes ven el bundle roto. La forma confiable de detectarlo: contar filas en `ir_module_module WHERE state='installed'` (debe ser ≥14, no 7) y verificar que el bundle CSS de un clone pesa >100 KB y no contiene la cadena `"A css error occured"`.

El dump del template bueno se conserva en `/mnt/nkr/templates/<cell>-base-latest.sql.gz` como artifact reproducible. Si una cell se corrompe, restaurar desde ese dump es una operación de minutos.

## `POST /instances` — clone + boot + admin password en una request

El cuerpo del request:

```json
{
  "nkr_name": "cliente-42",
  "mode": "production",
  "odoo_version": "19.0",
  "edition": "community",
  "workers": 2,
  "admin_passwd": "MasterPwd-xxxxxxxx",
  "admin_user_password": "PasswordParaLoginAdmin-yyyy",
  "dns": "cliente-42.tudominio.com"
}
```

NKR procesa en orden:

1. **Validación** — `nkr_name`, `dns`, `admin_passwd` (charset `[A-Za-z0-9._-]{16,128}`), `admin_user_password` (`{8,128}`). Re-validación en el daemon.
2. **Resolución de cell** — si `cell` se omite, auto-selecciona la cell con `odoo_version` igual y menos `used_odoos`. 409 si ninguna disponible.
3. **Derivación de recursos** — `workers=N` produce automáticamente `chrs=2N+1`, `ram_mb=1024·N`, `limit_memory_soft=400·N MB`, `limit_memory_hard=750·N MB`. Para `workers=2`: `ram=2048 MB`, `soft=800 MB`, `hard=1500 MB` (suficiente para instalar `account` con sus ~30 dependencias sin matar el worker por OOM).
4. **Clone de archivos** — `chattr +C` (no-CoW sobre btrfs) + `cp` del rootfs ext4 + creación de `var_lib_odoo.ext4` (filestore vacío).
5. **Clone de DB** — `CREATE DATABASE db-<tenant> TEMPLATE db-<cell>-odoo-template` (file-level copy en PG, ~3 s para 80 MB). El admin user heredado del template tiene `password=admin`.
6. **Edition opt-in** — si `edition=community` (o `null`), `append_compose_block` filtra la línea `/mnt/nkr/enterprise/<v>:/mnt/extra-enterprise:ro` del compose generado y `rewrite_odoo_conf_full` quita `/mnt/extra-enterprise` del `addons_path`. Si `edition=enterprise`, ambos se mantienen → tenant ve los manifests del repo enterprise descargado vía `POST /enterprise/git`. Esto permite mezclar tenants community y enterprise en la misma cell.
7. **Append compose block** — el template del bloque del tenant hereda del template seed con tres overrides forzados: `disabled: false`, `skip_warmup: true`, `logfile = /var/log/odoo/odoo.log`.
8. **`compose up -d`** — el daemon lanza el stack de la cell. Los servicios db/pgbouncer ya están vivos (idempotente); solo arranca el nuevo Odoo.
9. **Si `admin_user_password` está presente** — NKR hace polling en `:8069` hasta TCP up (max 120 s) y luego ejecuta dos JSON-RPC contra el guest:
   ```
   POST /web/session/authenticate { db, login:"admin", password:"admin" }
   POST /web/dataset/call_kw { model:"res.users", method:"change_password",
                                args:["admin", new_password] }
   ```
   Si todo OK, devuelve `201` con tenant arrancado y password aplicada. Si falla, `503 admin_password_setup_failed` con detalle — el tenant queda arrancado pero con `admin/admin`. Esto cierra la ventana donde la password default heredada del template seguía funcionando entre el `POST /instances` y la primera acción del panel.
10. **Persiste `meta.json`** — `<instance_dir>/meta.json` con todos los parámetros originales para que el `GET /instance/{name}` reconstruya el estado.

## `POST /addons/git` — explote automático multi-módulo

Repos OCA típicos contienen múltiples módulos (`account-financial-tools` exporta `account_chart_update`, `account_payment_term_aux`, etc., cada uno en un subdir con su propio `__manifest__.py`). El `addons_path` de Odoo escanea solo el primer nivel de cada path declarado, así que clonar el repo a `addons/account-financial-tools/` deja todos los módulos invisibles.

NKR resuelve esto en el lado servidor:

1. Clone temporal a `addons/.nkr-tmp-<subdir>/`.
2. Detección del layout:
   - `tmp/__manifest__.py` → single-module: rename a `addons/<subdir>/`.
   - `tmp/<m1>/__manifest__.py`, `tmp/<m2>/__manifest__.py`, … → multi-module: cada `<m>/` se mueve a `addons/<m>/`.
   - Sin manifests → `422 no_modules_found`.
3. Cada módulo movido escribe un tracker `addons/<m>/.nkr-source` con `repo_url=`, `ref=`, `sha=`. Esto permite re-clones idempotentes sobre el mismo repo (overwrite legítimo) y detección de conflictos genuinos: si un tenant ya tenía `account_payment` desde `OCA/account-payment` y otro repo intenta reescribirlo, NKR responde `409 module_conflict { existing_repo, attempted_repo }` sin tocar nada.
4. `POST /actions {action:"restart"}` (responsabilidad del panel) recarga manifests y los nuevos `ir.module.module` aparecen para instalar desde la UI.

Este patrón unifica el `addons_path` a una sola ruta (`/mnt/extra-addons`) sin importar el layout de origen.

## SSO HMAC y `systemouts-addons` — **Nuevo en v1.6.4**

El panel puede dejar a un operador logueado dentro del backend Odoo de cualquier tenant sin manejar jamás el password del usuario. `POST /cells/{cell}/instances/{name}/sso {user}` devuelve una URL `https://<dns>/nkr-sso?u=<login>&exp=<unix_ts>&sig=<hmac_sha256(secret, "u|exp")>` con TTL de 30 segundos. NKR escribe una clave HMAC fresca de 256 bits por-tenant en `odoo.conf` bajo la sección `[nkr_sso]` clave `secret` al clonar (`cell.rs::rewrite_odoo_conf_full` — fuera de `[options]` para evitar el WARNING benigno `unknown option` de Odoo; el legacy `nkr_sso_secret` en `[options]` sigue funcionando como fallback). Un controller HTTP de Odoo (`auth="none"`) verifica la firma en tiempo constante (`hmac.compare_digest`), chequea el expiry, busca al usuario activo, y crea una sesión sudo — `request.session.session_token = user._compute_session_token(request.session.sid)` — y redirige a `/odoo`. El password del usuario nunca sale del host; comprometer el secret implica login arbitrario a *ese* tenant, así que rotar = editar `[nkr_sso] secret` + `POST /actions {restart}`. Defensa en profundidad opcional: un filtro `nkr_sso_allowed_referer` en `odoo.conf`.

El módulo que hace esto — `nkr_sso` — vive en **`systemouts-addons`**, un directorio a nivel célula (`cells/<cell>/systemouts-addons/`) montado **read-only** en cada instancia como `/mnt/systemouts-addons` e insertado en `addons_path` *antes* de `/mnt/extra-addons` (el `addons/` propio del tenant). Consecuencias: (1) un módulo del cliente con el mismo nombre que uno interno no puede shadowearlo (gana el primer match, y el path interno va primero); (2) `POST /addons/git` nunca toca `systemouts-addons` → el cliente no lo ve, edita ni sobrescribe; (3) una copia por célula, RO — cero trabajo per-tenant. `nkr_sso` viene pre-instalado en cada `<cell>-odoo-template` (código en `systemouts-addons/nkr_sso/` + `state=installed` en la DB del template) así que los clones heredan el código (vía el share RO a nivel célula) y el estado instalado (vía `CREATE DATABASE … TEMPLATE`); sólo el secret se regenera fresh por tenant. El mountpoint `/mnt/systemouts-addons` está horneado en el rootfs maestro (y, desde v1.6.4, el initramfs monta un tmpfs sobre `/mnt` así que cualquier share `/mnt/*` *futuro* funciona sin rebuild del maestro — ver §Initramfs).

## Hardening del edge nginx — rate limit y `444`

Cada vhost de tenant generado por `POST /dns` incluye:

```nginx
include /etc/nginx/snippets/nkr-hardening.conf;   # 444 sobre extensiones
                                                  # legacy y paths CMS

location ~ ^/web/(login|database/selector|database/manager) {
    limit_req zone=nkr_login_limit burst=5 nodelay;
    limit_req_status 429;
    proxy_pass http://up_<tenant>;
}
```

El `nkr-hardening.conf` (compartido por todos los tenants) responde `444` (cierre TCP sin respuesta) sobre:
- archivos terminados en `.php`, `.env`, `.sql`, `.bak`, `.yml`, `.tar`, `.gz`, `.zip` — Odoo no sirve nada de eso, son scanners
- paths internos `/.git/(...)`, `/.svn/(...)` — bloquea descubrimiento de repos públicos accidentales
- substrings `wp-admin`, `wp-login`, `MyAdmin`, `phpmyadmin`, `adminer` — herramientas CMS conocidas

`444` es preferible a `403/404` porque no confirma que el server existe (anti-fingerprint) y no consume bandwidth para escribir un response.

El rate limit `3 r/s burst=5 nodelay` sobre las rutas de login y database manager protege contra brute-force. Funciona porque Cloudflare está en modo DNS-only (no proxied), así que `$binary_remote_addr` es la IP real del atacante. Una zona global de 10 MB rastrea ~160 k IPs únicas. Bursts de hasta 5 requests pasan inmediatos (cubre el caso del usuario que tipea login + recordar password); del 6° en adelante recibe `429`. Brute-force típico (10+ requests/seg) se bloquea al ~6° intento.

## Cache server-side de estáticos

`/web/static/*` (assets de módulos: imágenes, fonts, css de templates) y `/web/assets/<hash>/*` (bundles compilados) se cachean en disco en `/var/cache/nginx/nkr_static` con `proxy_cache_path keys_zone=nkr_static:50m max_size=2g inactive=24h`. Para que el cache funcione sobre el server block del tenant —que tiene `proxy_buffering off` para SSE/long-polling— cada `location` cacheable redeclara `proxy_buffering on` localmente.

| ruta | TTL en cache | invalidación |
|---|---|---|
| `/web/static/*` | 24 h | `POST /admin/cache/purge` (URL fija — el cliente cambia el logo y la URL no cambia) |
| `/web/assets/<hash>/*` | 30 d (`Cache-Control: public, immutable`) | automática — el `<hash>` cambia cuando el bundle cambia |

`POST /admin/cache/purge` ejecuta un `rm -rf /var/cache/nginx/nkr_static/*` desde el daemon root y devuelve `{purged, size_bytes_freed}`. Reconstrucción es orgánica: la próxima request a un asset cacheado va a Odoo y vuelve a poblar la entry. Cost aceptable: sub-segundo de overhead la primera vez que cada asset se vuelve a pedir.

`nginx -s reload` **no purga** el cache — los archivos en disco sobreviven reloads, restarts del servicio y reboots del host. Sin este endpoint, la única alternativa era SSH al host.

## sitecustomize.py — IP real del cliente en el log de Odoo

Por default, werkzeug y `gevent.pywsgi` (el server detrás de `:8072` longpolling) loguean `REMOTE_ADDR` — la IP de la conexión TCP que recibe la request. Como nginx hace `proxy_pass`, esa IP siempre es `10.0.x.1` (gateway de la cell), no la del cliente. `proxy_mode = True` en `odoo.conf` corrige el comportamiento interno de Odoo (sesiones, security_groups), pero **no afecta los handlers de log** de werkzeug ni gevent.

NKR resuelve esto inyectando un `sitecustomize.py` en `<tenant>-overrides/` que se carga automáticamente por Python al arrancar (vía `PYTHONPATH=/tmp/nkr-overrides:$PYTHONPATH` exportado por el `nkr-start.sh` del initramfs). El script monkey-patchea dos métodos:

```python
import werkzeug.serving as _ws
_orig_addr = _ws.WSGIRequestHandler.address_string
def _nkr_addr(self):
    h = getattr(self, 'headers', None)
    if h is not None:
        ip = h.get('X-Real-IP') or (h.get('X-Forwarded-For') or '').split(',')[0].strip()
        if ip: return ip
    return _orig_addr(self)
_ws.WSGIRequestHandler.address_string = _nkr_addr

import gevent.pywsgi as _pw
_orig_fmt = _pw.WSGIHandler.format_request
def _nkr_fmt(self):
    env = getattr(self, 'environ', None) or {}
    ip = env.get('HTTP_X_REAL_IP') or (env.get('HTTP_X_FORWARDED_FOR') or '').split(',')[0].strip()
    if ip:
        orig = self.client_address
        try:
            self.client_address = (ip, orig[1] if isinstance(orig, tuple) else 0)
            return _orig_fmt(self)
        finally:
            self.client_address = orig
    return _orig_fmt(self)
_pw.WSGIHandler.format_request = _nkr_fmt
```

El initramfs adicionalmente hace `chown odoo:odoo /var/log/odoo` antes del privilege drop para que Odoo (corriendo como user `odoo` en el guest) pueda escribir el `logfile = /var/log/odoo/odoo.log` sobre el share virtio-fs RW. Sin ese chown, Odoo cae a stdout silenciosamente y el host nunca ve `odoo.log`, dejando el endpoint `GET /logs` con response vacío.

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
| v1.3 + VirtIO-FS DAX | ~370 MB | ~85 |
| v1.4+ todas las features + Balloon | ~310 MB | **~100** |

*La densidad efectiva viene de tres mecanismos coordinados: VirtIO-FS+DAX comparte código (binarios, libs, `.pyc`) entre VMs leyendo del mismo backing file del host; VirtIO-PMEM+DAX elimina la copia del rootfs RO en la page cache del guest; VirtIO-Balloon retira RAM ociosa del guest y la devuelve al host vía `MADV_DONTNEED`. **KSM no aplica en este layout** (ver §VirtIO-Balloon — el `memfd+MAP_SHARED` requerido por vhost-user es incompatible con `MADV_MERGEABLE`).*

> **Nota técnica sobre las métricas de densidad.** Nuestras mediciones actuales proyectan
> un consumo de **70–90 GB para 100 instancias bajo carga activa**, representando un 55%
> de ahorro frente a un setup Docker tradicional. El objetivo de **100+ instancias en
> 32 GB de RAM** es un escenario *Optimized High-Density* alcanzable cuando los working
> sets de los guests están mayormente inactivos: se logra apoyándose en VirtIO-Balloon
> para recuperar la RAM ociosa de cada guest y en VirtIO-FS con DAX para compartir los
> binarios de código y los `.pyc` entre VMs, reduciendo la huella efectiva por inquilino
> a ~310 MB. Ambas cifras son válidas; describen el mismo sistema bajo distintas
> hipótesis de carga.

---

# Observabilidad y Métricas

NKR incorpora un sistema de telemetría de bajo nivel implementado en el propio hipervisor que mide y expone los recursos utilizados en tiempo real por cada micro-VM, sin necesidad de desplegar agentes dentro de los *guests*. Hoy estas métricas son **host-side** — lo que cada VM le cuesta al host (RSS del proceso VMM, tiempo de CPU del VMM, bytes de la TAP, I/O de bloque del VMM). Las métricas internas del guest (RAM/CPU/disco *vistos desde dentro* del tenant — útiles para mostrarle al cliente "tu Odoo usa X de Y RAM") están diseñadas pero aún no implementadas; ver *Hoja de Ruta*.

El motor de métricas extrae información mediante sondas ligeras desde `procfs` y del subsistema de red del host:

- **CPU%**: Ventana de muestreo síncrona de ~50 ms sobre `/proc/{pid}/stat` (200 ms para el CLI `nkr stats`). Jittery por la ventana corta — promediar en Grafana para dashboards, o derivar un counter (`nkr_guest_cpu_seconds_total`, planeado).
- **RAM (VmRSS)**: RSS físico desde `/proc/{pid}/status`. La métrica clave de densidad — memoria real del host que la VM cuesta *ahora*, frente a la RAM pre-asignada a la VM.
- **Balloon**: MB actualmente inflados en el VirtIO-Balloon (= devueltos al host). Refleja el estado **runtime** — 0 cuando ACTIVE, p.ej. 256 cuando un tenant DEV decayó a IDLE. Se actualiza en cada transición ACTIVE↔IDLE (v1.6.4).
- **Ahorro DAX**: estimación de RAM no duplicada como page cache del guest (`max(0, ram_allocated − rss − balloon − 50 MB overhead)`) para VMs con un path DAX (rootfs virtio-fs/pmem).
- **Disco (E/S)**: Bytes acumulados de lecturas/escrituras desde `/proc/{pid}/io`. *Salvedad*: en tenants Odoo el rootfs es virtio-fs servido por un proceso `virtiofsd` aparte → esas lecturas no las ve el VMM; este counter sólo cubre el block device (`/var/lib/odoo`), así que suele ser bajo o 0.
- **Red (TAP)**: Transferencia y recepción volumétrica de la interfaz TAP usando `/proc/net/dev`.
- **Estado KSM**: lectura de `/sys/kernel/mm/ksm/` para visibilidad operativa. En v1.4+ esta métrica reporta `0 MB` por diseño: el layout `memfd+MAP_SHARED` requerido por vhost-user es incompatible con `MADV_MERGEABLE` y el kernel rechaza la merge. Se conserva la lectura para detectar si algún día se reactiva con un layout de memoria híbrido.

```bash
sudo nkr stats                        # todas las VMs
sudo nkr stats nazcatex-odoo-01       # filtrado por nombre/hash/id
```

## Exportador Prometheus Nativo — **Nuevo en v1.1**

`GET /metrics` en `:9090` (servido por `nkr-api-server`, **sin auth** — pensado para scrape directo de Prometheus; ponerlo en una red privada o detrás de una allowlist de IPs), exposition `text/plain; version=0.0.4`. Cada scrape toma una ventana de CPU de ~50 ms. Implementado con `std::net::TcpListener` + un string builder, sin crates extra.

**Métricas expuestas** (todas las per-VM llevan `vm="<nkr_name>"`):

| Métrica | Tipo | Descripción |
|---|---|---|
| `nkr_cpu_pct{vm}` | Gauge | % CPU del proceso VMM (ventana ~50 ms) — jittery; promediar en Grafana |
| `nkr_rss_mb{vm}` | Gauge | RAM física real (RSS) del proceso VMM, en MB |
| `nkr_ram_allocated_mb{vm}` | Gauge | RAM asignada a la VM al boot (`compose.ram`) |
| `nkr_balloon_mb{vm}` | Gauge | RAM inflada en el VirtIO-Balloon — valor runtime (0 ACTIVE / p.ej. 256 IDLE) |
| `nkr_dax_savings_mb{vm}` | Gauge | RAM estimada ahorrada por DAX/virtio-pmem (sin duplicación de page cache) |
| `nkr_total_savings_mb{vm}` | Gauge | `balloon_mb + dax_savings_mb` |
| `nkr_io_read_bytes{vm}` / `nkr_io_write_bytes{vm}` | Counter | Bytes de block device leídos/escritos por el VMM (acumulado) — ver salvedad virtio-fs arriba |
| `nkr_net_rx_bytes{vm}` / `nkr_net_tx_bytes{vm}` | Counter | Bytes recibidos/enviados por el TAP (acumulado) |
| `nkr_ksm_savings_mb` | Gauge | MB ahorrados globalmente por KSM (en v1.4+ siempre 0; ver §VirtIO-Balloon) |

**Agregado en v1.6.4:** `nkr_cpu_seconds_total{vm}` / `nkr_cpu_throttled_seconds_total{vm}` (counters del `cpu.stat` del cgroup de la VM — superseden al jittery `nkr_cpu_pct`), `nkr_cgroup_memory_bytes{vm}` (de `memory.current`), `nkr_up{vm,cell,tier}` (1/0, incluye tenants parados — métrica info: `metric * on(vm) group_left(cell,tier) nkr_up`), `nkr_build_info{version}`, y totales de cluster (`nkr_vm_count`, `nkr_total_{rss,balloon,dax_savings}_mb`).

**Endpoint JSON per-instancia (v1.6.4):** `GET /api/v1/cells/{cell}/instances/{name}/metrics` devuelve un snapshot JSON de *una* VM (cgroup CPU/mem, balloon, ahorro DAX, RSS, red, IO, un array `disk` vía `du`/`stat` host-side, cell/tier, uptime, chrs, `as_of`/`stale`). El daemon cachea el resultado ~30s por VM (el `du` de disco ~5min) → un panel que pollea cada 30-60s mientras una pestaña "Métricas" está abierta no cuesta prácticamente nada — la caché *es* el rate-limit (sin 429s). Esto es lo que usa el panel SaaS para la vista de métricas per-tenant; `/metrics` (Prometheus) queda para un Grafana futuro. El disco per-VM a propósito *no* está en `/metrics` (un `du` por scrape × 100+ VMs sería O(segundos)).

**También agregado en v1.6.4 — RAM interna del guest:** `nkr_guest_mem_total/available/free/cached_bytes{vm}` (y `guest_mem` en el JSON per-instancia) — RAM *como la ve el guest* (`MemTotal/MemAvailable/MemFree/Cached`), vía el stats virtqueue del virtio-balloon (la 3ª queue del device, `VIRTIO_BALLOON_F_STATS_VQ`; el vmm la drena cada ~30s y persiste el snapshot al state file de la VM). Es lo que el panel le muestra al cliente como "tu Odoo usa X de Y RAM".

*Aún no expuesto* (sólo ops, futuro): métricas del host (`/proc/meminfo`, `/proc/stat`, `statvfs` → "cuánta RAM queda en el server").

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

## NKR vs Odoo.sh (PaaS)

| Dimensión | Odoo.sh (PaaS) | NKR (Private Cloud) |
|---|---|---|
| **Modelo de aislamiento** | Containers compartidos | Micro-VMs aisladas por hardware |
| **Control de infraestructura** | Gestionada (caja negra) | Control total (kernel / OS / red) |
| **Densidad de memoria** | Limitada por arquitectura PaaS | Ultra-alta (DAX + ballooning) |
| **Despliegue multi-versión** | Complejo entre proyectos | Nativo vía Cell System |
| **Latencia de I/O** | Variable (cloud público) | Predecible (NVMe mirror + io_uring) |

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
- **Separación de privilegios** (v1.5): el frontend HTTP (parser, deserialización JSON, validación de input) corre en un proceso unprivileged sin capabilities. Un RCE en el proxy no compromete KVM/cgroups/iptables — el atacante queda confinado a lo que el daemon le permita vía IPC, todo re-validado en el daemon
- **Validación doble** (v1.6): identificadores (`nkr_name`, `cell`, `dns`) y paths (`addons_path`) pasan whitelist regex tanto en el proxy como en el daemon; YAML/shell/path-traversal injection bloqueados en ambos bordes
- **Edge nginx** (v1.6): rate limit con `$binary_remote_addr` real (Cloudflare DNS-only), `444` close sobre paths CMS/legacy + dirs `.git/.svn`, cache server-side de estáticos para reducir hits a workers Python

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
| **Cache de nginx granularidad global** | `POST /admin/cache/purge` afecta a todos los tenants; no per-tenant selectivo | `ngx_cache_purge` (módulo third-party) requiere recompilar nginx |
| **Upgrade community→enterprise post-creación** | Requiere `PATCH /config` manual de `addons_path` + restart; el remount del share enterprise no es automático | Endpoint `POST /edition/upgrade` planificado |
| **Bots headless avanzados pasan rate limit** | Bots tipo Puppeteer con un request cada 9s no son brute-force, no caen en el rate limit | CF Bot Fight Mode / fail2ban patrón User-Agent / IP reputation |

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

**Implementado en v1.4:**
- VirtIO-PMEM activo por default (`pmem: true`) ✓
- `skip_warmup: true` en clones (auto-inyectado por `append_compose_block`) ✓
- Filestore rename dentro del guest (sin loop-mounts en host) ✓
- Serialización de operaciones netlink (`flock /tmp/nkr-netlink.lock`) ✓
- `iptables -w 5` (espera del xtables lock del kernel) ✓
- Hardening de validación en bordes del API ✓
- Janitor cron interno (cada 5 min limpia mounts/cgroups/loops/locks huérfanos) ✓

**Implementado en v1.5:**
- Separación de privilegios: daemon root (UDS) + proxy unprivileged (HTTP) ✓
- 16 endpoints REST documentados ✓
- Systemd hardening del proxy (CapabilityBoundingSet vacío, MemoryDenyWriteExecute, …) ✓
- Constant-time Bearer compare ✓
- Async init-db (202 + polling) ✓
- Mejor clasificación de errores git (401 auth, 404 not_found, 504 timeout, …) ✓

**Implementado en v1.6:**
- Template DB seed canónico vía `/web/database/create` con `lang=es_419` ✓
- Edition opt-in per-instance (community/enterprise filtering automático) ✓
- `admin_user_password` aplicada vía JSON-RPC al boot del tenant ✓
- Auto-explote de repos OCA multi-módulo a `addons/<modulo>/` con tracker `.nkr-source` ✓
- Cache nginx server-side (`/web/static/*` 24h, `/web/assets/<hash>/*` 30d) ✓
- Endpoint `POST /admin/cache/purge` ✓
- Hardening edge nginx (444 sobre `.php/.git/.zip` + paths CMS, rate limit en `/web/login`) ✓
- `sitecustomize.py` para IP real del cliente en log werkzeug + gevent.pywsgi ✓
- `chown odoo:odoo /var/log/odoo` en initramfs antes del privilege drop ✓
- PgBouncer ram subido a 128 MB (era 64 MB, dejaba poco margen) ✓

**Implementado en v1.6.1:**
- **Sistema de tiers** (`production` / `staging` / `dev`) con perfiles fijos para tiers no-prod ✓ — *nota: el `dev_mode=reload,qweb,xml` original para DEV/STAGING se quitó en v1.6.3 (ver abajo); `dev_mode` queda vacío en todos los tiers* ✓
- Sizing canónico (fuente de verdad: `api.rs::derive_resources_for_tier`): STAGING (1024 MB, workers=0, soft=600/hard=700, balloon boot/ACTIVE=256, IDLE=768), PROD (`max(1024, 512 + 768·W)` MB, soft=`640·W`/hard=`768·W`, balloon=0). DEV empezó en 768/400/512 y se subió a **1300 MB, soft=800/hard=1000** en v1.6.2 (ver abajo) ✓
- Canal HVC0 `REL_OD` — SIGUSR1 al PID de la VM → vmm inyecta `REL_OD\n` por hvc0 → guest dispatch (SIGTERM+supervisor para threaded, SIGHUP master para prefork). ~3s sin reiniciar la VM ✓
- Supervisor loop en `nkr-start.sh` (`while true; do exec odoo; sleep 1; done`) para threaded mode ✓
- Endpoint `POST /reload` y auto-reload por defecto tras `POST /addons/git` ✓
- Edge dual Cloudflare (proxied + DNS-only transparentes, `set_real_ip_from` con rangos CF + `real_ip_header CF-Connecting-IP`) ✓
- Initramfs SIGTERM grace 25s → 5s (tenants Odoo bajan de ~70s a ~25s en restart) ✓
- DELETE asíncrono (cierra la ventana de timeout HTTP del panel) ✓
- DNS guest defensivo: bind-mount `/etc/resolv.conf` con 1.1.1.1 + 8.8.8.8, default route derivada de `GUEST_IP` ✓
- `iptables -I FORWARD 1` para que las reglas NKR pasen por encima de UFW ✓
- `KillMode=process` en systemd unit (las VMs sobreviven `systemctl restart nkr`) ✓

**Implementado en v1.6.2:**
- **Ballooning IDLE/ACTIVE** dinámico por tier: la VM arranca en estado IDLE (squeeze máximo, balloon=ram-256), el panel marca ACTIVE vía `POST /balloon` → vmm aplica `set_target_mb(active) + IRQ config_change` (~2s deflate). Tras 600s sin renovación, decay automático a IDLE. PROD se queda estático (balloon=0 siempre) para evitar latencia de desinflado en picos de tráfico ✓
- **Safety check de SIGUSR2**: el daemon valida `/proc/<pid>/cmdline` antes de mandar la señal (las VMs lanzadas con un binario previo a 1.6.2 no tienen handler y serían terminadas — devuelve `202 applied=false` en su lugar) ✓
- **Higiene Doble** sobre `POST /addons/git`: `git clean -ffdx` recursivo (parent + cada submódulo) + wipe completo de `addons/` antes de re-poblar. `addons/` queda como espejo determinista del meta-repo. Nuevo campo `removed` en la response (módulos que estaban antes y ya no vienen) ✓
- **Validación 422 estricta** de submódulos: cada `path = X` declarado en `.gitmodules` debe ser módulo Odoo (manifest al raíz) o agrupador (con `.gitmodules` propio). Submódulos sin manifest ni agrupador → `422 submodule_no_manifest`. Doctrina: "el meta-repo no es un basurero de scripts" ✓
- **Ciclo `chattr +i`** del master ext4: `nkr build` desbloquea (`-i`) antes de escribir y vuelve a bloquear (`+i`) al final. Cubre todo path bajo `/mnt/nkr/images/`. Reflinks (`cp --reflink=auto`) siguen funcionando contra master inmutable ✓
- Fórmula de RAM PROD canónica: `VM_RAM ≥ 256 (OS) + 256 (master) + 768 × workers`, con validación en API (`400 ram_insufficient_for_workers`) ✓
- Regla del Grifo: workers > 4 → balloon=0 forzado en compose ✓
- Workers=0 (threaded) **obligatorio** en DEV/STAGING (no override permitido); workers configurable 1..16 sólo en PROD ✓

**Implementado en v1.6.3:**
- **`dev_mode` vaciado en todos los tiers** — `reload` agota el `fs.inotify.max_user_watches` del guest (default 8192) al recursar sobre `/usr/lib/python3/.../odoo/addons` → `OSError [Errno 28]` → Odoo muere rc=1 → loop de respawn del supervisor → `:8069` nunca levanta (postmortem en `BUG_inotify_dev_mode.md`). `qweb,xml` activa el `watchdog` interno de Odoo que recompila templates QWeb/XML en *cada* request (incluidos los keepalive de nginx cada 30s) → presión CPU/GC correlacionada con cuelgues del proceso `nkr` host-side. El hot-reload canónico es `REL_OD` por HVC0; `dev_mode=` queda vacío para producción *y* dev/staging ✓
- **Perfil DEV subido a 1300 MB** (soft/hard 800/1000) tras `Server memory limit reached` ciclando con Odoo 19 + ~31 módulos custom en modo threaded (el soft de 400 MB era inalcanzable bajo carga DEV normal) ✓
- **Watchdog** (`watchdog.rs`) — thread del daemon, sonda TCP de `:8069` por tenant en ejecución cada 15s; tras `HUNG_THRESHOLD_SECS=60` fallos consecutivos, auto-`restart` vía `api::handle_action`. Bypass con `NKR_WATCHDOG_DISABLED=1`. **Hoy se distribuye deshabilitado** a pedido (los auto-restart interferían con el panel pusheando cambios activamente) ✓
- Restart más rápido: diff per-módulo en `POST /addons/git` + timer drain reducido ✓

**Implementado en v1.6.4 (sprint de seguridad/operabilidad):**
- **SSO firmado por HMAC** — `POST /cells/{cell}/instances/{name}/sso {user}` emite `https://<dns>/nkr-sso?u=<login>&exp=<ts>&sig=<hmac_sha256(secret, "u|exp")>` (TTL 30s), firmada con una clave HMAC de 256 bits por-tenant escrita por `cell.rs::rewrite_odoo_conf_full` en `odoo.conf` sección `[nkr_sso]` clave `secret` (fuera de `[options]` — evita el WARNING benigno `unknown option` de Odoo; el legacy `nkr_sso_secret` en `[options]` sigue funcionando como fallback). El módulo Odoo `nkr_sso` verifica la firma en tiempo constante (`hmac.compare_digest`) + chequea expiry + crea una sesión sudo (`request.session.session_token = user._compute_session_token(sid)`) — **el password del usuario nunca sale del host**. Rotar = editar `[nkr_sso] secret` + `POST /actions {restart}` ✓
- **`systemouts-addons`** — `cells/<cell>/systemouts-addons/` montado RO en cada instancia como `/mnt/systemouts-addons`, insertado en `addons_path` *antes* de `/mnt/extra-addons` (el `addons/` propio del tenant) → un módulo del cliente con el mismo nombre que uno interno no puede shadowearlo. `POST /addons/git` nunca lo toca → el cliente no lo ve ni lo puede sobrescribir. Una copia por célula, RO. Hoy contiene `nkr_sso/`; pre-instalado en cada `<cell>-odoo-template` (código + `state=installed` en la DB del template) → los clones heredan ambos vía `CREATE DATABASE … TEMPLATE` + el share RO a nivel célula. El secret se regenera fresh por tenant ✓
- **`POST /instances` asíncrono** — valida sincrónicamente (todos los 4xx al toque), despacha el clone en un thread, devuelve `202 {nkr_name, poll}`. El panel pollea `GET /cells/{cell}/instances/{name}/create-status` hasta `status=ready|failed`; status file en `/mnt/nkr/cells/{cell}/.nkr-creates/{name}.json` (a nivel célula, sobrevive al rollback del clone). Elimina el `504` falso cuando PROD prefork bootea ~140s — más que el timeout HTTP del panel/Cloudflare ✓
- **`POST /diag`** — captura host-side de stacks/wchan/cpu de los threads del proceso `nkr` del tenant (text/plain, ~50ms, idempotente) — forensics pre-restart ✓
- **Tmpfs sobre `/mnt` en el initramfs** — tras montar el rootfs maestro RO, el initramfs monta un tmpfs sobre `/newroot/mnt` → cualquier share virtio-fs con un guest path nuevo bajo `/mnt/*` (`mount -t virtiofs … /mnt/foo`) hace `mkdir -p` sobre el tmpfs y "just works" **sin reconstruir el rootfs maestro** (que es RO en el guest) ✓
- **Fix de `POST /reload` para `workers=0`** — el watcher HVC0 lee `workers = N` del `odoo.conf` del guest: vacío/`0` → `pkill -TERM /usr/bin/odoo` (el supervisor loop de `nkr-start.sh` lo respawnea con código fresh); `>0` → `pkill -HUP` master (prefork). Sin downtime de la VM en ningún caso. Obsoleta el workaround "usar `POST /actions {restart}` para workers=0" ✓
- **Fix del balloon dinámico para `tier=dev`** — `vmm.rs::configure_linux_boot` ahora anuncia el `virtio_mmio.device` del balloon al guest cuando hay ballooning dinámico configurado (`balloon_idle_mb ≠ balloon_mb`), no sólo cuando `balloon_mb > 0`. Antes, DEV (que bootea ACTIVE con `balloon_mb=0`) nunca recibía el device en el cmdline → el guest nunca attacheaba el driver → el `set_target_mb(256)` del decay a IDLE era un no-op. Ahora sí infla. `state::update_balloon_mb` escribe el target runtime al state file de la VM en cada transición ACTIVE↔IDLE → `nkr_balloon_mb` / `nkr ps` reflejan el target actual, no el de boot ✓
- **Log de consola de boot por instancia** — cada VM escribe la consola serial del guest (echos del initramfs + `dmesg`) + stderr del VMM a `<instance>/.<config>-vm-boot.log` (se trunca en cada boot) — diagnostica mounts virtio-fs, panics del guest, truncación del cmdline ✓
- **Fix de truncación del cmdline del kernel** — `COMMAND_LINE_SIZE` es chico (~1024 B); con muchos shares virtio-fs el cmdline se truncaba (se perdían `init=` / `nkr.ip=` del final). Fix: omitir los params redundantes `nkr.fs0/fsm0/fsr0` del rootfs (el initramfs lo monta vía `nkr.rootfs=`, no `nkr.fs0=`) y emitir `nkr.fsr{i}=` sólo cuando es `ro` (la ausencia ⇒ `rw`). ~60 B de holgura ✓

**Alta prioridad:**
- Validación end-to-end con 5 células × 20 Odoos en un único host
- Tests automatizados sobre el API HTTP (hoy sólo unit tests en `fsutil`)
- IPs placeholder en compose (`${PG_IP}`, `${PGB_IP}`)
- Migración a nftables bridge (reemplazar ebtables)

**Prioridad media:**
- Endpoint `POST /edition/upgrade` (community→enterprise sin recreate)
- Granularidad per-tenant en cache purge (vía `ngx_cache_purge` recompilado)
- Soporte de múltiples vCPUs
- Copia de seguridad automatizada de PostgreSQL por inquilino
- Pre-compile de QWeb en build-time del template (eliminar 5s de primer boot)
- **Vista global/host de ops** — métricas del host (`/proc/meminfo`, `/proc/stat`, `statvfs` → "cuánta RAM/CPU/disco queda en el server") + un dashboard que junte eso con el agregado per-VM. (Todas las métricas de *tenant* — host-side per-VM, `nkr_up{vm,cell,tier}`, build info, totales de cluster, más el endpoint JSON per-instancia con CPU/mem del cgroup, disco vía `du`, y RAM interna del guest vía el stats virtqueue del virtio-balloon — entregadas en v1.6.4.)
- Paridad de la célula v17: pre-instalar `nkr_sso` en el template `odoo-v17` (el mountpoint `/mnt/systemouts-addons` ya está en el rootfs maestro v17)

**Prioridad baja:**
- Migración en vivo entre servidores
- Snapshots en caliente sin detener la VM
- KSM con layout de memoria híbrida (anon-private + memfd-shared) — solo si 110+ VMs no entran en 32 GB

---

# Conclusión

NKR demuestra que es posible conseguir **densidad y simplicidad operacional a nivel de contenedor** con **aislamiento y garantías de recursos a nivel de VM**, en aproximadamente 21.700 líneas de Rust, compilando a dos binarios (~2.4 MB el daemon, ~660 KB el proxy) sin dependencias en tiempo de ejecución.

La versión 1.3 elevó el techo de densidad a 103+ instancias Odoo en un servidor de 32 GB mediante VirtIO-FS + DAX, VirtIO-Balloon, VirtIO-Console hvc0, el Sistema de Células y `nkr cell clone`. Las versiones 1.4–1.6 transforman NKR de un *runtime* operado manualmente a una **plataforma SaaS dirigida por API**: el daemon root expone exclusivamente un UDS interno y todo el frontend HTTP queda en un proceso *unprivileged* sin capabilities, con endpoints REST cubiertos por validación doble (proxy + daemon), rate limit per-IP, *cache* de estáticos en nginx, *hardening* contra scanners (444 sobre paths CMS/legacy), y un pipeline de aprovisionamiento de tenants que gestiona DB, DNS, certificados, password de admin user y addons OCA en una secuencia de 4 calls. Las versiones 1.6.1–1.6.4 cierran la doctrina de operación: el **sistema de tiers** desacopla los perfiles de iteración (DEV/STAGING con `workers=0` threaded, supervisor loop, **`dev_mode` vacío** — el reload `REL_OD`/HVC0 reemplaza a `dev_mode=reload`, incompatible con virtio-fs y que agota los `inotify` del guest) de la operación productiva (PROD con prefork multi-worker, sin auto-reload); el canal **HVC0 `REL_OD`** resuelve la limitación virtio-fs+inotify recargando workers en ~3s sin reiniciar la VM (threaded → `pkill -TERM` + respawn del supervisor; prefork → `pkill -HUP` al master); el **ballooning dinámico IDLE/ACTIVE** con decay automático maximiza la densidad bajo carga real (32 GB host → ~110 instancias DEV en estado idle, desinfladas a demanda); la **Higiene Doble + 422 estricta** convierten `addons/` en un espejo determinista del meta-repo, eliminando deriva entre `git push` y el filesystem del tenant; el **ciclo `chattr +i`** sobre el master ext4 cierra una clase entera de fallos por mutación accidental del rootfs compartido; el **SSO firmado por HMAC** + el path de addons read-only a nivel célula **`systemouts-addons`** permiten al panel auto-loguear usuarios en cualquier tenant (URL firmada con TTL de 30s, el password nunca sale del host) usando un módulo Odoo interno que el cliente no puede ver ni sobrescribir; **`POST /instances` asíncrono** + el polling `create-status` eliminan el `504` falso en arranques lentos de PROD; y un **tmpfs sobre `/mnt` en el initramfs** hace que los shares virtio-fs nuevos "just work" sin reconstruir el master inmutable.

Para los operadores que gestionan docenas de inquilinos SaaS en un único servidor, NKR ofrece un equilibrio fundamentalmente distinto al de Docker, las VMs tradicionales o un PaaS hosteado:

- **Cada inquilino obtiene aislamiento hardware**, no solo separación de namespaces
- **Cada inquilino obtiene recursos garantizados**, no pools compartidos de CPU y memoria — el panel sólo elige `workers=N` y NKR deriva ram, chrs, soft/hard memory limits con una fórmula determinista
- **El operador mantiene los flujos de trabajo de Docker**, con patrones familiares de build, run y compose
- **La infraestructura se consolida**: 1 PostgreSQL + 1 PgBouncer por célula + N Odoos + 1 nginx en el host; en vez de N stacks completos solapados
- **El panel externo gestiona todo vía REST**: crear tenant, clonar repos OCA con explote automático, aplicar admin password, provisionar DNS+TLS, cambiar workers, ejecutar SQL sandboxed, tail de logs en cursor — sin SSH al host

NKR es software con propósito. En lugar de intentar ser un hipervisor de propósito general como QEMU o una plataforma de contenedores de propósito general como Kubernetes, NKR se enfoca en un patrón de carga de trabajo específico y de alto valor: **SaaS multi-tenant Odoo denso sobre bare metal**. Este enfoque le permite ser lo suficientemente simple como para comprenderse completamente, lo suficientemente pequeño como para auditarse línea a línea, lo suficientemente rápido como para arrancar en segundos, y lo suficientemente robusto como para soportar 100+ tenants productivos detrás de un único panel de control.

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

(Aproximado, a v1.6.4 — `~21.700` líneas de Rust en total en `src/`.)

| Módulo | Fichero | Líneas | Responsabilidad |
|---|---|---|---|
| Handlers de API | `api.rs` | ~3.930 | Dispatch IPC, ciclo de vida de instancias (validación sync + clone async), DNS, init-db, módulos, sandbox psql, cache purge, `handle_sso`/`handle_diag`/`handle_create_status`, `derive_resources_for_tier` |
| Motor VMM | `vmm.rs` | ~2.860 | Init KVM, PIT2, cargador ELF/bzImage, MMIO, cgroups, ebtables, slot PMEM, seccomp, apagado hvc0 + `REL_OD`, state machine del balloon dinámico, ensamblado del cmdline |
| Sistema de Células | `cell.rs` | ~2.740 | Registry de células, gestión de bridges, directorios de instancias, `clone_instance`, edition opt-in, `rewrite_odoo_conf_full` (secret HMAC, share `systemouts-addons` + addons_path, `dev_mode` vacío) |
| Proxy HTTP | `bin/nkr_api_server.rs` | ~2.600 | Traducción HTTP→IPC, validación, límites de body, thread pool, auth, `/metrics`, PUT pylibs |
| Compose | `compose.rs` | ~1.670 | YAML, orquestación, health checks, modo daemon, flujo Nitro/warmup, log de consola de boot por instancia |
| Initramfs | `initramfs.rs` | ~1.080 | Entornos de boot (template per-instancia), tmpfs sobre `/mnt`, carga de módulos FS/PMEM/virtiofs, watcher hvc0 (detección de modo `REL_OD`), inyección de sitecustomize.py |
| Compartir FS | `virtio_fs.rs` | ~685 | VirtIO-FS (DAX, vhost-user) reemplazando 9P |
| Métricas | `metrics.rs` | ~630 | Telemetría /proc, KSM, exportador Prometheus |
| Main | `main.rs` | ~510 | Punto de entrada, dispatch completo incluyendo Cell/Clone |
| Estado | `state.rs` | ~450 | Registro de VMs, tracking ciclo de vida, detección de zombies, `nkr ps`, `update_balloon_mb` |
| Balloon | `balloon.rs` | ~400 | VirtIO-Balloon, evicción MADV_DONTNEED de páginas ociosas |
| Bloque | `block.rs` | ~390 | VirtIO-block, E/S asíncrona io_uring + fallback síncrono |
| Red | `net.rs` | ~365 | VirtIO-net, backend TAP, hilos RX/TX, io_uring TX |
| CLI | `cli.rs` | ~360 | CLI completa: run/ps/stop/restart/nitro/compose/pull/build/stats/ksm/serve/cell |
| Helpers HTTP de API | `api_http.rs` | ~350 | Regexes de validación, helpers de rutas |
| Janitor | `janitor.rs` | ~350 | Cron interno, limpieza de huérfanos |
| Fsutil | `fsutil.rs` | ~300 | Creación de ext4 con `chattr +C`, chequeos de integridad |
| IPC | `ipc.rs` | ~285 | Wire JSON con length prefix sobre UDS (incl. `Sso`/`Diag`/`GetCreateStatus`) |
| PMEM | `pmem.rs` | ~280 | VirtIO-PMEM + DAX, mmap(MAP_SHARED), manejador FLUSH |
| Servidor IPC | `ipc_server.rs` | ~250 | Bucle de requests UDS, dispatch a `api::*` |
| Consola | `console.rs` | ~165 | Dispositivo VirtIO-Console (hvc0) |
| Watchdog | `watchdog.rs` | ~155 | Sonda TCP `:8069` por tenant, auto-restart a 60s (deshabilitado por defecto) |
| Seccomp | `seccomp.rs` | ~135 | Jailer BPF (allowlist ~120 syscalls) |
| **Total** | | **~21.700** | (~19.100 en el binario daemon `nkr` + ~2.600 en el `nkr-api-server` unprivileged) |

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

# ── API HTTP (operación moderna v1.6) ─────────────────────────────────────────
# El daemon corre como servicio systemd; el panel externo gestiona todo vía REST.

# Arrancar daemon root + proxy unprivileged:
sudo systemctl enable --now nkr.service              # daemon root, UDS only
sudo systemctl enable --now nkr-api-server.service   # proxy HTTP localhost:9090

# Health (sin auth):
curl http://nkr-host:9090/api/v1/health
# → {"ok":true,"version":"1.6.4"}

# Crear tenant production (con admin password aplicada al boot):
curl -X POST http://nkr-host:9090/api/v1/instances \
  -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
  -d '{
    "nkr_name": "cliente-42",
    "mode": "production",
    "odoo_version": "19.0",
    "edition": "community",
    "workers": 2,
    "admin_passwd": "MasterPwd-xxxxxxxxxxxxxx",
    "admin_user_password": "PasswordAdminLogin-yyyy",
    "dns": "cliente-42.tudominio.com"
  }'

# Provisionar DNS + cert Let's Encrypt + vhost nginx:
curl -X POST http://nkr-host:9090/api/v1/cells/odoo-v19/instances/cliente-42/dns \
  -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
  -d '{"dns":"cliente-42.tudominio.com","enable_websocket":true}'

# Clonar repo OCA multi-módulo (auto-explote a addons/<modulo>/):
curl -X POST http://nkr-host:9090/api/v1/cells/odoo-v19/instances/cliente-42/addons/git \
  -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
  -d '{"repo_url":"https://github.com/OCA/account-financial-tools.git","ref":"19.0"}'
# → { "module_count": 23, "modules": [...] }

# Restart para que Odoo recargue los manifests:
curl -X POST http://nkr-host:9090/api/v1/cells/odoo-v19/instances/cliente-42/actions \
  -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
  -d '{"action":"restart"}'

# Tail de logs (cursor-paginated, soporta long-poll):
curl -H "Authorization: Bearer $TOKEN" \
  "http://nkr-host:9090/api/v1/cells/odoo-v19/instances/cliente-42/logs?tail=100"

# Vaciar cache nginx (tras actualizar logo o assets en /web/static/):
curl -X POST -H "Authorization: Bearer $TOKEN" \
  http://nkr-host:9090/api/v1/admin/cache/purge
```

---

*NKR es software de código abierto. Las contribuciones y comentarios son bienvenidos.*

*© 2026 NKR Contributors. Licencia MIT.*
