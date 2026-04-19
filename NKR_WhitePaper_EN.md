---
title: "Nano-Kernel Runtime (NKR): A Bare-Metal Micro-VM Hypervisor for Multi-Tenant SaaS Workloads"
subtitle: "White Paper — Version 1.3"
date: "April 2026"
lang: en
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
  - \fancyhead[R]{\small April 2026}
  - \fancyfoot[C]{\thepage}
  - \usepackage{booktabs}
  - \usepackage{longtable}
  - \renewcommand{\arraystretch}{1.3}
  - \usepackage{listings}
  - \lstset{basicstyle=\ttfamily\small, breaklines=true, frame=single, backgroundcolor=\color{gray!10}}
  - \usepackage{xcolor}
---

\newpage

> **Abstract.** The *Nano-Kernel Runtime* (NKR) is an open-source bare-metal hypervisor written in Rust that replaces container runtimes like Docker with hardware-isolated micro-VMs, running directly on Linux KVM. NKR is designed for operators managing dense multi-tenant SaaS deployments—especially Odoo ERP—on a single server with limited resources (16–32 GB RAM). By eliminating the overhead of QEMU, libvirt, and container-level sharing, NKR achieves full hardware isolation with a binary of just 2–4 MB, VM boot times under one second, exclusive CPU scheduling (the "chrs" model), and a Docker-compatible workflow for building disk images. Version 1.1 added six key capabilities: live filesystem sharing via VirtIO-FS, controlled CPU bursting via cgroupv2, L2 network isolation with ebtables, per-tenant database limits, a native Prometheus metrics exporter, and automatic multi-tenant compose file generation. Version 1.2 introduced four further optimizations targeting 100+ Odoo instances on 32 GB RAM: VirtIO-PMEM + DAX (eliminating ~150–200 MB of duplicated page cache per instance), async I/O via io_uring (reducing syscall overhead ~70% under high concurrency), uncompressed ELF vmlinux loading (~20 ms faster boot), and a Seccomp BPF jailer. Version 1.3 adds the **Cell System** — multi-stack isolation with per-cell L2/L3 bridges and subnet — VirtIO-FS with DAX (replacing VirtIO-9P for 3–5× faster file serving), VirtIO-Balloon (idle RAM reclamation), a VirtIO-Console shutdown channel (sub-2s clean restart), and instance cloning (`nkr cell clone`). This document presents the full architecture, implementation, and production deployment model of NKR v1.3.

---

\newpage

# Introduction and Motivation

## The Problem

Service providers managing dozens of SaaS tenants on shared infrastructure face a fundamental tension between **density** (maximizing the number of tenants per server) and **isolation** (avoiding the noisy neighbor effect). Docker containers offer high density but share the host kernel, exposing a large attack surface and lacking strict CPU or RAM guarantees. Traditional VMs (QEMU/KVM with libvirt) provide solid isolation but impose prohibitive memory and disk overhead for dense deployments.

Consider a practical scenario: an operator managing **50 Odoo 17 ERP instances** on a single 16–32 GB server using Docker:

| Issue | Impact with Docker | Impact with NKR |
|---|---|---|
| **Disk Usage** | 50 × 1.5 GB images ≈ **75 GB** | Shared ext4 base + CoW snapshots |
| **RAM Consumption** | 50 × ~1 GB ≈ **50 GB** | 50 × ~256 MB ≈ **12.5 GB** (exclusive) |
| **CPU Contention** | Shared scheduler, no guarantees | Pinned cores with the "chrs" model |
| **Restart Latency** | ~3 minutes per stack restart | ~2 seconds (hot restart via hvc0) |
| **Deployment Cycle** | git pull → rebuild → restart | git pull → rsync → restart Odoo only |
| **Infrastructure Footprint** | 50 Odoo + 50 PostgreSQL + 50 nginx | N Odoo + **1** PostgreSQL + **1** PgBouncer + **1** nginx |

NKR was created to eliminate these compromises, providing VM-level isolation with the operational simplicity of a container.

## What is NKR?

**Nano-Kernel Runtime (NKR)** is a purpose-built hypervisor that:

- Runs micro-VMs directly over `/dev/kvm` without QEMU, libvirt, or containerd
- Provides each "container" with a real Linux kernel, an ext4 root filesystem, and VirtIO devices
- Compiles to a **single ~2–4 MB binary** (Rust, LTO, stripped)
- Offers a Docker-compatible CLI (`nkr run`, `nkr ps`, `nkr stop`, `nkr restart`, `nkr compose up`)
- Manages **Cells**: isolated multi-stack groups with dedicated L2/L3 networks (`nkr cell create/up/down/clone`)
- Uses Docker **only** at build time to generate disk images from OCI/Dockerfiles

---

# Design Goals

The design of NKR is guided by five principles:

1. **Zero runtime external dependencies.** The `nkr` binary only requires a Linux kernel with KVM support. No QEMU, no libvirt, no container runtime.

2. **Hardware isolation with container ergonomics.** Each workload runs in a full KVM virtual machine—with its own kernel, page tables, and interrupt controller—even though operators interact with it using familiar Docker-style commands and compose files.

3. **Deterministic resource allocation.** RAM is mapped exclusively to each VM. CPU cycles are guaranteed via core pinning. There is no memory overcommit.

4. **Minimal footprint.** The hypervisor binary weighs 2–4 MB. Guest overhead is bounded: a 256 MB VM uses exactly 256 MB of host RAM.

5. **Production-ready for multi-tenant SaaS.** First-class support for multi-tenant Odoo deployments with shared PostgreSQL (backed by PgBouncer), hot module updates, automated provisioning, and Cell-level network isolation for running multiple Odoo versions simultaneously.

---

# Architectural Overview

```
┌─────────────────────────────────────────────────────────────────┐
│                    Host Server (Linux + KVM)                    │
│                                                                 │
│  Cell "nazcatex" (cell_id=1)    Cell "cafeteria" (cell_id=2)   │
│  ┌─────┐ ┌─────┐ ┌─────┐       ┌─────┐ ┌─────┐ ┌─────┐        │
│  │ PG  │ │PgBnc│ │Odoo │ ...   │ PG  │ │PgBnc│ │Odoo │ ...    │
│  │2GB  │ │128M │ │256M │       │2GB  │ │128M │ │256M │        │
│  └──┬──┘ └──┬──┘ └──┬──┘       └──┬──┘ └──┬──┘ └──┬──┘        │
│     └───────┴───────┘             └───────┴───────┘            │
│  ┌──────────────────────┐   ┌──────────────────────┐           │
│  │ nkr-br1 10.0.1.0/24 │   │ nkr-br2 10.0.2.0/24 │           │
│  └──────────────────────┘   └──────────────────────┘           │
│         │                          │                            │
│  ┌──────┴──────────────────────────┴──────┐                    │
│  │    iptables: NAT / DNAT / MASQUERADE   │                    │
│  └────────────────────────────────────────┘                    │
│  ┌────────────────────────────────────────┐                    │
│  │  nginx (host) — reverse proxy + SSL    │                    │
│  │  SNI map → cell_id IP:8069 / :8072     │                    │
│  └────────────────────────────────────────┘                    │
└─────────────────────────────────────────────────────────────────┘
```

Each micro-VM is a complete virtual machine featuring:

- A Linux kernel (highly optimized `nanolinux` ELF or legacy `bzImage`, binary shared among all VMs in a cell)
- An ext4 root filesystem (created from OCI images), optionally exposed via VirtIO-PMEM + DAX
- VirtIO-MMIO devices for block storage, networking, VirtIO-FS (file sharing with DAX), persistent memory, balloon, and console
- An initramfs that manages module loading, network setup, VirtIO-FS mount, and rootfs pivoting
- Exclusive RAM and CPU pinning via cgroupv2 + `sched_setaffinity`

---

# VMM Engine: From KVM to Boot

The VMM engine (`vmm.rs`, ~1,600 lines) implements the full lifecycle of a micro-VM using direct KVM ioctls via the `rust-vmm` ecosystem—the same foundation powering AWS Firecracker and Intel Cloud Hypervisor.

## KVM Initialization

