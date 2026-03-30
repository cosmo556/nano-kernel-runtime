# AI-README — Guía para IAs que trabajen en este proyecto

> **Este archivo es una referencia técnica completa para cualquier IA (Copilot, Claude, GPT, etc.) que asista en el desarrollo de NKR. Léelo antes de hacer cualquier cambio.**

---

## 0. Contexto de Negocio — Por qué existe NKR

### El Problema

El autor mantiene **~50 clientes con ~50 instancias de Odoo** en producción sobre Docker. Esta arquitectura tiene problemas críticos de escalabilidad:

| Problema Docker                    | Impacto                                                                     | Solución NKR                                      |
|------------------------------------|-----------------------------------------------------------------------------|---------------------------------------------------|
| **Disco: imagen Odoo pesa ~1.5 GB** | 50 instancias × 1.5 GB = **~75 GB** solo en imágenes (sin datos)           | Disco ext4 compartible + deltas mínimos           |
| **RAM: ~1 GB por instancia Odoo**  | 50 × 1 GB = **50 GB de RAM** (imposible en servidor de 16-32 GB)           | Micro-VMs con RAM exclusiva y ajustada al mínimo  |
| **CPU compartido sin exclusividad** | Picos de uso llegan al 100% → **colas de espera** entre usuarios de distintos clientes | Modelo de **chrs** (CPU pinning exclusivo): cada VM tiene % de core garantizado |
| **Reinicio de stack: ~3 minutos**  | Cada deploy/actualización requiere restart; **clientes no toleran 3 min de downtime** | Hot-update de módulos (~5s downtime) sin reiniciar VM |
| **Deploy lento (GitHub → restart)**| Ciclo: push → pull repo → restart contenedores → **minutos de espera**     | Sync módulos en disco + restart solo de Odoo (~5s) |
| **Networking: 1 PG + 1 Odoo por cliente** | 50 clientes = **100 contenedores** con 50 nginx configs                    | **1 PostgreSQL compartido** + N Odoos + **1 nginx** global |

### Arquitectura Objetivo

```
Servidor (16-32 GB RAM, KVM habilitado)
│
├── 1× PostgreSQL (micro-VM, 1-2 GB RAM)
│   └── Todas las bases de datos de los 50 clientes
│
├── N× Odoo (micro-VMs, ~256-512 MB RAM c/u)
│   ├── cliente-1 (id=2, IP 10.0.0.3, puerto interno 8069)
│   ├── cliente-2 (id=3, IP 10.0.0.4, puerto interno 8069)
│   ├── ...
│   └── cliente-50 (id=51, IP 10.0.0.52, puerto interno 8069)
│
├── nginx (instalado directo en el host, NO en micro-VM)
│   └── Proxy reverso + SSL (Let's Encrypt), routea por dominio → IP interna del Odoo
│
└── Puertos expuestos al exterior: SOLO 80, 443, 5566 (SSH)
    Todos los demás puertos CERRADOS
```

### Flujo de Deploy (Objetivo)

```
Desarrollador push a GitHub
        ↓
Webhook → git pull en /opt/nkr/modules/
        ↓
sudo ./deploy/update.sh
        ↓
Sync módulos al disco Odoo (mount + rsync)
        ↓
Restart solo Odoo (~5 segundos)
        ↓
Cliente operativo de nuevo
```

### Prioridades del Proyecto

1. **Reducir uso de disco** — Compartir kernel y base del filesystem entre instancias
2. **Reducir uso de RAM** — Micro-VMs con RAM mínima y exclusiva (no overcommit)
3. **Garantizar CPU** — Chrs exclusivos para evitar colas de espera entre clientes
4. **Minimizar downtime** — Hot-update de módulos en <10 segundos
5. **Consolidar infraestructura** — 1 PG + N Odoos + nginx en el host (no 50×2 contenedores)
6. **Seguridad de red** — Solo puertos 80, 443, 5566 expuestos; todo lo demás interno en `nkr0`

---

## 1. ¿Qué es NKR?

**Nano-Kernel Runtime (NKR)** es un hipervisor bare-metal escrito en Rust que reemplaza Docker para ejecutar contenedores como **micro-VMs** directamente sobre `/dev/kvm`, sin dependencias externas (no usa QEMU, libvirt ni containerd).

