# NKR (Nano-Kernel Runtime)

Orquestador Rust para levantar Micro-VMs KVM con densidad extrema. Objetivo: **100 Odoos en ~32 GB RAM** distribuidos en **5 celdas × (1 pg + 1 pgbouncer + 20 odoos)**.

## Stack real

- **Orquestador:** Rust 2021 `std`, binario `nkr` (Cargo v1.3.0). Build con `cargo build --release`.
- **Initramfs:** C/busybox estático en `tools/initramfs/` (~1.4 MB). Generado por `nkr build` / auto-regenerado por `compose up`.
- **Kernel:** NanoLinux compilado aparte en `build-kernel/` (Makefile + Docker). Boot <100 ms.
- **Hipervisor:** KVM directo vía `kvm-ioctls` (rust-vmm). No Firecracker, no Cloud-Hypervisor.
- **Tooling:** `cargo` para Rust. `make` solo aplica a `build-kernel/`. `qemu` no se usa en runtime.

## CLI (src/cli.rs)

```
nkr run | ps | stop | restart | stats | ksm | serve | nitro <vm>
nkr pull <image>          # docker → ext4 en /mnt/nkr/images/
nkr build                 # Nkrfile → ext4 + initramfs
nkr compose up|down|ps    # orquestación multi-servicio
nkr cell create|ls|up|down|ps|destroy
```

## 1. Almacenamiento y memoria

- **RootFS maestro RO:** ext4 inmutable en `/mnt/nkr/images/*.ext4`. Montado por virtio-blk. Sobre btrfs host: creados con `chattr +C` antes de allocate vía `src/fsutil.rs::create_ext4_disk`. Nunca usar `truncate`/`fallocate` directo contra un ext4 en `/mnt/nkr/**`.
- **virtio-fs:** shares `host_path:guest_path`, RO por defecto, RW si se marca.
- **virtio-pmem + DAX:** **activo por default** (`pmem: true` es el default en compose desde v1.4). Bypasa page-cache del guest; ahorra ~150–200 MB/VM. Desactivar solo con `pmem: false` explícito para backing >4 GB con acceso random.
- **Discos de estado:** ext4 crudos por instancia (`odoo.ext4`, `pg/data.ext4`) creados con `+C` sobre btrfs. `e2fsck -p` previo al boot.
- **KSM:** **desactivado por diseño** (v1.4). `madvise(MADV_MERGEABLE)` sobre la RAM guest (src/vmm.rs:705-746) retorna `EINVAL` silenciosamente porque NKR usa `memfd+MAP_SHARED` (requerido por `vhost-user SET_MEM_TABLE` para virtiofsd), y el kernel rechaza MERGEABLE en VMAs con `VM_SHARED`. Ergo `full_scans=0, pages_scanned=0`. La densidad viene de **virtio-fs + DAX** (dedupa binarios Python, .pyc, libs) y (pendiente) pre-compile de assets QWeb en build-time. KSM se re-visita solo si algún día se migra a layout de memoria híbrida (anon-private + memfd-shared separados).
- **io_uring:** activo en el backend blk del host.
- **btrfs host:** `/mnt/nkr` es btrfs (`compress=zstd:3`). Todos los ext4 deben nacer con `+C` aplicado al archivo vacío para evitar fragmentación CoW catastrófica.

## 2. Red y topología de celdas

**Fórmula determinista** (src/registry.rs:215):
- `IP = 10.0.{cell_id}.{vm_id + 1}`
- `MAC = 52:54:00:{cell_id}:34:{vm_id}`

**IDs convencionales por celda:** `pg=1`, `pgbouncer=2`, `odoo-NN=3..N`.
Ejemplo cell_id=2: db→`10.0.2.2`, pgbouncer→`10.0.2.3`, odoo-01→`10.0.2.4`.

**Bridges** (src/cell.rs:116-122):
- `cell_id=0` (legacy) → `nkr0`
- `cell_id>0` → `nkr-br{N}`, gateway `10.0.{N}.1/24`

**TAPs** (src/vmm.rs:956-974):
- `nkr-tap{vm_id}` (legacy) o `nkr-c{cell_id}-tap{vm_id}`.

