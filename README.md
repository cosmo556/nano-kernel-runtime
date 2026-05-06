# NKR — Nano-Kernel Runtime

**A Rust orchestrator that replaces Docker with KVM micro-VMs for extreme density.**

Target: **100 Odoo instances in ~32 GB RAM**, distributed across 5 cells (each cell = 1 Postgres + 1 PgBouncer + 20 Odoos).

NKR runs each "container" as a real micro-VM on `/dev/kvm`, with its own kernel, memory isolation, network isolation, and direct hardware I/O (virtio-blk, virtio-fs, virtio-pmem+DAX). Boot time is under one second. An idle Odoo footprint is ~240 MB RSS.

---

## Why NKR instead of Docker?

| | Docker/containerd | NKR |
|---|---|---|
| Isolation | shared kernel (namespaces) | full KVM (hardware-enforced) |
| Kernel per workload | host kernel shared | nanolinux per VM (<100 ms boot) |
| Idle RAM / instance | 80-150 MB | ~240 MB |
| Disk I/O | overlay + page cache duplication | virtio-pmem + DAX (bypass guest page cache) |
| Network | shared iptables + bridge | per-cell `nkr-br{N}` + tap + NAT (isolated) |
| Blast radius on kernel bug | kernel-wide | scoped to one VM |
| Dedup of shared code pages | none | virtio-fs DAX (host-side page cache shared) |

For a SaaS operator running many Odoo tenants, NKR trades ~100 MB per tenant for **real isolation** and deterministic networking.

---

## Architecture

```
┌──────────── host (bare metal, cloud VM, anything with /dev/kvm) ─────────────┐
│                                                                               │
│  nginx (SNI map → cell IP:8069)      ← tenant-x.example.com lands here        │
│   │                                                                           │
│   ├─ cell "company_client" (cell_id=1, bridge nkr-br1, 10.0.1.0/24)           │
│   │    • db (Postgres)             10.0.1.2                                   │
│   │    • pgb (PgBouncer)           10.0.1.3                                   │
│   │    • odoo-01, odoo-02, ...     10.0.1.4..24                               │
│   │                                                                           │
│   └─ cell "other_client" (cell_id=2, bridge nkr-br2, 10.0.2.0/24)             │
│        ...                                                                    │
│                                                                               │
│  /mnt/nkr                                 btrfs (compress=zstd:3)             │
│   ├─ images/<app>.ext4                    RO master rootfs, shared via DAX    │
│   ├─ cells/<cell>/instances/<name>/       per-tenant state                    │
│   ├─ cells/<cell>/.nkr-data/              per-instance RW disks (filestore)   │
│   └─ kernel/nanolinux                     custom kernel (built separately)    │
│                                                                               │
│  nkr serve :9090                          HTTP API + Prometheus metrics       │
└───────────────────────────────────────────────────────────────────────────────┘
```

Every ext4 under `/mnt/nkr` is created with `chattr +C` (NODATACOW) to avoid catastrophic CoW fragmentation on btrfs.

---

## Prerequisites

- **Host:** Linux kernel ≥ 5.15, `/dev/kvm` available, root privileges.
- **Filesystem:** `/mnt/nkr` mounted on **btrfs** (strongly recommended — `chattr +C` is critical for random-write workloads like Postgres).
- **Dependencies** (checked at startup by `verify_dependencies()`):
  ```
  ip iptables mount umount mkfs.ext4 e2fsck losetup
  chattr psql pg_isready virtiofsd
  ```
- **Rust toolchain:** Rust 2021 (`cargo build --release`).
- **Custom kernel:** NanoLinux, built separately. See `build-kernel/` for Makefile + Docker recipe. Output: `/mnt/nkr/kernel/nanolinux`.

---

## Installation

```bash
git clone <your-nkr-repo> ~/nano-kernel-runtime
cd ~/nano-kernel-runtime

# Build the binary (release mode, LTO enabled)
cargo build --release

# Install system-wide
sudo install -m 755 target/release/nkr /usr/local/bin/nkr
nkr --version   # → nkr 1.3.0

# Place the nanolinux kernel
sudo mkdir -p /mnt/nkr/kernel
# ... copy your nanolinux bzImage to /mnt/nkr/kernel/nanolinux ...
```

One-time host setup:

```bash
# Mount /mnt/nkr on btrfs (example: loopback image)
sudo mkdir -p /mnt/nkr
sudo truncate -s 50G /var/lib/nkr-data.img
sudo mkfs.btrfs /var/lib/nkr-data.img
sudo mount -o loop,compress=zstd:3 /var/lib/nkr-data.img /mnt/nkr

# Enable KVM + IP forwarding
sudo modprobe kvm_intel    # or kvm_amd
sudo sysctl -w net.ipv4.ip_forward=1
```

---

## CLI reference

```
nkr run | ps | stop | restart | stats | ksm | nitro <vm>
nkr pull <image>                 # OCI image → ext4 under /mnt/nkr/images/
nkr build -f Nkrfile -o out.ext4 # Dockerfile-compatible build → ext4 + initramfs
nkr compose up|down|ps           # multi-service orchestration (per-cell YAML)
nkr cell create|ls|up|down|ps|destroy|clone
nkr serve --port 9090            # HTTP API (Prometheus metrics + control plane)
```

---

## Step-by-step tutorial

### 1. Pull a Docker image and convert it to an ext4 disk

NKR uses Docker as a *build-time* extractor — not a runtime. It exports any OCI image into a raw ext4.

```bash
sudo nkr pull odoo:17.0 --size-mb 4096
# → /mnt/nkr/images/odoo.ext4            (4 GB, chattr +C on btrfs)
# → /mnt/nkr/initramfs/odoo.cpio.gz      (auto-generated)
```

What happens:
1. `docker create odoo:17.0` instantiates the container without starting it.
2. `docker export` dumps the filesystem to a tar.
3. NKR creates a 4 GB ext4 with `chattr +C` **before** allocating any extents (btrfs-aware).
4. `mkfs.ext4` + `mount -o loop` + `tar -xf` → the image becomes a real disk.
5. A generic initramfs is generated, embedding the Docker `ENTRYPOINT` + `CMD` from `docker inspect`.

### 2. Build a custom image from an Nkrfile

Nkrfile is Dockerfile-compatible. NKR invokes `docker build` as the backend and exports the resulting image.

```dockerfile
# Nkrfile
FROM odoo:17.0
RUN pip3 install phonenumbers pandas
COPY ./custom-addons/ /mnt/extra-addons/
COPY ./odoo.conf /etc/odoo/odoo.conf
```

```bash
sudo nkr build -f Nkrfile -o /mnt/nkr/images/myodoo.ext4 --size-mb 4096 --context .
# → builds the docker image
# → exports it as ext4 (with +C on btrfs)
# → auto-generates the initramfs
```

### 3. Create a cell (isolated network zone)

A cell groups VMs sharing a subnet. Each cell has its own Linux bridge, NAT rules, and a single Odoo version.

```bash
sudo nkr cell create company_client --odoo-version 17.0
# → /mnt/nkr/cells/company_client/cell.yml
# → allocates cell_id (e.g. 1) in the registry
# → bridge nkr-br1 (10.0.1.1/24) created on first `compose up`
```

Convention within each cell:

| Role | vm_id | Guest IP |
|---|---|---|
| Postgres | 1 | 10.0.{cell_id}.2 |
| PgBouncer | 2 | 10.0.{cell_id}.3 |
| Odoo-01 | 3 | 10.0.{cell_id}.4 |
| Odoo-NN | 3..N | 10.0.{cell_id}.{N+1} |

### 4. Write the cell's compose file

`/mnt/nkr/cells/company_client/nkr-compose.yml` — one file per cell.