Cada "contenedor" es una VM completa con:
- Kernel Linux real (bzImage)
- Disco ext4 (rootfs extraído de imágenes OCI/Docker Hub)
- Red VirtIO con TAP backend y bridge (`nkr0`)
- Aislamiento total a nivel de hardware (KVM)

**Caso de uso principal:** Plataforma multi-tenant de Odoo 17 con ~50 clientes en un servidor de 16-32 GB, reemplazando Docker por micro-VMs con recursos garantizados (CPU, RAM) y deploys de ~5 segundos.

---

## 2. Stack Tecnológico

| Componente         | Tecnología                                                  |
|--------------------|-------------------------------------------------------------|
| Lenguaje           | Rust (edition 2021)                                         |
| Hipervisor         | KVM directo (`/dev/kvm`) via `kvm-ioctls`                   |
| Memoria guest      | `vm-memory` (GuestMemoryMmap)                               |
| Carga de kernel    | `linux-loader` (bzImage)                                    |
| Dispositivos I/O   | VirtIO-MMIO (bloque, red) — implementación propia           |
| CLI                | `clap` v4 (derive)                                          |
| Serialización      | `serde` + `serde_yaml` + `serde_json`                       |
| Networking         | TAP devices + bridge `nkr0` + iptables (NAT/port-forward)   |
| Build de discos    | Docker como motor de build → export → disco ext4            |
| Sistema operativo  | Requiere Linux con KVM habilitado                           |
| Binario final      | `target/release/nkr` (~2-4 MB, LTO + strip)                |

---

## 3. Estructura del Proyecto

```
nano-kernel-runtime/
├── Cargo.toml              # Dependencias Rust (rust-vmm ecosystem)
├── bzImage                 # Kernel Linux pre-compilado para las VMs
├── nkr-compose.yml         # Definición del stack Odoo+PG (como docker-compose)
├── Nkrfile.odoo            # Dockerfile para construir disco Odoo
├── Nkrfile.pg              # Dockerfile para construir disco PostgreSQL
├── Nkrfile.nginx           # Dockerfile para proxy reverso nginx+certbot
│
├── src/                    # ═══ Código Rust principal ═══
│   ├── main.rs             # Entry point — dispatch de subcomandos CLI (~87 líneas)
│   ├── cli.rs              # Definición de CLI con clap (Run, Ps, Stop, Compose, Pull, Build) (~137 líneas)
│   ├── vmm.rs              # Motor VMM completo (~1130 líneas):
│   │                         - Creación de VM KVM (IRQ chip, PIT, memory)
│   │                         - Boot protocol Linux x86_64 (page tables, GDT, zero page)
│   │                         - Dispositivos VirtIO-MMIO (bloque + red)
│   │                         - Bridge nkr0 auto-setup (ensure_bridge)
│   │                         - Volúmenes (inject pre-boot / extract post-shutdown)
│   │                         - Variables de entorno (/etc/nkr-env)
│   │                         - Port forwarding (iptables DNAT/SNAT)
│   │                         - CPU pinning (modelo de "chrs")
│   │                         - Bucle vCPU con emulación MMIO
│   │                         - Shutdown limpio (SIGTERM handler)
│   ├── block.rs            # VirtIO Block Device (~205 líneas, lectura/escritura sectores 512B)
│   ├── net.rs              # VirtIO Net Device (~286 líneas, TAP backend, RX thread, TX queue)
│   ├── compose.rs          # Orquestador multi-servicio YAML (~792 líneas):
│   │                         - up/down/ps, healthchecks, auto-build
│   │                         - NKR Data Directory (/mnt/nkr o NKR_DATA_DIR)
│   │                         - Snapshots CoW (cp --reflink=auto)
│   │                         - Resolución inteligente de disco/kernel/initramfs
│   │                         - Rotación de logs (10 MB máx, 3 rotados)
│   │                         - Modo daemon con PID file (/tmp/nkr-compose.pid)
│   ├── pull.rs             # Descarga imagen OCI → disco ext4 (~108 líneas, docker create/export)
│   ├── build.rs            # Construye disco ext4 desde Nkrfile (~163 líneas, docker build + export)
│   └── state.rs            # Tracking de VMs activas en /tmp/nkr-vms/*.json (~215 líneas)
│
├── deploy/                 # ═══ Scripts de deploy para producción (Odoo 17) ═══
│   ├── setup.sh            # Setup inicial: descarga imágenes, crea discos, initramfs
│   ├── start.sh            # Inicia PG + Odoo como VMs independientes
│   ├── stop.sh             # Detiene stack (SIGTERM → SIGKILL fallback)
│   ├── update.sh           # Hot-update de módulos Odoo (~262 líneas):
│   │                         - Modo default: sync + restart (~5s downtime)
│   │                         - --test: probar en puerto 8070 sin tocar prod
│   │                         - --rollback: restaurar backup anterior
│   │                         - --update-db: forzar -u all en Odoo
│   │                         - Backup automático antes de actualizar
│   ├── clients.yml         # Registro de clientes multi-tenant (YAML)
│   ├── mt-common.sh        # Funciones comunes para parsear clients.yml
│   ├── mt-provision.sh     # Provisionar clientes: disco CoW, config Odoo, nginx
│   └── config/
│       └── odoo.conf       # Config Odoo inyectada vía --volume
│
├── tools/                  # ═══ Utilidades ═══
│   ├── setup-net.sh        # Script para crear bridge/TAP manualmente
│   ├── initramfs/          # Initramfs base con init script para guests (~189 líneas)
│   │   └── init            # Script init del guest (módulos, red, switch_root)
│   ├── mods/               # Módulos del kernel para initramfs (6.6.117-0-virt)
│   └── modules/            # Directorio para módulos Odoo custom
│
└── target/                 # Build output (no editar)
    └── release/nkr         # Binario final compilado
```