**NAT/Forwarding:** `iptables -C` previo (idempotente). `MASQUERADE` de `10.0.{N}.0/24`. Filtros ebtables + tc opcionales para anti-spoofing L2.

**Serialización netlink (`src/netlock.rs`):** creación de TAP, unión al bridge, reglas iptables/ebtables/tc y teardown corren bajo `flock(/tmp/nkr-netlink.lock)`. Esto elimina la carrera entre N procesos `nkr run` spawneados en paralelo por `nkr compose up` (sin esto aparecían "RTNETLINK answers: File exists" y reglas iptables duplicadas). El lock se libera por RAII o al morir el proceso. Además, todas las llamadas a `iptables` pasan por `netlock::iptables()` que inyecta `-w 5` (espera hasta 5s por el xtables lock del kernel — protege contra colisiones con fail2ban, docker, ufw y otros tocadores de iptables del host).

**Registros:**
- `cell-registry.json` — `cell_name → cell_id`.
- `registry.json` — `cell_name/vm_name → vm_id`.

## 3. Boot / initramfs

- **Kernel cmdline:** `nkr.ip=`, `nkr.rootfs=`, `nkr.fsN=<tag>`, `nkr.blkN=<dev>` (hasta 10 shares/discos).
- **Init genérico:** monta `/proc /sys /dev`, tmpfs en `/tmp /run`, `ip link set lo up`, configura eth0 con IP estática, default route hacia `10.0.{cell_id}.1`.
- **Bind-mount de overrides:** `odoo.conf` inyectado vía share RO (sin copiar a tmpfs).
- **Bypass del entrypoint de Docker** (sin `chown -R` masivos); `exec su -p` para privilege drop. PID 1 queda como el proceso final.
- **Canal de control:** `/dev/hvc0` (virtio-console, src/console.rs). El init del guest bloquea en `read -r < /dev/hvc0`. Al recibir SIGTERM el vmm inyecta `"SHUTDOWN\n"` en la receiveq + IRQ; el watcher hace `killall5 -15` (o SIGTERM a postgres coordinado vía postmaster.pid) y espera hasta 25s antes de `poweroff`.
- **Shutdown robusto** (src/vmm.rs:1911-1944): tras SIGTERM se arma `setitimer(SIGALRM, 1s)` para romper `vcpu.run()` en HLT, y se re-inyecta SHUTDOWN cada 2s mientras el guest no responda. Timeout de fallback 60s → break del vCPU loop. `state::is_pid_alive` trata zombies (`Z*` en `/proc/<pid>/status`) como muertos para que `nkr stop/restart` no cuelgue 90s esperando wait() del compose padre.

## 4. Compose (src/compose.rs)

- **IPs:** calculadas dinámicamente desde `cell_id + vm_id` y emitidas **literales** en `environment:` (ej. `DB_HOST: "10.0.1.3"`). No hay sintaxis `@name` ni `${VAR}` hoy.
- **Lanzamiento secuencial:** db → TCP-probe `:5432` → pgbouncer → TCP-probe → odoos en paralelo.
- **Rutas de instancia:** `/mnt/nkr/cells/<cell>/instances/<nkr_name>/{config,addons,filestore,logs}/`.
- **Clave lógica:** `nkr_name` (friendly, deriva rutas + DB name + backend nginx). `vm_id` (numérico, deriva IP/MAC/TAP).
- **DB por instancia:** `db-<nkr_name>`. `dbfilter=^db-<nkr_name>$`, `list_db=False`. User `odoo` compartido.

## 5. Nitro / Warmup / Cgroups

**Cgroup v2** (src/vmm.rs:1465-1545):
- Path: `/sys/fs/cgroup/nkr/{vm_name}/`.
- `cpu.max = {chrs*20000} 100000` (1 chr = 20% de core). **chrs es QUOTA, no reserva**: podés dar 5 chrs a 20 Odoos en un host de 2 vCPU sin problema; el scheduler reparte cuando compiten.
- `cpu.max.burst` si kernel ≥ 5.15 y `burst: true`.
- `memory.max = ram_mb × 1.15` (headroom 15% para stack + kernel guest). Si la VM se pasa, OOM killer local — no arrastra host ni otras VMs.