```
1. Open /dev/kvm
2. KVM_CREATE_VM       → VM file descriptor
3. KVM_CREATE_IRQCHIP  → in-kernel PIC + IOAPIC
4. KVM_CREATE_PIT2     → Programmable Interval Timer
5. Map guest memory    → GuestMemoryMmap (two RAM regions; optional PMEM slot)
6. KVM_CREATE_VCPU     → single vCPU (id=0)
7. Setup CPUID, SREGs, General Purpose Registers
```

## Guest Memory Map (x86_64)

NKR uses a two-region memory model compatible with the Linux boot protocol:

| Address | Content | Size |
|---|---|---|
| `0x0000–0x9FFFF` | Base RAM (conventional) | 640 KB |
| `0x0500` | GDT (Global Descriptor Table) | 32 bytes |
| `0x7000` | Zero Page (boot parameters) | 4 KB |
| `0x9000` | PML4 (Page Map Level 4) | 4 KB |
| `0xA000` | PDPTE (Page Directory Pointer) | 4 KB |
| `0xB000` | PDE (Page Directory, 2 MB pages) | 4 KB |
| `0x20000` | Kernel command line | variable |
| `0x100000` | bzImage load address | ~10 MB |
| `0x800_0000` | Initramfs | variable |
| `0x1_0000_0000` | VirtIO-PMEM slot (if `--pmem`) | = disk size |
| `0x2_0000_0000` | VirtIO-FS DAX window (if `--share`) | 4 GB |

## Boot Protocol

NKR supports kernel formats auto-detected by magic bytes:

- **ELF nanolinux** (default): Detected by `\x7fELF` magic. Loaded via `linux-loader::Elf::load()`. vCPU starts directly in 64-bit long mode (`EFER=0xD01, CR0=0x80050033, CR4=PAE, CS.l=1`). Eliminates in-guest gzip decompression, drastically speeding up boot.
- **bzImage** (legacy): 32-bit Linux boot protocol. Kernel loaded at `0x100000` via `linux-loader::BzImage::load()`. vCPU starts in 32-bit protected mode.

Boot sequence (shared):

1. **Kernel Load** — ELF (in blocks) or bzImage loaded at `0x100000`
2. **Initramfs Load** — Copied to `0x800_0000` in guest memory
3. **Zero Page Setup** — Boot parameters populated at `0x7000`
4. **Page Table Write** — 2 MB pages with identity mapping via PML4 → PDPT → PD
5. **GDT Write** — A 4-entry table: null, code64, data, null
6. **vCPU Setup** — RIP = kernel entry point; sregs configured for 64-bit or 32-bit

The kernel command line configures all VirtIO devices inline:

```
console=ttyS0 panic=1 pci=off noapic nolapic clocksource=jiffies tsc=nowatchdog
virtio_mmio.device=4K@0xd0000000:5     # network
virtio_mmio.device=4K@0xd0001000:6     # disk 0
virtio_mmio.device=4K@0xd0002000:7     # disk 1 (if exists)
virtio_mmio.device=4K@0xd0010000:8     # VirtIO-FS share 0 (if --share)
virtio_mmio.device=4K@0xd0020000:16    # PMEM (if --pmem)
virtio_mmio.device=4K@0xd0030000:17    # Balloon
virtio_mmio.device=4K@0xd0040000:18    # VirtIO-Console (hvc0)
root=/dev/vda rw init=/sbin/init nkr.ip=10.0.{cell_id}.{vm_id+1}
# With --pmem: root=/dev/pmem0 rootflags=dax
```

## Time and Clock Management

Micro-VMs can suffer clock drift in high-density environments with CPU contention. NKR resolves this via two mechanisms:

1. **PIT2:** Explicitly instantiated (`KVM_CREATE_PIT2`), providing the base system clock interrupt vital for the guest scheduler.
2. **Kernel Clock Sources:** The cmdline param `clocksource=jiffies tsc=nowatchdog` forces the guest kernel to use interrupt-driven timekeeping and disables the TSC watchdog, preventing hangs caused by TSC inconsistencies under heavy CPU contention.

## vCPU Loop

The core loop executes `KVM_RUN` continuously, dispatching four key exit reasons:

| Exit Type | Handler |
|---|---|
| `IoOut` (I/O port write) | Serial console output (COM1 `0x3F8`) |
| `IoIn` (I/O port read) | Serial status registers |
| `MmioWrite` | VirtIO-MMIO device register writes (config, queues, notifications) |
| `MmioRead` | VirtIO-MMIO device register reads (features, status, config space) |
| `Hlt` / `Shutdown` | Clean exit |

## Robust Shutdown and Restart — *New in v1.3*

**VirtIO-Console (hvc0) shutdown channel** (`console.rs`): The guest init process blocks on `read -r < /dev/hvc0`. When the VMM injects `"SHUTDOWN\n"`, the init executes `killall5 -15`, waits for PostgreSQL postmaster (up to 25s), then calls `poweroff`.

**SIGTERM handling flow** (`vmm.rs:1916–1944`):

1. `SIGTERM` received → `SHUTDOWN_REQUESTED` AtomicBool set
2. First vCPU loop iteration (phase=0):
   - Store `SHUTDOWN_STARTED_MS` timestamp
   - Inject `"SHUTDOWN\n"` into hvc0 receiveq
   - Arm `SIGALRM` every 1s (`setitimer`) to break `vcpu.run()` when guest is in HLT
   - Advance to phase=1
3. Subsequent iterations (phase=1):
   - Re-inject `"SHUTDOWN\n"` every 2s — mitigates race where guest driver initialization delayed reading the first injection
   - After 60s timeout: force-break the vCPU loop
4. After break: extract RW volumes, teardown TAP + bridge rules, unregister state