---

## 4. Arquitectura del VMM (vmm.rs)

### Layout de Memoria del Guest (x86_64)

| Dirección       | Contenido                        |
|-----------------|----------------------------------|
| `0x0500`        | GDT (Global Descriptor Table)    |
| `0x7000`        | Zero Page (boot params)          |
| `0x9000`        | PML4 (Page Map Level 4)          |
| `0xA000`        | PDPT                             |
| `0xB000`        | PD (Page Directory, 2MB pages)   |
| `0x20000`       | Kernel command line              |
| `0x100000`      | Kernel bzImage load address      |
| `0x0800_0000`   | Initramfs load address           |

### Mapa MMIO de Dispositivos VirtIO

| Dirección         | Dispositivo                    | IRQ |
|-------------------|--------------------------------|-----|
| `0xD000_0000`     | VirtIO-Net (red)               | 5   |
| `0xD000_1000`     | VirtIO-Block disco 0 (root)    | 6   |
| `0xD000_2000`     | VirtIO-Block disco 1           | 7   |
| `0xD000_3000`     | VirtIO-Block disco 2           | 8   |
| ... (+0x1000)     | Discos adicionales             | +1  |

### Registros VirtIO-MMIO (offsets comunes)

| Offset  | Registro             | Dirección |
|---------|----------------------|-----------|
| `0x000` | MagicValue           | Read      |
| `0x004` | Version              | Read      |
| `0x008` | DeviceID             | Read      |
| `0x010` | DeviceFeatures       | Read      |
| `0x014` | DeviceFeaturesSel    | Write     |
| `0x020` | DriverFeatures       | Write     |
| `0x030` | QueueSel             | Write     |
| `0x034` | QueueNumMax          | Read      |
| `0x038` | QueueNum             | Write     |
| `0x044` | QueueReady           | R/W       |
| `0x050` | QueueNotify          | Write     |
| `0x060` | InterruptStatus      | Read      |
| `0x064` | InterruptACK         | Write     |
| `0x070` | Status               | R/W       |
| `0x080` | QueueDescLow         | Write     |
| `0x084` | QueueDescHigh        | Write     |
| `0x090` | QueueAvailLow        | Write     |
| `0x094` | QueueAvailHigh       | Write     |
| `0x0A0` | QueueUsedLow         | Write     |
| `0x0A4` | QueueUsedHigh        | Write     |
| `0x100+`| Device Config Space  | Read      |

### Red (Networking)