**Flujo warmup** (compose.rs, `run_health_check` + `run_warmup`):
1. Boot con `nitro_relax_cgroup()` → `cpu.max = max 100000`.
2. Health-check TCP: defaults `initial_delay=3s, interval=2s, retries=60` (120s total).
3. Primera conexión TCP OK → log `[NKR-TCP-UP]`, dispara `run_warmup()`.
4. `run_warmup()` (paralelo): 4 GETs concurrentes a `/web/assets/debug/web.assets_frontend.{css,js}`, `/web/assets/debug/web.assets_backend.js`, `/web/login`. Tiempo total = max(asset), no sum. Logs `[NKR-WARMUP] ✅ X compilado (Ts, N bytes)`.
5. Al completar warmup → `nitro_throttle_cgroup()` aplica el límite configurado. Log `[NKR-READY]`.

**Sin gracia post-warmup**: eliminado el `sleep(30s)` hardcoded (el warmup ya calentó assets + intérprete; la primera request real es <500ms).

**`nkr nitro <vm>`:** desbloqueo manual temporal; un hilo background restaura el throttle.

## 6. Odoo multi-worker

- `workers = 2+` (abandona modo werkzeug single-thread).
- `:8069` HTTP síncrono, `:8072` gevent para long-polling/websockets.

## 6b. API HTTP para panel externo (privilege-separated desde v1.5)

**Dos procesos, dos binarios:**

- **`nkr`** (root daemon) — `src/main.rs` + `src/ipc_server.rs`. Escucha UDS en `/var/run/nkr.sock` (override `NKR_SOCKET_PATH`). Despacha `IpcRequest` a los handlers en `src/api.rs`. No habla HTTP ni TCP.
- **`nkr-api-server`** (unprivileged, user `nkr-api`) — `src/bin/nkr_api_server.rs`. Escucha HTTP en `127.0.0.1:9090` (override `NKR_BIND_ADDR`/`NKR_API_PORT`). Traduce HTTP → IPC usando `src/ipc.rs::call`. No linkea kvm-ioctls, vmm, cell, state — dep-free vía `#[path]` a `ipc.rs` + `api_http.rs` (verificado: 453 KB vs 1.9 MB del daemon).

**Wire IPC (`src/ipc.rs`):** frame length-prefixed JSON (u32 LE + body), max 8 MiB. Corta conexión por request (sin multiplexing). UDS chmod `0660 root:nkr-api` — el grupo `nkr-api` es el gate.