```yaml
services:
  db:
    id: 1
    disks:
      - /mnt/nkr/images/postgres.ext4
    ram: 1024
    chrs: 4                        # 1 chr = 20% of one core (quota, not reservation)
    burst: true
    nkr_name: "company_client-db"
    shares:
      - "/mnt/nkr/cells/company_client/pg/data.ext4:/var/lib/postgresql/data"
    environment:
      POSTGRES_USER: odoo
      POSTGRES_PASSWORD: odoo
      POSTGRES_DB: odoo
      PGDATA: /var/lib/postgresql/data/pgdata
    healthcheck:
      port: 5432
      initial_delay: 5
      interval: 3
      retries: 15

  pgbouncer:
    id: 2
    disks: [/mnt/nkr/images/pgbouncer.ext4]
    ram: 64
    chrs: 1
    burst: false
    nkr_name: "company_client-pgb"
    environment:
      DB_HOST: "10.0.1.2"
      DB_PORT: "5432"
      DB_USER: odoo
      DB_PASSWORD: odoo
      LISTEN_ADDR: "0.0.0.0"
      LISTEN_PORT: "6432"
      POOL_MODE: transaction
      DEFAULT_POOL_SIZE: "20"
      MAX_CLIENT_CONN: "200"
    healthcheck:
      port: 6432
      initial_delay: 15
      interval: 3
      retries: 15

  odoo-01:
    id: 3
    disks:
      - /mnt/nkr/cells/company_client/instances/company_client-odoo-01/odoo.ext4
    ram: 1024
    chrs: 3
    nkr_name: "company_client-odoo-01"
    volumes:
      - "/mnt/nkr/cells/company_client/instances/company_client-odoo-01/config/odoo.conf:/etc/odoo/odoo.conf"
      - "/mnt/nkr/cells/company_client/instances/company_client-odoo-01/addons:/mnt/extra-addons"
      - "/mnt/nkr/cells/company_client/instances/company_client-odoo-01/filestore:/var/lib/odoo:rw"
    shares:
      - "/mnt/nkr/cells/company_client/instances/company_client-odoo-01/logs:/var/log/odoo:rw"
    environment:
      DB_HOST: "10.0.1.3"          # pgbouncer
      DB_PORT: "6432"
      DB_USER: odoo
      DB_PASSWORD: odoo
    healthcheck:
      port: 8069
      initial_delay: 20
      interval: 3
      retries: 20
```

### 5. Bring the cell up

```bash
cd /mnt/nkr/cells/company_client
sudo nkr compose up -d
```