- Bridge: `nkr0` con IP `10.0.0.1/24`
- Cada VM tiene IP `10.0.0.{vm_id + 1}` (ej: vm_id=1 → 10.0.0.2)
- TAP auto-creado: `nkr-tap{vm_id}` conectado al bridge
- MAC: `52:54:00:12:34:{vm_id}`
- NAT/Masquerade para salida a internet
- Port forwarding via iptables DNAT/SNAT

### Volúmenes

- **Pre-boot:** el disco se monta con `mount -o loop`, se copian archivos host→guest
- **Post-shutdown:** los volúmenes marcados `:rw` se extraen guest→host
- Formato: `host_path:guest_path` (ro) o `host_path:guest_path:rw`

### Variables de Entorno

- Se escriben en `/etc/nkr-env` dentro del disco root antes del boot
- El initramfs hace `source /etc/nkr-env`
- Formato CLI: `--env KEY=VALUE`

---

## 5. Comandos CLI

```bash
# Ejecutar una micro-VM
sudo nkr run --disk odoo.ext4 --ram 1024 --chrs 2 --id 1 \
  --kernel bzImage --port 8069:8069 \
  --volume ./odoo.conf:/etc/odoo/odoo.conf \
  --env DB_HOST=10.0.0.2

# Listar VMs activas
sudo nkr ps

# Detener una VM
sudo nkr stop 1

# Descargar imagen Docker Hub → disco ext4
sudo nkr pull postgres:15 postgres.ext4 --size-gb 2

# Construir disco desde Nkrfile
sudo nkr build -f Nkrfile.odoo -o odoo.ext4 --size-gb 4

# Orquestar stack multi-servicio
sudo nkr compose up -f nkr-compose.yml -d   # daemon mode
sudo nkr compose ps
sudo nkr compose down
```

---

## 6. Modelo de CPU: "Chrs"

- 1 chr = 20% de un core físico (5 chrs = 1 core completo)
- Se implementa vía `sched_setaffinity` (CPU pinning)
- Los chrs son exclusivos: la VM se pinea a cores dedicados
- `cores_needed = ceil(chrs / 5)`

---

## 7. Formato Compose (nkr-compose.yml)

```yaml
services:
  nombre_servicio:
    disks: ["/ruta/disco.ext4"]        # Obligatorio: primer disco = root
    ram: 512                            # MB de RAM (default: 512)
    chrs: 1                             # Chrs de CPU (default: 1)
    id: 1                               # ID de VM (default: índice+1)
    kernel: ./bzImage                   # Se resuelve: local → /mnt/nkr/kernel/ → junto a nkr
    initramfs: /ruta/initramfs.cpio.gz  # Se resuelve: explícito → local → /mnt/nkr/initramfs/ → auto-detect
    ports: ["8069:8069"]
    volumes: ["host:guest:rw"]
    environment:                        # Se inyectan como /etc/nkr-env
      KEY: value
    build:                              # Auto-build si disco no existe
      nkrfile: Nkrfile.odoo
      context: "."
      size_gb: 4
    healthcheck:
      port: 8069
      initial_delay: 20
      interval: 5
      retries: 15
```

### Ejemplo real (nkr-compose.yml actual)

```yaml
services:
  db:
    disks:
      - /opt/nkr/disks/postgres.ext4
    build:
      nkrfile: Nkrfile.pg
      size_gb: 2
    initramfs: /opt/nkr/initramfs/pg_initramfs.cpio.gz
    kernel: ./bzImage
    ram: 512
    chrs: 1
    ports:
      - "5432:5432"
    volumes:
      - "/opt/nkr/data/pg:/var/lib/postgresql/data:rw"
    healthcheck:
      port: 5432
      initial_delay: 15
      interval: 5
      retries: 12

  odoo:
    disks:
      - /opt/nkr/disks/odoo-prod.ext4
    build:
      nkrfile: Nkrfile.odoo
      size_gb: 4
    initramfs: /opt/nkr/initramfs/odoo_initramfs.cpio.gz
    kernel: ./bzImage
    ram: 1024
    chrs: 2
    ports:
      - "8069:8069"
    volumes:
      - "/opt/nkr/config/odoo.conf:/etc/odoo/odoo.conf"
      - "/opt/nkr/modules:/mnt/extra-addons"
      - "/opt/nkr/data/filestore:/var/lib/odoo:rw"
    healthcheck:
      port: 8069
      initial_delay: 20
      interval: 5
      retries: 15
```