**Zombie detection** (`state.rs:249–256`): `is_pid_alive()` combines `kill(pid, 0)` with parsing `/proc/{pid}/status` for `State: Z`. Zombie processes (parent compose hasn't called `wait()`) are treated as dead, preventing `nkr stop`/`nkr restart` from hanging 90s unnecessarily.

**`nkr restart`** (`main.rs:126–208`):

1. Read `/proc/{pid}/cmdline` — captures original `nkr run ...` argv
2. Stop VM via SIGTERM (90s timeout with SIGKILL fallback)
3. Sleep 500ms — allow TAP/bridge cleanup to complete
4. Re-spawn with `setsid()` (detached from terminal), stdout/stderr redirected to `/tmp/nkr-restart-{vm_id}.log`

Result: typical restart cycle completes in ~2s with the hvc0 shutdown channel, vs. 60s timeout when the channel is unavailable.

---

# VirtIO Device Model

NKR implements the VirtIO-MMIO (Memory-Mapped I/O) transport, not PCI. The kernel boot parameter `pci=off` disables PCI enumeration entirely.

## MMIO Address Map

| Address | Device | IRQ | Since |
|---|---|---|---|
| `0xD000_0000` | VirtIO-Net (network) | 5 | v1.0 |
| `0xD000_1000` | VirtIO-Block disk 0 (rootfs) | 6 | v1.0 |
| `0xD000_2000` | VirtIO-Block disk 1 | 7 | v1.0 |
| `0xD000_3000+` | Additional disks (+0x1000 each) | 8+ | v1.0 |
| `0xD001_0000+` | VirtIO-FS shares (+0x1000 each, DAX) | 8+ | **v1.3** |
| `0xD002_0000` | VirtIO-PMEM (persistent memory, DAX) | 16 | v1.2 |
| `0xD003_0000` | VirtIO-Balloon | 17 | **v1.3** |
| `0xD004_0000` | VirtIO-Console (hvc0) | 18 | **v1.3** |

The `0xD001_0000+` range never collides with the block zone (up to `0xD000_9000` with 9 disks). PMEM, Balloon, and Console are statically reserved.

## VirtIO Block Device

**Implementation:** `block.rs` — ~310 lines

- **Queue:** Single virtqueue, 256 descriptors
- **Sector size:** 512 bytes
- **Operations:** Read (`type=0`), Write (`type=1`)
- **Descriptor Chain:** Header (16 bytes: type + sector) → Data Buffer → Status Byte
- **Interrupts:** IRQ injected via `irqfd` after each completion batch
- **Feature Negotiation:** `VIRTIO_F_VERSION_1` (bit 32)
- **Async I/O (v1.2):** Uses `io_uring` (ring depth 128) for non-blocking reads/writes. Each request is submitted as `opcode::Read`/`opcode::Write` SQE; completions are drained at the top of each vCPU loop iteration via `poll_completions()`. Silent fallback to synchronous `pread64/pwrite64` if `io_uring` unavailable (kernel < 5.1).

The `get_host_address()` method on `GuestMemoryMmap` provides the raw host pointer for descriptor buffers, enabling zero-copy DMA-style I/O directly into guest memory.

## VirtIO Network Device

**Implementation:** `net.rs` — ~310 lines

- **Queues:** Two virtqueues (RX=0, TX=1), 256 descriptors each
- **Backend:** Linux TAP device (`/dev/net/tun`, `TUNSETIFF`)
- **Header:** 12-byte VirtIO-net header (stripped/prepended on TX/RX)
- **RX Path:** Dedicated background thread executes blocking reads on the TAP fd, injects packets into the RX queue, signals IRQ
- **TX Path (v1.2):** `opcode::Write` SQE to the TAP fd via `io_uring` (ring depth 64). The `Vec<u8>` payload is held in `tx_pending` until the CQE is drained, preventing premature deallocation. Falls back to `tap.write_all()` if ring unavailable.
- **Features:** `VIRTIO_NET_F_MAC` | `VIRTIO_NET_F_STATUS` | `VIRTIO_F_VERSION_1`
- **Config Space:** 6-byte MAC address + 2-byte link status

## VirtIO-FS Filesystem Sharing — *New in v1.3*

**Implementation:** `virtio_fs.rs`

NKR v1.3 replaces the legacy VirtIO-9P server with VirtIO-FS + DAX, delivering 3–5× faster filesystem access for Python library loading and hot Odoo module updates.

- **Protocol:** vhost-user socket connecting to external `virtiofsd` daemon
- **DAX window:** 4 GB mapped at guest physical address `0x2_0000_0000` (KVM slot 3). Guest reads translate directly to host page-cache without copying.
- **Device ID:** 26 over VirtIO-MMIO
- **POSIX semantics:** Full `fcntl`, `O_DIRECT`, `flock` compatibility
- **CLI:** `--share host_path:guest_path` (repeatable; first share at `0xD001_0000`, each additional +0x1000)
- **Performance:** Cold start for 30 micro-VMs sharing a common Odoo rootfs drops from ~90s (9P) to ~25s (VirtIO-FS DAX)

In the guest, the initramfs mounts VirtIO-FS shares declared in the kernel cmdline:

```bash
nkr run --disk odoo.ext4 --share /opt/modules:/mnt/extra-addons
# guest: mount -t virtiofs virtiofs0 /mnt/extra-addons
```

## VirtIO-PMEM + DAX — *New in v1.2*

**Implementation:** `pmem.rs` — ~200 lines

VirtIO-PMEM (device ID 27) maps the guest's root disk into host memory via `mmap(MAP_SHARED)` and registers it as a KVM memory slot at guest physical address `0x1_0000_0000` (4 GB). The guest kernel (with `CONFIG_VIRTIO_PMEM=y` and `CONFIG_FS_DAX=y`) exposes it as `/dev/pmem0` and mounts the rootfs with the `dax` option, bypassing the guest page cache entirely.

- **MMIO:** `0xD002_0000`, IRQ 16, device ID 27
- **Config space (offset 0x100):** `[u64 start_phys_addr][u64 size]`
- **KVM slot:** Slot 2 (slots 0 and 1 used by the two RAM regions)
- **Host mmap:** `MAP_SHARED | PROT_READ | PROT_WRITE` + `MADV_HUGEPAGE` hint to reduce TLB pressure
- **FLUSH requests:** Guest sends `VIRTIO_PMEM_REQ_TYPE_FLUSH`; NKR responds with `msync(MS_ASYNC)` without stalling the vCPU
- **E820 entry:** Type 12 (persistent memory) for `[4 GB, 4 GB + disk_size]`
- **Cmdline change:** `root=/dev/pmem0 rootflags=dax` replaces `root=/dev/vda rw`
- **Degradation:** Silent fallback to VirtIO-Block if disk cannot be mmap'd
- **Guest kernel requirement:** `CONFIG_VIRTIO_PMEM=y`, `CONFIG_FS_DAX=y`
- **CLI:** `--pmem` flag on `nkr run`

**Memory saving:** With DAX, guest filesystem reads access the host page-cache directly — no second copy exists in guest RAM. For a typical Odoo instance with a ~300 MB active read working set, this eliminates 150–200 MB of duplicated page cache per VM.

## VirtIO-Balloon — *New in v1.3*

**Implementation:** `balloon.rs` — ~150 lines

VirtIO-Balloon (device ID 5) reclaims unused memory from idle VMs and returns it to the host kernel via `madvise(MADV_DONTNEED)`.

- **MMIO:** `0xD003_0000`, IRQ 17
- **Operation:** The VMM writes the desired balloon target (in pages) to the device config space; the guest balloon driver inflates/deflates by allocating/freeing pages
- **CLI:** `--balloon-mb N` on `nkr run` pre-inflates the balloon by N MB at boot
- **Combined effect:** A 700 MB VM with `--balloon-mb 300` effectively occupies only ~400 MB of host RAM
- **Compose:** `balloon_mb: 200` in service spec

Combined with PMEM+DAX page-cache elimination and KSM deduplication, VirtIO-Balloon enables 103+ concurrent Odoo instances on 32 GB RAM.

## VirtIO-Console (hvc0) — *New in v1.3*

**Implementation:** `console.rs`

VirtIO-Console provides a bidirectional control channel between the VMM and the guest init process, used exclusively for coordinated shutdown.

- **Device ID:** 3 over VirtIO-MMIO at `0xD004_0000`, IRQ 18
- **Guest side:** Init blocks on `read -r cmd < /dev/hvc0`. On receiving `"SHUTDOWN\n"`: executes ordered shutdown (SIGTERM to services, waits PostgreSQL postmaster, `poweroff`)
- **Host side:** `try_inject(b"SHUTDOWN\n")` writes to the receiveq and raises IRQ. `poll_pending()` retries if the queue was full
- **Race mitigation:** VMM re-injects every 2s during the shutdown window in case the first injection was missed before hvc0 driver initialization

---

# Networking

## Bridge Topology

NKR supports two networking modes:

**Legacy mode** (`cell_id=0`): Single bridge `nkr0`, subnet `10.0.0.0/24`. All VMs share one L2 domain.

**Cell mode** (`cell_id=1..254`): Per-cell bridge `nkr-br{N}`, subnet `10.0.{N}.0/24`. Each cell is an isolated L2/L3 domain with its own NAT.

```
Legacy (cell_id=0):              Cell 1 (nazcatex):    Cell 2 (cafeteria):
nkr0  10.0.0.0/24               nkr-br1 10.0.1.0/24   nkr-br2 10.0.2.0/24
nkr-tap1  → VM 10.0.0.2         nkr-c1-tap1 10.0.1.2  nkr-c2-tap1 10.0.2.2
nkr-tap2  → VM 10.0.0.3         nkr-c1-tap2 10.0.1.3  nkr-c2-tap2 10.0.2.3
```

## Deterministic Address Formula

Defined in `src/registry.rs:216`:

```
IP  = 10.0.{cell_id}.{vm_id + 1}
MAC = 52:54:00:{cell_id}:34:{vm_id}
TAP = nkr-c{cell_id}-tap{vm_id}   (cell_id>0)
    = nkr-tap{vm_id}               (cell_id=0, legacy)
```

Conventional slot assignments per cell: `pg=vm_id 1`, `pgbouncer=vm_id 2`, `odoo-NN=vm_id 3..N`.

Example for cell_id=2: db → `10.0.2.2`, pgbouncer → `10.0.2.3`, odoo-01 → `10.0.2.4`.

## Automatic Configuration

For each VM, the VMM (`vmm.rs:956–974`):

1. Creates the TAP device `nkr-c{cell_id}-tap{vm_id}` (or legacy `nkr-tap{vm_id}`)
2. Attaches it to bridge `nkr-br{cell_id}` (or `nkr0`)
3. Assigns the MAC `52:54:00:{cell_id}:34:{vm_id}`
4. Passes the IP to the guest via kernel cmdline (`nkr.ip=`)
5. Configures iptables rules:
   - `POSTROUTING MASQUERADE` for internet access
   - `FORWARD ACCEPT` for inter-VM traffic within the cell
   - `PREROUTING DNAT` + `OUTPUT DNAT` for port forwarding (if `--port` specified)

Rules are checked with `iptables -C` first (idempotent) and torn down on VM shutdown.

## Port Forwarding

```bash
nkr run --disk odoo.ext4 --port 8069:8069 --id 2
# Creates: host:8069 → 10.0.0.3:8069 (DNAT + MASQUERADE)
```

## L2 Isolation with ebtables — *New in v1.1*

Three ebtables rules per TAP confine traffic to the hypervisor-assigned MAC+IP:

```
ebtables -A INPUT -i nkr-c1-tapN -p ARP --arp-mac-src 52:54:00:01:34:N -j ACCEPT
ebtables -A INPUT -i nkr-c1-tapN -p IPv4 --ip-src 10.0.1.(N+1) -s 52:54:00:01:34:N -j ACCEPT
ebtables -A INPUT -i nkr-c1-tapN -j DROP
```

These rules prevent a compromised VM from spoofing another tenant's MAC/IP. Rules are removed on cleanup via `teardown_tap_isolation()`. If `ebtables` is not installed, NKR emits a warning and continues without L2 isolation (silent degradation).

---

# Cell Architecture — *New in v1.3*

The Cell System is NKR's answer to running **multiple independent Odoo stacks** (e.g., Odoo 15, 17, and 19 tenants) on the same host without IP/network conflicts.

## What is a Cell?

A *cell* is a named group of micro-VMs with:
- A dedicated Linux bridge and subnet (`10.0.{cell_id}.0/24`)
- Its own `nkr-compose.yml` orchestrating the full stack (PG + PgBouncer + N Odoos)
- Isolated instance directories under `/mnt/nkr/cells/<name>/instances/`
- Cell-scoped VM registry (IDs 2–254 per cell, independent between cells)

Up to 254 cells can coexist on a single host (cell_ids 1–254). `cell_id=0` is the legacy flat mode.

## Directory Structure

```
/mnt/nkr/
├── cell-registry.json              # cell_name → cell_id
├── registry.json                   # "cell_name/vm_name" → vm_id (scoped)
└── cells/
    └── nazcatex/                   # cell "nazcatex" (cell_id=1)
        ├── cell.yml                # { name, cell_id, odoo_version }
        ├── nkr-compose.yml         # Full stack compose
        └── instances/
            ├── nazcatex-odoo-01/
            │   ├── config/odoo.conf
            │   ├── addons/
            │   ├── filestore/
            │   └── logs/
            └── nazcatex-odoo-02/
                └── ...
```

## Registry System

**`cell-registry.json`** — maps `cell_name → cell_id` (integer, 1–254).
**`registry.json`** — maps `"cell_name/vm_name" → vm_id` (integer, 2–254, scoped per cell).

The scoped key format means `nazcatex/odoo-01` and `cafeteria/odoo-01` can both hold `vm_id=3` without conflict — they live on different subnets (`10.0.1.4` vs `10.0.2.4`).

`resolve_id_scoped(cell_name, vm_name)` in `registry.rs:106` assigns the next free ID within the cell scope, or returns the existing one if already registered. `register_explicit_scoped()` registers a specific ID and verifies it isn't already taken within the same cell scope.

## Cell Lifecycle

```bash
# Create a cell — registers cell_id, creates bridge + directory structure
sudo nkr cell create nazcatex --odoo-version 17.0

# Generate compose file (external script or hand-craft nkr-compose.yml)
# Then start the full stack:
sudo nkr cell up nazcatex -d        # compose up in daemon mode

# Status
sudo nkr cell ls                    # table of all cells
sudo nkr cell ps nazcatex           # active VMs in this cell

# Teardown
sudo nkr cell down nazcatex         # stop all VMs
sudo nkr cell destroy nazcatex      # remove from registry (data preserved)
```

## Cell Compose Format

Cell compose files include `cell_id` and `nkr_name` per service:

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
      DB_HOST: "10.0.1.2"        # PG IP: 10.0.{cell_id}.{pg_vm_id+1}
      PGB_HOST: "10.0.1.3"       # PgBouncer IP
```

IPs in `environment:` are emitted as **literals** computed from `cell_id + vm_id` at compose generation time.

## Instance Cloning — `nkr cell clone`

`clone_instance()` in `cell.rs:659` provides atomic cloning of an Odoo instance within a cell — the primary workflow for creating test/staging environments from production.

**Algorithm:**

1. Scan `cells/*/instances/<src>/` to locate the owning cell
2. Reject if `dst` already exists
3. Warn if `src` VM is active (PG sessions will be briefly interrupted)
4. Register `dst_vm_id` via `resolve_id_scoped` (next free ID in cell scope)
5. `cp -a --reflink=auto <src_dir> <dst_dir>` — O(1) on btrfs/XFS (CoW), physical copy on ext4
6. Clear destination logs
7. `rewrite_odoo_conf()` — replace all occurrences of `src_nkr` → `dst_nkr` in `odoo.conf` (db_name, dbfilter, paths)
8. `clone_database()` — atomic PostgreSQL clone:
   - `ALTER DATABASE "{src}" WITH ALLOW_CONNECTIONS false`
   - `SELECT pg_terminate_backend(...)` — evict existing connections
   - `CREATE DATABASE "{dst}" WITH TEMPLATE "{src}" OWNER odoo`
   - `ALTER DATABASE "{src}" WITH ALLOW_CONNECTIONS true`
   - Connectivity verified with `pg_isready` before attempting; rollback on failure
9. `append_compose_block()` — text-based YAML edit (preserves comments and formatting):
   - Locates src service block by `nkr_name:` match
   - Clones block with new header, new `id:`, all `src_nkr` → `dst_nkr` substitutions
   - Creates timestamped backup (`nkr-compose.yml.bak.{unix_ts}`)

**Flags:**
- `--no-db` — skip database clone (copy files only)
- `--no-compose` — skip compose file modification

```bash
# Full clone (files + DB + compose)
sudo nkr cell clone nazcatex-odoo-01 nazcatex-odoo-04

# Safe smoke test (no DB, no compose modification)
sudo nkr cell clone nazcatex-odoo-01 nazcatex-odoo-04 --no-db --no-compose
```

---

# Disk Lifecycle: From OCI to ext4

NKR uses Docker exclusively as a **build tool** to transform OCI images into raw ext4 filesystems. Docker is completely removed from the runtime equation.

## Build Pipeline

```
┌──────────────┐    ┌──────────────┐    ┌──────────────┐
│  Docker Hub  │    │   Nkrfile    │    │ Local Image  │
│ (OCI image)  │    │ (Dockerfile) │    │              │
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
                                         disk.ext4
                                    (ready for nkr run)
```

## Nkrfile Format

Nkrfiles are standard Dockerfiles. NKR provides templates for common services:

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

## Copy-on-Write Snapshots

For multi-tenant deployments, NKR creates CoW snapshots from a base disk:

```bash
cp --reflink=auto odoo-base.ext4 client1.ext4
```

On filesystems supporting reflinks (btrfs, XFS with reflink), this operation is instantaneous and consumes zero additional disk space until writes diverge. On other filesystems, NKR falls back to `cp --sparse=always`.

## Volumes

NKR provides a volume system to inject configuration and persist data:

- **Pre-boot injection:** The root disk is loop-mounted and files are copied from host to guest paths
- **Post-shutdown extraction:** Volumes marked with `:rw` are copied back from guest to host
- **Syntax:** `host_path:guest_path` (read-only) or `host_path:guest_path:rw`

```bash
nkr run --disk odoo.ext4 \
  --volume ./odoo.conf:/etc/odoo/odoo.conf \
  --volume /opt/data/filestore:/var/lib/odoo:rw
```

## Environment Variables

Environment variables are written to `/etc/nkr-env` inside the root disk prior to boot:

```bash
nkr run --disk pg.ext4 --env POSTGRES_USER=odoo --env POSTGRES_PASSWORD=secret
```

The initramfs sources this file during early boot, making variables available to the guest init process.

---

# The CPU Model: "Chrs"

NKR introduces a CPU allocation unit dubbed the **chr** (pronounced "core"):

| Value | Meaning |
|---|---|
| 1 chr | 20% of a physical core |
| 5 chrs | 1 full physical core |
| 10 chrs | 2 physical cores |

## Implementation

CPU allocation is enforced using `sched_setaffinity()`:

```rust
let cores_needed = ((chrs as f32) / 5.0).ceil() as u32;
let cores_to_use = cores_needed.min(num_cpus);
// Pin the vCPU thread to cores [0..cores_to_use]
sched_setaffinity(0, &cpuset);
```

Chrs are **exclusive** — the VM process is pinned to dedicated physical cores, preventing contention with other VMs.

## CPU Bursting with cgroupv2 — *New in v1.1*

NKR adds controlled CPU burst via the cgroupv2 `cpu.max` controller. The minimum guarantee remains `1 chr = 20% of a core`, but the VM can absorb idle host cycles without impacting other tenants.

```
cgroupv2 for N chrs:
  cpu.max        →  "{N×20000} 100000"   (N×20% quota per 100 ms period)
  cpu.max.burst  →  "{N×5000}"           (extra burst credit — kernel ≥ 5.15)
```

The hierarchy is created at `/sys/fs/cgroup/nkr/{vm-name}/` and removed on shutdown via `teardown_cgroup()`. If cgroupv2 is unavailable, NKR falls back to `sched_setaffinity` only.

## `nkr nitro` — Temporary CPU Unlock

```bash
nkr nitro nazcatex-odoo-01 --duration 10m
```

Writes `max 100000` to the VM's `cpu.max`, giving it unbounded CPU for the specified duration (default 10m). A background `sh -c "sleep N; echo quota > cpu.max"` (detached with `setsid()`) restores the throttle. Used when installing heavy Odoo modules (`-i account`, `mrp`, `website`).

## Dynamic Nitro during Compose Boot

During `compose up`, each service with a `healthcheck:` goes through an automatic CPU lifecycle:

1. **`nitro_relax_cgroup()`** — set `cpu.max = max 100000` at VM start (full CPU for boot)
2. **TCP health check** — wait for service port to accept connections
3. **`run_warmup()`** — issue HTTP GETs to `/web/assets/debug/*.{css,js}` and `/web/login` to force QWeb asset compilation before the first real client
4. **30s grace period** — keep CPU at maximum for the first backend request
5. **`nitro_throttle_cgroup()`** — restore the configured `chrs` quota

Logs: `[NKR-WARMUP] ✅ X compiled (Ts, N bytes)` for each compiled asset.

## Disk I/O Throttling with cgroupv2 — *New in v1.1*

The same cgroupv2 hierarchy applies per-device I/O rate limits:

```
io.max  →  "MAJ:MIN rbps=209715200 wbps=104857600"   (200 MB/s read, 100 MB/s write)
```

Device numbers (major:minor) are obtained via `libc::stat()` on the disk path. Enforcement is done by the kernel `blk-mq` scheduler with zero additional CPU cost in the hypervisor.

**Deployment Example (8-core server):**

| Service | Chrs | Cores Used |
|---|---|---|
| PostgreSQL | 10 (2 cores) | Cores 0–1 |
| Odoo #1 | 5 (1 core) | Core 2 |
| Odoo #2 | 5 (1 core) | Core 3 |
| Odoo #3–#8 | 1 each | Cores 4–7 (shared pool) |

---

# Initramfs Generation

NKR bundles an automatic initramfs generator (`initramfs.rs`, ~410 lines) that crafts tailored boot environments for each service.

## Boot Sequence

```
Initramfs boots (PID 1)
    │
    ├─ Mount /proc, /sys, /dev
    ├─ Load kernel modules:
    │   crc32c → libcrc32c → crc16 → mbcache → jbd2 → ext4
    │   virtio_blk → failover → net_failover → virtio_net
    │   fuse → virtiofs     (if VirtIO-FS shares declared — v1.3)
    │   virtio_pmem → nd_btt → dax    (if --pmem active — v1.2)
    │
    ├─ Wait for /dev/vda or /dev/pmem0 (up to 3 seconds)
    ├─ Parse nkr.ip= from /proc/cmdline
    ├─ Configure eth0: IP/24, default route → 10.0.{cell_id}.1
    │
    ├─ Mount /dev/vda (or /dev/pmem0 with dax) → /newroot (ext4)
    ├─ Mount extra disks /dev/vdb..vde → /newroot/mnt/disk0..3
    ├─ Mount VirtIO-FS shares (if present in cmdline — v1.3):
    │   mkdir -p /newroot/${NKR_FS0_MNT}
    │   mount -t virtiofs virtiofs0 /newroot$mnt
    ├─ Bind-mount /proc, /sys, /dev into /newroot
    │
    ├─ Write /etc/nkr-net.sh (network config script)
    ├─ Write /etc/resolv.conf (DNS: 8.8.8.8, 8.8.4.4)
    ├─ Setup networking via chroot
    │
    ├─ Detect init: /sbin/init → systemd → Docker entrypoint
    ├─ Build wrapper /sbin/nkr-init:
    │   - Source /etc/nkr-env (NKR environment variables)
    │   - Start hvc0 watcher: read -r cmd < /dev/hvc0 (blocks)
    │   - Execute the detected init
    │
    └─ exec switch_root /newroot /sbin/nkr-init
```

## Automatic Entrypoint Detection

When building via `nkr pull` or `nkr build`, NKR:

1. Extracts `ENTRYPOINT` + `CMD` from the Docker image metadata
2. Mounts the disk read-only and scans for known entrypoint scripts (`/entrypoint.sh`, `/docker-entrypoint.sh`, etc.)
3. Generates a bespoke init script that loads NKR variables and launches the correct entrypoint

This enables NKR to boot unmodified Docker images — PostgreSQL, PgBouncer, nginx, Redis, Odoo — as micro-VMs without any image modification.

---

# Orchestration with Compose

NKR features a compose system (`compose.rs`, ~1,400 lines) modeled after Docker Compose but engineered for VM orchestration.

## Compose File Format

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

## Sequential Boot Order

Compose boots services in dependency order:

1. `db` — PostgreSQL, wait TCP probe on `:5432`
2. `pgbouncer` — wait TCP probe on `:6432`
3. All `odoo-*` services — launched in parallel once PgBouncer is healthy

## Resource Resolution

NKR compose resolves resources intelligently, walking a priority chain:

| Resource | Resolution Order |
|---|---|
| **Disk** | YAML path → `<yaml_dir>/<name>` → `/mnt/nkr/images/<name>` |
| **Kernel** | Explicit → `<yaml_dir>/nanolinux` → `/mnt/nkr/kernel/nanolinux` → alongside `nkr` binary |
| **Initramfs** | Explicit → by service name → by disk name → heuristic → auto-generation |

## Features

- **Auto-build:** Automatically builds the disk if a service defines a `build:` section and the disk is absent
- **Health checks:** TCP monitoring with configurable delay, interval, and retries
- **Daemon mode:** `nkr compose up -d` executes in the background with log rotation (max 10 MB, 3 rotated files)
- **CoW Snapshots:** Automatically creates snapshots when a base disk is already locked by another VM
- **Deterministic IDs:** Services use `nkr_name` + optional `id:`; IDs are cell-scoped in `registry.json`
- **Warmup + Nitro:** Automatic CPU relaxation during boot, QWeb asset pre-compilation, 30s grace period

## NKR Data Directory

```
/mnt/nkr/                          # Default (NKR_DATA_DIR variable)
├── images/                         # Base ext4 disk images
├── initramfs/                      # .cpio.gz files per service
│   ├── base/                       # busybox + kernel modules (shared)
│   ├── pg.cpio.gz
│   └── odoo.cpio.gz
├── kernel/                         # Shared nanolinux ELF / bzImage
├── snapshots/                      # CoW snapshots per stack
├── cell-registry.json              # cell_name → cell_id
├── registry.json                   # "cell/vm" → vm_id (scoped)
└── cells/                          # Cell instance directories
    └── nazcatex/
        ├── cell.yml
        ├── nkr-compose.yml
        └── instances/
```

---

# Multi-Tenant Deployment

NKR ships with an end-to-end deployment toolkit tailored for multi-tenant Odoo.

## Tenant Registry

Clients are defined in `deploy/clients.yml`:

```yaml
global:
  pg_ram: 2048
  odoo_ram: 256
  odoo_chrs: 1
  base_disk: /mnt/nkr/images/odoo-base.ext4
  db_statement_timeout: 60000   # ms — max query duration per tenant (v1.1)
  db_conn_limit: 10             # max simultaneous connections per DB (v1.1)

clients:
  - name: acme
    domain: acme.example.com
    db_name: acme_prod
  - name: globex
    domain: globex.example.com
    db_name: globex_prod
    ram: 512        # override
    chrs: 2         # override
    db_conn_limit: 20  # override — higher-load client
```

## Provisioning Pipeline

```
mt-provision.sh <client-name>
    │
    ├── Create CoW disk:      cp --reflink=auto base.ext4 → <client>.ext4
    ├── Craft Odoo config:    odoo.conf with db_name, dbfilter, workers=2+
    ├── Craft nginx config:   <domain> → 10.0.{cell_id}.<vm_ip>:8069/8072
    ├── Link nginx site:      ln -s sites-available → sites-enabled
    ├── Reload nginx:         nginx -s reload
    └── PostgreSQL limits (v1.1):
        ├── ALTER DATABASE "<db>" SET statement_timeout = '<N>ms';
        └── ALTER DATABASE "<db>" CONNECTION LIMIT <N>;
```

## Odoo Multi-Worker

Each Odoo instance uses `workers = 2+` (abandons werkzeug single-thread mode):
- `:8069` — HTTP synchronous workers
- `:8072` — gevent for long-polling and WebSockets

## Hot Module Update

The `deploy/update.sh` script facilitates near-zero downtime module updates:

| Mode | Command | Downtime |
|---|---|---|
| **Production** | `update.sh` | ~2s (hvc0 clean shutdown + restart) |
| **Test** | `update.sh --test` | 0 (runs on port 8070) |
| **Rollback** | `update.sh --rollback` | ~2s |
| **DB Update** | `update.sh --update-db` | ~30s |

**Update Flow:**

1. Automatically back up current modules (retains the last 5)
2. Stop the Odoo VM via `nkr stop` (SIGTERM → hvc0 SHUTDOWN → clean exit ~2s)
3. Mount the disk, rsync modules featuring `__manifest__.py`
4. Start the Odoo VM via `nkr restart` or compose

## Target Architecture

```
Server (16–32 GB RAM), 5 cells × (1 PG + 1 PgBouncer + 20 Odoos)
│
├── Cell 1 "nazcatex" — nkr-br1, 10.0.1.0/24
│   ├── VM nazcatex-pg          (id=1, 10.0.1.2, 2GB RAM)
│   ├── VM nazcatex-pgbouncer   (id=2, 10.0.1.3, 128MB RAM)
│   ├── VM nazcatex-odoo-01     (id=3, 10.0.1.4, 256MB RAM)
│   └── ... nazcatex-odoo-20   (id=22, 10.0.1.23, 256MB RAM)
│
├── Cell 2 "cafeteria" — nkr-br2, 10.0.2.0/24
│   └── ... (same structure)
│
├── nginx (on the host)   — SNI map → cell IP:8069/8072
└── Exposed ports: 80, 443, SSH
    Everything else: internal per-cell bridge
```

**Resource Budget — Density Scaling (32 GB server):**

| Scenario | RAM/Instance | Instances on 32 GB |
|---|---|---|
| v1.1 baseline | ~640 MB | 50 |
| v1.2 + PMEM | ~440 MB | 72 |
| v1.2 + PMEM + KSM | ~330 MB | 96 |
| v1.3 all features + KSM + Balloon | ~310 MB | **103+** |

*With KSM enabled, effective RAM per Odoo instance drops to ~330 MB (additional 25–30% page-sharing savings across identical Python/library pages between VMs in the same cell).*

---

# Observability and Metrics

NKR incorporates a low-level telemetry system built directly into the hypervisor that measures and exposes real-time resources utilized by each micro-VM, bypassing the need to deploy additional agents within the guests.

The metrics engine extracts intelligence via lightweight probes from `procfs` and the host networking subsystem:

- **CPU%**: Synchronous 200 ms sampling window against `/proc/{pid}/stat`. Sampling is shared globally when inspecting multiple VMs simultaneously.
- **RAM (VmRSS)**: Physical RSS from `/proc/{pid}/status`. Shows actual host memory occupied vs. pre-allocated VM RAM.
- **Disk (I/O)**: Cumulative read/write bytes via `/proc/{pid}/io`.
- **Network (TAP)**: Volumetric TX/RX counters on the TAP interface via `/proc/net/dev`.
- **KSM State:** MB saved globally by the kernel page deduplicator, read from `/sys/kernel/mm/ksm/`.

```bash
sudo nkr stats                    # all VMs
sudo nkr stats nazcatex-odoo-01   # filter by name/hash/id
```

## Native Prometheus Exporter — *New in v1.1*

```bash
sudo nkr serve --port 9090
# Exposes: http://host:9090/metrics
```

Implemented with only `std::net::TcpListener` (~30 lines). No additional crates.

**Exposed metrics:**

| Metric | Type | Description |
|---|---|---|
| `nkr_cpu_pct{vm="..."}` | Gauge | CPU percentage consumed (50 ms window) |
| `nkr_rss_mb{vm="..."}` | Gauge | Physical RAM (RSS) in MB |
| `nkr_io_read_bytes{vm="..."}` | Counter | Disk read bytes (cumulative) |
| `nkr_io_write_bytes{vm="..."}` | Counter | Disk write bytes (cumulative) |
| `nkr_net_rx_bytes{vm="..."}` | Counter | TAP received bytes (cumulative) |
| `nkr_net_tx_bytes{vm="..."}` | Counter | TAP transmitted bytes (cumulative) |
| `nkr_ksm_savings_mb` | Gauge | MB saved globally by KSM |

---

# Comparison with Existing Solutions

## NKR vs Docker

| Dimension | Docker | NKR |
|---|---|---|
| **Isolation** | Shared kernel (namespaces + cgroups) | Full VM (KVM, separate kernel) |
| **Kernel Vulnerability** | Impacts all containers | Impacts only the affected guest VM |
| **CPU Guarantee** | cgroups shares (soft limit) | Core pinning + cgroupv2 (strict limit) |
| **RAM** | Overcommit by default | Exclusive, no overcommit |
| **Binary Size** | dockerd ~100 MB + containerd + runc | ~2–4 MB single binary |
| **Boot Time** | ~1–3s (process initialization) | ~1–2s (complete VM boot) |
| **Restart Time** | ~3–5s | ~2s (hvc0 clean shutdown) |
| **Disk Format** | Layered overlay filesystem | Raw ext4 (CoW snapshots) |
| **Networking** | veth + bridge | TAP + per-cell bridge + iptables |
| **Multi-Stack** | Manual compose per stack | `nkr cell` with isolated subnets |

## NKR vs Firecracker

| Dimension | Firecracker | NKR |
|---|---|---|
| **Language** | Rust | Rust |
| **KVM Interface** | Direct (`kvm-ioctls`) | Direct (`kvm-ioctls`) |
| **VirtIO** | MMIO | MMIO |
| **Focus area** | Serverless (AWS Lambda) | Multi-tenant SaaS (Odoo) |
| **Disk plumbing** | External | Built-in (`nkr pull/build`, OCI→ext4) |
| **Orchestration** | None (external: containerd) | Built-in (`nkr compose`, `nkr cell`) |
| **MT Tooling** | None | Comprehensive (Cell System, instance clone) |
| **Volume Injection** | External | Built-in (pre-boot loop mount + VirtIO-FS) |
| **CPU Model** | Standard vCPU | "Chrs" (20% granularity + pinning) |
| **Shutdown** | Kill process | VirtIO-Console coordinated shutdown (~2s) |
| **Code Lines** | ~70,000+ | ~6,000 (tightly focused scope) |

## NKR vs QEMU/KVM

| Dimension | QEMU/KVM | NKR |
|---|---|---|
| **Binary Size** | ~20–50 MB | 2–4 MB |
| **Device Model** | Heavyweight x86 prep (PCI, USB, ACPI...) | Minimal VirtIO-MMIO only |
| **Configuration** | Complex CLI / libvirt XML | Straightforward CLI flags / YAML |
| **Boot Time** | ~3–10 seconds | ~1–2 seconds |
| **Dependencies** | libvirt, qemu, virt-manager | None (just `/dev/kvm`) |
| **Attack Surface** | Large (full emulation) | Minimal (6 MMIO device types) |

---

# Security Model

## Isolation Boundaries

| Layer | Mechanism |
|---|---|
| **CPU** | KVM hardware virtualization (VT-x/AMD-V). Guest runs in ring 0 of an isolated address space. |
| **Memory** | `GuestMemoryMmap` creates dedicated memory regions. No shared memory between VMs. |
| **Disk** | Each VM owns a distinct ext4 file. No shared overlay filesystem. |
| **Network** | Dedicated TAP per VM. Per-cell L2 bridge. Per-VM iptables rules. ebtables L2 rules (v1.1) prevent MAC/IP spoofing. |
| **Process** | Every VM runs as a discrete host process. SIGTERM → hvc0 → clean shutdown. Zombie state detected via `/proc/pid/status`. |
| **Syscalls** | Seccomp BPF jailer (v1.2) restricts the vCPU process to ≤31 allowed syscalls after initialization. |

## Attack Surface

The attack surface of NKR is fundamentally smaller than both Docker and QEMU:

- **No Userspace Device Emulation** (vs QEMU): only lean native MMIO handlers (net, block, VirtIO-FS, Balloon, PMEM, Console, serial)
- **No Shared Kernel** (vs Docker): A kernel exploit within the guest cannot pivot to the host
- **No Container Escape Routes**: No namespaces, no cgroups cross-talk, no procfs sharing
- **Minimal Host Interaction**: file I/O (disk/mmap), TAP read/write (network), serial output
- **L2 Isolation** (v1.1): ebtables rules prevent MAC/IP spoofing between tenant VMs on the bridge
- **Cell L3 Isolation** (v1.3): Per-cell subnets; inter-cell routing is not enabled by default

## Seccomp BPF Jailer — *New in v1.2*

**Implementation:** `seccomp.rs` — ~170 lines

Before entering the vCPU run loop, NKR installs a `SECCOMP_MODE_FILTER` program built at runtime from a static allowlist of 31 syscalls. The filter uses raw `libc::prctl` with no additional dependencies.

- **Preamble:** `prctl(PR_SET_NO_NEW_PRIVS, 1)` (required before installing a filter)
- **Policy:** `SECCOMP_RET_KILL_PROCESS` for any syscall not on the allowlist
- **Allowlist:** `read`, `write`, `ioctl` (KVM ioctls), `mmap`, `madvise`, `clone` (thread::spawn), `futex`, `io_uring_*`, `epoll_*`, `eventfd2`, `openat`, `pread64/pwrite64`, `clock_gettime`, `exit_group`, and stdlib essentials
- **Timing:** Installed *after* `VirtioNetDevice::new()` (which spawns the RX thread)
- **Degradation:** If `prctl` fails (kernel < 3.17 or capability denied), NKR emits a warning and continues without the filter

## Operational Security

- Externally, only ports 80, 443, and SSH (configurable) are exposed
- All inter-VM broadcast traffic is confined to the per-cell bridge
- Requires root access for KVM/TAP/iptables configuration (by design — no rootless mode)
- Seccomp filter constrains the vCPU process to the minimum syscall footprint after VM initialization

---

# Limitations and Future Work

## Current Limitations

| Limitation | Impact | Planned Resolution |
|---|---|---|
| **Single vCPU per VM** | No SMP inside guests | Multi-vCPU support (medium priority) |
| **VirtIO-MMIO only** | No PCI passthrough | Adequate for target SaaS workloads |
| **VirtIO-FS tied to vhost-user** | Needs external `virtiofsd` daemon | Setup automation in a future release |
| **PMEM requires kernel guest support** | `CONFIG_VIRTIO_PMEM=y` + `CONFIG_FS_DAX=y` needed | Documented; silent fallback to VirtIO-Block |
| **No Live Migration** | VM must be halted to migrate | Future consideration |
| **No Hot Snapshots** | VM must be halted to snapshot | Future consideration |
| **No Automated Testing** | Manual verification only | CI test suite integration |
| **ebtables optional** | L2 isolation only if ebtables installed | Migration to nftables bridge |
| **Linux Host Only** | Requires Linux with KVM | By design |
| **Compose IPs are literals** | Changing cell topology requires regenerating compose | Placeholder syntax (`${PG_IP}`) planned |

## Roadmap

**Implemented in v1.1:**
- `mt-compose-gen.sh` auto-generates `nkr-compose.yml` ✓
- VirtIO-FS for live directory sharing with DAX ✓
- Prometheus exporter (`nkr serve`) ✓
- ebtables L2 isolation ✓
- Per-tenant `statement_timeout` + `conn_limit` ✓
- cgroupv2 `cpu.max` + `cpu.max.burst` bursting ✓

**Implemented in v1.2:**
- ELF vmlinux loader (–20 ms boot) ✓
- io_uring async I/O (~70% syscall reduction) ✓
- VirtIO-PMEM + DAX (–150–200 MB/VM page cache) ✓
- Seccomp BPF jailer ✓

**Implemented in v1.3:**
- Cell System (isolated multi-stack with per-cell L2/L3) ✓
- VirtIO-FS with DAX replacing VirtIO-9P (3–5× faster file I/O) ✓
- VirtIO-Balloon (idle RAM reclamation) ✓
- VirtIO-Console hvc0 (coordinated ~2s clean shutdown) ✓
- `nkr cell clone` (atomic instance duplication with DB) ✓
- `nkr restart` (detached re-launch preserving original argv) ✓
- Zombie detection in `is_pid_alive()` (no more 90s hangs) ✓
- Dynamic Nitro warmup flow during compose boot ✓

**High Priority:**
- End-to-end validation deploying 5 cells × 20 Odoos
- Automated Let's Encrypt tooling via certbot
- Migration to nftables bridge (replace ebtables)
- Compose placeholder IPs (`${PG_IP}`, `${PGB_IP}`)

**Medium Priority:**
- Multi-vCPU enablement
- Better async VirtIO-FS vhost-user stability
- Automated per-tenant PostgreSQL snapshotting

**Low Priority:**
- Live VM migration across bare-metal clusters
- Live disk snapshots without VM interruption
- Web-based fleet management panel

---

# Conclusion

NKR demonstrates that attaining **container-level density and operational simplicity** paired with **VM-level isolation and strict resource framing** is possible in fewer than 6,000 lines of Rust packed into a 2–4 MB statically linked binary with zero runtime dependencies.

Version 1.3 pushes the density ceiling to 103+ Odoo instances on a single 32 GB server. VirtIO-FS + DAX delivers 3–5× faster Python library loading and eliminates the external `virtiofsd` complexity through seamless host page-cache sharing. VirtIO-Balloon reclaims up to 300 MB from idle VMs without rebooting them. VirtIO-Console enables sub-2s clean restarts — coordinated shutdown via the guest init instead of force-killing the process. The Cell System allows parallel deployment of Odoo 15, 17, and 19 on the same host, each in its own isolated L2/L3 network with zero configuration conflict.

For service operators sustaining dozens of SaaS tenants strapped to a single commercial server, NKR constitutes a fundamentally different tradeoff compared to Docker or classical Virtual Machines:

- **Every tenant receives hardware isolation**, not merely namespace separation
- **Every tenant enjoys guaranteed throughput**, evading chaotic shared memory pools and CPU scheduler contention
- **Operators preserve their Docker workflow**, leveraging familiar build, run, and compose semantics
- **The infrastructure consolidates natively**: 1 PostgreSQL + 1 PgBouncer backing N Odoo processes, per cell, instead of N complete overlapping stacks

NKR is opinionated software. Instead of trying to be a sprawling general-purpose hypervisor like QEMU or an unopinionated container nexus like Kubernetes, NKR narrows its sights onto a high-value operational payload: **Hyper-dense, multi-tenant bare-metal SaaS**. This focused scope permits the hypervisor to be simple enough to comprehend entirely, slight enough to audit exhaustively, and brisk enough to boot in seconds.

---

\newpage

# Appendix A: Technology Stack

| Component | Technology | Version | Since |
|---|---|---|---|
| Language | Rust | Edition 2021 | v1.0 |
| KVM Interface | `kvm-ioctls` | 0.19 | v1.0 |
| KVM Bindings | `kvm-bindings` | 0.10 | v1.0 |
| Guest Memory | `vm-memory` (GuestMemoryMmap) | 0.14 | v1.0 |
| Kernel Loader | `linux-loader` (bzImage + ELF) | 0.11 | v1.0 / v1.2 |
| VirtIO Queues | `virtio-queue` | 0.12 | v1.0 |
| CLI Argument Parsing | `clap` (derive) | 4.x | v1.0 |
| Serialization | `serde` + `serde_yaml` + `serde_json` | 1.x / 0.9 / 1.x | v1.0 |
| System Utilities | `vmm-sys-util` | 0.12 | v1.0 |
| Async I/O | `io-uring` | 0.6 | v1.2 |
| Guest Kernel | Linux vmlinux ELF / bzImage | 6.6.117-0-virt | v1.0 |

# Appendix B: Source Code Metrics

| Module | File | Lines | Responsibility |
|---|---|---|---|
| VMM Engine | `vmm.rs` | ~1,600 | KVM init, PIT2, ELF/bzImage loader, MMIO dispatch, cgroups, ebtables, PMEM slot, seccomp, hvc0 shutdown |
| Compose | `compose.rs` | ~1,400 | YAML parsing, orchestration, health checks, daemon mode, warmup/Nitro flow |
| Cell System | `cell.rs` | ~730 | Cell registry, bridge management, instance directories, `clone_instance` |
| Initramfs | `initramfs.rs` | ~410 | Boot envs, FS/PMEM/virtiofs module loading |
| Metrics | `metrics.rs` | ~420 | Telemetry, /proc analytics, KSM, Prometheus exporter |
| Networking | `net.rs` | ~310 | VirtIO-net, TAP backend, RX/TX threading, io_uring TX |
| Block | `block.rs` | ~310 | VirtIO-block, io_uring async I/O + sync fallback |
| FS Share | `virtio_fs.rs` | ~200 | VirtIO-FS (DAX, vhost-user) |
| PMEM | `pmem.rs` | ~200 | VirtIO-PMEM + DAX, mmap(MAP_SHARED), flush handler |
| Pull | `pull.rs` | 201 | Docker Hub → ext4 pipeline |
| Build | `build.rs` | 192 | Nkrfile → ext4 pipeline |
| Registry | `registry.rs` | 219 | Cell-scoped persistent name-to-ID mapping |
| State | `state.rs` | 272 | VM registry, lifecycle supervision, zombie detection, `nkr ps` |
| Balloon | `balloon.rs` | ~150 | VirtIO-Balloon, MADV_DONTNEED idle page eviction |
| Console | `console.rs` | ~120 | VirtIO-Console (hvc0), SHUTDOWN injection, poll_pending |
| Seccomp | `seccomp.rs` | ~170 | BPF filter construction + installation via prctl |
| CLI | `cli.rs` | ~330 | Full CLI: run/ps/stop/restart/nitro/compose/pull/build/stats/ksm/serve/cell |
| Main | `main.rs` | ~480 | Entry point, full command dispatch including Cell/Clone |
| **Total** | | **~7,900** | (+~2,200 lines vs v1.2) |

# Appendix C: Quick Start

```bash
# Compile NKR from source
cargo build --release
# Binary: target/release/nkr (~2–4 MB)

# ── Pull & Build ──────────────────────────────────────────────────────────────
# Pull a PostgreSQL image → ext4 disk
sudo ./target/release/nkr pull postgres:15 postgres.ext4 --size-mb 2048

# Build from Nkrfile
sudo ./target/release/nkr build -f Nkrfile.odoo -o odoo.ext4 --size-mb 4096

# ── Basic Run ─────────────────────────────────────────────────────────────────
# Spin up a micro-VM
sudo ./target/release/nkr run \
  --disk postgres.ext4 --ram 512 --chrs 1 --id 1 --port 5432:5432

# Run with VirtIO-FS live directory sharing
sudo ./target/release/nkr run \
  --disk odoo.ext4 --ram 256 --chrs 1 --id 2 \
  --share /opt/modules:/mnt/extra-addons \
  --share /mnt/nkr/cells/nazcatex/instances/nazcatex-odoo-01/config:/etc/odoo

# Run with VirtIO-PMEM + DAX (~150–200 MB RAM savings)
sudo ./target/release/nkr run \
  --disk odoo.ext4 --ram 256 --chrs 1 --id 3 --pmem

# Run with VirtIO-Balloon (reclaim 200 MB from idle VM)
sudo ./target/release/nkr run \
  --disk odoo.ext4 --ram 512 --chrs 1 --id 4 --balloon-mb 200

# ── Lifecycle ─────────────────────────────────────────────────────────────────
sudo ./target/release/nkr ps                           # list running VMs
sudo ./target/release/nkr stats                        # CPU/RAM/IO/NET live
sudo ./target/release/nkr stop nazcatex-odoo-01        # clean shutdown via hvc0
sudo ./target/release/nkr restart nazcatex-odoo-01     # stop + re-launch detached

# ── Nitro (temporary CPU unlock) ──────────────────────────────────────────────
sudo ./target/release/nkr nitro nazcatex-odoo-01 --duration 10m

# ── KSM deduplication ─────────────────────────────────────────────────────────
sudo ./target/release/nkr ksm on
sudo ./target/release/nkr ksm status

# ── Prometheus metrics ────────────────────────────────────────────────────────
sudo ./target/release/nkr serve --port 9090
curl http://localhost:9090/metrics

# ── Cell System ───────────────────────────────────────────────────────────────
# Create a cell (registers cell_id, creates bridge nkr-br1, directories)
sudo ./target/release/nkr cell create nazcatex --odoo-version 17.0

# List all cells
sudo ./target/release/nkr cell ls

# Start the full stack (requires nkr-compose.yml in cell directory)
sudo ./target/release/nkr cell up nazcatex -d

# Check active VMs in a cell
sudo ./target/release/nkr cell ps nazcatex

# Stop all VMs in a cell
sudo ./target/release/nkr cell down nazcatex

# Remove cell from registry (data preserved)
sudo ./target/release/nkr cell destroy nazcatex

# ── Instance Cloning ──────────────────────────────────────────────────────────
# Full clone: files + DB + compose block
sudo ./target/release/nkr cell clone nazcatex-odoo-01 nazcatex-odoo-04

# Smoke test: files only, no DB or compose modification
sudo ./target/release/nkr cell clone nazcatex-odoo-01 nazcatex-odoo-04 \
  --no-db --no-compose

# ── Compose ───────────────────────────────────────────────────────────────────
sudo ./target/release/nkr compose up -f nkr-compose.yml -d
sudo ./target/release/nkr compose down -f nkr-compose.yml
sudo ./target/release/nkr compose ps
```

---

*NKR is open-source software. Contributions and feedback are highly appreciated.*

*© 2026 NKR Contributors. MIT Licensed.*