**TLS:** no se maneja en Rust. Nginx/Caddy al frente del proxy (el mismo edge que sirve Odoo en §7 ya tiene Let's Encrypt wildcard). Ejemplo nginx:

```nginx
server {
    listen 443 ssl http2;
    server_name nkr.tudominio.com;
    ssl_certificate     /etc/letsencrypt/live/tudominio.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/tudominio.com/privkey.pem;
    allow 203.0.113.10;                 # IP(s) del panel externo
    deny  all;
    location / {
        proxy_pass http://127.0.0.1:9090;
        proxy_http_version 1.1;
        proxy_set_header Host $host;
        proxy_read_timeout 120s;        # clone tarda ~30-40s
        proxy_send_timeout 120s;
    }
}
```

Con esto el panel pega HTTPS directo (sin SSH tunnel) y escala a N clientes de NKR en servidores distintos.

**Endpoints expuestos por el proxy** (todos traducen a variantes de `IpcRequest`):
- `GET /metrics` — Prometheus text exposition 0.0.4 (scraping Grafana/Prometheus; sin auth por default — Prometheus no manda Bearer)
- `GET /api/v1/health` — health check (sin auth)
- `GET /api/v1/cells` — lista cells con `odoo_version`, `used_odoos`, `free_slots`, `max_odoos`
- `POST /api/v1/instances` — crea instancia con **auto-selección de cell** por `odoo_version`
- `POST /api/v1/cells/{cell}/instances` — crea forzando cell explícita (valida versión igual)
- `GET  /api/v1/cells/{cell}/instances/{nkr_name}` — info + `nkr_status`
- `DELETE /api/v1/cells/{cell}/instances/{nkr_name}?drop_db=1` — elimina
- `POST /api/v1/cells/{cell}/instances/{nkr_name}/actions` — `{"action":"start|stop|restart"}`
- `GET  /api/v1/cells/{cell}/instances/{nkr_name}/logs?tail=N` — tail `odoo.log`

**Auth:** si `NKR_API_TOKEN` está en el env del proxy (`nkr-api-server`), todas las rutas excepto `/metrics` y `/api/v1/health` requieren `Authorization: Bearer $NKR_API_TOKEN`. Sin la env var, el API pasa sin auth (modo dev). Comparación constant-time (`api_http::ct_eq`).

**Deploy con systemd:** units listas en `deploy/systemd/{nkr.service,nkr-api-server.service}`. El daemon corre con hardening mínimo (necesita KVM, cgroups, iptables). El proxy corre con `NoNewPrivileges=true`, `ProtectSystem=strict`, `ProtectHome`, `PrivateTmp`, `PrivateDevices`, `ProtectKernel{Tunables,Modules,Logs}`, `ProtectControlGroups`, `ProtectProc=invisible`, `ProcSubset=pid`, `RestrictNamespaces`, `RestrictRealtime`, `RestrictSUIDSGID`, `LockPersonality`, `MemoryDenyWriteExecute`, `RemoveIPC`, `RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6`, `SystemCallFilter=@system-service ~@privileged @resources @mount @cpu-emulation @debug @obsolete`, `CapabilityBoundingSet=` (vacío), `LimitNPROC=256`, `LimitNOFILE=1024`. Instrucciones completas en `deploy/systemd/README.md`.

**Validación defense-in-depth:** el proxy valida identificadores (`is_safe_identifier`/`is_safe_dns`/`is_safe_addons_path` en `api_http.rs`) ANTES de mandar al UDS; el daemon **re-valida** todo en los `handle_*` de `api.rs` por si alguien con acceso al socket bypasea el proxy. YAML/shell/path-traversal bloqueados en ambos bordes.

**Body de POST /instances (campos opcionales marcados con `?`):**
```json
{
  "nkr_name": "tst-1",              // corto ("tst-1") o completo ("company_client-tst-1")
  "mode": "dev" | "production",
  "odoo_version": "17.0",           // REQUERIDO — cada cell soporta una sola versión
  "cell": "company_client?",              // si se omite, auto-selecciona la cell menos llena
  "source": "company_client-odoo-01?",    // si se omite, usa el primer Odoo de la cell como template
  "dns": "company_client-tst-1.systemouts.com?",
  "edition": "community" | "enterprise" | null,
  "pg_version": "15?",
  "workers": 2,
  "list_db": false,
  "limit_memory_soft": 2147483648,
  "limit_memory_hard": 2684354560,
  "addons_path": "/usr/lib/python3/.../addons,/mnt/extra-addons?",
  "python_libs": [],
  "balloon_mb": null                 // opcional: override del default derivado de workers
}
```
- `mode=dev` → clona archivos + DB (CREATE DATABASE ... TEMPLATE).
- `mode=production` → clona archivos, DB vacía (el panel la hidrata).
- `python_libs` no vacío → 500 hoy (requiere rebuild del master ext4, endpoint `/build` pendiente).
- `balloon_mb` opcional: si se omite (caso normal) NKR aplica el default `derive_resources()` = `max(0, ram_mb − limit_memory_hard_mb − 256)` con floor a 0 si <64 MB. Tabla por workers: 1→0, 2→292, 4→840, 8→1936. El panel puede mandar override (ej. 0 para desactivar, o 512 para densidad agresiva con `workers=2`). El cap de seguridad interno es `ram_mb × 256` páginas (=100 % de la RAM); cualquier valor razonable está debajo.

**Reglas de validación (src/cell.rs + src/api.rs):**
- `odoo_version` debe coincidir con `cell.odoo_version` (del `cell.yml`). Si no, 409 `version_mismatch`.
- Máx **20 Odoos por cell** (constante `MAX_ODOOS_PER_CELL`). Si la cell está llena, 409 `cell_full`.
- Auto-selección de cell: filtra por `odoo_version` igual, ordena por `used_odoos` ascendente (menos llena primero). Si ninguna matchea, 409 `no_cell_available` (el mensaje lista las disponibles).
- `nkr_name` se auto-prefija con el cell name si llega corto (ej. "tst-1" → "company_client-tst-1").
- `source` opcional: si el panel no lo manda, el backend toma el primer instance dir alfabético de la cell seleccionada como template.

**Respuesta:** `InstanceInfo` con `guest_ip`, `dns`, `addons_path`, `logs_path`, `config_path`, `db_name`, y `nkr_status { running, pid, ram_mb, uptime_s, port_8069_up }`. El panel usa `addons_path` para apuntar el webhook de GitHub.

**Metadata persistida:** cada instancia creada vía API escribe `meta.json` junto al dir (`/mnt/nkr/cells/<cell>/instances/<name>/meta.json`) con los parámetros originales — el GET reconstruye el estado leyéndolo.

**Config Odoo escrita:** `rewrite_odoo_conf_full` en `src/cell.rs` hace upsert (INI-style) sobre `odoo.conf` de las keys `dbfilter`, `db_name`, `workers`, `list_db`, `limit_memory_soft`, `limit_memory_hard`, `addons_path`. Las demás keys del `odoo.conf` original se conservan.

**Delete:** detiene VM (SIGTERM), drop DB (opcional vía `drop_db=0` para preservar), remueve bloque del `nkr-compose.yml` (con backup `.bak.<ts>`), libera `vm_id` del registry, borra dir de instancia, limpia `.nkr-data/<short>-*` (filestore + overrides).

**Skip warmup en clones (v1.4):** el warmup HTTP contra `/web/assets/debug/*` fuerza recompile de assets ignorando el cache heredado (55s + CPU spike que impacta cells vecinas). Auto-descubierto: el clone TEMPLATE + filestore ext4 YA preserva `ir_attachment` + archivos físicos (22 rows + 697 archivos validados), así que el warmup runtime es redundante. `append_compose_block` ahora inyecta `skip_warmup: true` en cada bloque clonado. `run_health_check` lo detecta y marca `[NKR-READY]` inmediatamente tras el TCP-UP. Primer `/web/login` real es ~5s (Python import) vs 55s de warmup previo. Cambio: -50s por clone + 0% CPU spike en cells activas.

**Filestore rename (post-clone):** en vez de montar el `.ext4` en loop desde el host (bloquea kernel bajo clones concurrentes, riesgo de corrupción mid-mv), `clone_instance_with_opts` inyecta al compose block del dst las env vars:
```yaml
NKR_RENAME_FILESTORE_FROM: "db-<src>"
NKR_RENAME_FILESTORE_TO:   "db-<dst>"
```
El `nkr-start.sh` del initramfs, antes de `exec odoo`, detecta las vars y hace `mv /var/lib/odoo/filestore/db-<src> /var/lib/odoo/filestore/db-<dst>`, sellando un marker `.nkr-filestore-renamed` para idempotencia en reboots. El rename ocurre en la CPU del guest, paralelo por VM, sin tocar el host.

### Ejemplos curl (panel externo)

Arrancar ambos servicios con token (producción vía systemd — ver `deploy/systemd/README.md`):
```bash
# daemon UDS (root)
sudo systemctl enable --now nkr.service

# proxy HTTP (unprivileged, lee NKR_API_TOKEN de /etc/nkr/api.env)
sudo systemctl enable --now nkr-api-server.service
# Con nginx al frente → panel hace HTTPS directo (sin SSH tunnel).
```
En dev/local (un solo host, sin systemd):
```bash
sudo nohup nkr serve >/tmp/nkr-daemon.log 2>&1 & disown
sudo -u nkr-api NKR_API_TOKEN=dev nohup nkr-api-server >/tmp/nkr-api.log 2>&1 & disown
```

**1. Health (sin auth):**
```bash
curl -s http://nkr-host:9090/api/v1/health
# → {"ok":true,"version":"1.3.0"}
```

**2. Listar cells con capacidad:**
```bash
curl -s -H "Authorization: Bearer $TOKEN" http://nkr-host:9090/api/v1/cells
# → {
#   "cells": [
#     { "name":"company_client", "cell_id":1, "odoo_version":"17.0",
#       "used_odoos":3, "max_odoos":20, "free_slots":17 }
#   ],
#   "max_odoos_per_cell": 20
# }
```

**3. Crear instancia DEV (clon completo con DB), auto-selección de cell:**
El panel sólo sabe la versión del cliente — NKR elige la cell menos llena con esa versión:
```bash
curl -s -X POST http://nkr-host:9090/api/v1/instances \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "nkr_name": "cliente-42",
    "mode": "dev",
    "odoo_version": "17.0",
    "dns": "cliente-42.systemouts.com",
    "edition": "community",
    "workers": 2,
    "list_db": false,
    "limit_memory_soft": 2147483648,
    "limit_memory_hard": 2684354560
  }'
# → 201 {
#   "nkr_name": "company_client-cliente-42",    ← prefijo cell auto-añadido
#   "cell": "company_client",
#   "vm_id": 6, "guest_ip": "10.0.1.7",
#   "dns": "cliente-42.systemouts.com",
#   "db_name": "db-company_client-cliente-42",
#   "addons_path": "/mnt/nkr/cells/company_client/instances/company_client-cliente-42/addons",
#   "logs_path":   "/mnt/nkr/cells/company_client/instances/company_client-cliente-42/logs/odoo.log",
#   "config_path": "/mnt/nkr/cells/company_client/instances/company_client-cliente-42/config/odoo.conf",
#   "instance_dir":"/mnt/nkr/cells/company_client/instances/company_client-cliente-42",
#   "meta": { ... },
#   "nkr_status": { "running": true, "pid": 170123, "ram_mb": 248, "uptime_s": 4, "port_8069_up": true }
# }
```

**4. Crear instancia PRODUCCIÓN (sin DB, el panel la hidrata):**
```bash
curl -s -X POST http://nkr-host:9090/api/v1/instances \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "nkr_name": "cliente-prod-1",
    "mode": "production",
    "odoo_version": "17.0",
    "edition": "enterprise",
    "workers": 4,
    "limit_memory_hard": 4294967296
  }'
# → 201 { ..., "mode":"production" }
# El panel luego crea la DB a mano (CREATE DATABASE, restore dump, etc.)
```

**5. Crear forzando cell explícita (error si la versión no matchea):**
```bash
curl -s -X POST http://nkr-host:9090/api/v1/cells/company_client/instances \
  -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
  -d '{ "nkr_name":"x", "mode":"dev", "odoo_version":"16.0" }'
# → 409 {
#   "error":"version_mismatch",
#   "cell":"company_client", "cell_version":"17.0", "requested_version":"16.0",
#   "message":"Cell 'company_client' está en odoo_version=17.0, panel pidió 16.0"
# }
```

**6. No hay cell libre con la versión pedida:**
```bash
curl -s -X POST http://nkr-host:9090/api/v1/instances \
  -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
  -d '{ "nkr_name":"x", "mode":"dev", "odoo_version":"19.0" }'
# → 409 { "error":"no_cell_available",
#         "message":"No hay cells con odoo_version=19.0. Cells disponibles: [\"company_client=17.0\"]",
#         "requested_version":"19.0" }
```

**7. Ver info + estado real (nkr_status) de una instancia:**
```bash
curl -s -H "Authorization: Bearer $TOKEN" \
  http://nkr-host:9090/api/v1/cells/company_client/instances/company_client-odoo-01
# → { ..., "nkr_status": { "running":true, "pid":163257, "ram_mb":473,
#                          "uptime_s":2153, "port_8069_up":true } }
```

**8. Lifecycle — start / stop / restart (async desde v1.5.1):**
```bash
curl -s -X POST http://nkr-host:9090/api/v1/cells/company_client/instances/company_client-odoo-01/actions \
  -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
  -d '{"action":"restart"}'
# → 202 { "action":"restart", "status":"accepted", "async":true,
#        "info": { ..., nkr_status SNAPSHOT pre-acción } }
# Los valores válidos de action: "start" | "stop" | "restart"
#
# IMPORTANTE: la acción se ejecuta en background. La 202 vuelve en <50ms.
# El `nkr_status` del payload es del MOMENTO PREVIO a la acción —
# para detectar readiness, polear:
#   GET /api/v1/cells/{cell}/instances/{name} → nkr_status.port_8069_up
# Cadencia recomendada: cada 1s, hasta 180s.
#
# Si ya hay un start/stop/restart en vuelo para la misma instancia:
# → 409 { "error":"action_in_progress", "nkr_name":"...", "message":"..." }
# El panel debería esperar a que el anterior termine (polear) en lugar
# de reintentar inmediatamente.
```

**9. Tail logs en vivo (el panel los muestra en la UI web):**
```bash
curl -s -H "Authorization: Bearer $TOKEN" \
  "http://nkr-host:9090/api/v1/cells/company_client/instances/company_client-odoo-01/logs?tail=100"
# → { "nkr_name":"company_client-odoo-01",
#     "logs_path":"/mnt/nkr/cells/company_client/instances/company_client-odoo-01/logs/odoo.log",
#     "tail":100, "lines":[ "...", "...", ... ] }
# tail default=200, máximo=10000
```

**10. Eliminar instancia (con y sin drop DB):**
```bash
# Drop DB (default): borra todo incluida la DB PG
curl -s -X DELETE -H "Authorization: Bearer $TOKEN" \
  http://nkr-host:9090/api/v1/cells/company_client/instances/company_client-cliente-42
# → 200 { "deleted":true, "cell":"company_client", "drop_db":true }

# Preservar DB (útil si querés migrarla a otra instancia):
curl -s -X DELETE -H "Authorization: Bearer $TOKEN" \
  "http://nkr-host:9090/api/v1/cells/company_client/instances/company_client-cliente-42?drop_db=0"
# → 200 { "deleted":true, "drop_db":false }
```

**11. Flujo típico del panel (añadir cliente nuevo):**
```bash
# (a) Panel pregunta capacidad por versión
GET /api/v1/cells
→ cells con free_slots

# (b) Panel crea la instancia (auto-select)
POST /api/v1/instances  { nkr_name, mode, odoo_version, dns, workers, ... }
→ 201 con addons_path + logs_path

# (c) Panel configura webhook GitHub apuntando a info.addons_path
# (d) Panel actualiza el nginx SNI map: dns → guest_ip:8069
# (e) Panel hace reload nginx (fuera de NKR)
# (f) Panel consulta periódicamente info.nkr_status.port_8069_up para marcar "ready"
```

**12. Error típico — capacity llena:**
```bash
# Cuando la cell company_client llega a 20 Odoos:
POST /api/v1/cells/company_client/instances { ..., "odoo_version":"17.0" }
# → 409 { "error":"cell_full", "cell":"company_client", "used":20, "max":20 }
# El panel debería: (a) crear otra cell company_client-2 / (b) informar al operador.
```

## 6c. Hardening del API (v1.4)

Audit completo aplicado — lista de mitigaciones activas:

- **Privilege separation (v1.5)**: el API HTTP corre en un proceso aparte (`nkr-api-server`, user `nkr-api`, sin capabilities). El daemon root (`nkr serve`) sólo expone UDS en `/var/run/nkr.sock` (0660 root:nkr-api). Un RCE en el parser HTTP **no** pisa KVM/cgroups/iptables — lo más que consigue es lo que los handlers del daemon permiten.
- **Bind default `127.0.0.1`**: el proxy no escucha en la red por default. Nginx/Caddy al frente para TLS + ACL de IPs del panel. Override `NKR_BIND_ADDR=0.0.0.0` (emite WARN; sólo para labs).
- **Timing-safe Bearer compare** (`netlock::ct_eq` — reusado en api.rs): loop constante sin short-circuit, inmune a timing attacks.
- **Input validation en el boundary**: `nkr_name`, `cell`, `source`, `odoo_version`, `pg_version` deben matchear `[A-Za-z0-9._-]{1,64}`. `dns` sólo `[A-Za-z0-9.-]{1,253}`. `addons_path` rechaza newlines/quotes/backticks/`$`. Bloquea de raíz: YAML injection, shell injection, log injection, path traversal.
- **Body size limits**: POST /instances max 64 KB, POST /actions max 1 KB. 413 si se excede.
- **Bounded log reader**: `read_tail_lines` hace seek reverse por chunks de 64 KiB con cap de 4 MiB en RAM. Rechaza DoS con `odoo.log` multi-GB.
- **Thread pool bounded**: `MAX_INFLIGHT=64` handlers HTTP concurrentes; excedentes reciben 503 con `Retry-After: 1`. `InflightGuard` RAII decrementa incluso si el handler panic.
- **Info disclosure hardening**: errores genéricos al cliente (`{"error":"not_found"}`), detalle sólo a stderr del operador.
- **Panic protection**: `read_request` usa `?` en vez de `.unwrap()`; request malformado → 400, no thread crash.
- **verify_dependencies() al boot**: `nkr serve` chequea 11 binarios críticos (ip, iptables, mount, mkfs.ext4, e2fsck, losetup, chattr, psql, pg_isready, virtiofsd, umount) y loguea los faltantes.
- **Shell hardening**: `eval "exec $COMMAND"` del `nkr-start.sh` generado ya NO interpola `DB_HOST/PASSWORD/*` en el string del comando — `su -p` hereda el env. Evita inyección si una var tiene `;` o `$(...)`. Trap `EXIT INT TERM` limpia temp dirs si el script muere.

## 7. Nginx edge (host GCP)

- TLS wildcard Let's Encrypt `*.tudominio.com`.
- Enrutamiento **O(1) por SNI** con `map` estático → IP interna de la celda.
- `/websocket` → `:8072` con headers Upgrade. `/` → `:8069`.
- `proxy_cache` para `/web/static/` (los 99 clientes restantes no tocan la VM).
- WAF básico: 403 sobre payloads PHP conocidos.

---

## Pendientes (arquitectónicos, no bloqueantes)

- **KSM con memoria híbrida**: hoy `MADV_MERGEABLE` no funciona sobre `memfd+MAP_SHARED`. Para habilitarlo haría falta rediseñar el layout a `anon-private` (parte mergeable) + `memfd-shared` separado (sólo para vhost-user descriptors). 1-2 semanas de trabajo, ganancia ~60-120 MB/VM. Sólo si 110 VMs no entran en 32 GB con el stack actual.
- **Pre-compile QWeb en build-time**: actualmente el primer `/web/login` de un master recién buildeado compila assets on-demand (~55s). Bakear attachments + `template_odoo_XX` DB en el master elimina ese costo. Ver §6b.
- ~~**Daemon unpriv + unix socket**~~ ✅ **Completado v1.5** — ver §6b. Split en `nkr` (root, UDS) + `nkr-api-server` (user `nkr-api`, HTTP). TLS externo por nginx/caddy.
- **Hot-path `.unwrap()` recovery**: 50+ `.lock().unwrap()` en MMIO/TAP/BLK handlers tumban la VM si el mutex se envenena. Aceptable mientras no haya signal delivery raro; para alta disponibilidad, reemplazar por `expect()` con contexto + circuit breaker.
- **Integration tests API**: hoy sólo 4 unit tests en `fsutil`. Falta cobertura de create/start/stop/delete via API contra cell efímera.
- **Compose portable entre cells**: los YAML de ejemplo llevan IPs hardcodeadas en `environment:`. Resolver con placeholders (`${PG_IP}`/`${PGB_IP}`) expandidos en `compose.rs` antes de armar el env.

## Condición transitoria conocida

**Loops con `(deleted)` tras migración +C**: si se corrió `scripts/migrate-nocow.sh` (o `fsutil::preserve_nocow` implícito en clones) con VMs corriendo, las VMs activas mantienen FDs al inode viejo (archivo `.premigrate` renombrado/borrado). Se ven en `losetup -a`. Benignos, pero ocupan entries del loop table. Se limpian definitivamente con un `compose down` completo de la cell → `compose up -d`.

---

## Convenciones del agente

- **Sin pleasantries.** Respuestas directas, sin "entendido" ni "claro".
- **Diffs, no rewrites.** Solo líneas cambiadas o unified diff, salvo que se pida rewrite explícito.
- **Verifica antes de leer.** `ls`/`find`/`Glob` antes de `Read` cuando no sepas la ruta exacta.
- **Build largos:** si un build supera 5 min, avisa y espera input.
- **Bullets > prosa.** Explicar lógica solo en refactors no-triviales (ACPI/IOAPIC/cgroups).