### Resolución de recursos en Compose (compose.rs)

Compose resuelve rutas de disco, kernel e initramfs con esta prioridad:

| Recurso     | Orden de búsqueda                                                        |
|-------------|--------------------------------------------------------------------------|
| **Disco**   | Ruta en YAML → `<yaml_dir>/<nombre>` → `/mnt/nkr/images/<nombre>`       |
| **Kernel**  | Explícito en YAML → `<yaml_dir>/bzImage` → `/mnt/nkr/kernel/bzImage` → junto al binario `nkr` |
| **Initramfs** | Explícito en YAML → `<yaml_dir>/<servicio>_initramfs.cpio.gz` → `/mnt/nkr/initramfs/<servicio>_initramfs.cpio.gz` → auto-generar desde disco |

### NKR Data Directory

```
/mnt/nkr/                  # Default (override con NKR_DATA_DIR env var)
├── images/                # Discos ext4 base
├── initramfs/             # Initramfs por servicio
├── kernel/                # bzImage
└── snapshots/             # Snapshots CoW por stack
```

### Snapshots CoW

Si el disco base existe, Compose crea un snapshot por servicio usando `cp --reflink=auto` para Copy-on-Write en filesystems que lo soportan (btrfs, xfs). Esto permite compartir el filesystem base entre múltiples instancias sin duplicar espacio en disco.

---

## 8. Estado de VMs (/tmp/nkr-vms/)

Cada VM activa crea un JSON en `/tmp/nkr-vms/{vm_id}.json`:

```json
{
  "vm_id": 1,
  "pid": 12345,
  "ram_mb": 512,
  "chrs": 1,
  "disks": ["postgres.ext4"],
  "guest_ip": "10.0.0.2",
  "ports": ["5432:5432"],
  "tap_name": "nkr-tap1",
  "started_at": 1709308800
}
```

`nkr ps` lee estos archivos. `nkr stop` envía SIGTERM al PID (con fallback a SIGKILL después de 3s).

---

## 9. Build de Discos

Tanto `nkr pull` como `nkr build` **usan Docker como motor** únicamente para generar el filesystem:

1. `docker create` / `docker build` → contenedor/imagen
2. `docker export` → tarball del filesystem
3. `truncate` + `mkfs.ext4` → disco vacío
4. `mount -o loop` + `tar -xf` → contenido extraído
5. Resultado: archivo `.ext4` listo para `nkr run --disk`

**Docker NO se usa en tiempo de ejecución**, solo para construir discos.

---

## 10. Reglas y Convenciones para IAs

### Al modificar código:

1. **Lenguaje del código:** Los comentarios, nombres de funciones y mensajes de log están en **español** (ej: `eprintln!("[NKR] Error fatal: {e}")`). Mantén esta convención.

2. **Mensajes de log:** Usan prefijos entre corchetes: `[NKR]`, `[NKR-BLOCK]`, `[NKR-NET]`, `[NKR-VOL]`, `[NKR-ENV]`, `[NKR-COMPOSE]`, `[NKR-PULL]`, `[NKR-BUILD]`, `[NKR-HEALTH]`, `[NKR-GUEST]`. Usa el prefijo apropiado para cada módulo.

3. **Error handling:** Se usa `Box<dyn std::error::Error>` como tipo de error genérico. Las funciones retornan `Result<(), Box<dyn std::error::Error>>`.