What happens, in order:
1. `cell_id` is resolved (from `cell.yml` or registered on first run).
2. Bridge `nkr-br1` + NAT rules are created idempotently under a cross-process `flock(/tmp/nkr-netlink.lock)`.
3. Each service is spawned in parallel: `nkr run` with its own initramfs, disks, shares, and kernel cmdline.
4. Health checks run in parallel (TCP connect to each service's declared port).
5. For HTTP ports (8069, 8072, 80, 443, …) a warmup phase follows: 4 parallel GETs to pre-compile Odoo assets. **Skipped for cloned instances** (they inherit the compiled cache).
6. `compose up -d` returns when every service reports `[NKR-READY]`.

Expected timings:
- Fresh cell (first boot of a master): ~60-120 s because of Odoo's asset compilation.
- Subsequent boots or clones: ~15-30 s (page cache warm, assets cached).

### 6. Inspect running VMs

```bash
sudo nkr ps
```

```
┌─────┬──────────────┬──────────────────────────┬────────┬───────┬──────┬────────────┐
│ ID  │     HASH     │           NAME           │  PID   │  RAM  │ CHRs │ Guest IP   │
├─────┼──────────────┼──────────────────────────┼────────┼───────┼──────┼────────────┤
│ 1   │ 00c2859d8342 │ company_client-db        │ 164674 │ 1024M │ 4    │ 10.0.1.2   │
│ 2   │ 015b090b8351 │ company_client-pgb       │ 164689 │ 64M   │ 1    │ 10.0.1.3   │
│ 3   │ 00c7e4ca8345 │ company_client-odoo-01   │ 164677 │ 1024M │ 3    │ 10.0.1.4   │
└─────┴──────────────┴──────────────────────────┴────────┴───────┴──────┴────────────┘
```

Live resource stats:

```bash
sudo nkr stats
# → CPU%, real RSS, balloon savings, DAX savings, net RX/TX, disk r/w, KSM status
```

### 7. Lifecycle operations

```bash
# Stop a single VM (SIGTERM with 60 s timeout, then SIGKILL)
sudo nkr stop company_client-odoo-01

# Restart (preserves the original argv → same config)
sudo nkr restart company_client-odoo-01

# Tear down the whole cell
cd /mnt/nkr/cells/company_client
sudo nkr compose down

# Temporarily unthrottle a VM (for a one-time heavy task like `odoo -i mrp`)
sudo nkr nitro company_client-odoo-01 --duration 10m
```

---

## HTTP API for an external control panel

The API is designed for a web panel that provisions, monitors and controls tenants without SSH'ing into the host.

### Start the API server

```bash
# Development mode (no auth, localhost-only)
sudo nkr serve --port 9090

# Production mode (Bearer token, still localhost — expose via SSH tunnel)
sudo NKR_API_TOKEN=$(openssl rand -hex 32) nkr serve --port 9090
# Share the token with the panel as a secret env var.

# From the panel host:
ssh -L 9090:127.0.0.1:9090 nkr-host
# The panel now talks to http://localhost:9090 locally.
```

### Endpoints

| Method | Route | Purpose |
|---|---|---|
| `GET` | `/api/v1/health` | liveness (no auth) |
| `GET` | `/metrics` | Prometheus exposition 0.0.4 (no auth) |
| `GET` | `/api/v1/cells` | list cells with `used_odoos / free_slots / odoo_version` |
| `POST` | `/api/v1/instances` | create with **auto-selected cell** (by version) |
| `POST` | `/api/v1/cells/{cell}/instances` | create forcing a specific cell (validates version match) |
| `GET` | `/api/v1/cells/{cell}/instances/{nkr_name}` | instance info + runtime status |
| `DELETE` | `/api/v1/cells/{cell}/instances/{nkr_name}?drop_db=1` | delete (optionally preserve DB) |
| `POST` | `/api/v1/cells/{cell}/instances/{nkr_name}/actions` | `{"action":"start\|stop\|restart"}` |
| `GET` | `/api/v1/cells/{cell}/instances/{nkr_name}/logs?tail=N` | tail `odoo.log` (max 10 000 lines) |

All mutating endpoints require `Authorization: Bearer $NKR_API_TOKEN` when the token is set.

### Example: provision a new tenant (main flow)

A new customer wants a fresh Odoo 17.0. The panel doesn't need to know which cell has room — NKR picks the least-full cell matching the requested version.

```bash
curl -s -X POST http://localhost:9090/api/v1/instances \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "nkr_name": "customer-42",
    "mode": "dev",
    "odoo_version": "17.0",
    "dns": "customer-42.example.com",
    "edition": "community",
    "workers": 2,
    "list_db": false,
    "limit_memory_soft": 2147483648,
    "limit_memory_hard": 2684354560
  }'
```

Response (abbreviated):

```json
{
  "nkr_name": "company_client-customer-42",
  "cell": "company_client",
  "vm_id": 6,
  "guest_ip": "10.0.1.7",
  "dns": "customer-42.example.com",
  "db_name": "db-company_client-customer-42",
  "addons_path": "/mnt/nkr/cells/company_client/instances/company_client-customer-42/addons",
  "logs_path":   "/mnt/nkr/cells/company_client/instances/company_client-customer-42/logs/odoo.log",
  "config_path": "/mnt/nkr/cells/company_client/instances/company_client-customer-42/config/odoo.conf",
  "nkr_status": { "running": false, "port_8069_up": false }
}
```

Fields the panel will use:
- `addons_path` → point the GitHub webhook here for `git pull` on module updates.
- `logs_path` → `GET /logs?tail=N` reads this file.
- `guest_ip` → add to your nginx SNI map.

### Modes

- `"mode": "dev"` → clones the source instance **with** its database (`CREATE DATABASE ... WITH TEMPLATE`). Useful for test environments.
- `"mode": "production"` → clones files (addons, config, filestore) but the DB is empty. The panel hydrates the DB separately (restore a dump, etc.).

### Validations

- `odoo_version` is **required** and must match `cell.odoo_version` (one version per cell by design). Mismatch → `409 version_mismatch`.
- Maximum 20 Odoo instances per cell. Full cell → `409 cell_full`.
- `nkr_name` is auto-prefixed with the cell name if short (e.g. `"tst-1"` → `"company_client-tst-1"`).
- All identifiers (`nkr_name`, `cell`, `source`, `odoo_version`) must match `[A-Za-z0-9._-]{1,64}`. Rejects YAML injection, shell injection, path traversal.

### Start the VM

Creation only provisions disk + DB + compose block. To actually boot:

```bash
curl -s -X POST http://localhost:9090/api/v1/cells/company_client/instances/company_client-customer-42/actions \
  -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
  -d '{"action":"start"}'
```

The response includes `nkr_status` with live `running`, `pid`, `ram_mb`, `uptime_s`, `port_8069_up`.

### List cells with capacity

```bash
curl -s -H "Authorization: Bearer $TOKEN" http://localhost:9090/api/v1/cells
```

```json
{
  "cells": [
    {
      "name": "company_client",
      "cell_id": 1,
      "odoo_version": "17.0",
      "used_odoos": 3,
      "max_odoos": 20,
      "free_slots": 17
    }
  ],
  "max_odoos_per_cell": 20
}
```

### Tail logs

```bash
curl -s -H "Authorization: Bearer $TOKEN" \
  "http://localhost:9090/api/v1/cells/company_client/instances/company_client-odoo-01/logs?tail=200"
```

```json
{
  "nkr_name": "company_client-odoo-01",
  "logs_path": "/mnt/nkr/cells/company_client/instances/company_client-odoo-01/logs/odoo.log",
  "tail": 200,
  "lines": [ "...", "..." ]
}
```

The reader is bounded: it caps at 4 MiB of RAM regardless of log size. Max `tail=10000`.

### Delete an instance

```bash
# Drop everything including the DB (default)
curl -s -X DELETE -H "Authorization: Bearer $TOKEN" \
  http://localhost:9090/api/v1/cells/company_client/instances/company_client-customer-42

# Preserve the DB (e.g. to migrate to another instance)
curl -s -X DELETE -H "Authorization: Bearer $TOKEN" \
  "http://localhost:9090/api/v1/cells/company_client/instances/company_client-customer-42?drop_db=0"
```

The DELETE cleanly:
1. Sends SIGTERM to the VM (graceful shutdown via `/dev/hvc0` virtio-console).
2. Drops the Postgres DB (if `drop_db=1`).
3. Removes the compose block from `nkr-compose.yml` (with a timestamped `.bak.*` backup).
4. Releases the `vm_id` in the registry.
5. Deletes `/mnt/nkr/cells/<cell>/instances/<name>/` and related `.nkr-data/<short>-*` artifacts.

### Typical panel onboarding flow

```
1. Panel checks capacity:
   GET /api/v1/cells
   → If all cells at 20/20 → trigger a "create new cell" workflow.

2. Panel creates the tenant:
   POST /api/v1/instances { nkr_name, mode, odoo_version, dns, workers, ... }
   → 201 with addons_path + logs_path.

3. Panel configures the GitHub webhook → addons_path.
4. Panel updates the nginx SNI map:  dns → guest_ip:8069.
5. Panel triggers nginx reload (outside NKR).
6. Panel POSTs {"action":"start"} to boot the VM.
7. Panel polls info.nkr_status.port_8069_up to mark the tenant "ready".
```

---

## Cloning instances (CLI)

If you prefer shell over the API:

```bash
# Dev clone (with DB — fast via Postgres TEMPLATE + btrfs reflink)
sudo nkr cell clone company_client-odoo-01 company_client-odoo-04

# Production clone (files only, DB empty)
sudo nkr cell clone company_client-odoo-01 company_client-customer-42 --no-db

# Bring up the clone
cd /mnt/nkr/cells/company_client && sudo nkr compose up -d
```

What `cell clone` does internally:
1. `btrfs subvolume snapshot` (or `cp --reflink=auto` fallback) of the source instance dir — O(1).
2. `preserve_nocow` re-applies `chattr +C` to every `.ext4` in the new dir (reflink alone doesn't inherit the flag).
3. Clones the matching `.nkr-data/<src-short>-*.ext4` files (per-instance disks holding filestore).
4. Rewrites `odoo.conf` (`dbfilter`, `db_name`, `workers`, `limit_memory_*`, `list_db`).
5. Runs `CREATE DATABASE "db-<dst>" WITH TEMPLATE "db-<src>"` via psql-over-stdin (so `CREATE DATABASE` isn't wrapped in an implicit transaction).
6. Appends a new service block to `nkr-compose.yml` with `skip_warmup: true` and injects `NKR_RENAME_FILESTORE_FROM/TO` env vars.
7. The guest's `nkr-start.sh` detects those env vars on first boot and renames `filestore/db-<src>/ → filestore/db-<dst>/` **inside** the guest — no host-side mount loop needed.

---

## Performance knobs per VM

| Knob | YAML | Meaning |
|---|---|---|
| `ram` | `ram: 1024` | MB of memfd-backed RAM. `MADV_NOHUGEPAGE` + `MAP_SHARED` (for vhost-user). |
| `chrs` | `chrs: 3` | CPU quota: `cpu.max = chrs * 20000 / 100000`. Quota, not reservation — safe to overcommit. |
| `burst` | `burst: true` | cgroup v2 `cpu.max.burst` — allows short spikes above quota. |
| `pmem` | `pmem: true` (default) | Enable virtio-pmem + DAX: guest mmaps the rootfs RO directly from the host page cache. Saves ~150 MB/VM of duplicated cache. |
| `balloon_mb` | CLI `--balloon-mb` | virtio-balloon: guest returns this many MB to the host when idle. |
| `skip_warmup` | `skip_warmup: true` | Skip the post-TCP-UP HTTP warmup (auto-set for clones). |

---

## Filesystem layout

```
/mnt/nkr/
├── images/                              RO master disks (shared via DAX)
│   ├── odoo.ext4                        4 GB, chattr +C
│   ├── postgres.ext4                    6 GB, +C
│   └── pgbouncer.ext4                   512 MB, +C
├── initramfs/
│   ├── base/                            static busybox + kernel modules
│   ├── odoo-01.cpio.gz                  generated per-service init
│   └── ...
├── kernel/
│   └── nanolinux                        custom kernel (~5 MB)
├── cells/
│   └── company_client/
│       ├── cell.yml                     { name, cell_id, odoo_version }
│       ├── nkr-compose.yml              multi-service YAML
│       ├── nkr-compose.yml.bak.*        timestamped backups
│       ├── logs/nkr-compose.log         compose log (rotated at 10 MB)
│       ├── pg/data.ext4                 2 GB, +C, per-cell PG state
│       ├── instances/
│       │   └── company_client-odoo-01/
│       │       ├── odoo.ext4            4 GB clone of master, +C
│       │       ├── config/odoo.conf
│       │       ├── addons/              GitHub webhook drop-zone
│       │       ├── filestore/
│       │       ├── logs/odoo.log
│       │       └── meta.json            API-tracked metadata
│       └── .nkr-data/
│           ├── odoo-01-var_lib_odoo.ext4    2 GB, +C, per-instance filestore
│           ├── odoo-01-overrides/           read-only bind sources (odoo.conf)
│           └── db-env                       nkr-env for PG VM
├── registry.json                        "cell_name/vm_name" → vm_id
└── cell-registry.json                   "cell_name" → cell_id
```

---

## Security

- The `nkr serve` HTTP listener **binds to `127.0.0.1` by default**. It runs inside the root-privileged daemon. To expose it to an external panel, use an SSH tunnel (`ssh -L 9090:127.0.0.1:9090 nkr-host`), **not** `NKR_BIND_ADDR=0.0.0.0` + firewall (supported but emits a WARN).
- Bearer token comparison is constant-time (no timing leak).
- All identifiers arriving via the API are validated against `[A-Za-z0-9._-]{1,64}` → YAML injection, shell injection, log injection and path traversal are blocked at the boundary.
- Body size caps: 64 KB for `POST /instances`, 1 KB for actions. Excess → 413.
- HTTP thread pool is bounded (`MAX_INFLIGHT=64`) with 503 + `Retry-After: 1` on overflow.
- Generated shell scripts (`nkr-start.sh` per VM) **do not** interpolate user-controlled values into the final command string. `su -p` inherits the environment, so `DB_PASSWORD` etc. are passed through the kernel's `envp[]`, not through shell expansion. `eval "exec $COMMAND"` is safe because `COMMAND` contains only binary paths.
- `chattr +C` on every `/mnt/nkr/*.ext4` prevents btrfs CoW on random writes — performance defense (no fragmentation explosion) and disk-space defense (no unbounded metadata growth).

---

## Monitoring

```bash
# Prometheus metrics — scrape this
curl http://localhost:9090/metrics
```

Exposed metrics include per-VM CPU%, RSS, `balloon_mb`, DAX savings, TAP RX/TX, disk read/write, and global KSM state. Point Grafana at the Prometheus scrape — you get one row per tenant.

For interactive monitoring:

```bash
sudo nkr stats    # table view, updates every 200 ms
sudo nkr ksm      # show or tune KSM parameters
```

---

## Known limitations

1. **KSM is disabled by design** with this memory layout. `madvise(MADV_MERGEABLE)` silently returns `EINVAL` on `memfd + MAP_SHARED` (which NKR uses for `vhost-user SET_MEM_TABLE`). Density relies on virtio-fs DAX (dedups Python binaries, `.pyc`, shared libs) and a future build-time asset pre-compile (see the CLAUDE.md design notes).
2. **First boot of a fresh master** compiles Odoo assets on-demand (~55 s). Clones don't: they inherit the cache via `CREATE DATABASE ... TEMPLATE` + filestore copy.
3. **`python_libs` in the API** (to add pip packages per-tenant) is **not yet implemented** — it requires a master ext4 rebuild pipeline. Returns 500 if non-empty. Use `nkr build` with a custom `Nkrfile` for now.
4. **`.premigrate` loop devices**: if you migrate a running cell to `+C` (via `preserve_nocow`), running VMs keep FDs open to the old inode. Cleanup requires a full `compose down` → `compose up -d` of the cell. Benign until then.

---

## Troubleshooting

**VM won't boot / stuck at `[NKR-HEALTH] retrying…`**
- Check `/mnt/nkr/cells/<cell>/logs/nkr-compose.log` for the serial console output.
- Common cause: `/mnt/nkr/kernel/nanolinux` missing or wrong version.
- Verify: `file /mnt/nkr/kernel/nanolinux` — should show an ELF x86-64 bzImage.

**"RTNETLINK answers: File exists" during `compose up`**
- The netlock should prevent this. Check that `/tmp/nkr-netlink.lock` is writable and not held by a dead process.
- `ls -la /tmp/nkr-netlink.lock && sudo lsof /tmp/nkr-netlink.lock`.

**`iptables` rule duplicated after running NKR alongside docker / fail2ban / ufw**
- NKR's `iptables` calls use `-w 5` — they wait for the kernel xtables lock. If duplicates still appear, another tool bypassed the lock. Check with `iptables -L -n -v --line-numbers`.

**Odoo reports `FileNotFoundError` on `ir.attachment`**
- You cloned from an instance whose filestore changed after the clone. Re-clone, or re-sync the filestore.

**Cell disk full (`btrfs: No space left`)**
- `sudo btrfs filesystem balance /mnt/nkr`.
- Check for growing backups: `ls -la /mnt/nkr/cells/*/nkr-compose.yml.bak.* | head`.

**KSM shows 0 `pages_shared` despite many identical VMs**
- Expected — see "Known limitations" above.

---

## License

MIT. See `LICENSE`.

---

## Contributing

NKR is a specialized orchestrator, not a general-purpose container runtime. PRs welcome for:
- Kernel nanolinux improvements (shrink boot further)
- Additional health-check protocols (gRPC, tonic)
- Panel integrations (reference implementations)
- Tests (very much wanted — current coverage is thin)

Please do not PR:
- Swarm/scheduler features (NKR is single-host by design)
- Alternative hypervisor backends (KVM ioctls only; simplicity is the point)

---

## Source map for contributors

| Module | Purpose |
|---|---|
| `src/main.rs` | CLI entry point, arg parsing |
| `src/cli.rs` | `clap` command definitions |
| `src/vmm.rs` | KVM ioctls, VM lifecycle, memory/CPU/IRQ setup |
| `src/block.rs` | virtio-blk device (io_uring backend) |
| `src/net.rs` | virtio-net + TAP |
| `src/virtio_fs.rs` | virtio-fs vhost-user client (talks to `virtiofsd`) |
| `src/pmem.rs` | virtio-pmem + DAX window |
| `src/balloon.rs` | virtio-balloon (memory reclaim) |
| `src/console.rs` | virtio-console on `/dev/hvc0` for the control plane |
| `src/compose.rs` | Multi-service YAML orchestration |
| `src/cell.rs` | Cell management, clone, delete, capacity |
| `src/pull.rs` | OCI image → ext4 |
| `src/build.rs` | Nkrfile → ext4 |
| `src/initramfs.rs` | Generic initramfs generator |
| `src/registry.rs` | Persistent `vm_id` ↔ name registry |
| `src/state.rs` | Live VM state (`/tmp/nkr-vms/*.json`) |
| `src/metrics.rs` | Prometheus exporter + HTTP listener |
| `src/api.rs` | HTTP API dispatcher + request types |
| `src/fsutil.rs` | btrfs detection + `chattr +C` helpers |
| `src/netlock.rs` | Inter-process `flock` for netlink ops + `iptables -w 5` helper |
| `src/seccomp.rs` | Seccomp filter for the VMM process |
| `tools/initramfs/` | Static busybox + kernel modules (pre-built, versioned) |
| `build-kernel/` | Makefile + Docker recipe for NanoLinux |