4. **Dependencias:** Todas las crates de virtualización vienen del ecosistema [rust-vmm](https://github.com/rust-vmm) (Firecracker, Cloud Hypervisor). Las versiones están fijadas en `Cargo.toml` para compatibilidad cruzada.

5. **Permisos:** El binario requiere **root/sudo** para operar (acceso a `/dev/kvm`, `mount`, `iptables`, TAP devices).

6. **No hay multithreading en vCPU:** Hay un solo vCPU por VM. El único thread extra es el de RX en `net.rs` (lee paquetes del TAP).

7. **Sin QEMU/libvirt:** Todo es implementación propia directa sobre KVM. No proponer soluciones que dependan de QEMU.

8. **VirtIO-MMIO (no PCI):** Los dispositivos usan transporte MMIO, no PCI. El kernel guest se configura con `pci=off`.

### Al modificar la CLI:

- Los subcomandos están en `cli.rs` usando `clap derive`
- El dispatch está en `main.rs`
- Los defaults están documentados en los `#[arg()]` attributes

### Al modificar networking:

- El bridge `nkr0` se auto-crea en `vmm.rs::ensure_bridge()`
- TAPs se auto-crean como `nkr-tap{vm_id}`
- Subnet fijo: `10.0.0.0/24`, gateway `10.0.0.1`
- Port forwarding usa iptables (PREROUTING DNAT + OUTPUT DNAT + MASQUERADE)

### Al modificar VirtIO:

- Block device: cola única de 256 entradas, sectores de 512 bytes
- Net device: 2 colas (RX=0, TX=1) de 256 entradas, header de 12 bytes
- Feature negotiation: VIRTIO_F_VERSION_1 (bit 32)
- El guest notifica vía QueueNotify (offset 0x050), el host inyecta IRQs vía irqfd

### Al modificar compose:

- Los servicios se ordenan alfabéticamente para IDs determinísticos
- Auto-build: si `build:` está definido y el disco no existe, se construye
- Health checks son TCP (no HTTP), corren en threads separados
- Modo daemon: re-ejecuta `nkr compose up` como proceso background con logs en `logs/nkr-compose.log`

---

## 11. Compilación y Ejecución

```bash
# Compilar (release, optimizado)
cargo build --release

# El binario queda en target/release/nkr (~2-4 MB)
# Ejecutar:
sudo ./target/release/nkr run --disk mi_disco.ext4

# Setup completo de Odoo:
sudo ./deploy/setup.sh   # Primera vez
sudo ./deploy/start.sh   # Iniciar stack
sudo ./deploy/stop.sh    # Detener stack
sudo ./deploy/update.sh  # Actualizar módulos Odoo
```

### Deploy Multi-Tenant

```bash
# Agregar cliente en deploy/clients.yml, luego:
sudo ./deploy/mt-provision.sh                # Provisionar TODOS los clientes
sudo ./deploy/mt-provision.sh cliente1        # Provisionar solo uno
sudo ./deploy/mt-provision.sh --base          # Solo crear disco base Odoo

# Cada provisión genera:
#   - Disco CoW desde base (/opt/nkr/disks/<cliente>.ext4)
#   - Config Odoo personalizada (/opt/nkr/config/<cliente>.conf)
#   - Config nginx con proxy reverso (/etc/nginx/sites-available/nkr-<cliente>)
```

### Actualización de Módulos Odoo

```bash
sudo ./deploy/update.sh              # Producción: sync + restart (~5s downtime)
sudo ./deploy/update.sh --test       # Probar en puerto 8070 sin tocar producción
sudo ./deploy/update.sh --rollback   # Restaurar backup anterior de módulos
sudo ./deploy/update.sh --update-db  # Actualizar + forzar -u all en Odoo
```

Flujo del update:
1. Backup automático de módulos actuales (mantiene últimos 5)
2. Stop Odoo (PG sigue corriendo)
3. Rsync de módulos con `__manifest__.py` desde `/opt/nkr/modules/` al disco
4. Restart Odoo → ~5 segundos de downtime total

---

## 12. Archivos Importantes que NO debes modificar sin contexto

| Archivo         | Razón                                                  |
|-----------------|--------------------------------------------------------|
| `bzImage`       | Kernel pre-compilado. No es código fuente.             |
| `*.ext4`        | Discos binarios de VMs. Se generan con pull/build.     |
| `init`          | Script de init del guest (tools/initramfs/init)        |
| `target/`       | Output de compilación Rust. No editar manualmente.     |

---

## 13. Problemas Comunes y Debugging

- **"Fallo al abrir /dev/kvm":** KVM no habilitado o sin permisos. Verificar: `ls -la /dev/kvm`.
- **"Fallo ip tuntap add":** Requiere root. Ejecutar con `sudo`.
- **VM se queda sin responder:** Verificar que el initramfs tiene los módulos correctos (`virtio_blk.ko`, `virtio_net.ko`, `ext4.ko`).
- **Port forwarding no funciona:** Verificar que `ip_forward=1` y que no hay reglas iptables conflictivas.
- **Disco muy pequeño:** Usar `--size-gb` en pull/build para aumentar tamaño.
- **VM arranca pero el servicio no inicia:** El initramfs hace `switch_root` al disco ext4. El disco debe tener un `/sbin/init` ejecutable (systemd o similar). Si no lo tiene, NKR crea un wrapper `/sbin/nkr-init` que busca el init del sistema.

---

## 13.1 Flujo de Boot del Guest (initramfs → rootfs)

El init del guest (`tools/initramfs/init`, ~189 líneas) sigue este flujo:

```
1. mount /proc, /sys, /dev + redirect a /dev/console
2. insmod: crc32c, libcrc32c, crc16, mbcache, jbd2, ext4, virtio_blk, failover, net_failover, virtio_net
   (módulos del kernel 6.6.117-0-virt en /lib/modules/)
3. Esperar /dev/vda (hasta 3s, intervalos de 100ms)
4. Parsear nkr.ip=X.X.X.X del kernel cmdline (default: 10.0.0.2)
5. mount /dev/vda → /newroot (ext4)
6. mount /dev/vdb,vdc,vdd,vde → /newroot/mnt/disk0,disk1...
7. Mover /proc,/sys,/dev a /newroot + montar tmpfs en /run y /tmp
8. Generar /etc/nkr-net.sh (script de red con IP y gateway)
9. Crear /etc/resolv.conf (DNS 8.8.8.8, 8.8.4.4)
10. Configurar red ANTES de switch_root (via chroot temporal)
11. Buscar init real: /sbin/init → /usr/sbin/init → systemd → entrypoint Docker
12. Crear /sbin/nkr-init wrapper:
    - Ejecuta /etc/nkr-net.sh
    - Source /etc/nkr-env (variables de entorno NKR)
    - Exec init real del sistema
    - Fallback: sleep loop si no hay init
13. exec switch_root /newroot /sbin/nkr-init
```

**Importante:** El disco ext4 (creado por `nkr pull` o `nkr build`) debe contener un sistema de archivos completo (rootfs). Docker export genera exactamente esto.

**Emergency shell:** Si `/dev/vda` no aparece o el mount falla, el initramfs cae a un shell de emergencia (`/bin/sh`) con diagnósticos.

---

## 14. Roadmap / Trabajo Pendiente

### Prioridad Alta (bloquean producción multi-tenant)

- ~~**Multi-tenant Odoo:** Soporte para N instancias de Odoo apuntando a 1 PostgreSQL compartido~~ **✅ IMPLEMENTADO**
  - ✅ `deploy/clients.yml` — registro centralizado de clientes
  - ✅ `deploy/mt-common.sh` — funciones comunes (parse YAML, calcular IDs/IPs)
  - ✅ `deploy/mt-provision.sh` — provisionar disco CoW, config Odoo, config nginx por cliente
  - ⬜ `deploy/mt-compose-gen.sh` — generar nkr-compose.yml multi-tenant (pendiente)
  - ⬜ Probar flujo completo end-to-end con múltiples clientes
- ~~**Nginx único como gateway:**~~ **✅ IMPLEMENTADO** (auto-generación de configs nginx en mt-provision.sh)
  - ✅ Genera config por dominio → IP interna de cada Odoo
  - ✅ Soporte WebSocket/longpolling
  - ✅ Template HTTPS con Let's Encrypt (comentado, listo para activar)
  - ⬜ Let's Encrypt automático (requiere `certbot --nginx` manual por ahora)
- ~~**Optimización de RAM:**~~ **✅ IMPLEMENTADO** en configs generadas
  - ✅ `workers=0` (modo threaded) + `limit_memory_hard=512MB` + `limit_memory_soft=400MB`
  - ✅ Default 256 MB RAM por instancia Odoo en clients.yml
- ~~**Optimización de disco:**~~ **✅ IMPLEMENTADO**
  - ✅ `cp --reflink=auto` en mt-provision.sh (CoW real en btrfs/xfs)
  - ✅ `ensure_snapshot()` en compose.rs para snapshots automáticos
  - ✅ Resolución de disco base compartido (`/mnt/nkr/images/`)
- ~~**Hot-update de módulos:**~~ **✅ IMPLEMENTADO** en `deploy/update.sh`
  - ✅ Modo default (~5s downtime): sync + restart
  - ✅ `--test`: probar en puerto 8070
  - ✅ `--rollback`: restaurar backup anterior
  - ✅ `--update-db`: forzar actualización de base de datos
  - ✅ Backup automático (mantiene últimos 5)
  - ⬜ Hot-update masivo para N instancias secuencialmente

### Prioridad Media (mejoran operación)

- **VirtIO Console** (`0xD0002000`): stub implementado, falta funcionalidad real
- **Múltiples vCPUs:** Actualmente solo 1 vCPU por VM
- **VirtIO-FS:** Para volúmenes compartidos más eficientes (actual: copy pre/post-boot)
- **Métricas/Monitoring:** Dashboard de uso de RAM/CPU/disco por VM
- **Backup automatizado:** Snapshot de discos ext4 + dump de PostgreSQL por cliente
- **Scaling automático:** Agregar/quitar instancias Odoo sin editar YAML manualmente

### Prioridad Baja (nice-to-have)

- **Live migration:** Mover VMs entre servidores
- **Snapshots en caliente:** Sin detener la VM
- **Tests unitarios:** No hay tests automatizados

---

## 15. Deploy Multi-Tenant — Registro de Clientes (clients.yml)

Archivo `deploy/clients.yml` define todos los clientes y la configuración global:

```yaml
global:
  kernel: ./bzImage
  pg_initramfs: /opt/nkr/initramfs/pg_initramfs.cpio.gz
  odoo_initramfs: /opt/nkr/initramfs/odoo_initramfs.cpio.gz
  base_disk: /opt/nkr/disks/odoo-base.ext4
  disk_dir: /opt/nkr/disks
  data_dir: /opt/nkr/data
  config_dir: /opt/nkr/config
  modules_dir: /opt/nkr/modules
  pg_ram: 2048              # RAM para PostgreSQL compartido
  pg_chrs: 2
  odoo_ram: 256             # Default RAM por instancia Odoo
  odoo_chrs: 1
  odoo_disk_size_gb: 4
  nginx_sites_dir: /etc/nginx/sites-available
  nginx_enabled_dir: /etc/nginx/sites-enabled

clients:
  - name: cliente1
    domain: cliente1.midominio.com
    db_name: cliente1_prod
  - name: cliente2
    domain: cliente2.midominio.com
    db_name: cliente2_prod
    ram: 512                # Override del default
    chrs: 2
```

### Asignación de IDs

- `vm_id=1` → PostgreSQL (IP `10.0.0.2`)
- `vm_id=2+` → Clientes Odoo en orden de lista (IP `10.0.0.{vm_id + 1}`)

### Scripts Multi-Tenant

| Script              | Función                                                              |
|---------------------|----------------------------------------------------------------------|
| `mt-common.sh`      | Funciones bash: `yaml_global()`, `parse_clients()`, `get_client()`, `get_vm_id()`, `vm_ip()`, `print_client_table()` |
| `mt-provision.sh`   | Provisiona clientes: disco CoW, config Odoo, config nginx + reload   |
| `clients.yml`       | Registro YAML de clientes con defaults y overrides                   |

### Estructura de directorios en producción

```
/opt/nkr/
├── disks/              # Discos ext4
│   ├── odoo-base.ext4  # Disco base (se crea una vez con nkr build)
│   ├── postgres.ext4
│   ├── cliente1.ext4   # CoW desde base
│   └── cliente2.ext4
├── config/             # Configs Odoo por cliente
│   ├── cliente1.conf
│   └── cliente2.conf
├── data/               # Datos persistentes
│   ├── pg/             # PostgreSQL data
│   ├── cliente1/filestore/
│   └── cliente2/filestore/
├── modules/            # Módulos Odoo compartidos
├── initramfs/          # Initramfs por servicio
│   ├── pg_initramfs.cpio.gz
│   └── odoo_initramfs.cpio.gz
└── backups/            # Backups de módulos (update.sh)
```

---

*Última actualización: 2026-03-03*
