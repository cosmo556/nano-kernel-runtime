# NKR API — Referencia para el panel de control

Documento destinado al agente que opera el panel. Describe exactamente qué endpoints existen, cómo autenticarse, qué mandar, qué esperar, y cómo integrar el flujo de webhooks de GitHub con el sistema de addons.

> NKR v1.5 — privilegios separados: `nkr` (root daemon, sólo UDS) + `nkr-api-server` (unprivileged, HTTP). Sólo el proxy HTTP está expuesto al panel.

## TL;DR para panel-Claude

### Credenciales y URL

```bash
# URL pública del API (HTTPS via nginx, TLS Let's Encrypt):
export NKR_API_BASE=https://nkr-api.systemouts.com

# Token (hex de 64 chars — guardalo como secret en el panel, nunca en git):
export NKR_API_TOKEN=c192362ccacce6688c2bb68f0e0f856c14d6a865cfb26ff4942d2062cf7c9000

# Header estándar en todos los requests (salvo /metrics y /api/v1/health):
export TOK="Authorization: Bearer $NKR_API_TOKEN"
```

**Estado deployment (2026-04-22):**
- DNS `nkr-api.systemouts.com` → `116.202.240.179` ✅ activo.
- Cert Let's Encrypt ✅ instalado (expira 2026-07-21, auto-renewal vía certbot systemd timer).
- Vhost nginx ✅ deployed en `/etc/nginx/sites-enabled/nkr-api` (ver §11.1).
- API HTTPS lista para consumo externo. Smoke test:
  ```
  curl -s https://nkr-api.systemouts.com/api/v1/health
  # → {"ok":true,"version":"1.6.4"}
  ```

### Conceptos mínimos

- **Cells hoy disponibles:** `odoo-v17` (PG16 + PgBouncer, Odoo 17.0) y `odoo-v19` (PG16 + PgBouncer, Odoo 19.0). Ambas con 19 slots libres (1 ocupado por el template).
- **Patrón mental:** cell = rack (versión fija + infra compartida), instancia = tenant.
- **Cada cell tiene un `<cell>-odoo-template` reservado** como source automático de `mode=production`. Está apagado por default — su DB vive en PG y los archivos en disco; eso basta para clonar. Ver §4.4 "Convención del template".
- **`POST /instances` es ASÍNCRONO (v1.6.4+)** — valida sync (4xx al toque), despacha el clone en background y devuelve `202`. **SLA v1.6.5: `status=ready` típico en 10–20 s** en cualquier tier (dev/staging/production) y cualquier edition (community/enterprise — el theme enterprise viene pre-instalado en el template, ver §4.4.2). Defaults de arranque:<br>
  • Sin `admin_user_password` y sin `auto_start:true` → cold-prepared (VM no arranca; bloque del compose con `disabled: true`). Panel arranca después con `POST /actions {start}`.<br>
  • Con `admin_user_password` (sólo permitido si source es template del cell) → NKR rota admin/admin → admin/`<tu_pwd>` y arranca el tenant.<br>
  • Con `auto_start: true` (típico en clones-from-tenant: staging, mode=dev) → arranca el tenant sin tocar credenciales (el clone hereda la pwd del source).<br>
  Secuencia del panel:
  1. `POST /instances` con `mode=production` (cliente nuevo) o `mode=dev + source` (clone de tenant existente) → `202 {nkr_name, poll}`. **Usar el `nkr_name` devuelto.**
  2. Poll `GET {poll}` (= `GET /instances/{name}/create-status`) cada 2-3s hasta `status=ready` (o `status=failed` → leer `error`/`message`/`hint`). Ver §4.4.1. Si esperás >60 s sin `ready`, es señal de problema (no de boot lento normal).
  3. `POST /addons/git` / `PUT /pylibs` — opcional, con VM apagada.
  4. `POST /actions {action:"start"}` — devuelve `202` en <50 ms (async desde v1.5.1). El boot real tarda ~30-60 s (PROD prefork más); el panel **debe polear** `GET /instances/{name}` → `nkr_status.phase` hasta `loading|ready`. (Si mandaste `admin_user_password` en el paso 1, el tenant ya quedó arrancado — verificá con el `create-status` que `running=true`.)
  5. Updates posteriores — **la estrategia depende del tier** (ver §7.0):
     - **`tier=staging` o `tier=dev`**: `POST /addons/git` con `auto_reload=true` (default). NKR dispara `REL_OD` vía HVC0 al PID de la VM y el supervisor loop respawnea Odoo con el código nuevo en ~3 s. **No llamar restart NI upgrade** — `auto_reload` ya lo cubre. NO depende de `dev_mode` (NKR ya no lo setea en staging/dev desde v1.6.3 por incompatibilidad con virtio-fs + presión runtime del watcher; ver §7.0).
     - **`tier=production`, módulo NUEVO**: `POST /addons/git` y listo. Visible para Install desde Apps (UI). No requiere restart.
     - **`tier=production`, módulo ya instalado que cambió código**: `POST /addons/git` + `POST /actions {action:"restart"}`. **NO usar `POST /modules/upgrade` automático en multi-worker** — solo refresca código en el worker que procesa el upgrade, los demás siguen con código viejo (gotcha conocido de Odoo en `workers≥2`). El restart es la opción correcta para garantizar que TODOS los workers cargan el código nuevo. Tiempo: ~15-25s de downtime con el initramfs v1.6.1+ (timer drain reducido a 5s). En workflows con CI/CD apuntando a prod, el restart automático tras git push es válido — solo asegurate de que el panel polee `nkr_status.port_8069_up` para no servir tráfico mid-restart.
     - **Cambio de pip libs / odoo.conf / kernel** (cualquier tier): requiere `POST /actions {action:"restart"}`.
  6. **Métricas de un tenant** (para una pestaña tipo "Métricas" en el panel): `GET /api/v1/cells/{cell}/instances/{name}/metrics` → JSON con CPU/RAM/disco/red/etc. de esa instancia. Pollear cada ~2s mientras la pestaña esté abierta (cacheado server-side, no sobrecarga). Ver §4.1.1.
- **Para probar sin romper nada**: usá nombres con prefijo `test-` o `smoke-`. El endpoint `DELETE` te los limpia después (incluye drop de DB). Ver §12 al final.

### Primer request de prueba

```bash
curl -s -H "$TOK" $NKR_API_BASE/api/v1/cells | python3 -m json.tool
# Si devuelve las 2 cells → ok, podés empezar a crear tenants.
# Si devuelve 401 → token mal. Si 000/timeout → URL mal.
```

---

## 1. Arquitectura de la API

```
┌─────────────┐     HTTPS (nginx)     ┌────────────────────┐   UDS /var/run/nkr.sock   ┌──────────────┐
│  Panel de   │ ───────────────────▶  │  nkr-api-server    │ ────────────────────────▶ │  nkr (root)  │
│   control   │                       │  127.0.0.1:9090    │   framed JSON IPC         │   daemon     │
└─────────────┘                       │  user=nkr-api      │                           └──────────────┘
                                      │  sin capabilities  │                                   │
                                      └────────────────────┘                                   ▼
                                                                                        KVM, cells, PG,
                                                                                        iptables, cgroups
```

- **Puerto default:** `127.0.0.1:9090` (HTTP plano). Nginx/Caddy al frente para TLS + ACL de IPs.
- **Override binding:** `NKR_BIND_ADDR=0.0.0.0 NKR_API_PORT=9090`. Emite WARN si escucha público.
- **Auth:** `Authorization: Bearer $NKR_API_TOKEN` (excepto `/metrics` y `/api/v1/health`). Si la env var está vacía en el proxy, modo dev (sin auth).
- **Comparación timing-safe** (`ct_eq`) — inmune a timing attacks.
- **Concurrencia:** 64 handlers HTTP concurrentes máx. Excedentes reciben `503 Retry-After: 1`.
- **Body size:** `POST /instances` max 64 KiB, `POST /actions` max 1 KiB.

---

## 2. Formato de errores

Todos los errores son JSON:

```json
{ "error": "slug_identificador", "message": "texto humano opcional" }
```

Códigos HTTP usados:

| Código | Cuándo |
|--------|--------|
| `200`  | OK (GET) |
| `201`  | (legacy) creación síncrona — ya no se usa para `POST /instances` (ahora 202, ver §4.4) |
| `202`  | Acción/operación aceptada y corriendo en background: `POST /instances` (create async, §4.4), `POST /actions {start,stop,restart}`, `DELETE /instances`, `POST /init-db` |
| `400`  | JSON inválido, identificador inválido, action desconocida, campos faltantes |
| `401`  | Bearer token mal o ausente |
| `404`  | Instancia no existe (o, en `/create-status`, no hay ni create en curso ni instancia) |
| `409`  | Conflicto: `cell_full`, `version_mismatch`, `no_cell_available`, `cell_template_missing`, `instance_already_exists`, `create_in_progress`, `action_in_progress`, `source_not_allowed_in_production`, … |
| `413`  | Body excede el límite |
| `500`  | Error interno |
| `502`  | Daemon UDS no responde (`daemon_unreachable`) |
| `503`  | Server busy (`retry_after: 1s`) o `spawn_failed` (no se pudo lanzar el thread del job async) |

---

## 3. Reglas de validación (proxy + daemon re-validan)

- **`nkr_name`, `cell`, `source`, `odoo_version`, `pg_version`**: regex `[A-Za-z0-9._-]{1,64}`.
- **`dns`**: `[A-Za-z0-9.-]{1,253}`.
- **`addons_path`**: rechaza `\n`, `\r`, `"`, `'`, backtick, `$`, NUL. Max 1024 chars.
- **Máx 20 Odoos por cell** (constante `MAX_ODOOS_PER_CELL`).
- **Una sola versión de Odoo por cell** (definida en `cell.yml`).

---

## 4. Endpoints

### 4.1 `GET /metrics` — Prometheus (sin auth)

Formato `text/plain; version=0.0.4` (Prometheus exposition). Sin auth — pensado para scrape directo. Servido por `nkr-api-server` (puerto `:9090`) → IPC `RenderMetrics` → daemon (`src/metrics.rs::render_prometheus_metrics`). Cada scrape toma una ventana de CPU de ~50 ms (latencia del request `<100 ms`). Poné un Prometheus/Grafana al frente con `scrape_interval: 15s` típico.

**Catálogo de métricas** (todas las per-VM llevan label `vm="<nkr_name>"`, p.ej. `vm="odoo-v19-intech-devp"`):

| Métrica | Tipo | Significado |
| :--- | :--- | :--- |
| `nkr_cpu_pct{vm}` | gauge | % CPU del proceso `nkr` (VMM) — ventana instantánea de ~50 ms, valor 0–100. **Es jittery** (ventana corta): para dashboards estables promediá en Grafana (`avg_over_time(...[1m])`) o derivá de `nkr_rss_mb`/tráfico. |
| `nkr_rss_mb{vm}` | gauge | RSS real del proceso VMM en el host (RAM física que la VM le cuesta al host *ahora*). La métrica clave de densidad. |
| `nkr_ram_allocated_mb{vm}` | gauge | RAM asignada a la VM al boot (`compose.ram`). DEV=1300, STAG=1024, PROD=`max(1024,512+768·W)`, db=1024, pgb=128. |
| `nkr_balloon_mb{vm}` | gauge | RAM inflada en el VirtIO-Balloon (= devuelta al host). Refleja el estado **runtime**: 0 cuando ACTIVE, 256 cuando un tenant DEV decayó a IDLE, etc. Se actualiza en cada transición ACTIVE↔IDLE. |
| `nkr_dax_savings_mb{vm}` | gauge | Estimación de RAM ahorrada por DAX/virtio-pmem (el page-cache del guest no se duplica en el host): `max(0, ram_allocated − rss − balloon − 50 MB overhead)`. Sólo en VMs con DAX (rootfs virtio-fs/pmem). |
| `nkr_total_savings_mb{vm}` | gauge | `balloon_mb + dax_savings_mb` por VM. |
| `nkr_io_read_bytes{vm}` | counter | Bytes leídos de disco por el proceso VMM (`/proc/<pid>/io`). **Nota**: en tenants Odoo el rootfs es virtio-fs servido por un proceso `virtiofsd` aparte → esas lecturas NO las ve el VMM; este counter sólo cuenta el block device `/var/lib/odoo`, así que suele ser bajo o 0. No es bug — es la fuente de datos. |
| `nkr_io_write_bytes{vm}` | counter | Bytes escritos a disco por el VMM. Idem nota de arriba (el grueso de escrituras de Odoo a virtio-fs no cuenta acá). |
| `nkr_net_rx_bytes{vm}` | counter | Bytes recibidos en la interfaz TAP de la VM (tráfico externo→VM). Usar `rate()` en Grafana. |
| `nkr_net_tx_bytes{vm}` | counter | Bytes transmitidos por la TAP (VM→externo). |
| `nkr_cpu_seconds_total{vm}` | counter | **(v1.6.4)** Segundos de CPU consumidos por el cgroup de la VM (`/sys/fs/cgroup/nkr/<vm>/cpu.stat usage_usec`) — incluye el thread vCPU + los helpers `virtiofsd`/vhost atribuidos a esa VM, así que es más completo y menos jittery que `nkr_cpu_pct`. Usar `rate()` en Grafana → "% CPU" sostenido. **Esta es la métrica de CPU correcta para dashboards/billing del panel** (la VM está limitada a `chrs` cores; `rate / chrs` = utilización). |
| `nkr_cpu_throttled_seconds_total{vm}` | counter | **(v1.6.4)** Segundos que la VM estuvo retenida fuera de la CPU por su cuota `cpu.max` (= `chrs`). Si crece sostenidamente, el tenant necesita más `chrs` (o está bajo abuso). |
| `nkr_cgroup_memory_bytes{vm}` | gauge | **(v1.6.4)** RAM física del host cargada al cgroup de la VM (`memory.current`) — VMM + helpers virtiofsd/vhost + page cache que el kernel le carga al cgroup. Más completo que `nkr_rss_mb` (que es sólo el proceso VMM). Combinado con `nkr_ram_allocated_mb` da un proxy razonable de "cuánta RAM usa el tenant" hasta que estén las métricas guest-internas (ver abajo). |
| `nkr_guest_mem_total_bytes{vm}` / `_available_bytes` / `_free_bytes` / `_cached_bytes` | gauge | **(v1.6.4)** RAM *vista desde dentro del guest* (`MemTotal`/`MemAvailable`/`MemFree`/`Cached`), vía el stats virtqueue del virtio-balloon. Sólo aparece para VMs cuyo guest ya reportó (driver del balloon arriba + ~30s). `available_bytes` es el bueno para "cuánto le queda de RAM al tenant". Se refresca cada ~30s. |
| `nkr_up{vm,cell,tier}` | gauge | **(v1.6.4)** `1` si la VM corre, `0` si es un tenant conocido pero parado. Es una **métrica info** — lleva los labels `cell` y `tier` para que el panel/Grafana haga el join: `nkr_rss_mb * on(vm) group_left(cell,tier) nkr_up`. (Las demás series no duplican `cell`/`tier` — se obtienen vía este join, que es el patrón idiomático de Prometheus.) |
| `nkr_build_info{version}` | gauge | **(v1.6.4)** Siempre `1`; leer el label `version` (= versión del daemon `nkr`). |
| `nkr_vm_count` | gauge | **(v1.6.4)** Nº de micro-VMs corriendo registradas. |
| `nkr_total_rss_mb` / `nkr_total_balloon_mb` / `nkr_total_dax_savings_mb` | gauge | **(v1.6.4)** Sumas de cluster de las métricas per-VM correspondientes. |

**Qué NO está en `/metrics`** (a propósito o pendiente):
- **Disco por-VM (`nkr_guest_disk_*`)** — NO se emite acá: con 100+ instancias un `du` sobre cada filestore en cada scrape sería O(segundos). Está en el endpoint per-instancia `GET .../instances/{name}/metrics` (§4.1.1), que el panel pide de a un tenant.
- **Métricas *guest-internas*** (`nkr_guest_mem_*` vía el stats vq del balloon) — ya implementado, ver §4.1.2.
- **`nkr_vm_start_time_seconds` (uptime)** — está en el JSON per-instancia (`uptime_seconds`), no en Prometheus.
- El `vm=` es el `nkr_name` interno, no el `dns` ni el nombre del tenant que ve el cliente — el panel hace el mapping.

#### 4.1.1 `GET /api/v1/cells/{cell}/instances/{nkr_name}/metrics` — snapshot JSON de UNA instancia

**Esto es lo que el panel usa para la pestaña "Métricas" de cada instancia.** Devuelve un snapshot JSON del momento (no historial — si querés un gráfico en el tiempo, el panel acumula las muestras del lado cliente). Auth bearer (a diferencia de `/metrics`). El daemon **cachea el resultado ~2s por VM** (y el `du` de disco ~5 min aparte) → un poll cada 2s siempre recibe una muestra fresca, y los bursts/duplicados se coalescen; recomputar es ~1ms (un puñado de lecturas de procfs/cgroup), lo único caro (`du`) queda cacheado 5 min aparte, así que pollear rápido no sobrecarga a NKR (la caché *es* el rate-limit, no devuelve 429). **Recomendado: el panel pollea cada ~2s mientras la pestaña esté abierta** (más lento se ve el gráfico choppy), y para de pollear al cerrarla. El campo `as_of` (unix ts) + `stale` (bool) indican la frescura. Nota: `guest_mem` (RAM interna del guest) sólo se refresca cada ~10s — ese número va más lento que el resto.

**Respuesta 200 (VM corriendo):**
```json
{
  "vm": "odoo-v19-intech-devp", "cell": "odoo-v19", "tier": "dev",
  "running": true, "uptime_seconds": 48392, "chrs": 5,
  "ram_allocated_mb": 1300, "balloon_mb": 256, "rss_mb": 561,
  "cgroup_memory_bytes": 782786560,
  "guest_mem": { "total_bytes": 1316167680, "available_bytes": 967421952, "free_bytes": 976322560, "cached_bytes": 122191872 },
  "cpu_seconds_total": 2464.21, "cpu_throttled_seconds_total": 0.0,
  "dax_savings_mb": 433,
  "net_rx_bytes": 15877891, "net_tx_bytes": 2491354,
  "io_read_bytes": 0, "io_write_bytes": 712704,
  "disk": [
    {"mount": "addons",    "used_bytes": 11516576, "total_bytes": 0},
    {"mount": "filestore", "used_bytes": 0,        "total_bytes": 0},
    {"mount": "logs",      "used_bytes": 1308254,  "total_bytes": 0},
    {"mount": "pylibs",    "used_bytes": 97532620, "total_bytes": 0}
  ],
  "as_of": 1778614175, "stale": false
}
```
Notas para el panel:
- **CPU**: `cpu_seconds_total` es un *counter* del cgroup de la VM (`cpu.stat usage_usec`, incluye los helpers virtiofsd/vhost). Para mostrar "% CPU" guardá dos muestras y calculá `(c₂−c₁)/(t₂−t₁)/chrs` (la VM está limitada a `chrs` cores). `cpu_throttled_seconds_total` creciendo = el tenant choca con su cuota `cpu.max`.
- **RAM**: `guest_mem` (si presente) = RAM *vista desde dentro* del guest (`MemTotal/MemAvailable/MemFree/Cached`), vía el stats virtqueue del virtio-balloon — esto es lo que mostrar al cliente como "tu Odoo usa X de Y RAM" (`available_bytes` ≈ cuánto le queda). Aparece tras ~30s de que el guest levante el driver del balloon; antes de eso o si el guest no lo soporta, el campo es `null`. `cgroup_memory_bytes` = RAM física que la VM le cuesta al *host* (VMM + helpers + page cache cargado al cgroup) — útil para tu vista de ops. `ram_allocated_mb` = el cap. `balloon_mb` = RAM devuelta al host (0 ACTIVE / 256 si un DEV decayó a IDLE).
- **Disco**: `du -sb` host-side sobre los dirs virtio-fs de la instancia (`addons`, `filestore`, `logs`, `pylibs`; cacheado 5 min) + `st_blocks×512` sobre los `.ext4` block (`mount="disk:<stem>"`). `total_bytes=0` = sin cap fijo (el dir crece sobre el fs del host). **Salvedad**: si la instancia tiene el filestore backeado por un `.ext4` block share en vez del dir `filestore/`, `mount="filestore"` reporta 0 (esos shares no están en el state de la VM todavía) — `addons`/`pylibs`/`logs` son exactos.

**Respuesta 200 (VM parada, tenant conocido):** `{"vm":..., "cell":..., "tier":..., "running": false, "as_of":..., "stale": false}`.
**404 `vm_not_found`** si `nkr_name` no matchea ninguna VM corriendo ni tenant conocido. **400 `invalid_nkr_name`** si el nombre no pasa el charset.

**Las métricas de NKR están completas con esto** (host-side per-VM + `nkr_up`/build/totales en `/metrics`; CPU/RAM-host/disco/RAM-interna-del-guest en el JSON per-instancia). Lo único que falta es la **vista global/host** para ops (RAM/CPU/disco del servidor — `/proc/meminfo`, `/proc/stat`, `statvfs`), sin diseñar en detalle todavía.

#### 4.1.2 Métricas del *guest* (RAM/CPU/disco internos) — estado

Las métricas de §4.1 son **host-side** (lo que la VM le cuesta al host). Para "tu Odoo usa X de Y RAM, el disco está al Z%":

**✓ CPU + RAM-del-host del tenant — HECHO (v1.6.4):** `nkr_cpu_seconds_total{vm}` / `nkr_cpu_throttled_seconds_total{vm}` (counters, del `cpu.stat` del cgroup — incluye virtiofsd/vhost) + `nkr_cgroup_memory_bytes{vm}` (del `memory.current` del cgroup). En `/metrics` (§4.1) y en el JSON per-instancia (§4.1.1).

**✓ Disco del tenant — HECHO (v1.6.4):** en el JSON per-instancia (§4.1.1), array `disk: [{mount, used_bytes, total_bytes}]` — `du -sb` host-side sobre los dirs virtio-fs (`addons`/`filestore`/`logs`/`pylibs`, cacheado 5 min) + `st_blocks×512` sobre los `.ext4` block. **No** está en `/metrics` (Prometheus) a propósito — un `du` por scrape × 100 VMs sería O(segundos). Salvedad de los filestores backeados por `.ext4` share documentada en §4.1.1.

**✓ Salud / labels — HECHO (v1.6.4):** `nkr_up{vm,cell,tier}` (1/0, incluye tenants parados) — métrica info para joins en Grafana (`metric * on(vm) group_left(cell,tier) nkr_up`). + `nkr_build_info{version}`, `nkr_vm_count`, `nkr_total_{rss,balloon,dax_savings}_mb`. Uptime: `uptime_seconds` en el JSON per-instancia (§4.1.1).

**✓ RAM *interna real* del guest — HECHO (v1.6.4):** vía el **stats virtqueue del virtio-balloon** (3ª queue, `VIRTIO_BALLOON_F_STATS_VQ`). `balloon.rs` ahora maneja la statsq (índice 2): el guest empuja un buffer con pares `(le16 tag, le64 val)` (`MEMFREE`/`MEMTOT`/`AVAIL`/`CACHES`/`SWAP_IN/OUT`/`MAJFLT/MINFLT`), el vmm lo drena cada ~30s y persiste el snapshot al state file (`state::update_guest_mem`, patrón `update_balloon_mb`). El daemon lo expone como `guest_mem: {total/available/free/cached_bytes}` en el JSON per-instancia (§4.1.1) + `nkr_guest_mem_total/available/free/cached_bytes{vm}` en `/metrics`. Verificado en `intech-devp` (~1255 MiB total / ~922 MiB available idle) sin romper inflate/deflate (la statsq se suma a inflateq/deflateq, las rutas existentes intactas).

**Pendiente — vista global / host** (lo único que queda; para ops, no para el cliente): métricas del *host* — `nkr_host_mem_{total,available}_bytes` (de `/proc/meminfo`), `nkr_host_cpu_seconds_total` (de `/proc/stat`), `nkr_host_disk_{used,total}_bytes{mount}` (`statvfs` de `/mnt/nkr` y `/`) — para "cuánta RAM me queda en el server". + un dashboard global que junte eso con el agregado per-VM (`/metrics` ya lo tiene). Sin diseñar en detalle todavía.

### 4.2 `GET /api/v1/health` — Health check (sin auth)

```bash
curl -s http://nkr-host:9090/api/v1/health
# → {"ok":true,"version":"1.6.4"}
```

### 4.3 `GET /api/v1/cells` — Listar cells con capacidad

```bash
curl -s -H "Authorization: Bearer $TOKEN" http://nkr-host:9090/api/v1/cells
```

Respuesta:
```json
{
  "cells": [
    {
      "name": "odoo-v17",
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

El panel lo usa para: (a) discovery inicial, (b) decidir si hace falta crear una cell nueva.

### 4.4 `POST /api/v1/instances` — Crear con auto-selección de cell

El panel sólo conoce la versión de Odoo que el cliente necesita. NKR elige la cell con **más RAM libre** que matchee la versión.

> **Contrato v2 (2026-05-15+).** El panel ahora puede mandar el body mínimo:
> ```json
> {
>   "nkr_name": "cliente-42",
>   "odoo_version": "19",
>   "tier": "dev",
>   "enterprise": false,
>   "admin_passwd": "16-chars-min-...-128-max"
> }
> ```
> NKR deriva todo lo demás del `tier` (workers, RAM, chrs, balloon, soft/hard limits — ver matriz en §7.0). El campo **`workers` ya NO debe mandarse**: si llega, se ignora silenciosamente (transición compatible con paneles legacy). Y `enterprise: bool` es el atajo para `edition: "enterprise"|"community"`.
>
> El contrato viejo (`POST /api/v1/cells/{cell}/instances` con `cell` + `workers` + `edition` en body) **sigue funcionando** por ahora — coexisten hasta que el panel migre. Cuando confirmen migración completa, el viejo pasará a 410 Gone.

**Body completo (todos los campos disponibles, mostrados para referencia):**
```json
{
  "nkr_name": "cliente-42",
  "tier": "production",
  "odoo_version": "19",
  "admin_passwd": "panel-lo-genera-y-guarda-encriptado-16-a-128-chars",
  "enterprise": false,
  "dns": "cliente-42.systemouts.com",
  "admin_user_password": null,
  "auto_start": null,
  "proxy_mode": null,
  "source": null,
  "addons_path": null,
  "pg_version": null,
  "python_libs": [],
  "balloon_mb": null
}
```

**Campos obligatorios:** `nkr_name`, `odoo_version`, `admin_passwd`. Todo lo demás es opcional. **NO incluir** `cell`, `workers`, `edition` en el contrato v2 (cell se auto-selecciona, workers se deriva del tier, edition se mapea desde `enterprise`).

**Auto-selección de cell (v1.6.10+):** ordenamos por **RAM committed ASC** (suma de `ram_mb` de las VMs ya registradas en la cell), tie-break por `cell_id` ASC. Cuando hay mix de tiers (10 prod@2GB vs 15 dev@1.3GB), gana la que pesa menos en RAM real, no la que tiene menos tenants. **Version matching es tolerante a major/minor**: panel manda `"19"` y matchea cell con `odoo_version: "19.0"`. Si todas las cells de la versión están llenas (≥ 20 Odoos), → 409 `no_cell_available`.

**`tier` vs `mode` — orden de prioridad:**
- Si mandás **`tier`** (recomendado, ver §7.0): NKR ignora `mode` y aplica las reglas del tier:
  - `tier=production` → comportamiento legacy (cell template, sin source).
  - `tier=staging` → clona DB de un tenant production (source REQUERIDO).
  - `tier=dev` → standalone con DB del template, sin source (rejected si lo mandás).
- Si **NO** mandás `tier`: default `tier=production`. Entonces `mode` controla:
  - `mode=production` (default si tampoco se manda) → cell template, sin source.
  - `mode=dev` → clone de tenant existente, source REQUERIDO.

**Tip:** mandar solo `tier` y olvidarse de `mode`. `mode` queda para back-compat con paneles que no conocen el campo `tier` todavía.

**Sizing — `workers` es la única input.** El panel sólo manda `workers` (entero). NKR deriva automáticamente el resto:
- **Compose (VM-level):** `chrs` (CPU quota cgroup, 1 chr = 20 % de un core), `ram` (MB de RAM física que KVM asigna al guest), `balloon_mb` (MB que el guest devuelve al host vía VirtIO-Balloon en el boot).
- **odoo.conf (proceso-level):** `workers`, `limit_memory_soft`, `limit_memory_hard`.

| Campo | Valor | Efecto |
|-------|-------|--------|
| `tier` | `"production"` (default si se omite) | **Tenant productivo** con sizing real (workers≥1, default 2, **prefork**), rate-limit en /web/login, cache nginx, balloon=0 (siempre ACTIVE). Sizing derivado de `workers` — ver tabla abajo. Para módulo nuevo en prod, restart manual o automatizado tras webhook. Ver §7.0. |
| `tier` | `"staging"` | **Perfil fijo**: workers=0 (threaded), chrs=5, RAM **1024 MB**, `limit_memory_soft/hard` = **600 / 700 MB**, balloon boot(ACTIVE)=256 / IDLE=768, `limit_time_cpu/real`=600/1200s, `dev_mode` vacío, `list_db=True`, sin rate-limit/cache nginx. Reload vía REL_OD/HVC0 (`POST /reload` o `auto_reload` de `addons/git`). Clona DB de un tenant `tier=production` (source REQUERIDO). Ver §7.0. |
| `tier` | `"dev"` | **Perfil fijo**: workers=0 (threaded), chrs=5, RAM **1300 MB**, `limit_memory_soft/hard` = **800 / 1000 MB**, balloon boot(ACTIVE)=0 / IDLE=256, `limit_time_cpu/real`=600/1200s, `dev_mode` vacío, `list_db=True`, sin rate-limit/cache nginx. Standalone — DB del template (sin source). Source PROHIBIDO (rejected con 409). Las dev no son clonables. Ideal para módulos nuevos desde cero. Ver §7.0. |
| `mode` | `"production"` (default, **opcional** si mandás `tier`) | **Legacy / back-compat.** Cuando `tier=production`: tenant fresh con DB del template, sin source. NKR fuerza `source = <cell>-odoo-template`. Cuando se manda `tier=staging`/`tier=dev`, este campo se ignora. |
| `mode` | `"dev"` (**opcional**) | **Legacy / back-compat.** Cuando `tier=production`: clone de un tenant existente — `source` obligatorio. Para el caso "clonar de producción para testing" usar `tier=staging` (más explícito y aplica config dev). |
| `cell` | `null` (recomendado, contrato v2) | Auto-selecciona la cell con `odoo_version` match (matching tolerante major/minor) y **más RAM libre** (= menor sum de `ram_mb` de las VMs en la cell). Tie-break por `cell_id` ASC. |
| `cell` | `"foo"` (legacy) | Fuerza esa cell. 409 si versión no matchea o está llena. Mandar `cell` activa el contrato viejo donde `workers` SÍ se respeta. |
| `enterprise` | `bool` \| `null` (v1.6.10+) | **Atajo del contrato v2.** `true` → equivale a `edition: "enterprise"`. `false` → `edition: "community"`. Tiene precedencia sobre `edition` si ambos vienen. |
| `source` | `null` (con `mode=production`) | NKR resuelve **automáticamente** según `edition`: community → `<cell>-odoo-template`, enterprise → `<cell>-odoo-template-enterprise` (v1.6.5+, ver §"Sembrar template enterprise"). **No mandar source en mode=production**: si lo mandás, 409 `source_not_allowed_in_production`. Si el template requerido no existe → 409 `cell_template_missing` (community) o `enterprise_template_missing` (enterprise). |
| `source` | `"<tenant>"` (con `mode=dev`) | **Obligatorio.** Tenant a clonar (mismo cell). Si falta en `mode=dev` → 400 `source_required`. |
| `edition` | `"enterprise"` \| `"community"` \| `null` | **v1.6.5+ semántica:** determina cuál template usar como source default. `enterprise` → `<cell>-odoo-template-enterprise` (con `web_enterprise` pre-instalado). `community` o `null` → `<cell>-odoo-template`. La activación del theme **NO se hace runtime** (eliminada en v1.6.5) — viene horneada en el template. Para clones-from-tenant (`mode=dev`/`tier=staging`), `edition` no aplica (el clone hereda del source). |
| `admin_user_password` | string `[A-Za-z0-9._-]{8,128}` \| `null` | **OPCIONAL — pero las reglas cambiaron en v1.6.5:**<br>• **Source es template del cell** (community o enterprise): si se manda, NKR rota admin/admin → admin/`<tu_pwd>` post-boot (compose up + JSON-RPC login + `res.users.change_password`). Implícitamente activa `auto_start`. Si se omite: tenant queda cold-prepared (admin/admin remains — el panel debe rotar vía UI/JSON-RPC antes de exponer).<br>• **Source es otro tenant** (clone-from-tenant): **PROHIBIDO** — 400 `admin_user_password_not_applicable_for_clone`. El clone hereda `res_users.password` del source vía `CREATE DATABASE TEMPLATE`; intentar rotar con admin/admin falla porque ya no es esa la pwd. Si querés arrancar el clone, usá `auto_start: true`. El panel ya conoce la pwd del source (la generó él mismo). |
| `auto_start` | `bool` \| `null` (v1.6.5+) | **Default:** `admin_user_password.is_some()` (back-compat). Si `true`, NKR arranca la VM al final del create (compose up + wait :8069). Si `false` o ausente sin pwd, el create se queda cold-prepared (`disabled: true` en el bloque del compose) hasta que el panel haga `POST /actions {start}`. Usar `auto_start: true` explícito en clones-from-tenant donde `admin_user_password` está prohibido. |
| `python_libs` | `[]` | Si no vacío: 500 hoy (requiere rebuild del master ext4 — pendiente). |
| `workers` | int `1..=16` \| `null` | **Contrato v2 (sin cell):** ignorado silenciosamente — NKR deriva workers del `tier` (prod=2, staging/dev=0). El panel **no debe mandarlo**. **Contrato viejo (con cell):** sigue respetándose como override de tier=production. Default `2`. |
| `balloon_mb` | int `0..` \| `null` | **OPCIONAL.** Override del VirtIO-Balloon. Si `null` (recomendado), NKR aplica el default derivado de `workers` (ver tabla). Si se manda valor explícito, ese valor reemplaza al default. `0` desactiva el balloon (no recomendado para Odoo en cells densas). Subirlo más allá del default agresivo (40 % de la RAM) puede provocar OOM-killer del guest bajo picos de carga (imports, generación de PDF, install/upgrade de módulos). |
| `admin_passwd` | string `[A-Za-z0-9._-]{16,128}` | **OBLIGATORIO.** Master password del Odoo del tenant. El panel es única fuente — lo genera, lo guarda encriptado, y lo manda en este body. NKR nunca lo genera ni lo devuelve. Se persiste sólo en `odoo.conf` del tenant. Si se omite → `400 admin_passwd_required`. |
| `proxy_mode` | `true` (default) \| `false` | Productivo SIEMPRE `true`. `false` sólo para tests locales sin nginx. |
| `list_db` | cualquier valor | **Ignorado**. NKR fuerza `False` en cells productivas (ver §9.1). |
| `addons_path` | cualquier valor | Se inyecta en `odoo.conf`. |

**Tabla de derivación de recursos (`tier=production`):**

Fuente de verdad: `src/api.rs::derive_resources_for_tier`. Aplica **solo a `tier=production`** (staging/dev usan los perfiles fijos de §7.0, no esta fórmula). Las VMs de pg/pgbouncer tampoco pasan por acá — siguen su sizing en el `nkr-compose.yml` de la cell.

| `workers` (W) | compose `chrs` | compose `ram` (MB) | compose `balloon_mb` | odoo.conf `limit_memory_soft` | odoo.conf `limit_memory_hard` | Uso |
|-----------|----------------|--------------------|----------------------|-------------------------------|-------------------------------|-----|
| 1 | 3 | 1280 | 0 | 640 MB (671088640 b) | 768 MB (805306368 b) | mínimo prefork |
| 2 (default) | 5 | 2048 | 0 | 1280 MB (1342177280 b) | 1536 MB (1610612736 b) | producción típica |
| 3 | 7 | 2816 | 0 | 1920 MB | 2304 MB | uso medio |
| 4 | 9 | 3584 | 0 | 2560 MB | 3072 MB | retail / muchos usuarios |
| 5 | 11 | 4352 | 0 | 3200 MB | 3840 MB | alta carga (regla del Grifo: balloon=0 obligatorio si W>4) |
| 8 | 17 | 6656 | 0 | 5120 MB | 6144 MB | tenants pesados / multi-empresa |
| N | 2N+1 | max(1024, 512 + 768·N) | 0 | 640·N MB | 768·N MB | — |

**Fórmulas (`tier=production`):**
- compose `chrs` = `2·W + 1`
- compose `ram_mb` = `max(1024, 256 + 256 + 768·W)` (256 OS + 256 master prefork + 768/worker)
- compose `balloon_mb` = `0` (PROD nace y se queda ACTIVE — sin decay; doctrina "evita latencia de desinflado en picos")
- odoo.conf `limit_memory_soft` (bytes) = `640 · W · 1 MiB`
- odoo.conf `limit_memory_hard` (bytes) = `768 · W · 1 MiB`

> ⚠️ Override de `workers` > 4 en producción requiere `balloon_mb = 0` (regla del Grifo) — ya se cumple porque PROD siempre deriva `balloon=0`. La RAM mínima para W workers es `512 + 768·W` MB (= la fórmula de `ram_mb`); el daemon nunca acepta menos.

**Por qué dos capas de RAM:**
- `compose.ram` (KVM) = RAM física asignada al guest.
- `odoo.conf.limit_memory_hard` = tope interno del proceso Odoo (worker recycle).
- La fórmula da `compose.ram ≥ workers · hard + holgura` para kernel guest + PG client + filesystem cache. Con `workers=2`: `2 GB compose - 1.5 GB hard = 500 MB` para sistema y master, y el cgroup `memory.max = ram·1.15 = 2355 MB` cubre picos transitorios. Soporta installs pesadas (account + ~30 deps) sin matar workers.

**Por qué el balloon va aparte:**
- El balloon NO reduce `compose.ram`. La VM sigue arrancada con `ram_mb` MB asignados a KVM. Lo que hace el driver `virtio_balloon` del guest es marcar `balloon_mb` MB como "donados" via `MADV_DONTNEED` → el host los recupera para usarlos con otros tenants (page-cache, otra VM).
- Si Odoo crece y necesita la RAM donada, el driver guest hace **deflate** transparente y la recupera. Ese mecanismo funciona bien hasta el `limit_memory_hard`; más allá, el OOM-killer del guest mata workers de Odoo.
- Por eso el default `ram − limit_hard − 256` deja exactamente el peak working-set proyectado libre y dona el resto. Con `workers=2` da `292 MB` que es ~14 % de la RAM — conservador y seguro.
- **Override del panel:** si una cell tiene RAM de host abundante o el panel quiere apostar a más densidad, mandar `balloon_mb` mayor en el body (ej. `512` para `workers=2`). Recomendación: nunca pasar de **40 %** de `ram_mb` sin monitoreo de OOM en logs (`grep oom-kill`). Subir `balloon_mb` reduce la cantidad de host RAM que las VMs consumen sumadas, permitiendo más instancias por host.
- **Defensa interna:** NKR cap-ea el HashSet de páginas infladas a `ram_mb × 256` (=100 % de la RAM) por VM, así que un guest comprometido no puede causar DoS al host inflando arbitrariamente. Cualquier `balloon_mb` razonable está debajo de ese techo.

**🆕 v1.6.4 — `POST /instances` es ASÍNCRONO.** El clone + boot tarda 30-200s (filesystem reflink + DB TEMPLATE + `nkr compose up` con readiness wait — que para PROD prefork se pega al borde de 140s — + opcional set de `admin_user_password`). Bloquear el HTTP request hasta el final hacía que clientes con timeout corto (panel, o Cloudflare ~100s) vieran **504 aunque el create terminara OK** (caso real 2026-05-11: tenant `johao-y-richavo` mostró 504 en el panel y quedó perfectamente creado y corriendo). Ahora: **toda la validación es síncrona** (los 4xx se devuelven al toque), después el clone se despacha en background y se devuelve **202** inmediato. El panel pollea hasta saber el resultado.

**Respuesta 202 (create aceptado, corriendo en background):**
```json
{
  "nkr_name": "odoo-v17-cliente-42",
  "cell": "odoo-v17",
  "source": "odoo-v17-odoo-template",
  "status": "accepted",
  "async": true,
  "message": "create despachado en background (30-200s típico; PROD prefork es más lento). Poll GET /api/v1/cells/{cell}/instances/{name}/create-status hasta status=ready|failed, o GET /instances/{name} hasta nkr_status.phase=ready.",
  "poll": "/api/v1/cells/odoo-v17/instances/odoo-v17-cliente-42/create-status",
  "started_at": 1778512345
}
```

> Si ya hay un create en curso para ese nombre, también devuelve **202** con el mismo `poll` y un campo `job` con el estado actual (idempotente ante reintentos del panel).

**Flujo del panel tras el 202:**
1. Guardar el `nkr_name` devuelto (puede tener prefijo de cell auto-añadido — **usar el devuelto, no el que enviaste**).
2. Poll `GET {poll}` (= `GET /api/v1/cells/{cell}/instances/{name}/create-status`) cada 3-5s.
3. Cuando `status == "ready"` → el tenant está clonado (y arrancado+password seteada si mandaste `admin_user_password`). Hacer `GET /instances/{name}` para el `InstanceInfo` completo.
4. Cuando `status == "failed"` → leer `error` + `message` + `hint`. El instance dir típicamente quedó limpio (rollback del clone) salvo en `error=admin_password_setup_failed` donde el tenant SÍ existe pero con `admin/admin`.
5. Si querés saltarte `create-status`: pollear `GET /instances/{name}` directamente — durante el clone devuelve 404 (la instancia aún no existe en el registry hasta los primeros ~1-2s), luego devuelve el `InstanceInfo` con `nkr_status.phase` que pasa de `provisioning` → `ready`. Pero `create-status` es la fuente autoritativa para el caso de fallo (donde el GET seguiría dando 404 sin distinguir "falló" de "todavía en curso").

**Errores síncronos** (devueltos al toque en el POST, antes de despachar):
```json
// 400 — body inválido / campos faltantes / charset inválido
{ "error":"invalid_json" }                    { "error":"admin_passwd_required" }
{ "error":"invalid_nkr_name" }                { "error":"invalid_admin_passwd" }
{ "error":"invalid_admin_user_password" }     { "error":"invalid_workers" }   // ... etc

// 404 — cell forzada no existe
{ "error":"cell_not_found", "cell":"foo" }

// 409 — varios casos de pre-validación
{ "error":"version_mismatch", "cell":"foo", "cell_version":"17.0", "requested_version":"16.0" }
{ "error":"no_cell_available", "message":"...", "requested_version":"19.0" }
{ "error":"cell_full", "cell":"odoo-v17", "used":20, "max":20 }
{ "error":"cell_template_missing", "cell":"foo", "expected_template":"foo-odoo-template" }
{ "error":"source_not_allowed_in_production", "message":"..." }
{ "error":"source_required", "message":"..." }
{ "error":"enterprise_not_provisioned", "cell":"...", "enterprise_path":"..." }
{ "error":"instance_already_exists", "nkr_name":"...", "cell":"..." }   // ya hay un tenant con ese nombre
{ "error":"create_in_progress", "nkr_name":"...", "cell":"..." }        // dos POST concurrentes del mismo nombre

// 503 — no se pudo lanzar el thread (extremadamente raro)
{ "error":"spawn_failed", "nkr_name":"...", "cell":"..." }
```

**Errores asíncronos** (aparecen en `create-status`, NO en el POST — ver §4.4.1):
- `clone_failed` — falló el clone (disk reflink, DB TEMPLATE, compose). El instance dir se hace rollback.
- `admin_password_setup_failed` — el clone OK pero el cambio de password del user `admin` falló. El tenant queda arrancado con `admin/admin`. `hint` en el job explica cómo recuperar.

**Importante:** el panel envía `admin_passwd` en el body y es responsable de persistirlo encriptado **antes** de llamar. NKR lo escribe en `config_path/odoo.conf` y no lo devuelve en ninguna respuesta. Si el panel pierde el valor, la única forma de recuperarlo es leer `config_path/odoo.conf` vía SSH al host NKR (fuera de la API).

---

### 4.4.1 `GET /api/v1/cells/{cell}/instances/{nkr_name}/create-status` — Estado del create asíncrono

Devuelve el estado del job background lanzado por `POST /instances`. El status file vive en `/mnt/nkr/cells/{cell}/.nkr-creates/{nkr_name}.json` (a nivel cell, no instancia, para sobrevivir al rollback del clone si éste falla).

**Respuesta 200 — en curso:**
```json
{ "nkr_name":"odoo-v17-cliente-42", "cell":"odoo-v17", "source":"odoo-v17-odoo-template",
  "status":"provisioning", "phase":"cloning", "started_at":1778512345 }
```
`phase` ∈ `cloning` → (`setting_admin_pwd` si rota pwd | `starting` si auto_start sin pwd | nada si cold) → `done`. Si `status:failed`, `phase` indica dónde paró: `cloning` (clone falló), `setting_admin_pwd` (rotar pwd falló), `starting` (boot/wait :8069 falló).

**Respuesta 200 — terminado OK:**
```json
{ "nkr_name":"odoo-v17-cliente-42", "cell":"odoo-v17", "source":"odoo-v17-odoo-template",
  "status":"ready", "phase":"done", "started_at":1778512345, "finished_at":1778512360,
  "elapsed_ms":15000, "running":true, "port_8069_up":true,
  "guest_ip":"10.0.1.7", "db_name":"db-odoo-v17-cliente-42", "dns":"cliente-42.systemouts.com" }
```

> **🆕 v1.6.5 — `running`/`port_8069_up` reflejan el ESTADO REAL al cierre del job.** Antes (1.6.4) se reportaba el snapshot pre-boot del `info` devuelto por el clone → siempre `running:false` aunque la VM estuviera arriba, lo que confundía al panel. Ahora NKR releé `get_instance_info` después de `boot_and_set_admin_password` y reporta el estado verdadero.

> **SLA de creación 10–60s (v1.6.5):** Para community (tier=dev/staging/production), `status=ready` aparece típicamente en **10–20 s** independiente del tier:
> - Clone: 3 s (reflink + CREATE DATABASE TEMPLATE)
> - Boot Odoo (con DB ya inicializada por TEMPLATE): 5–7 s threaded, 7–10 s prefork
> - HTTP poll `/web/database/list` + JSON-RPC `change_password`: 2–4 s
>
> Si `edition=enterprise`, ver §4.4.2 — el `status=ready` llega igual en 10–20 s y la activación del theme corre en una fase post-ready independiente que el panel puede mostrar como progreso pero NO bloquear la UX del create.

**Respuesta 200 — falló:**
```json
{ "nkr_name":"odoo-v17-cliente-42", "cell":"odoo-v17",
  "status":"failed", "phase":"cloning",
  "error":"clone_failed", "message":"cp -a falló al copiar ...",
  "started_at":1778512345, "finished_at":1778512360, "elapsed_ms":15000 }
```
o, si el clone OK pero el set de password falló:
```json
{ "status":"failed", "phase":"setting_admin_pwd", "error":"admin_password_setup_failed",
  "message":"login admin/admin rechazado: ...", "hint":"El tenant se clonó y arrancó, pero el password del user 'admin' sigue siendo el del template. Reintentar via JSON-RPC/PATCH, o borrar y re-crear.", ... }
```

**Respuesta 200 — sin registro de create async pero la instancia existe:**
```json
{ "nkr_name":"odoo-v17-cliente-42", "cell":"odoo-v17", "status":"ready",
  "note":"sin registro de create async — la instancia ya existe (creada con NKR previo, o status file purgado)", "running":true }
```

**Respuesta 404 — no hay ni create en curso ni instancia:**
```json
{ "error":"no_create_record", "nkr_name":"...", "cell":"...", "message":"no hay create en curso ni instancia con ese nombre" }
```

**Errores:** `400 invalid_cell`, `400 invalid_nkr_name`.

**Ejemplo de poll loop (panel):**
```js
async function waitForCreate(cell, name, { intervalMs = 4000, timeoutMs = 300000 } = {}) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const r = await fetch(`${NKR}/api/v1/cells/${cell}/instances/${name}/create-status`,
      { headers: { Authorization: `Bearer ${TOKEN}` } });
    const j = await r.json();
    if (j.status === "ready")  return { ok: true,  job: j };
    if (j.status === "failed") return { ok: false, error: j.error, message: j.message, hint: j.hint };
    await new Promise(res => setTimeout(res, intervalMs));
  }
  return { ok: false, error: "poll_timeout" };
}
```

#### Convención del template (importante)

Cada cell tiene **un instance reservado** llamado `<cell>-odoo-template` que NKR usa como source automático cuando `mode=production`. La DB `db-<cell>-odoo-template` vive en PostgreSQL de la cell con `base + web` ya cargados (un Odoo recién inicializado, sin datos del cliente).

**Reglas:**
- El template se crea una vez por cell, fuera de la API, por el operador. Viene listo en las cells `odoo-v17` y `odoo-v19` actuales.
- El template **cuenta dentro del `used_odoos` de `GET /cells`**, así que los 20 slots se reparten entre 19 tenants reales + 1 template. `free_slots` lo refleja correctamente.
- **El proceso Odoo del template está apagado por default** (`disabled: true` en su bloque de `nkr-compose.yml`). No consume RAM ni CPU. Su DB sigue existiendo en PG y los archivos en disco — eso es todo lo que NKR necesita para clonar. **No hace falta que el panel lo encienda.**
- El operador puede arrancar el template manualmente para mantenerlo (instalar/upgrade módulos pre-bakeados): editar el yaml a `disabled: false`, `nkr compose up -d`, hacer cambios, `nkr stop <cell>-odoo-template`, restaurar `disabled: true`.
- **No intentes borrar el template** con `DELETE`. Si lo hacés, la siguiente creación en esa cell devuelve `409 cell_template_missing`. No hay recuperación vía API — el operador tiene que recrearlo.

**Cómo se usa según `mode`:**

| `mode` | Source | Boot inicial | Caso de uso |
|---|---|---|---|
| `production` | NKR fuerza `<cell>-odoo-template` (panel NO manda `source`) | **30-60 s** (DB con base+web preinstalado) | Cliente nuevo desde cero |
| `dev` | Panel manda `source` explícito (otro tenant existente) | 30-60 s + tamaño de la DB del source | Staging clonado de producción, debugging |

**Por qué `production` es ahora rápido:** NKR clona la DB del template via `CREATE DATABASE ... TEMPLATE` (CoW a nivel filesystem PG, ~5 segundos). Cuando Odoo arranca, encuentra la DB ya inicializada y sólo carga workers — boot completo en ~30-60 s para workers=0, más para PROD prefork (workers≥2: master forkea N workers HTTP + 1 cron, cada uno carga el registry completo → puede llegar a ~140s, justo el límite del readiness wait de `nkr compose up`; por eso el create es async, ver arriba). Si en cambio `mode=production` no copiara la DB, Odoo arrancaría contra una DB vacía y se autoinicializaría (cargando `base` desde XML/CSV) → 3-5 minutos. Ese era el comportamiento previo a v1.6.

> Lista completa de errores (síncronos y asíncronos): ver arriba en §4.4 ("Errores síncronos" / "Errores asíncronos") y §4.4.1.

### 4.4.2 Sembrar template enterprise (runbook del operador, v1.6.5+)

Cada cell que vaya a soportar `edition=enterprise` necesita un **segundo template** llamado `<cell>-odoo-template-enterprise` con `web_enterprise` pre-instalado. Sin él, `POST /instances edition=enterprise` devuelve **409 `enterprise_template_missing`**.

**Por qué este modelo (vs activación en runtime):** Instalar `web_enterprise` toma 2–5 min y dispara `update_list()` sobre ~750 manifests del repo enterprise. Hasta v1.6.4, NKR lo hacía en cada create → frágil (Bug F: en muchos casos `web_enterprise` no aparecía tras `update_list`), lento (rompía SLA 60 s), y no testeable (resultado variable per-instancia). v1.6.5+ mueve la instalación al template — se hace **una sola vez por cell**, el operador la prueba bien, y todos los clones nacen O(1) vía `CREATE DATABASE … TEMPLATE`.

**Runbook (una vez por cell, ej. `odoo-v19`) — probado 2026-05-14:**

```bash
# Pre-flight checks:
ls /mnt/nkr/enterprise/19.0/web_enterprise/__manifest__.py  # debe existir
ls /mnt/nkr/cells/odoo-v19/instances/odoo-v19-odoo-template-enterprise/  # debe NO existir
TOKEN="..."
PG_PWD=$(awk '/POSTGRES_PASSWORD:/{gsub(/[",]/,""); print $2; exit}' /mnt/nkr/cells/odoo-v19/nkr-compose.yml | head -1)

# 1. Clonar el template community con edition=enterprise (esto inyecta la
#    share /mnt/extra-enterprise + addons_path; mode=dev permite source
#    explícito; el clone hereda admin/admin del template community).
curl -sS -X POST $NKR/api/v1/cells/odoo-v19/instances \
  -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
  -d '{"nkr_name":"ent-tpl-build","odoo_version":"19.0",
       "tier":"production","mode":"dev",
       "source":"odoo-v19-odoo-template",
       "edition":"enterprise","workers":1,
       "admin_passwd":"<gen 16-128 chars>", "admin_user_password":"<gen 8-128>"}'
# Poll create-status hasta status=ready. ~15-20 s.

# 2. Instalar web_enterprise via JSON-RPC (NO via UI — más rápido + scriptable).
#    update_list registra ~226 manifests del repo enterprise (~7 s).
#    button_immediate_install activa el theme (~10 s).
HOST=<guest_ip de ent-tpl-build>
PWD=<admin_user_password que pasaste>
COOKIE=$(curl -sS -i -X POST "http://$HOST:8069/web/session/authenticate" \
  -H 'Content-Type: application/json' \
  -d "{\"jsonrpc\":\"2.0\",\"method\":\"call\",\"params\":{\"db\":\"db-odoo-v19-ent-tpl-build\",\"login\":\"admin\",\"password\":\"$PWD\"}}" \
  | grep -i '^Set-Cookie' | head -1 | awk '{print $2}' | tr -d ';')
# 2a. update_list — escanea /mnt/extra-enterprise
curl -sS -X POST "http://$HOST:8069/web/dataset/call_kw" \
  -H "Cookie: $COOKIE" -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","method":"call","params":{"model":"ir.module.module","method":"update_list","args":[],"kwargs":{}}}'
# 2b. install web_enterprise
WE_ID=$(curl -sS -X POST "http://$HOST:8069/web/dataset/call_kw" \
  -H "Cookie: $COOKIE" -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","method":"call","params":{"model":"ir.module.module","method":"search","args":[[["name","=","web_enterprise"]]],"kwargs":{}}}' \
  | python3 -c 'import json,sys;print(json.load(sys.stdin)["result"][0])')
curl -sS -X POST "http://$HOST:8069/web/dataset/call_kw" \
  -H "Cookie: $COOKIE" -H 'Content-Type: application/json' \
  -d "{\"jsonrpc\":\"2.0\",\"method\":\"call\",\"params\":{\"model\":\"ir.module.module\",\"method\":\"button_immediate_install\",\"args\":[[$WE_ID]],\"kwargs\":{}}}"
# 2c. CRÍTICO — restablecer admin/admin (NKR le rotó la pwd en el step 1; el
#     template DEBE tener admin/admin para que clones futuros lo puedan rotar):
curl -sS -X POST "http://$HOST:8069/web/dataset/call_kw" \
  -H "Cookie: $COOKIE" -H 'Content-Type: application/json' \
  -d "{\"jsonrpc\":\"2.0\",\"method\":\"call\",\"params\":{\"model\":\"res.users\",\"method\":\"change_password\",\"args\":[\"$PWD\",\"admin\"],\"kwargs\":{}}}"

# 3. Parar la VM.
curl -sS -X POST $NKR/api/v1/cells/odoo-v19/instances/odoo-v19-ent-tpl-build/actions \
  -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
  -d '{"action":"stop"}'
# Espera ~60s (graceful shutdown del VMM).

# 4. Promoción a template:
#    a) Renombrar la DB en PG. Las conexiones de pgbouncer la mantienen
#       abierta — terminar primero:
PGPASSWORD=$PG_PWD psql -h 10.0.2.2 -U odoo -d postgres -c "
  SELECT pg_terminate_backend(pid) FROM pg_stat_activity
  WHERE datname='db-odoo-v19-ent-tpl-build' AND pid <> pg_backend_pid();"
PGPASSWORD=$PG_PWD psql -h 10.0.2.2 -U odoo -d postgres -c \
  "ALTER DATABASE \"db-odoo-v19-ent-tpl-build\" RENAME TO \"db-odoo-v19-odoo-template-enterprise\";"

#    b) Renombrar el dir de la instancia + nkr-data files:
mv /mnt/nkr/cells/odoo-v19/instances/odoo-v19-ent-tpl-build \
   /mnt/nkr/cells/odoo-v19/instances/odoo-v19-odoo-template-enterprise
for f in /mnt/nkr/cells/odoo-v19/.nkr-data/ent-tpl-build-*; do
  mv "$f" "${f/ent-tpl-build-/odoo-template-enterprise-}"
done

#    b2) CRÍTICO — renombrar la carpeta filestore DENTRO del .ext4 (no la del
#        host: la que está montada bajo /var/lib/odoo/filestore/ del guest).
#        Sin este paso, todos los clones nacen con sus íconos / CSS rotos:
#        el initramfs intenta renombrar filestore/<db-from> → filestore/<db-to>
#        pero <db-from> NO existe en el .ext4 (sigue siendo el nombre original
#        ent-tpl-build), entonces el rename falla silente y los attachments
#        del template (ir_attachment.store_fname) apuntan a un path inexistente.
#        Mide tu propia carpeta antes: lo que está adentro es el db-name del
#        clone fuente, NO el nuevo nombre del template.
mkdir -p /tmp/fix-ent
mount -o loop,rw /mnt/nkr/cells/odoo-v19/.nkr-data/odoo-template-enterprise-var_lib_odoo.ext4 \
                 /tmp/fix-ent
mv /tmp/fix-ent/filestore/db-odoo-v19-ent-tpl-build \
   /tmp/fix-ent/filestore/db-odoo-v19-odoo-template-enterprise
# Si existía el marker, lo dejamos (el rename ya pasó para el template):
# el clone hijo lo va a heredar y el initramfs SKIPEA el rename — entonces
# el next-marker hay que dejarlo BORRADO para que el initramfs del clone SÍ
# renombre:
rm -f /tmp/fix-ent/filestore/.nkr-filestore-renamed
sync && umount /tmp/fix-ent && rmdir /tmp/fix-ent

#    c) Actualizar odoo.conf paths internos:
sed -i 's|ent-tpl-build|odoo-template-enterprise|g' \
  /mnt/nkr/cells/odoo-v19/instances/odoo-v19-odoo-template-enterprise/config/odoo.conf

#    d) Actualizar meta.json (nkr_name):
python3 -c "
import json
p='/mnt/nkr/cells/odoo-v19/instances/odoo-v19-odoo-template-enterprise/meta.json'
d=json.load(open(p)); d['nkr_name']='odoo-v19-odoo-template-enterprise'
json.dump(d, open(p,'w'), indent=2)"

#    e) Actualizar /mnt/nkr/registry.json (re-key del vm_id):
python3 -c "
import json
p='/mnt/nkr/registry.json'
d=json.load(open(p))
e=d['entries']
if 'odoo-v19/odoo-v19-ent-tpl-build' in e:
    e['odoo-v19/odoo-v19-odoo-template-enterprise']=e.pop('odoo-v19/odoo-v19-ent-tpl-build')
    json.dump(d, open(p,'w'), indent=2)"

#    f) Editar el bloque del nkr-compose.yml. CAMBIOS NECESARIOS:
#       - header: `  ent-tpl-build:` → `  odoo-template-enterprise:`
#       - disabled: true (no debe arrancar)
#       - nkr_name: "odoo-v19-odoo-template-enterprise"
#       - ram: 512, chrs: 1, balloon_mb: 128 (sizing template)
#       - todos los paths /mnt/nkr/cells/odoo-v19/instances/odoo-v19-ent-tpl-build/
#         → /mnt/nkr/cells/odoo-v19/instances/odoo-v19-odoo-template-enterprise/
#       - REMOVER NKR_RENAME_FILESTORE_FROM/TO del environment (template no
#         necesita rename; el clone los inyectará al clonar).
#       - REMOVER balloon_idle_mb / balloon_decay_secs si los hay (template
#         estático).
#    (En el script de seeding usé un pequeño Python: src/scripts/promote-block.py.)

# 5. Verificación final:
nkr ps | grep odoo-template-enterprise  # NO debe aparecer (disabled:true)
ls /mnt/nkr/cells/odoo-v19/instances/odoo-v19-odoo-template-enterprise/  # debe existir

# 6. Probar:
curl -sS -X POST $NKR/api/v1/cells/odoo-v19/instances \
  -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
  -d '{"nkr_name":"ent-test","odoo_version":"19.0","tier":"production",
       "edition":"enterprise","workers":1,
       "admin_passwd":"<...>","admin_user_password":"<...>"}'
# → status=ready en ~15-20s. web_enterprise heredado.
```

**Tiempo total ≈ 5 minutos** una vez que tenés los comandos a mano (la mayoría es esperar al stop). Validado en odoo-v19.

**Validar después:**
```bash
nkr ps                  # NO debe listar odoo-v19-odoo-template-enterprise (disabled:true).
curl -sS $NKR/api/v1/cells | jq '.cells[] | select(.name=="odoo-v19")'
# El template enterprise NO debe contarse en `used_odoos` distinto del community
# (ambos son `disabled: true`, ambos cuentan 1 slot del max=20).
```

**Errores comunes:**
- 409 `enterprise_template_missing` tras crear el dir/DB pero no editar el compose → la API resuelve source por presencia del bloque en compose YAML, no del dir solo. Asegurar paso 4c.
- Tenants enterprise creados ANTES de seedear el template (legacy v1.6.4): siguen funcionales pero sin theme. Activar manual via UI (Update Apps List → Install Web Enterprise) per-tenant; o borrar y recrear ahora que el template existe.

### 4.5 `POST /api/v1/cells/{cell}/instances` — Crear forzando cell

Mismo body que 4.4, pero la cell viene en el path. Valida que `odoo_version` coincida con `cell.odoo_version`.

```bash
curl -s -X POST http://nkr-host:9090/api/v1/cells/odoo-v17/instances \
  -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
  -d '{"nkr_name":"tst-1","mode":"dev","odoo_version":"17.0"}'
```

### 4.6 `GET /api/v1/cells/{cell}/instances/{nkr_name}` — Info + estado

```bash
curl -s -H "Authorization: Bearer $TOKEN" \
  http://nkr-host:9090/api/v1/cells/odoo-v17/instances/odoo-v17-odoo-01
```

Respuesta: mismo `InstanceInfo` de 4.4. Los valores de `nkr_status` se consultan en vivo cada request (no cacheado).

Uso típico del panel:
- Después de crear, poll cada 2s hasta `nkr_status.port_8069_up == true`.
- Renderizar el card con RAM/uptime/PID.

### 4.7 `DELETE /api/v1/cells/{cell}/instances/{nkr_name}` — Eliminar (ASÍNCRONO desde v1.5.2)

**ASÍNCRONO desde v1.5.2.** El endpoint encola el delete y devuelve `202` en <50 ms; antes era síncrono y bloqueaba 60–90 s (SIGTERM graceful + DROP DATABASE + remove dir). Eso forzaba al cliente HTTP del panel a cortar por timeout, devolviendo 500 falso en la UI mientras NKR seguía borrando OK por detrás. El panel ahora debe poller hasta `404`.

Query params:
- `drop_db=1` (default) — borra la DB PG también.
- `drop_db=0` — preserva la DB (útil si querés migrar la DB a otra instancia antes).

```bash
# Default: borra todo
curl -s -X DELETE -H "Authorization: Bearer $TOKEN" \
  http://nkr-host:9090/api/v1/cells/odoo-v17/instances/odoo-v17-cliente-42
```

Respuesta 202 (<50 ms, instancia existía):
```json
{
  "deleted": "pending",
  "nkr_name": "odoo-v17-cliente-42",
  "cell": "odoo-v17",
  "dns": "cliente-42.systemouts.com",
  "drop_db": true,
  "status": "accepted",
  "async": true,
  "message": "delete dispatched in background. Poll GET /instances/{name} until 404."
}
```

Respuesta 200 (instancia ya no existía — idempotente, **no es error**):
```json
{
  "deleted": true,
  "already_deleted": true,
  "nkr_name": "odoo-v17-cliente-42",
  "drop_db": true
}
```

Respuesta 409 (delete o action concurrente sobre la misma instancia):
```json
{
  "error": "action_in_progress",
  "nkr_name": "odoo-v17-cliente-42",
  "message": "ya hay un start/stop/restart/delete en curso para esta instancia"
}
```

**Importante para el panel:**

- Tratá `200 already_deleted=true` igual que un delete exitoso. Es la respuesta a un retry o a un delete sobre algo ya borrado.
- Tras `202`, polear `GET /api/v1/cells/{cell}/instances/{name}` cada **1 s** hasta `404 not_found`. Cuando llegue el 404, la instancia terminó de borrarse (filesystem + DB + compose block).
- Cadencia: hasta **180 s**. La DB de un tenant grande puede tardar más en hacer `DROP DATABASE` si tiene miles de tablas (Odoo con muchos módulos).
- En `409 action_in_progress`: NO reintentar inmediatamente. Polear el `GET /instances/{name}` que confirme la acción previa, después reenviar el DELETE.
- El campo `dns` del 202 es solo informativo. **El panel debe llamar explícitamente a `DELETE /api/v1/cells/{cell}/instances/{name}/dns`** (§4.14) antes del delete de la instancia para limpiar el vhost de nginx — el delete de instancia no toca DNS automáticamente.

```bash
# Preservar DB (útil para migrar la DB a otra instancia)
curl -s -X DELETE -H "Authorization: Bearer $TOKEN" \
  "http://nkr-host:9090/api/v1/cells/odoo-v17/instances/odoo-v17-cliente-42?drop_db=0"
```

Acciones internas del delete (en background tras 202):
1. SIGTERM a la VM (espera hasta 90 s shutdown graceful, si timeout SIGKILL).
2. Drop DB (opcional, contra pgbouncer de la cell).
3. Remueve bloque del `nkr-compose.yml` (con backup `.bak.<ts>`, rotado a las últimas 20 versiones).
4. Libera `vm_id` del registry (con flock + atomic rename).
5. Borra `instance_dir` completo.
6. Limpia `.nkr-data/<short>-*` (filestore + overrides + per-instance ext4s).

**Concurrencia:** un delete en curso bloquea simultáneamente cualquier `start/stop/restart/delete` sobre el mismo `nkr_name`. Otras instancias de la misma cell no se ven afectadas — cada nombre tiene su propio slot inflight.

### 4.8 `POST /api/v1/cells/{cell}/instances/{nkr_name}/actions` — Start / Stop / Restart

**ASÍNCRONO desde v1.5.1.** El endpoint encola el trabajo y devuelve `202` en <50 ms. Antes era síncrono y podía bloquear hasta 60–130 s — el panel cortaba por timeout y el restart quedaba colgado en el medio (stop terminaba pero start no llegaba a verse).

```bash
curl -s -X POST http://nkr-host:9090/api/v1/cells/odoo-v17/instances/odoo-v17-odoo-01/actions \
  -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
  -d '{"action":"restart"}'
```

Body:
```json
{ "action": "start" | "stop" | "restart" }
```

Respuesta 202 (<50 ms):
```json
{
  "action": "restart",
  "status": "accepted",
  "async": true,
  "info": { ... InstanceInfo con nkr_status SNAPSHOT pre-acción ... }
}
```

**Importante para el panel:**
- El campo `info.nkr_status` del 202 es del **momento previo** a la acción (running/pid/uptime de antes del SIGTERM). Útil para conocer `addons_path`/`dns`/`db_name`, **no** para readiness.
- Para detectar que la acción terminó:
  ```
  GET /api/v1/cells/{cell}/instances/{name}
  → nkr_status.port_8069_up == true   (start/restart)
  → nkr_status.running    == false    (stop)
  → nkr_status.phase      == "ready"  (start/restart cuando DB ya existe)
  ```
- Cadencia recomendada: cada **1 s**, hasta **180 s**. Si en ese plazo no llega a `ready`, leer `nkr_status.last_error` o `GET /logs?tail=200`.

**Conflicto 409 — `action_in_progress`:**
```json
{
  "error": "action_in_progress",
  "nkr_name": "odoo-v17-odoo-01",
  "message": "ya hay un start/stop/restart en curso para esta instancia"
}
```
NKR mantiene un slot por `nkr_name` mientras la acción está en vuelo (evita races en TAP/iptables/state cuando el webhook dispara dos restarts seguidos). El panel debería **polear** el `nkr_status` del que ya está corriendo en lugar de reintentar inmediatamente. Si después de 200 s el slot sigue ocupado, alertar al operador (acción colgada — el daemon log dice "[API] action(...) async error/ok").

**Tiempos típicos del trabajo en background (no del HTTP):**
| Acción | Duración real | Notas |
|---|---|---|
| `start` | 5–15 s | `nkr compose up -d` + boot kernel + Odoo inicializa workers. |
| `stop` | 1–10 s (típ. 2–5) | SIGTERM + drain de workers + cleanup. **Path generic** (Odoo, pgbouncer, otros): timer drain 5s desde v1.6.1, era 25s. **Path postgres**: timer propio de 15s (lee `postmaster.pid`, hace SIGTERM coordinado al postmaster). |
| `restart` | **5–25 s** (Odoo / pgbouncer) / 20–40 s (postgres) | stop + 200 ms + start. Tenants Odoo y pgbouncer: ~25s post-v1.6.1 (path generic con timer 5s). Postgres: ~20-40s (15s SIGTERM coordinado + checkpoint final). `skip_warmup=true` en clones evita los 55 s de warmup HTTP. |

**Nota:** los tiempos arriba aplican a **instancias creadas o re-iniciadas después de v1.6.1** (con el initramfs nuevo que reduce el timer de drain genérico de 25s → 5s). Instancias creadas previamente usan el initramfs viejo hasta su próximo `compose up` (ahí se regenera). Para forzar la regeneración: `nkr compose down && nkr compose up -d` desde el dir de la cell.

**Cómo el panel debe orquestar un webhook (`git push`) — actualizado v1.6.2:**

**Lo simple**: `POST /addons/git` con `auto_reload: true` (default) — NKR clona, detecta el cambio per-módulo, y dispara reload de workers automáticamente. **~5-7 segundos total**, sin downtime de VM/master, en CUALQUIER tier (production / staging / dev).

```
1. POST /addons/git                     → 200 con sha + diff + reloaded:true
   (auto_reload=true por default → NKR ya hizo el reload internamente)

Total: ~5-7 segundos. Cero llamadas extra al API.
```

**Por qué reemplaza al "no hacer nada" + inotify**: virtio-fs no propaga eventos inotify del host al guest (limitación FUSE). El reload explícito vía SIGUSR1 → hvc0 → SIGHUP es 0% CPU en idle y latencia ~3s. Ver §4.17.1.

**Sobre `POST /modules/upgrade`** (NO se recomienda automatizar tras git push):
- En multi-worker, solo refresca código en el worker que procesa el upgrade. Los demás siguen con código viejo en `sys.modules` (gotcha conocido de Odoo). Bug silencioso — el usuario que pega un worker "no upgradeado" ve comportamiento viejo sin error visible.
- Válido SOLO cuando hay **cambios de schema** que requieren migrations (campos nuevos en models). Pero esos también necesitan reload después para que el código en memoria coincida con la DB.
- En `tier=staging/dev` (workers=0, threaded): es seguro porque hay un solo proceso, pero `POST /reload` es más rápido y simple.

Si entre llamadas el panel mete otro `POST /actions` o `POST /modules/{op}` para la misma instancia, el segundo recibe `409 action_in_progress` — **no es un fallo**, polear hasta que el primero termine.

**Tiempos comparados (mismo cambio: 1 módulo OCA con código Python modificado):**
| Estrategia | Tiempo total | Garantía refresh TODOS los workers |
|------------|--------------|-------------------------------------|
| `addons/git` con `auto_reload:true` (**v1.6.2 default**) | **~5-7 s** | ✅ siempre |
| `addons/git` + `actions {restart}` (full VM) | ~20-30 s | ✅ |
| `addons/git` + `modules/upgrade` (multi-worker) | 10-15 s | ❌ (solo el worker upgrader) ⚠️ |
| Solo `addons/git` + esperar inotify | nunca | ❌ (virtio-fs limita inotify) |

### 4.8.1 `nkr_status` — campos y máquina de estados

Cualquier endpoint que devuelva `InstanceInfo` incluye `nkr_status`:

```json
{
  "running": true,
  "pid": 170123,
  "ram_mb": 248,
  "uptime_s": 4,
  "port_8069_up": true,
  "phase": "ready",
  "db_present": true,
  "odoo_version_running": "17.0",
  "last_error": null
}
```

**Phases (máquina de estados para el UI del panel):**

| Phase | Significado | Cuándo aparece |
|---|---|---|
| `provisioning` | Clon hecho, VM no arrancada | Inmediatamente después de `POST /instances` |
| `booting` | VM running, Odoo aún no escucha en 8069 | Segundos después de `POST /actions {start}` (~5-15s) |
| `loading` | `:8069` responde pero DB del tenant no existe | Después del boot, antes de `POST /init-db` |
| `ready` | VM + Odoo + DB — listo para uso | Después de `POST /init-db` exitoso |
| `error` | VM debería estar viva pero no lo está (según meta.json) | OOM kill, crash de Odoo, etc. `last_error` trae la última línea sospechosa del log |

El panel polea este campo en `GET /instances/{name}` hasta llegar a `ready`.

**`db_present`**: NKR hace `psql -c "SELECT 1 FROM pg_database WHERE datname='db-<nkr_name>'"` contra el PG de la cell. Rápido (~50ms), no mantiene pool. Si el PG no responde, devuelve `false`.

**`odoo_version_running`**: leído de `meta.json` → `cell.yml` (no del proceso Odoo — sería otro round-trip HTTP).

**`last_error`**: sólo cuando `phase == "error"`. Scan del `nkr-compose.log` por líneas que contengan el `nkr_name` + `ERROR`/`FATAL`/`Process exited`/`Traceback`, último match (truncado a 300 chars).

### 4.9 `GET /api/v1/cells/{cell}/instances/{nkr_name}/logs` — Tail + live-follow de logs

Dos modos de operación sobre el mismo endpoint — el panel elige según el caso:

| Modo | Query | Uso |
|---|---|---|
| **Tail (snapshot)** | `?tail=N` | Abrir la vista de logs: últimas N líneas. |
| **Cursor (live-follow)** | `?from_offset=<byte>&max_lines=M&wait_ms=<ms>` | Seguimiento incremental tipo `tail -f`. |

**Query params:**

| Param | Default | Máximo | Descripción |
|---|---|---|---|
| `tail` | 200 | 10000 | Modo snapshot. Ignorado si `from_offset` está presente. |
| `from_offset` | — | — | Byte offset desde donde leer. Obtenido de `next_offset` del response previo. |
| `max_lines` | 500 | 10000 | Cap de líneas en modo cursor. |
| `wait_ms` | 0 | 25000 | Long-poll: si el archivo no creció, bloquea hasta este timeout. Default 0 = no bloquear. |

**Respuesta (modo tail):**
```json
{
  "nkr_name": "odoo-v17-odoo-01",
  "logs_path": "/mnt/nkr/cells/odoo-v17/instances/odoo-v17-odoo-01/logs/odoo.log",
  "mode": "tail",
  "tail": 200,
  "lines": ["2026-04-23 ..."],
  "next_offset": 18234567,
  "file_size": 18234567
}
```

**Respuesta (modo cursor):**
```json
{
  "nkr_name": "odoo-v17-odoo-01",
  "logs_path": "/mnt/nkr/cells/...",
  "mode": "cursor",
  "lines": ["2026-04-23 ..."],
  "from_offset": 18234567,
  "next_offset": 18240192,
  "file_size": 18240192,
  "rotated": false,
  "eof": true
}
```

**Flujo live-follow recomendado (panel):**
1. `GET /logs?tail=200` → pintar las 200 líneas en la vista; guardar `next_offset` (llámalo `cursor`).
2. Loop cada ~500 ms: `GET /logs?from_offset=<cursor>&max_lines=500&wait_ms=5000`.
   - El endpoint bloquea hasta 5 s esperando nuevas líneas (reduce polling).
   - Append `lines` al final; actualizar `cursor = next_offset`.
3. Si `rotated:true` → el archivo fue truncado/rotado; el panel re-empieza con `tail=200` y sigue.

**Ejemplos:**
```bash
# Snapshot inicial (última página)
curl -s -H "$TOK" \
  "$NKR_API_BASE/api/v1/cells/odoo-v17/instances/$NAME/logs?tail=200"
# → { mode:"tail", lines:[...], next_offset:1234567, ... }

# Live-follow: long-poll desde el cursor
curl -s -H "$TOK" \
  "$NKR_API_BASE/api/v1/cells/odoo-v17/instances/$NAME/logs?from_offset=1234567&max_lines=500&wait_ms=5000"
# → { mode:"cursor", lines:[...], next_offset:1239876, rotated:false, eof:true }
```

**Cap en memoria:** 4 MiB por call, tanto en tail como en cursor. Las líneas parciales (sin `\n`) no se devuelven — `next_offset` se ajusta hasta el último `\n` leído, así nunca perdés bytes al paginar.

### 4.9.1 `GET /api/v1/cells/{cell}/instances/{nkr_name}/logs/download` — Descargar `odoo.log`

Descarga raw el archivo `odoo.log` como `text/plain` con `Content-Disposition: attachment`. Útil para análisis offline (grep local, ingestión en ELK, archivo).

- Sin query params.
- Cap de lectura: **64 MiB**. Si `odoo.log` es más grande → se devuelven los **últimos 64 MiB** y el response incluye el header `X-NKR-Log-Truncated: <original_file_size_bytes>`. Para paginar el histórico completo, usar `GET /logs?from_offset=0` iterando.

```bash
curl -s -H "$TOK" -o odoo.log \
  "$NKR_API_BASE/api/v1/cells/odoo-v17/instances/$NAME/logs/download"

# Con header truncation: ver cuánto faltó
curl -sI -H "$TOK" \
  "$NKR_API_BASE/api/v1/cells/odoo-v17/instances/$NAME/logs/download" \
  | grep -i '^x-nkr-log-truncated'
# X-NKR-Log-Truncated: 157286400   ← archivo era 150 MB, cap 64 MB
```

**Errores:**
- `404 log_not_found` — el archivo no existe (instancia nunca booteada).

### 4.10 `POST /api/v1/cells/{cell}/instances/{nkr_name}/addons/git` — Git clone de addons

Clona un repo Git en `addons/` y **explota los módulos al nivel `addons/<modulo>/`** para que `addons_path` quede en una sola ruta `/mnt/extra-addons` sin importar el layout del repo de origen.

**Soporta submódulos privados con jerarquía profunda** (padre/hijo/nieto/etc., a profundidad arbitraria). NKR clona recursivo, escanea todo el árbol resultante, y aplana TODOS los módulos Odoo encontrados a `addons/<modulo>/`. Los dirs intermedios (agrupadores sin manifest) se descartan.

Layout final dentro del tenant (independiente del layout del repo):
```
instances/<tenant>/addons/
  ├── modulo_a/__manifest__.py
  ├── modulo_b/__manifest__.py
  └── modulo_c/__manifest__.py
```

**Cómo decide el layout NKR:**

| layout del repo clonado | acción |
|---|---|
| `repo/__manifest__.py` (single-module en raíz) | mueve todo a `addons/<subdir>/` (`subdir` = lo que el panel pidió o el basename del repo) |
| `repo/<m1>/__manifest__.py`, `repo/<m2>/__manifest__.py`, ... (multi-módulo en primer nivel) | mueve cada `<m>/` con `__manifest__.py` a `addons/<m>/`. Los archivos en raíz del repo (README, LICENSE, .git) se descartan. |
| `repo/.gitmodules` con jerarquía padre/hijo/nieto (submódulos a profundidad arbitraria) | clone con `--recurse-submodules`, scan recursivo del árbol completo, mueve cada dir con `__manifest__.py` a `addons/<m>/` **sin importar la profundidad de Git**. Los agrupadores intermedios (submódulos sin manifest) se ignoran. Ejemplo en §4.10.1. |
| sin manifests en ninguna parte del árbol | `422 no_modules_found` |
| dos módulos con mismo dirname encontrados en distintas ramas del árbol | `409 module_name_collision` (deploy abortado, NO toca el filesystem destino) |

**Idempotencia + Plan C (v1.6.3 final):** cada `POST /addons/git` ejecuta un flujo determinista. Diseñado para **densidad multi-tenant (100+ Odoos en 32 GB)** — cero downtime perceptible durante el deploy:

1. **Setup staging hermano** — `mkdir <instance>/.nkr-addons-new/` (al lado de `addons/`, NO bajo addons/). El guest no ve este path porque virtio-fs sólo expone `addons/`.
2. **Clone a `staging/.nkr-clone-tmp/`** — `git clone --recurse-submodules` dentro del staging. `addons/` del tenant queda intacto sirviendo tráfico.
3. **Higiene de Origen** — `git clean -ffdx` recursivo sobre el clone fresco (parent + cada submódulo).
4. **Validación 422 estricta** — walk recursivo de `.gitmodules`: cada `path = X` debe ser módulo Odoo o agrupador. Submódulos vacíos → `422 submodule_clone_partial`. Submódulos basureros → `422 submodule_no_manifest`. Falla acá NO toca el tenant.
5. **Scan + colisiones + hashes** — encuentra módulos, computa `git rev-parse HEAD:<rel>` per-módulo. Detección de colisiones → `409 module_name_collision`.
6. **Snapshot del estado previo** — lee `addons/<m>/.nkr-source` actual (para diff).
7. **Populate intra-staging** — `rename(clone_tmp/<rel>, staging_dir/<m>/)` por módulo + escribir `.nkr-source` con tracker. Cleanup del `clone_tmp/` (sobra el `.git`).
8. **Replace per-módulo + trash sibling** — por cada módulo en staging:
   - Si `addons/<m>` existe → `rename(addons/<m>, <instance>/.nkr-trash/<ts>-<m>)`. **El trash es HERMANO de `addons/`**, fuera del addons_path → Odoo no lo escanea jamás.
   - `rename(staging/<m>, addons/<m>)` — el nuevo módulo aparece atómicamente en el lugar correcto.
   - Los fds que Odoo viejo tenga abiertos al módulo viejo siguen vivos (POSIX inode semantics): apuntan al inode movido al trash, que persiste hasta el `rm -rf` lazy.
9. **Cleanup lazy (60s después)** — background thread hace `rm -rf <instance>/.nkr-trash/*`. Para entonces ya pasó el REL_OD vía HVC0 y los fds del Odoo viejo se cerraron (proceso muerto, supervisor respawneó).

**Por qué el trash en `.nkr-trash/` y no dentro de `addons/`** (lección aprendida 2026-05-11): Odoo 19 `update_list()` escanea TODOS los dirs de addons_path **incluyendo dotfiles**. Si el trash queda dentro del addons_path (como `addons/.nkr-trash-...`), Odoo lo trata como módulo, lee su `__manifest__.py`, y crashea con `FileNotFoundError: Invalid module name` cada vez que la UI hace Update Apps List, button_immediate_upgrade, o cualquier cron interno dispara `update_list`. Los crashes silenciosos acumulados degradan progresivamente la estabilidad del proceso. **El trash DEBE estar fuera del addons_path** — sibling del dir `addons/`, no hijo.

**Por qué NO usamos `renameat2(RENAME_EXCHANGE)` del top-level `addons/`** (lección aprendida 2026-05-11): el swap intercambia el INODE del dir top-level, pero virtio-fs **no propaga la invalidación al guest** — el guest mantiene su dentry/inode cache apuntando al viejo inode. Tras el cleanup del staging (= viejo inode), el guest empieza a ver archivos fantasmas en `addons/` (listdir muestra entries, `open()` retorna ENOENT). Reproducido con error "Some modules are not loaded" + "Missing model queue.job" tras un deploy.

**Garantía clave**: Odoo guest con archivos abiertos del addons viejo sigue leyéndolos hasta cerrar los fds. El próximo `os.listdir(addons)` cae sobre el nuevo set porque el dir top-level mantiene su inode. **Cero downtime perceptible, cero D-state, cero zombie.** Compatible con escala (100+ tenants × N deploys/día).

El campo `removed` en la response lista los módulos que estaban antes y NO vinieron en este ciclo (panel debe marcar esos addons como "desinstalar" en su UI si correspondiera).

**Body (SSH deploy key):**
```json
{
  "repo_url": "git@github.com:owner/module-foo.git",
  "subdir": "module-foo",
  "ref": "17.0",
  "action": "sync",
  "deploy_key_b64": "LS0tLS1CRUdJTiBPUEVOU1NIIFBSSVZBVEUgS0VZLS0tLS0K..."
}
```

**Body (HTTPS con Personal Access Token de GitHub):**
```json
{
  "repo_url": "https://github.com/owner/private-repo.git",
  "subdir": "private-repo",
  "ref": "17.0",
  "github_token": "ghp_AbCdEf123..."
}
```

| Campo | Requerido | Descripción |
|---|---|---|
| `repo_url` | sí | SSH `git@host:owner/repo[.git]` o HTTPS `https://host/owner/repo[.git]`. Whitelist hoy: `github.com`, `gitlab.com`. |
| `subdir` | no | Nombre del dir bajo `addons/` (default: basename del repo). Pasa `is_safe_identifier`. |
| `ref` | no | Branch, tag o SHA. Default: el HEAD remoto que vino con el clone. |
| `action` | no | `"sync"` (default): clone-si-no-existe, pull-si-existe. `"clone"` / `"pull"` para comportamiento explícito. |
| `deploy_key_b64` | no | Clave SSH privada PEM base64-encoded para repos privados vía `git@`. Proxy la escribe a tmpfile `mode=0600`, la usa vía `GIT_SSH_COMMAND`, la borra. Nunca se loguea. |
| `github_token` | no | Personal Access Token de GitHub para repos privados vía HTTPS. NKR lo inyecta como `https://x-access-token:<token>@github.com/...` para el clone, después hace `remote set-url` al original SIN token para no persistir credenciales en `.git/config`. Válido sólo si `repo_url` empieza con `https://`. |

**Ambos (deploy_key_b64 y github_token) son mutuamente exclusivos por URL**: si usás SSH URL (`git@github.com:...`) → `deploy_key_b64`. Si usás HTTPS URL → `github_token` o ninguno (para repos públicos).

**Respuesta 200:**
```json
{
  "repo_url": "https://github.com/OCA/account-financial-tools.git",
  "ref":      "17.0",
  "sha":      "abc123def456...",
  "module_count": 5,
  "modules":  ["account_chart_update", "account_payment_term_extension",
               "account_journal_lock_date", "account_move_name_sequence",
               "account_fiscal_year"],
  "added":     ["account_fiscal_year"],
  "updated":   ["account_chart_update"],
  "unchanged": ["account_payment_term_extension",
                "account_journal_lock_date",
                "account_move_name_sequence"],
  "removed":   ["account_legacy_obsoleto"]
}
```

**Auto-reload (`auto_reload` field, default `true`):** Tras `explode_modules` exitoso, NKR dispara automáticamente un reload de los workers Odoo (ver §4.17.1) — workers respawnean con código fresh sin reiniciar la VM ni el master. Resuelve la limitación virtio-fs+inotify (los archivos cambian en disco pero el watcher de Odoo no se entera). Pasá `auto_reload: false` en el body si preferís controlar el momento del reload manualmente vía `POST /reload` — **necesario cuando el commit también toca `requirements.txt`**: en ese caso el panel hace `POST /addons/git auto_reload:false` → `PUT /pylibs` (esperar 200) → un único `POST /actions {restart}` (no `reload`). Tabla completa de qué hacer según qué tocó el commit: ver §4.12 ("¿Cuándo el panel debe llamar a `PUT /pylibs`?"). La response incluye `reloaded: bool` y `reload_skipped_reason` para diagnóstico.

**Diff per-módulo (`added` / `updated` / `unchanged`):** NKR computa `git rev-parse HEAD:<path>` per-módulo (tree-hash determinístico) y lo persiste en `.nkr-source` (campo `content_hash`). Al próximo `POST /addons/git`:
- **`added`** — el módulo no existía antes en `addons/`, o el `.nkr-source` previo no tenía `content_hash` (caso de migración: primer push después de upgrade a v1.6.1+). El panel debe tratarlo como "disponible para Install" — no requiere `upgrade`.
- **`updated`** — existía y el tree-hash cambió. La acción correcta depende del tier (ver §7.0 + §4.8): `tier=staging`/`tier=dev` → nada (inotify reload automático). `tier=production` → `POST /actions {restart}` (NO `/modules/upgrade` automático: en multi-worker solo refresca un worker, los demás siguen viejo). Si el módulo no estaba instalado, basta con Install desde Apps cuando convenga.
- **`unchanged`** — existía y el tree-hash es idéntico. **Cero acción necesaria** (típico cuando el push no tocó ese módulo). Permite al panel evitar `upgrade` innecesarios.
- **`removed`** — estaba antes en `addons/` (con tracker `.nkr-source`) y NO vino en este ciclo. El módulo se movió al trash sibling (`.nkr-trash/`) y se borrará 60s después. **El panel debe avisar al operador**: si el módulo estaba instalado en la DB, Odoo seguirá teniendo registros pero sin el código → comportamiento indefinido en próxima carga. Acción recomendada: `POST /modules {op:"uninstall", modules:["<m>"]}` antes de hacer este `git push` que removió el módulo del meta-repo, o reaceptar el cambio si el módulo era no-instalado.

**Granularidad**: el diff funciona uniformemente con cualquier layout (single-module, multi-módulo, submódulos profundos). Para repos multi-módulo el tree-hash es **per-subdir**, así que un commit que sólo toca `module-a/` reporta `module-a` en `updated` y el resto en `unchanged`. Para submódulos: el `.git` más cercano al módulo es el del submódulo, y NKR usa ese para el rev-parse.

**Limitación conocida (migración):** las instancias creadas antes de v1.6.1 tienen `.nkr-source` sin `content_hash`. El primer `POST /addons/git` después de upgradear NKR clasificará TODO como `added` aunque los módulos no hayan cambiado realmente — falso positivo benigno (no rompe nada, sólo el diff es ruidoso ese deploy). Llamadas subsecuentes funcionan correctamente.

**Errores:**
- `400 invalid_repo_url` — URL fuera del whitelist o con metacaracteres de shell.
- `400 invalid_ref` — ref con chars inválidos o empezando con `-`.
- `400 invalid_deploy_key` — base64 inválido o no es PEM.
- `400 invalid_module_name` — algún subdir-modulo tiene caracteres fuera de `[A-Za-z0-9._-]{1,64}`.
- `400 git_ref_not_found` — la branch/tag de `ref` no existe en el remote.
- `401 git_auth_required` — **repo privado sin credenciales**. Reintentar con `github_token` (HTTPS) o `deploy_key_b64` (SSH). El body `message` explica qué falta.
- `401 git_ssh_auth_failed` — la deploy key no tiene acceso al repo (o no fue agregada en GitHub → Settings → Deploy keys).
- `404 instance_not_found` — la instancia no existe en disco.
- `404 git_repo_not_found` — el repo no existe o el token no tiene permiso de lectura.
- **`409 module_name_collision`** — dos `__manifest__.py` con el mismo dirname encontrados en distintas ramas del árbol Git (sólo aplica con submódulos). Body incluye `conflicts: [{module_name, found_at: [...]}]` listando los paths exactos donde apareció cada duplicado, más un `remediation`. **NKR aborta el deploy y no toca el filesystem destino** — la VM Odoo del tenant sigue corriendo con el árbol anterior. **El panel debe marcar este deploy como FAILED / DROPPED / ROJO en su UI**: NKR no eligió un ganador automáticamente para evitar pérdida silenciosa de código. El cliente debe renombrar uno de los módulos en conflicto en su repo Git (commit + push) y re-mandar el POST. Ver §4.10.2.
- `409 module_conflict` — uno o más módulos del repo ya existen en `addons/` y vienen de otro `repo_url` (según `.nkr-source`). Distinto de `module_name_collision`: este es overwrite vs OTRO repo previo, no colisión dentro del mismo deploy. Body incluye `conflicts: [{module, existing_repo, attempted_repo}, ...]`. Panel debe borrar manualmente los módulos en conflicto antes de reintentar.
- `422 no_modules_found` — el árbol clonado (incluyendo submódulos recursivos) no contiene ningún `__manifest__.py`. Probablemente el `ref` apunta a una rama vacía o a un repo de tooling sin módulos Odoo.
- **`422 submodule_clone_partial`** — algún submódulo del árbol quedó vacío post-clone (PAT no tiene scope sobre ese repo, SHA inexistente por force-push, owner GitHub distinto, etc.). Body incluye `failed_submodules: ["path/relativo", ...]` y `remediation`. **NKR no aplica nada al filesystem destino** — la VM Odoo sigue con el árbol anterior. El panel debe mostrar el deploy como fallido y solicitar al operador que corrija el scope del PAT y re-mande.
- **`422 submodule_no_manifest`** (CLAUDE.md v2.2 strict) — algún submódulo declarado en `.gitmodules` no es módulo Odoo (no tiene `__manifest__.py` al raíz) ni agrupador (no tiene `.gitmodules` propio). Doctrina: "el meta-repo no es un basurero de scripts; es para módulos Odoo". Body incluye `invalid_submodules: ["path/relativo", ...]` con los paths exactos. **El panel debe rechazar el deploy** y pedir al operador que limpie el `.gitmodules`. NKR aborta antes del populate — la VM sigue con su `addons/` previo intacto.
- **`500 module_trash_failed`** / **`500 module_install_failed`** — falla en el rename per-módulo (paso 8). Causa típica: EACCES, ENOSPC, o el FS no permitió la operación. El staging y el trash quedan en disco; el próximo `POST /addons/git` los limpia. La VM sigue con sus `addons/` previo + lo que ya se haya movido al trash (estado mixto si falló a mitad del loop). En ese caso, hacer `POST /actions {restart}` recupera estado consistente.
- `422 git_clone_failed` — git devolvió no-cero sin match a los anteriores. Body incluye `log_tail` (30 líneas).
- `500 move_failed` — `rename` falló (típicamente filesystem lleno o cross-device move). Body incluye el path origen/destino.
- `504 git_timeout` — git tardó más de 180 s (red lenta o repo muy grande).

Todos los errores devuelven `{ error, message, repo_url, ref, target, log_tail }` para que el panel pueda mostrar el detalle al operador.

**Ejemplo curl:**
```bash
# Repo OCA multi-módulo (público) — todos los módulos quedan al nivel addons/
curl -s -X POST -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"repo_url":"https://github.com/OCA/account-financial-tools.git","ref":"17.0"}' \
  http://nkr-host:9090/api/v1/cells/odoo-v17/instances/odoo-v17-cliente-42/addons/git
# → { "module_count": 23, "modules": ["account_chart_update", ...] }

# Repo privado single-module (deploy key base64) — explota a addons/<subdir>/
DEPLOY_KEY_B64=$(base64 -w0 < ~/keys/cliente-42.pem)
curl -s -X POST -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d "{\"repo_url\":\"git@github.com:cliente/propio.git\",\"subdir\":\"propio\",\"ref\":\"17.0\",\"deploy_key_b64\":\"$DEPLOY_KEY_B64\"}" \
  http://nkr-host:9090/api/v1/cells/odoo-v17/instances/odoo-v17-cliente-42/addons/git
# → { "module_count": 1, "modules": ["propio"] }
```

**Para que Odoo vea los módulos nuevos** (que físicamente ya están en `/mnt/extra-addons` después del clone, propagados por virtio-fs en tiempo real):

| Escenario | Acción recomendada | Tiempo |
|-----------|-------------------|--------|
| **Módulo nuevo** (no estaba antes) | UI Odoo → Apps → "Update Apps List" → Install. **Sin restart.** El path ya está en `addons_path` desde el primer boot. | <1 s + duración del Install |
| **Módulo ya instalado que cambió código** | `POST /modules/upgrade` con `{modules:["mod"]}` (§4.17). Odoo recarga in-process. | 10-15 s |
| **Módulo nuevo, panel quiere automatizar sin UI** | Hoy falla — `install` busca el módulo en `ir_module_module` y no lo encuentra (update_list no corrió). Workaround: el panel ejecuta `update_list` vía `/psql` antes del install, o hace el flow vía UI. | n/a |

**El `POST /actions {action:"restart"}` ya NO es la recomendación primaria** para módulos. Es overkill: tarda 60-70 s (mayormente shutdown gracioso del guest, ver §4.8) y reinicia toda la VM cuando Odoo puede recargar el código en-proceso vía `upgrade`. Reservalo para cambios fuera del ámbito de Odoo (pip libs, `odoo.conf` keys non-hot-reloadable, etc.).

**Timeout:** 180s. Si el repo es muy grande, el panel debería hacer el clone en vertientes (primero `--depth 1`, después fetch de refs adicionales).

### 4.10.1 Submódulos privados con jerarquía profunda

Caso de uso: el cliente quiere mantener cada módulo en un repo privado separado, agruparlos jerárquicamente, y que NKR los publique todos planos en `addons/`.

**Layout en GitHub (jerarquía padre/hijo/nieto, profundidad arbitraria):**

```
acme-customs/                      ← repo padre (privado)
├── .gitmodules
├── module-direct/                 ← submódulo HIJO (1 módulo Odoo, manifest en raíz)
│   └── __manifest__.py
├── group-frontend/                ← submódulo HIJO (NO módulo, agrupador)
│   ├── .gitmodules                ← submódulos anidados (nietos)
│   ├── module-x/                  ← submódulo NIETO
│   │   └── __manifest__.py
│   └── module-y/                  ← submódulo NIETO
│       └── __manifest__.py
└── group-backend/                 ← submódulo HIJO agrupador
    ├── .gitmodules
    └── module-z/                  ← submódulo NIETO
        └── __manifest__.py
```

`group-frontend` y `group-backend` **NO tienen `__manifest__.py` en su raíz** — son agrupadores. NKR los recorre buscando módulos adentro pero NO los publica como módulos.

**Resultado en NKR (aplanado total):**

```
/mnt/nkr/cells/.../instances/<tenant>/addons/
├── module-direct/__manifest__.py   ← venía del hijo
├── module-x/__manifest__.py        ← venía del nieto group-frontend/module-x
├── module-y/__manifest__.py        ← venía del nieto group-frontend/module-y
└── module-z/__manifest__.py        ← venía del nieto group-backend/module-z
```

`addons_path = /mnt/extra-addons`. Odoo enumera los 4 módulos. **Cero subdirs intermedios** en el filesystem del tenant — los agrupadores Git desaparecieron correctamente.

**Convenciones obligatorias del meta-repo:**

1. Cada submódulo que sea un módulo Odoo debe tener `__manifest__.py` en su raíz.
2. **Los nombres de directorio de los módulos deben ser únicos en todo el árbol.** Si dos repos distintos tienen un módulo llamado `module-x`, NKR aborta con `409 module_name_collision` (ver §4.10.2).
3. Submódulos sin `__manifest__.py` se tratan como agrupadores: NKR recorre adentro pero no los publica.
4. Todo el árbol (padre + hijos + nietos + ...) debe estar bajo el **mismo owner GitHub**. Cross-owner privado no se soporta — limitación de auth de GitHub.

**Auth con un solo PAT cubriendo todo el árbol:**

```bash
# 1. Crear PAT fine-grained en https://github.com/settings/personal-access-tokens
#    - Repository access: "Only select repositories" → meta-repo + cada repo de submódulo
#    - Permissions: Contents: Read-only, Metadata: Read-only
#    - Expiration: 90 días (renovable)
# 2. Pasarlo en el body:

curl -s -X POST -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "repo_url": "https://github.com/acme/customs.git",
    "ref": "main",
    "github_token": "ghp_..."
  }' \
  http://nkr-host:9090/api/v1/cells/odoo-v17/instances/odoo-v17-cliente-42/addons/git
# → { "module_count": 4, "modules": ["module-direct", "module-x", "module-y", "module-z"] }
```

NKR usa `url.insteadOf` internamente para que el PAT aplique recursivamente a todos los submódulos del owner, sin necesidad de modificar las URLs en los `.gitmodules` del cliente.

**Cero `git pull` incremental.** Cada `POST /addons/git` es un re-clone completo del árbol — no se mantiene `.git` activo en `addons/`. Tiempo típico: ~15-30 s para árboles de 5-15 módulos. Tráfico contra GitHub: 1 clone por cada repo del árbol (mantenemos `--depth 1` y `--shallow-submodules` para minimizar bytes transferidos).

### 4.10.2 Ejemplo de respuesta `409 module_name_collision`

Caso: el cliente tiene `module-x` declarado en `group-frontend/` Y en `group-backend/` (mismo nombre, distintas ramas del árbol):

```json
HTTP 409
{
  "error": "module_name_collision",
  "message": "dos o más módulos con el mismo nombre fueron encontrados en distintas ramas del árbol Git. Renombrar uno de ellos en el repo y re-mandar.",
  "conflicts": [
    {
      "module_name": "module-x",
      "found_at": [
        "group-frontend/module-x",
        "group-backend/module-x"
      ]
    }
  ],
  "remediation": "Renombrar el directorio del módulo en uno de los repos en conflicto y commit + push. NKR no eligió un ganador automáticamente para evitar perder código silenciosamente.",
  "repo_url": "https://github.com/acme/customs.git",
  "ref": "main"
}
```

**Comportamiento del panel ante este 409:**

1. Marcar el deploy como **failed / dropped / rojo** en la UI del operador.
2. Listar los módulos en conflicto con sus paths Git completos (de `conflicts[].found_at`).
3. Mostrar el campo `remediation` para guiar al operador.
4. **No reintentar automáticamente** — el cliente debe arreglar el árbol Git primero.

NKR **no aplica nada al filesystem destino** cuando hay colisión: la VM Odoo del tenant sigue corriendo con el árbol anterior. Una vez que el cliente renombra el módulo en su repo (commit + push) y el panel re-manda el POST, el deploy procede normalmente.

### 4.11 `POST /api/v1/cells/{cell}/enterprise/git` — Clone de Odoo Enterprise

Mismo body y semántica que 4.10, pero escribe a `/mnt/nkr/enterprise/<odoo_version>/` donde `<odoo_version>` se lee de `cell.yml` (no viene en el body — evita mismatches).

El path es **shared por cell** y **opt-in per instancia**: sólo las instancias creadas con `edition: "enterprise"` montan ese dir como `/mnt/extra-enterprise` (el campo se traduce a una share en el compose block del clone más una entrada en `addons_path`). Las instancias `community` no lo montan, no ven los manifests enterprise, y no warnean por dir vacío. Con 20 Odoos por cell, el repo se descarga una sola vez aunque sólo algunas lo usen — la share es read-only.

```bash
curl -s -X POST -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d "{\"repo_url\":\"git@github.com:odoo/enterprise.git\",\"ref\":\"17.0\",\"deploy_key_b64\":\"$DEPLOY_KEY_B64\"}" \
  http://nkr-host:9090/api/v1/cells/odoo-v17/enterprise/git
```

**Respuesta 200:** igual a 4.10, con `path` apuntando a `/mnt/nkr/enterprise/17.0/`.

**Nota:** el primer clone de enterprise es grande (~500 MB, varios minutos). El panel debe extender `proxy_read_timeout` de nginx para esta ruta. Después es `git pull` rápido.

### 4.11.1 `GET /api/v1/cells/{cell}/enterprise` — Estado del repo Odoo Enterprise

Reporta si la cell tiene el repo Enterprise descargado en `/mnt/nkr/enterprise/<odoo_version>/`. El panel lo usa para decidir antes de aceptar `edition: "enterprise"` al crear tenants — sin Enterprise descargado el tenant arrancaría con `addons_path` warnings y se comportaría como community pese a estar marcado enterprise.

**Sin body.** Respuesta 200:
```json
{
  "cell": "odoo-v19",
  "odoo_version": "19.0",
  "available": true,
  "module_count": 86,
  "path": "/mnt/nkr/enterprise/19.0",
  "size_bytes": 524288000,
  "sha": "abc123def456..."
}
```
- `available`: `true` si hay al menos 1 subdir con `__manifest__.py` en el path
- `module_count`: cantidad de módulos enterprise detectados
- `sha`: HEAD del repo si tiene `.git`, vacío si fue copiado sin git

Respuesta `404 cell_not_found` si la cell no existe en `/mnt/nkr/cells/`.

### 4.11.2 Flujo coordinado de aprovisionamiento Enterprise

El repo Enterprise es **shared per-cell**: todas las instancias enterprise de la cell montan el mismo dir `/mnt/nkr/enterprise/<v>/` como virtio-fs share read-only. El panel debe orquestar el orden:

```
PASO 1 — UNA VEZ por cell (cuando provisionás la cell):
  GET /api/v1/cells/{cell}/enterprise
  → Si available=true → skip (ya descargado, idempotente)
  → Si available=false:
      POST /api/v1/cells/{cell}/enterprise/git
      body: { repo_url: "git@github.com:odoo/enterprise.git",
              ref: "<odoo_version>",
              deploy_key_b64: "<base64 SSH private key>" }
      → 200 con sha del HEAD recién clonado.

PASO 2 — POR CADA tenant enterprise (después del paso 1):
  POST /api/v1/instances
  body: { ..., "edition": "enterprise" }
```

**Idempotencia del paso 1:** `POST /enterprise/git` con `action: "sync"` (default) hace `git clone` la primera vez, y `git pull` en las siguientes (rápido, solo deltas; no-op si nada cambió). El panel puede llamarlo siempre antes de cada wave de tenants enterprise sin coste — buen patrón si querés mantener Enterprise actualizado periódicamente (cron del panel: `POST /enterprise/git` semanal).

**Si el panel mandó `edition: "enterprise"` sin haber hecho el paso 1**, NKR rechaza el `POST /instances` con `409 enterprise_not_provisioned` y body:
```json
{
  "error": "enterprise_not_provisioned",
  "message": "edition=enterprise solicitada pero la cell 'odoo-v19' no tiene el repo Odoo Enterprise descargado en /mnt/nkr/enterprise/19.0. El panel debe llamar primero `POST /api/v1/cells/odoo-v19/enterprise/git` con el deploy_key_b64 (o github_token) del cliente.",
  "cell": "odoo-v19",
  "odoo_version": "19.0",
  "enterprise_path": "/mnt/nkr/enterprise/19.0"
}
```

Esto evita el caso silencioso donde el tenant arranca, queda registrado como `enterprise` en `meta.json`, pero los manifests enterprise nunca se cargan (Odoo escribe warnings `invalid addons directory '/mnt/extra-enterprise', skipped` y se comporta como community).

### 4.12 `PUT /api/v1/cells/{cell}/instances/{nkr_name}/pylibs` — Instalar librerías Python

Escribe `requirements.txt` e invoca `pip install --target` en el `pylibs/lib/` de la instancia. La instancia monta ese dir como virtio-fs `/mnt/extra-pylibs` RO, y el initramfs exporta `PYTHONPATH` antes de `exec odoo` (v1.5).

**Body:**
```json
{
  "requirements_txt": "phonenumbers==8.13.30\nnum2words==0.5.13\nxlsxwriter==3.1.9\n"
}
```

**Respuesta 200:**
```json
{
  "installed": true,
  "lib_dir": "/mnt/nkr/cells/odoo-v17/instances/odoo-v17-cliente-42/pylibs/lib",
  "pip_log_tail": "Successfully installed phonenumbers-8.13.30 num2words-0.5.13 xlsxwriter-3.1.9\n..."
}
```

**Limites:**
- Body max 128 KiB.
- `requirements_txt` max 64 KiB.
- Timeout: 300s.
- **Wheels y sdist pure-Python funcionan** (pandas, numpy, scipy, lxml, Pillow, cryptography, etc. tienen wheel oficial en PyPI y andan sin recompilar). Sólo fallan sdists que necesitan compilar C desde fuente porque el proxy no tiene `gcc`/`libpython-dev` — devuelve `422 pip_install_failed`. Para esos casos o para libs que requieren paquetes apt del sistema, se bakean en el master (ver §4.12.1).
- El `pip install` corre con `umask 022` (el proxy systemd tiene `UMask=0077`, pero el child lo override vía `pre_exec`) → los archivos quedan world-readable (0644/0755), que es lo que el guest Odoo (uid 101, virtio-fs sin UID remap) necesita para importarlos. v1.6.4+ ya no hace un `chmod -R` posterior (que logueaba `Operation not permitted` benigno sobre el `pylibs/lib/` root-owned).

**Errores:**
- `400 missing_or_invalid_requirements_txt` — body sin campo o vacío.
- `404 instance_not_found` — instancia no existe.
- `422 pip_install_failed` — pip falló. Body incluye `log_tail`. Causas típicas: wheel no disponible para el Python del guest + falta gcc, versión de dep no existe en PyPI, nombre typo.
- `500 write_requirements_failed` — ACL mal configurada (ver §11).

#### Cómo aplicarlo correctamente (lo que el panel debe saber)

Después de un `PUT /pylibs` exitoso, **el único paso para que Odoo recoja las libs es `POST /actions {action:"restart"}`** — pero hay 3 cosas que el panel tiene que respetar o las libs nuevas no se ven:

1. **`PUT /pylibs` es SÍNCRONO** — bloquea hasta que `pip install` termina y recién ahí devuelve `200` con `pip_log_tail`. **El panel debe esperar ese `200` antes de disparar el restart.** Si lanza el restart en paralelo (o sin esperar la respuesta), hay una race: el VM remonta `/mnt/extra-pylibs` mientras pip todavía está escribiendo → el guest cachea un listing parcial → `import <lib>` falla igual aunque el `PUT` "haya funcionado".

2. **Es `POST /actions {action:"restart"}`, NO `POST /reload`** (REL_OD/`reload_workers`). El `reload` solo respawnea el proceso Odoo dentro del guest — NO remonta `/mnt/extra-pylibs` ni re-corre el initramfs (que es quien exporta `PYTHONPATH=/mnt/extra-pylibs` antes de `exec odoo`). Sólo el `restart` (VM down→up) garantiza mount fresco + `PYTHONPATH` correcto. Usar `reload` después de `PUT /pylibs` es un error común y deja a Odoo sin ver las libs nuevas.

3. **El `restart` es async y puede tardar ~40-90s** según el tenant (10s gracia SIGTERM + ~30s gracia de health de NKR + boot de Odoo + carga de módulos custom). `POST /actions {restart}` devuelve `202` al toque; el panel polea `GET /instances/{name}` → `nkr_status.port_8069_up`/`phase` hasta `ready` (ver §4.8). No bloquees la UI esperando el `202` — vuelve en <50ms; lo que tarda es el boot.

#### ¿Cuándo el panel debe llamar a `PUT /pylibs`? — pasos según qué tocó el commit

**Regla: sólo cuando `requirements.txt` está en el commit / cambió — NO en cada push.** El `PUT /pylibs` con contenido sin cambios es casi-no-op (pip dice `Requirement already satisfied`), pero el `restart` obligatorio que viene después es ~40-90s de downtime — correrlo en cada push (incluso pure-code) es caro e innecesario, y genera el churn de restarts del que el watchdog se confunde.

El panel mira el diff del commit recién pusheado y decide:

| Qué tocó el commit | Pasos del panel |
|---|---|
| **Sólo código de módulos** (`.py`, `.xml`, `.csv`, assets, etc.) | `POST /addons/git` con `auto_reload: true` → REL_OD vía HVC0, Odoo respawnea con código fresh en ~3s. **Nada más.** Sin `PUT /pylibs`, sin restart completo. |
| **`requirements.txt`** (de cualquier módulo) — solo o junto con código | 1. `POST /addons/git` con `auto_reload: false` (no reloadear todavía). 2. `PUT /pylibs` con el `requirements.txt` consolidado del repo (concatená los `*/requirements.txt` de todos los módulos). **Esperá el `200`.** 3. (Opcional pero recomendado) leé el `pip_log_tail` del `200`: si dice `Requirement already satisfied` para todo ⇒ nada cambió realmente ⇒ **podés saltarte el restart** y hacer solo un `POST /reload`. Si dice `Successfully installed ...` ⇒ seguí al paso 4. 4. `POST /actions {action:"restart"}`. 5. `GET /instances/{name}` → poll hasta `nkr_status.phase == "ready"`. |
| **Varios cambios a la vez** (código + requirements + quizá config) | Batchealos: `POST /addons/git auto_reload:false` → `PUT /pylibs` (esperá 200) → `PATCH /config` si aplica → **UN solo `POST /actions {restart}`** → poll `phase == "ready"`. Un restart al final recoge todo. |
| **`odoo.conf` / workers / SMTP** (no viene en un git commit — es `PATCH /config`) | `PATCH /config` → restart **sólo si** la respuesta trae `restart_required: true` (workers/memory lo requieren; SMTP no). Ver §4.15. |

**Por qué nunca `auto_reload:true` + restart juntos:** el `auto_reload` ya respawnea Odoo; un restart después es redundante y duplica el downtime. Elegí uno: reload (rápido, código nuevo) o restart (necesario para pylibs/config/kernel). Disparar reload/restart después de cada paso de un batch puede dejar `:8069` caído >60s entre medio → el watchdog (§7.2) lo confunde con un cuelgue y dispara su propio restart automático encima.

### 4.12.1 Estrategia híbrida — libs bakeadas vs dinámicas

El set de libs más común se **bakea en el master ext4** al buildear la imagen, con un `RUN pip3 install ...` en `Nkrfile.odoo` / `Nkrfile.odoo19`. Esto da densidad óptima: 20 tenants de una cell comparten una sola copia en la page cache del host vía DAX. Para pandas (~13 MB) la diferencia es 13 MB vs 260 MB (20 copias).

**Cuándo bakear vs usar `PUT /pylibs`:**

| Caso | Estrategia |
|---|---|
| Lib la usan todos/mayoría de tenants | Bakear en master |
| Lib necesita paquetes apt (`libmagic1`, `wkhtmltopdf`, `libvips42`) | Bakear (apt install + pip install en Nkrfile) |
| Lib de nicho que sólo 1-2 clientes usan | `PUT /pylibs` |
| Versión específica que difiere entre clientes | `PUT /pylibs` — sobreescribe la bakeada vía PYTHONPATH que va primero |

**Rebuild del master** cuando cambie el set común: se toca `Nkrfile.odoo(19)`, se corre `nkr build`, se hace swap atómico del `odoo.ext4` master + `nkr cell down/up` de la cell. Tenants existentes pueden seguir corriendo durante el build (sólo reciben las libs nuevas al próximo restart). No es un endpoint de la API hoy — es operación del host admin.

### 4.13 `POST /api/v1/cells/{cell}/instances/{nkr_name}/dns` — Provisionar DNS del tenant

Emite cert Let's Encrypt para el hostname que pasa el panel, genera el vhost nginx, hace reload. **Todo ejecutado por el daemon NKR** — el panel no necesita SSH al host.

**Prerequisito manual (fuera de NKR):** el panel debe crear el A record `<dns> → 116.202.240.179` en el proveedor DNS antes de llamar a este endpoint. Sin eso, Let's Encrypt no puede completar el HTTP-01 challenge y el cert emit falla.

**Body:**
```json
{
  "dns": "cliente-42.systemouts.com",
  "enable_websocket": true
}
```

| Campo | Requerido | Descripción |
|---|---|---|
| `dns` | sí | Hostname público, pasa `is_safe_dns` (alphanum + `.-`, max 253). |
| `enable_websocket` | no | Default `true`. Si `true`, añade `location /websocket` y `/longpolling` apuntando al puerto `:8072` del guest (gevent) — necesario para POS, live chat, auto-refresh. Poner `false` para tenants que sólo usan HTTP síncrono. |

**Respuesta 200:**
```json
{
  "nkr_name": "odoo-v17-cliente-42",
  "dns": "cliente-42.systemouts.com",
  "guest_ip": "10.0.1.7",
  "https_url": "https://cliente-42.systemouts.com",
  "vhost_path": "/etc/nginx/sites-available/odoo-v17-cliente-42",
  "cert_path": "/etc/letsencrypt/live/cliente-42.systemouts.com/fullchain.pem",
  "websocket_enabled": true,
  "base_url_update": "updated"
}
```

`base_url_update` indica el resultado del paso de actualizar `web.base.url` en la DB del tenant (ver §4.13.1 abajo). Valores:
- `updated` — `web.base.url=https://<dns>` y `web.base.url.freeze=True` quedaron seteados.
- `skipped_db_missing` — la DB todavía no existe (panel llamó `/dns` antes de `/init-db`). El panel debe **re-llamar `/dns` después de `/init-db`** para sellar la URL.
- `failed_nonblocking` — un error inesperado en el psql; el cert + vhost ya quedaron OK pero la URL en la DB no se actualizó. Verificar logs del daemon (`journalctl -u nkr.service`).

**Qué hace internamente:**
1. `certbot certonly --webroot --webroot-path /var/www/html -d <dns>` — idempotente, si el cert ya existe no re-emite innecesariamente.
2. Escribe `/etc/nginx/sites-available/<nkr_name>` con vhost generado (upstreams `:8069` HTTP + opcional `:8072` WS, redirect 80→443, TLS headers hardened vía `nkr-ssl.conf`, cache de `/web/static/`).
3. Symlink a `/etc/nginx/sites-enabled/<nkr_name>`.
4. `nginx -t` (si falla, rollback del symlink y devuelve 422).
5. `systemctl reload nginx`.
6. **Best-effort:** UPSERT `web.base.url=https://<dns>` y `web.base.url.freeze=True` en `ir_config_parameter` de la DB del tenant. Si la DB no existe (init-db no llamó todavía), se omite con `base_url_update:"skipped_db_missing"` y NO falla el endpoint — el panel debe re-llamar tras `/init-db`.

**Por qué se hace el paso 6:** Odoo 19 sirve `/web/login` con un form que viene con la clase `d-none` por default y un componente OWL (`UserSwitch`) lo descubre en runtime via JS. Si `web.base.url` está en `http://` mientras la página se sirve por HTTPS, el browser bloquea por mixed-content los recursos referenciados con URL absoluta, el JS del frontend falla a mitad de la hidratación, y el form **se queda invisible**. El cliente ve sólo el logo + "Powered by Odoo" sin inputs. Con `web.base.url=https://...` + `freeze=True` (que evita que Odoo lo auto-overwrite con el host del primer request), el problema desaparece.

**Idempotencia:** llamarlo múltiples veces con el mismo dns es no-op efectivo (cert no re-emite, vhost se re-escribe idéntico, reload no rompe nada, el UPSERT de `web.base.url` es idempotente). Llamarlo con un dns **distinto** en el mismo nkr_name reemplaza el vhost, emite cert nuevo, y actualiza `web.base.url` al dns nuevo.

**Orden recomendado del flujo (importante):**

```
POST /instances              ← 202; clone corre en background (async, v1.6.4+)
GET  /instances/{name}/create-status   ← poll hasta status="ready"
POST /addons/git             ← repos del cliente (con github_token / deploy_key_b64)
POST /actions {start}        ← VM bootea
GET  /instances/{name}       ← poll hasta phase="loading"
POST /init-db                ← crea DB (async, devuelve 202)
GET  /instances/{name}       ← poll hasta nkr_status.db_present=true
POST /dns                    ← cert + vhost + (NEW) sella web.base.url
```

Si por alguna razón llamás `/dns` ANTES de `/init-db` (panel quiere validar el cert sin esperar a la DB), está bien — el endpoint funciona pero `base_url_update=skipped_db_missing`. **Tenés que re-llamarlo** después del init-db para que el form de login renderice correcto.

**Errores:**
- `400 invalid_dns` — caracteres prohibidos o formato inválido.
- `404 instance_not_found` — el `nkr_name` no existe en la cell.
- `422 cert_issue_failed` — certbot falló (típicamente porque el A record no resuelve o LE rate-limit). Body incluye `log_tail`.
- `422 nginx_config_invalid` — el vhost generado rompe `nginx -t`. Rollback automático del symlink.
- `500 nginx_reload_failed` — nginx reload devolvió no-zero.

**Ejemplo:**
```bash
# Paso 0: crear A record  cliente-42.systemouts.com  A  116.202.240.179 en tu DNS.
# Paso 1: llamar al endpoint — NKR se encarga del cert + vhost + reload.
curl -s -X POST -H "$TOK" -H "Content-Type: application/json" \
  -d '{"dns":"cliente-42.systemouts.com"}' \
  $NKR_API_BASE/api/v1/cells/odoo-v17/instances/odoo-v17-cliente-42/dns
# → 200 { "https_url":"https://cliente-42.systemouts.com", ... }

# Paso 2: test
curl -I https://cliente-42.systemouts.com/web/database/selector
```

### 4.13.1 `POST /api/v1/cells/{cell}/instances/{nkr_name}/init-db` — Crear DB inicial del tenant (ASÍNCRONO)

Proxy a `/web/database/create` de Odoo. NKR usa el `master_pwd` del tenant (del `admin_passwd` que el panel envió en `POST /instances`) y recibe del panel sólo las credenciales del admin user del cliente final.

**⚡ Importante: endpoint asíncrono.** Odoo tarda 30-90 s creando `base` + `web` + migraciones. Para evitar timeouts HTTP del panel, NKR **no espera**: valida + arranca un worker en background + devuelve **`202 Accepted`** al instante. El panel hace poll de `GET /instances/{name}` para detectar completion.

**🆕 v1.6.3 — Auto-seal de `web.base.url`:** al final del job background, si el tenant ya tiene vhost provisionado (`/dns` se llamó antes), NKR ejecuta automáticamente el UPSERT de `web.base.url=https://<dns>` + `web.base.url.freeze=True` en `ir_config_parameter`. Esto cierra el agujero histórico donde el panel tenía que recordar re-llamar `/dns` después de `/init-db` (sino el form de login renderizaba con `http://...` y fallaba por mixed-content). El log del daemon muestra `[API] init-db(...) auto-sealed web.base.url=https://...`. **El panel ya no necesita el re-call de `/dns` post-init-db** — opcional para compatibilidad. Si no había vhost al momento del init-db, el seal ocurrirá cuando el panel llame `POST /dns` después (mismo código path).

**Prerequisitos:** la VM tiene que estar **running** Y **port_8069_up=true** (phase `loading` o superior). Si no, NKR responde `503 odoo_not_ready_yet` con `retry_after_s` → reintentar más tarde.

**Body:**
```json
{
  "admin_login": "admin",
  "admin_password": "<password-del-usuario-admin-del-cliente>",
  "demo": false,
  "lang": "es_PE",
  "country_code": "PE",
  "phone": null,
  "db_name": "db-odoo-v17-cliente-42"
}
```

| Campo | Requerido | Descripción |
|---|---|---|
| `admin_login` | sí | Usuario admin inicial (lo que el cliente usa para loguear a Odoo). |
| `admin_password` | sí | Password del user admin (≥ 4 chars). El panel lo guarda encriptado. |
| `db_name` | no | Default = `db-<nkr_name>`. |
| `demo` | no | Default `false`. Si `true`, instala datos de demo. |
| `lang` | no | Default `en_US`. Códigos Odoo (`es_PE`, `pt_BR`, etc.). |
| `country_code` | no | ISO 3166-1 alpha-2 (`PE`, `AR`, `BR`, etc.). |
| `phone` | no | Se guarda en `res.company`. |

**Respuesta 202 — job aceptado:**
```json
{
  "nkr_name": "odoo-v17-cliente-42",
  "db_name": "db-odoo-v17-cliente-42",
  "admin_login": "admin",
  "status": "accepted",
  "message": "init-db corriendo en background (30-90s típico). Poll GET /instances/{name} y espera nkr_status.db_present=true."
}
```

**Respuesta 200 — idempotente (DB ya presente):**
```json
{ "status": "already_present", "db_name": "...", "message": "DB ya existe — no-op idempotente" }
```

**Respuesta 202 — job YA en curso (llamada repetida mientras corre):**
```json
{ "status": "running", "job": { "started_at": 1234567890, ... } }
```

**Respuesta 503 — VM no lista todavía (transitorio, retriable):**
```json
{ "error": "odoo_not_ready_yet", "retry_after_s": 5, "message": "..." }
```

#### Flujo de polling (crítico)

Después del 202, el panel debe poll `GET /instances/{name}` y leer estos campos de `nkr_status`:

| Campo | Significado | Acción del panel |
|---|---|---|
| `db_present: true` | **Éxito** — DB creada y lista. | Marcar tenant como "ready" y continuar flujo (install modules, etc.). |
| `init_db.status: "running"` | Job aún corriendo. | Esperar 3-5 s y re-pollear. |
| `init_db.status: "failed"` | Error. Ver `init_db.error`, `init_db.body_snippet`. | Mostrar al usuario. Recovery: `DELETE /instances` y re-crear, o debug vía logs. |
| `init_db.status: "success"` pero `db_present: false` | Raro (DB borrada externamente tras éxito). | Re-lanzar `POST /init-db`. |

Ejemplo del shape de `nkr_status.init_db` cuando hay un job completado:
```json
"init_db": {
  "status": "success",
  "db_name": "db-odoo-v17-cliente-42",
  "admin_login": "admin",
  "started_at": 1745538330,
  "finished_at": 1745538368,
  "elapsed_ms": 38214,
  "odoo_response_code": 200
}
```

#### Ejemplo E2E
```bash
# 1. Arrancar init-db (retorna inmediato)
curl -s -X POST -H "$TOK" -H "Content-Type: application/json" \
  -d '{"admin_login":"admin","admin_password":"C1i3ntePassword!","lang":"es_PE","country_code":"PE"}' \
  $NKR_API_BASE/api/v1/cells/odoo-v17/instances/$NAME/init-db
# → 202 { "status":"accepted", ... }

# 2. Poll hasta que db_present=true o init_db.status=failed
while true; do
  RESP=$(curl -s -H "$TOK" "$NKR_API_BASE/api/v1/cells/odoo-v17/instances/$NAME")
  DB=$(echo "$RESP" | python3 -c "import json,sys;print(json.load(sys.stdin)['nkr_status'].get('db_present'))")
  INIT_STATUS=$(echo "$RESP" | python3 -c "import json,sys;d=json.load(sys.stdin)['nkr_status'].get('init_db',{});print(d.get('status','none'))")
  echo "db_present=$DB init_db=$INIT_STATUS"
  [ "$DB" = "True" ] && echo "✅ ready" && break
  [ "$INIT_STATUS" = "failed" ] && echo "❌ init-db failed" && exit 1
  sleep 3
done
```

**Errores de validación (síncronos, sin spawn):**
- `400 invalid_admin_login` / `invalid_admin_password` / `invalid_db_name` / `invalid_nkr_name`.
- `404 instance_not_found`.
- `409 instance_not_running` — VM apagada, llamar `POST /actions {start}` primero.
- `503 odoo_not_ready_yet` — VM up pero Odoo :8069 no responde. Retriable (`retry_after_s`).

### 4.14 `DELETE /api/v1/cells/{cell}/instances/{nkr_name}/dns` — Quitar DNS del tenant

Remueve el vhost nginx. Opcionalmente borra el cert Let's Encrypt.

Query params:
- `delete_cert=1` → ejecuta `certbot delete --cert-name <dns>` además de quitar el vhost. Default `0` (sólo vhost).

```bash
# Sólo vhost (cert queda, útil si vas a re-provisionar en otro tenant)
curl -s -X DELETE -H "$TOK" \
  $NKR_API_BASE/api/v1/cells/odoo-v17/instances/odoo-v17-cliente-42/dns
# → 200 { "deleted":true, "dns":"cliente-42.systemouts.com", "cert_deleted":false }

# Quitar todo incluyendo cert
curl -s -X DELETE -H "$TOK" \
  "$NKR_API_BASE/api/v1/cells/odoo-v17/instances/odoo-v17-cliente-42/dns?delete_cert=1"
```

**Idempotente:** si el vhost no existe, devuelve 200 con `deleted:true, dns:null`.

**Nota:** `DELETE /api/v1/cells/.../instances/<name>` (4.7) **no** dispara automáticamente este cleanup de DNS. El panel debe llamar explícitamente a `DELETE /dns` antes (o después) del delete de instancia, según su flujo. Razón: un tenant puede cambiar su DNS sin re-crearse, y el vhost puede sobrevivir re-creations del tenant apuntando siempre a la misma IP interna.

### 4.15 `PATCH /api/v1/cells/{cell}/instances/{nkr_name}/config` — Ajustar workers (sizing) y SMTP

Upsert selectivo. El panel manda **`workers`** y/o keys de SMTP; NKR aplica los cambios atómicamente sobre las capas correspondientes:

- **`workers`** → re-deriva `ram` + `chrs` + `balloon_mb` (siempre 0 en producción) en `nkr-compose.yml` Y `workers` + `limit_memory_soft` + `limit_memory_hard` en `odoo.conf`. **Single input → outputs coherentes** (ver tabla en §4.4: `ram = max(1024, 512 + 768·W)`, `chrs = 2W+1`, `soft = 640·W` MB, `hard = 768·W` MB). Solo aplica a `tier=production` — en staging/dev `PATCH /config` con `workers`/`chrs` devuelve `409 sizing_locked_for_tier` (perfil fijo). Requiere `POST /actions {restart}` para que el cambio de `ram`/`chrs` (capa KVM) tome efecto; el de `workers`/`limit_memory_*` (capa Odoo) también.
- **SMTP** → UPSERT en la tabla `ir_mail_server` de la DB del tenant **(esto es lo que aparece en la UI de Odoo "Settings → Technical → Email → Outgoing Mail Servers" y lo que el motor de mail.thread realmente usa al enviar)**. Adicionalmente escribe los `smtp_*` en `odoo.conf` como fallback (Odoo cae a esas keys sólo si no hay registros en `ir.mail_server` — caso edge en boots fríos).

Sólo se tocan las capas correspondientes a los campos enviados; el resto se preserva.

**Body (todos opcionales, mandar sólo lo que se quiere cambiar):**

| Campo | Tipo | Capa | Requiere restart |
|---|---|---|---|
| `workers` | int `1..=16` | compose + odoo.conf | **Sí** |
| `smtp_server` | string `[A-Za-z0-9._\-:]{1,253}` | ir.mail_server + odoo.conf | No — efecto inmediato |
| `smtp_port` | u16 | ir.mail_server + odoo.conf | No |
| `smtp_user` | string ≤ 256 chars, no CR/LF/NUL | ir.mail_server + odoo.conf | No |
| `smtp_password` | string ≤ 512 chars, no CR/LF/NUL | ir.mail_server + odoo.conf | No |
| `smtp_encryption` | `"none"` \| `"ssl"` \| `"starttls"` | ir.mail_server | No |
| `smtp_ssl` (legacy) | bool | odoo.conf + map a `smtp_encryption` | No |
| `email_from` | string ≤ 256 chars, no CR/LF/NUL | ir.mail_server (`from_filter`) + odoo.conf | No |
| `restart` | bool | — | controla auto-restart |

**`smtp_encryption` vs `smtp_ssl`:** `smtp_encryption` es el campo nuevo y preciso (mapea 1:1 con el modelo Odoo). `smtp_ssl` se mantiene por compat: si `smtp_encryption` no se pasa, NKR deriva — `smtp_ssl=true` → `"ssl"` (típicamente port 465); `smtp_ssl=false` + port 587 → `"starttls"`; otherwise `"none"`. Recomendado pasar `smtp_encryption` directo.

**SMTP no requiere restart:** Odoo lee `ir.mail_server` en cada `mail.mail.send()`, así que el cambio es inmediato. El default `restart` queda `false` cuando sólo se tocan campos SMTP.

**Workers SÍ requiere restart:** cambia ram/chrs/balloon_mb del proceso `nkr run` y workers/memory del proceso Odoo.

**Respuesta 200:**
```json
{
  "nkr_name": "odoo-v17-cliente-42",
  "applied": ["smtp_server", "smtp_port", "smtp_user", "smtp_password", "email_from", "ir.mail_server"],
  "restart_required": false,
  "restarted": false,
  "mail_server_update": "updated"
}
```

`mail_server_update` es el resultado del UPSERT en la tabla `ir_mail_server`:
- `updated` — registro `name='NKR-managed'` upsertado correctamente.
- `not_requested` — el panel no envió ningún campo SMTP en este PATCH.
- `skipped_db_missing` — la DB del tenant no existe todavía (init-db no llamó). El panel debe re-llamar el PATCH después de `/init-db`.
- `failed_nonblocking` — error inesperado al hacer el UPSERT; los `smtp_*` quedaron en `odoo.conf` pero la UI no los va a mostrar. Revisar `journalctl -u nkr.service`.

**Campos NO editables** (inmutables post-clone): `dbfilter`, `db_name`, `list_db`, `proxy_mode`, `admin_passwd`, `db_host`, `db_port`, `db_user`, `db_password`. Tampoco `ram_mb`, `balloon_mb` ni `limit_memory_*` directos — esos sólo se cambian via `workers` (que re-deriva los tres). Si necesitás un balloon distinto del default (ej. tenant que normalmente carga PDFs grandes y querés desactivarlo), eso se setea **al crear** el tenant via `balloon_mb` en el body de `POST /instances`; post-creación, la única forma de cambiarlo es editar el yaml manualmente y restart.

```bash
# Cambiar de 2 a 4 workers (re-deriva ram=4096, chrs=9, balloon_mb=840, soft=1.6GB, hard=3GB)
curl -s -X PATCH -H "$TOK" -H "Content-Type: application/json" \
  -d '{"workers":4}' \
  $NKR_API_BASE/api/v1/cells/odoo-v17/instances/odoo-v17-cliente-42/config

# Configurar SMTP — Google Workspace (port 587 + STARTTLS)
curl -s -X PATCH -H "$TOK" -H "Content-Type: application/json" \
  -d '{
    "smtp_server":"smtp.gmail.com",
    "smtp_port":587,
    "smtp_encryption":"starttls",
    "smtp_user":"no-reply@cliente.com",
    "smtp_password":"app-specific-password-16-chars",
    "email_from":"no-reply@cliente.com"
  }' \
  $NKR_API_BASE/api/v1/cells/odoo-v17/instances/odoo-v17-cliente-42/config
# → 200 { applied:[...,"ir.mail_server"], mail_server_update:"updated", restart_required:false }
# El registro aparece inmediatamente en Odoo UI: Settings → Technical → Email → Outgoing Mail Servers

# SendGrid / Mailgun (port 587 + STARTTLS, user='apikey', pass=API key)
curl -s -X PATCH -H "$TOK" -H "Content-Type: application/json" \
  -d '{
    "smtp_server":"smtp.sendgrid.net",
    "smtp_port":587,
    "smtp_encryption":"starttls",
    "smtp_user":"apikey",
    "smtp_password":"SG.xxxxxxxxxxxxxxxxxxxxxx",
    "email_from":"no-reply@cliente.com"
  }' \
  $NKR_API_BASE/api/v1/cells/odoo-v17/instances/odoo-v17-cliente-42/config

# SMTP saliente con SSL puro (port 465)
curl -s -X PATCH -H "$TOK" -H "Content-Type: application/json" \
  -d '{
    "smtp_server":"mail.cliente.com","smtp_port":465,"smtp_encryption":"ssl",
    "smtp_user":"odoo@cliente.com","smtp_password":"...",
    "email_from":"odoo@cliente.com"
  }' \
  $NKR_API_BASE/api/v1/cells/odoo-v17/instances/odoo-v17-cliente-42/config

# Cambiar workers + SMTP en una sola llamada
curl -s -X PATCH -H "$TOK" -H "Content-Type: application/json" \
  -d '{"workers":2,"smtp_server":"smtp.sendgrid.net","smtp_port":587,"smtp_encryption":"starttls"}' \
  $NKR_API_BASE/api/v1/cells/odoo-v17/instances/odoo-v17-cliente-42/config
```

**Errores:**
- `400 no_fields` — body sin campos aplicables.
- `400 invalid_workers` — fuera del rango `1..=16`.
- `400 invalid_smtp_*` / `invalid_email_from` — ver tabla.
- `400 invalid_smtp_encryption` — valor distinto a `none|ssl|starttls`.
- `404 instance_not_found` — el tenant no existe.
- `500 patch_failed` — falla escritura de `odoo.conf` (permisos, disco lleno).
- `500 patch_compose_failed` — falla al reescribir el bloque del compose.

### 4.15.1 Comportamiento del registro `ir.mail_server`

NKR mantiene **un solo registro** con `name = 'NKR-managed'` por tenant. Cada PATCH con campos SMTP hace `DELETE` + `INSERT` de ese registro (atómico). Detalles:

- **Único:** si el cliente final crea otros registros desde la UI de Odoo (ej. para tener varios servidores con prioridades), NKR no los toca — sólo gestiona el `NKR-managed`.
- **Prioridad:** Odoo elige entre múltiples `ir.mail_server` por `sequence` (asc) y `from_filter`. NKR usa `sequence=10`, `active=true`. Si el cliente quiere que SU servidor tenga prioridad sobre el `NKR-managed`, debe darle `sequence < 10` o desactivar el `NKR-managed`.
- **Password en plaintext:** la columna `smtp_pass` de `ir_mail_server` es texto plano (no hay encryption nativa en Odoo OSS). El password viaja por TLS desde el panel y queda en la DB del tenant en plaintext — quien tenga acceso a la DB puede leerlo. Esto es comportamiento estándar Odoo, no NKR-específico.
- **Borrado:** el panel no tiene endpoint dedicado para borrar el `NKR-managed` registro. Si quiere quitarlo, puede hacer `POST /psql` con `UPDATE ir_mail_server SET active=false WHERE name='NKR-managed';`.

### 4.16 `POST /api/v1/cells/{cell}/instances/{nkr_name}/psql` — Ejecutar SQL contra la DB del tenant

Abre un `psql` contra la DB del tenant (`db-<nkr_name>` via el PG de la cell) y devuelve el output como CSV. Pensado para debug/recovery sin SSH: consultas puntuales, reportes, reparaciones one-shot.

**Body:**
```json
{
  "query": "SELECT id, name FROM res_partner LIMIT 5;",
  "max_rows": 1000
}
```

| Campo | Tipo | Default | Máximo |
|---|---|---|---|
| `query` | string | — | 16 KiB |
| `max_rows` | int | 1000 | 10000 |

**Filtros de seguridad (rechazo `400`):**
- Body > 16 KiB → `query_too_large`.
- `\c` o `\connect` (cambio de DB) → `meta_connect_forbidden`.
- `\!` (shell escape) → `shell_escape_forbidden`.
- `COPY ... PROGRAM` (exec arbitrario) → `copy_program_forbidden`.
- `DROP DATABASE` / `CREATE DATABASE` → `database_ddl_forbidden`.
- Null bytes → `null_byte`.

**Enforcement PG-side:**
- `statement_timeout = '30s'` y `idle_in_transaction_session_timeout = '30s'` inyectados antes de la query.
- `ON_ERROR_STOP=1` — primer error aborta el batch.
- Fixed `-d db-<nkr_name>` — el panel **no** puede saltar a otra DB aunque escriba `\c`.
- Audit log obligatorio en `/var/log/nkr-psql-audit.log`: `<timestamp>\t<nkr_name>\t<db>\t<query trunc 1 KiB>`.

**Respuesta 200:**
```json
{
  "nkr_name": "odoo-v17-cliente-42",
  "db_name": "db-odoo-v17-cliente-42",
  "exit_code": 0,
  "rows_returned": 3,
  "truncated": false,
  "csv": "id,name\n1,Admin\n2,Public User\n3,Antonio\n"
}
```

Primera línea del `csv` siempre es header. `truncated:true` si `rows_returned == max_rows` y había más (el CSV está cortado).

**Respuesta 400 (query inválida para PG):**
```json
{
  "error": "psql_error",
  "nkr_name": "...",
  "db_name": "...",
  "exit_code": 1,
  "stderr": "ERROR:  column \"foo\" does not exist..."
}
```

**Respuesta 504:** `psql_timeout` (la query excedió 35s total).

```bash
# Query simple
curl -s -X POST -H "$TOK" -H "Content-Type: application/json" \
  -d '{"query":"SELECT COUNT(*) FROM res_users;","max_rows":1}' \
  $NKR_API_BASE/api/v1/cells/odoo-v17/instances/odoo-v17-cliente-42/psql

# Recovery: borrar cron-jobs zombies
curl -s -X POST -H "$TOK" -H "Content-Type: application/json" \
  -d '{"query":"DELETE FROM ir_cron WHERE active=false AND nextcall < NOW() - INTERVAL '1 month';"}' \
  $NKR_API_BASE/api/v1/cells/odoo-v17/instances/odoo-v17-cliente-42/psql

# Leer configuración del tenant desde ir_config_parameter
curl -s -X POST -H "$TOK" -H "Content-Type: application/json" \
  -d '{"query":"SELECT key,value FROM ir_config_parameter ORDER BY key;","max_rows":500}' \
  $NKR_API_BASE/api/v1/cells/odoo-v17/instances/odoo-v17-cliente-42/psql
```

**Importante para el panel:**
- El endpoint asume que el panel YA es administrador del tenant — no es un escape de privilegios (el panel ya tiene `admin_passwd` para el DB manager de Odoo). Es un canal alternativo más directo para ops internas.
- Toda query queda en el audit log del host. Si el panel necesita mutar datos, preferir siempre el ORM de Odoo (via `/jsonrpc` con el `admin_passwd`) para no bypasear triggers/restricciones. `psql` es para **recovery/debug**, no para operación normal.
- No hay soporte de transacciones cross-request — cada call es una sesión nueva.

### 4.17 `POST /api/v1/cells/{cell}/instances/{nkr_name}/modules/{op}` — Install / upgrade / uninstall módulos Odoo

Equivalente HTTP de `odoo -i <mod>`, `odoo -u <mod>`, `odoo --uninstall <mod>`. Internamente NKR abre una sesión contra el Odoo del tenant (JSON-RPC a `:8069`) con las credenciales del `admin` user, busca los `ir.module.module` por nombre, y ejecuta `button_immediate_install` / `button_immediate_upgrade` / `button_immediate_uninstall` sobre los IDs. Bloquea hasta que Odoo termine (pueden ser varios minutos para módulos con muchas dependencias).

Valores válidos de `{op}`: `install` | `upgrade` | `uninstall`.

**Body:**
```json
{
  "modules": ["sale", "purchase", "account"],
  "admin_login": "admin",
  "admin_password": "<mismo password que se pasó a /init-db>"
}
```

| Campo | Tipo | Validación |
|---|---|---|
| `modules` | array<string> | 1..=64 items; cada uno `[A-Za-z0-9_]{1,64}` |
| `admin_login` | string | ≤ 128 chars, sin CR/LF/NUL |
| `admin_password` | string | 4..512 chars, sin CR/LF/NUL |

**Respuesta 200 (ok):**
```json
{
  "nkr_name": "odoo-v17-cliente-42",
  "op": "install",
  "modules": ["sale", "purchase", "account"],
  "elapsed_ms": 48213,
  "status": "ok"
}
```

**Respuesta 404 `modules_not_found`:**
```json
{
  "error": "modules_not_found",
  "missing": ["foo_custom"],
  "hint": "module must exist in the database (addons_path + update apps list)"
}
```

Si falta un módulo tipicamente es porque:
1. El repo con ese addon no está clonado en el tenant (→ `POST /addons/git` primero).
2. El módulo existe en disco pero Odoo no lo conoce (la DB no tiene la entrada `ir.module.module` para él) → hacer **upgrade** del módulo `base` primero (lo cual re-scanea `addons_path`).

**Respuesta 401 `odoo_auth_rejected`:** credenciales incorrectas, o la DB no existe / no fue inicializada aún.

**Respuesta 502 `odoo_not_ready`:** VM apagada o `:8069` no responde.

**Respuesta 500 `odoo_install_failed` / `odoo_upgrade_failed` / `odoo_uninstall_failed`:** Odoo rechazó la operación (conflicto de dependencias, error de migración, etc.). El campo `detail` contiene el error RPC crudo.

**Ejemplos:**
```bash
# Instalar varios módulos de un shot (típico onboarding de cliente)
curl -s -X POST -H "$TOK" -H "Content-Type: application/json" \
  -d '{"modules":["sale_management","purchase","account","stock"],
       "admin_login":"admin","admin_password":"cliente-password"}' \
  $NKR_API_BASE/api/v1/cells/$CELL/instances/$NAME/modules/install

# Upgrade de módulo custom tras push al repo addons
curl -s -X POST -H "$TOK" -H "Content-Type: application/json" \
  -d '{"modules":["custom_module"],"admin_login":"admin","admin_password":"..."}' \
  $NKR_API_BASE/api/v1/cells/$CELL/instances/$NAME/modules/upgrade

# Desinstalar
curl -s -X POST -H "$TOK" -H "Content-Type: application/json" \
  -d '{"modules":["website_sale"],"admin_login":"admin","admin_password":"..."}' \
  $NKR_API_BASE/api/v1/cells/$CELL/instances/$NAME/modules/uninstall
```

**Tips para el panel:**
- **Re-scaneo de addons:** tras llamar a `POST /addons/git` para traer nuevos módulos, el panel debe hacer `POST /modules/upgrade` con `modules:["base"]` para que Odoo relea `addons_path` y registre los nuevos `ir.module.module`. Sin este paso, intentar instalarlos devuelve `modules_not_found`.
- **Ops ORM ad-hoc** (crear/leer/escribir records): el panel puede hablar directamente con `https://<tenant-dns>/jsonrpc` usando su sesión autenticada. NKR no tiene que mediar para CRUD estándar. Este endpoint es específico para operaciones con **efectos estructurales** (install/upgrade/uninstall, que corren en transacciones largas).
- **Timeout:** wall-clock 600 s. Instalaciones muy grandes (migraciones + datos demo) pueden exceder — si pasa, la VM sigue instalando; el panel hace poll de `GET /logs?from_offset=...` para ver el progreso y `GET /instances/{name}` para ver cuando `port_8069_up` vuelve estable.

### 4.17.1 `POST /api/v1/cells/{cell}/instances/{nkr_name}/reload` — Reload de workers Odoo (sin reiniciar VM)

**Disponible desde v1.6.2.** Recicla los workers HTTP de Odoo del tenant **sin reiniciar la VM ni el master**, garantizando que el código nuevo en disco se cargue. ~3 segundos, **sin downtime de la VM**.

**Por qué existe** (background técnico):

`dev_mode=reload` de Odoo (que arrastra `watchdog` Python con backend **inotify** del kernel) es **incompatible con NKR por dos razones**:

1. **virtio-fs NO propaga eventos inotify del host al guest** (limitación fundamental del protocolo FUSE). El watcher en el guest está vivo pero nunca recibe eventos cuando NKR escribe archivos desde el host vía `POST /addons/git`. Polling alternativo (`WATCHDOG_FORCE_POLLING`) cuesta ~5% CPU/tenant constante → rompe la densidad (100 tenants × 5% = 5 cores quemados en idle).
2. **`watchdog` agota `fs.inotify.max_user_watches` del kernel guest** (default 8192) al recursar sobre `/usr/lib/python3/dist-packages/odoo/addons` durante el bootstrap → `OSError [Errno 28] inotify watch limit reached` → Odoo muere rc=1 → supervisor loop respawnea infinito → puerto 8069 nunca levanta → health-check 504 al panel. Ver `BUG_inotify_dev_mode.md`.

**Por estas razones NKR ya no setea `dev_mode=reload`** (desde v1.6.2 — opción A del bug postmortem).

**v1.6.3 — además NKR quitó `dev_mode=qweb,xml`** (= ahora `dev_mode=` vacío para todos los tiers). Razón: `qweb,xml` activa el watcher interno de Odoo que **recompila templates desde XML en CADA request**, incluyendo los keepalive de nginx cada 30s. En Odoo 19 el core trae templates con directivas `t-esc` deprecated → cada recompile loguea ~9 warnings (1080/h) + presión CPU/memoria/GC. Empíricamente correlaciona con cuelgues periódicos del proceso `nkr` host-side en busy loop (33% CPU, threads virtio muertos). Quitándolo, la frecuencia de cuelgues baja ~3× y desaparece el spam.

Para iteración rápida en DEV: `POST /addons/git` (auto_reload=true) ó `POST /reload` siguen siendo los mecanismos canónicos vía REL_OD/HVC0 — el supervisor loop respawnea Odoo con código fresh sin tocar el host. No se pierde nada respecto a `dev_mode=qweb,xml`.

Soluciones consideradas y descartadas:
- `WATCHDOG_FORCE_POLLING`: ~5% CPU constante por tenant — rompe el modelo de densidad de NKR (100 tenants × 5% = 5 cores quemados constantes en idle).
- Polling manual desde el host: complejidad sin beneficio claro.

**Solución: trigger explícito** desde el host vía cadena de signals/canales, sin polling.

**Mecanismo end-to-end (CLAUDE.md HVC0 Protocol v2.2):**

```
1. Panel/NKR → POST /reload  o  addons/git con auto_reload=true (default)
2. nkr-api-server → IPC ReloadWorkers → daemon nkr
3. daemon → kill(SIGUSR1, vm_pid)              [host signal]
4. vmm.rs SIGUSR1 handler → setea RELOAD_REQUESTED flag
5. vcpu loop ve la flag → console_dev.try_inject(b"REL_OD\n")  [virtio-console]
6. Guest /dev/hvc0 watcher (initramfs) lee "REL_OD"
7. Watcher detecta el modo leyendo workers = N de odoo.conf:
   ┌─ workers=0 (threaded, DEV/STAG):
   │  pkill -TERM -f /usr/bin/odoo → proceso muere
   │  → nkr-start.sh supervisor loop relanza con código fresh
   │
   └─ workers>0 (prefork, PROD):
      pkill -HUP -f /usr/bin/odoo → master kill_workers + respawn
      Master sobrevive (preserva estado en memoria)
8. ~2-3s después, código nuevo sirviendo
```

**Idempotente**: múltiples reloads rápidos colapsan en uno solo (la flag es one-shot, los reloads son secuenciales).

**Diferencias por modo:**
- En **threaded** (workers=0), el reload mata el proceso entero — el supervisor loop de `nkr-start.sh` (`while true; do exec odoo; sleep 1; done`) lo respawnea inmediato. Estado en memoria se pierde (no hay master).
- En **prefork** (workers>0), el master Odoo NUNCA se reinicia — solo los workers. Master mantiene caches, conexiones IAP, etc.

> ✅ **Arreglado (v1.6.4):** el watcher hvc0 del initramfs ahora detecta el modo (`workers = N` del `odoo.conf` del guest) y manda `pkill -TERM` cuando es threaded (workers=0) / `pkill -HUP` cuando es prefork. En workers=0 el supervisor loop de `nkr-start.sh` respawnea limpio. Verificado 2026-05-12 en `intech-devp` (workers=0): `POST /reload` → `:8069` vuelve en ~3s, sin necesidad del workaround `POST /actions {restart}`. (Para que un tenant ya corriendo recoja el fix: un `POST /actions {restart}` regenera su initramfs con la versión nueva.)

**Body**: vacío.

**Respuesta 202:**
```json
{
  "nkr_name": "company_client-cliente-42",
  "status": "accepted",
  "mechanism": "SIGUSR1 → vmm → hvc0 REL_OD → guest reload Odoo (SIGHUP master si prefork, SIGTERM+respawn si threaded)",
  "estimated_seconds": 3,
  "note": "Odoo recarga con código fresh del disco. Sin downtime de la VM."
}
```

**Errores:**
- `400 invalid_nkr_name` — nkr_name no matchea charset.
- `404 instance_not_found` — instancia no existe.
- `409 not_running` — la instancia está apagada (start primero).
- `409 pid_unknown` — instancia en transición (mid-create / mid-start), reintentar.
- `500 signal_failed` — `kill()` falló. Daemon log tiene detalle.

**Auto-trigger desde `POST /addons/git`:**

Por **default**, todo `POST /addons/git` exitoso dispara el reload automáticamente. La response incluye `reloaded: true`:

```json
{
  "module_count": 3,
  "modules": ["web_responsive", ...],
  "added": [], "updated": ["web_responsive"], "unchanged": [...],
  "reloaded": true,
  "reload_skipped_reason": null
}
```

Si querés cobranza manual (panel decide cuándo recargar), pasá `auto_reload: false` en el body del addons/git:
```json
{
  "repo_url": "...",
  "ref": "main",
  "auto_reload": false
}
```
La response indica `"reloaded": false, "reload_skipped_reason": "auto_reload=false"`. Después podés llamar `POST /reload` cuando quieras.

**Cuándo usarlo manualmente** (vía POST /reload directo):
- Editaste un `.py` por SSH al filesystem del host (sin pasar por addons/git)
- "Forzar Recarga" como botón en el panel para casos donde sospechás workers stale
- Tras un `POST /modules/upgrade` en `tier=production` con `workers≥2` para garantizar que TODOS los workers ven el código nuevo (no solo el que procesó el upgrade)

**Cuándo NO sirve:**
- Cambios de `odoo.conf` (workers, ram, dev_mode, etc.) → requiere `POST /actions {restart}` (full VM restart, ~13-25s) porque el master tampoco re-lee el conf.
- Cambios de pip libs en `pylibs/` → requiere restart full (Python re-import path).
- Cambios de la VM kernel / initramfs → requiere `compose down/up` de la cell.

**Comparativa final** (todas las opciones para "actualizar código Python en un tenant Odoo corriendo"):

| Estrategia | Tiempo | Downtime VM | Downtime master Odoo | Refresca TODOS los workers |
|------------|--------|-------------|----------------------|----------------------------|
| `POST /reload` (NUEVO) | **~3 s** | NO | NO | ✅ |
| `POST /modules/upgrade` (multi-worker) | 10-15 s | NO | NO | ❌ (solo el worker upgrader) |
| `POST /actions {restart}` (tier=dev) | ~13 s | sí (~10s) | sí | ✅ |
| `POST /actions {restart}` (tier=production) | ~20-30 s | sí (~25s) | sí | ✅ |
| Editar archivo + esperar inotify | nunca | NO | NO | ❌ (virtio-fs limita inotify) |

**`POST /reload` es la opción óptima** para cambios de código Python en cualquier tier.

---

### 4.17.2 `POST /api/v1/cells/{cell}/instances/{nkr_name}/balloon` — Ballooning IDLE/ACTIVE

Marca la VM como **ACTIVE** en el ballooning dinámico (CLAUDE.md v2.2). El estado IDLE no se setea explícitamente — se aplica automáticamente tras `balloon_decay_secs` (600s default) sin renovación.

**Cuándo usarlo:**
- Llamar al recibir actividad de usuario (login, click en menu, request a /web/...) en tenants `tier=staging` / `tier=dev`.
- Idempotente: múltiples calls renuevan el TS sin re-aplicar `config_change` (el state machine en vmm dedupe por `BALLOON_LAST_APPLIED_STATE`).
- En `tier=production` no tiene efecto runtime (la VM tiene balloon estático = 0 siempre, doctrine: "PROD evita latencia de desinflado en picos de tráfico"). NKR devuelve 202 igual para que el panel no necesite saber el tier.

**Mecánica (HVC0-less por decisión):**
```
panel POST /balloon → daemon → SIGUSR2 → vmm
                                        ↓
                  BALLOON_ACTIVE_REQUESTED_TS = now()
                                        ↓
                       vcpu loop poll (cada ≤5s vía SIGALRM)
                                        ↓
              balloon_dev.set_target_mb(active_mb) + raise IRQ config_change
                                        ↓
                     guest balloon driver desinfla a active_mb
```

Tras `balloon_decay_secs` sin nuevo SIGUSR2 (= sin POST /balloon), el state machine vuelve automáticamente a IDLE: `set_target_mb(idle_mb) + IRQ`. El guest infla.

**Tiempos:**
- IDLE → ACTIVE: ~1-2s (deflate del balloon es rápido).
- ACTIVE → IDLE: ~2-5s (inflate sincrono con sync de páginas del guest).

**Body** (opcional):
```json
{ "state": "active" }
```

Si el body es vacío, se asume `active`. `state: "idle"` se rechaza con `400 explicit_idle_not_supported` — IDLE es decay automático, no es seteable.

**Respuesta 202 (balloon dinámico aplicado):**
```json
{
  "nkr_name": "odoo-v17-cliente-42",
  "status": "accepted",
  "mechanism": "SIGUSR2 → vmm BALLOON_ACTIVE_TS=now → set_target_mb(active) + IRQ config_change",
  "note": "Renueva el TS active. Tras balloon_decay_secs sin nueva señal, decae a IDLE."
}
```

**Respuesta 202 (no aplicado — VM con balloon estático o legacy):**
```json
{
  "nkr_name": "odoo-v17-cliente-1",
  "status": "accepted",
  "applied": false,
  "reason": "vm_static_balloon_or_legacy",
  "note": "La VM no tiene ballooning dinámico configurado (tier=production o lanzada antes del upgrade del daemon a 1.6.2). SIGUSR2 no enviado para evitar terminate por handler ausente. Restart la VM para activar ballooning dinámico si su tier (dev/staging) lo justifica."
}
```

NKR aplica un **safety check** antes de mandar SIGUSR2: lee `/proc/<pid>/cmdline` y verifica que contenga `--balloon-active-mb`. Si no (= la VM se lanzó con un binario `nkr` previo a 1.6.2, o tier=production que no escribe ese flag), NKR omite la señal y devuelve 202 con `applied: false`. Esto evita matar VMs heredadas (la disposición default de SIGUSR2 sin handler es **terminate**). Para activar el ballooning dinámico en una VM heredada con tier=dev/staging, basta con `POST /actions {restart}` después del upgrade del daemon — el restart la relanza con los flags correctos.

**Errores:**
- `400 invalid_nkr_name`
- `400 invalid_state` — body con `state` distinto de `"active"`.
- `400 explicit_idle_not_supported` — body con `state: "idle"`.
- `404 instance_not_found`
- `409 not_running` — la VM está apagada.
- `409 pid_unknown` — VM en provisioning.
- `500 signal_failed` — el `kill SIGUSR2` falló (PID muerto, perm).

**Patrón de uso recomendado del panel:**
```js
// On every authenticated request to the tenant (or every login event):
fetch(`${NKR}/api/v1/cells/${cell}/instances/${name}/balloon`, {
  method: 'POST',
  headers: { 'Authorization': `Bearer ${TOKEN}` },
  // body opcional — { state: 'active' } o vacío
}).catch(() => {});  // fire-and-forget; los errores 5xx no bloquean al usuario
```

Sin esa renovación, el tenant cae a IDLE tras 10 min y el primer request post-IDLE paga ~3-5s extra de deflate. Con renovación cada N min < 600s, el balloon se queda ACTIVE de forma permanente mientras haya tráfico.

**Verificación:**
- Logs del proceso vmm: `[NKR-BALLOON] IDLE→ACTIVE (target=N MB)` y `[NKR-BALLOON] ACTIVE→IDLE por decay (target=M MB)`.
- Compose YAML del tenant: campo `balloon_active_mb` (si != `balloon_mb` → dinámico activado). Default según tier (ver §7.0).

**Cuándo NO llamar:**
- Tenant ya parado (devuelve 409).
- Tenant tier=production: idempotente pero no aporta — la VM está estática en balloon=0.

---

### 4.18 `POST /api/v1/cells/{cell}/instances/{nkr_name}/sso` — SSO firmado HMAC (auto-login sin password)

NKR firma una URL one-shot que permite al panel hacer auto-login en cualquier user del tenant **sin conocer su password**. Verificada por el módulo Odoo `nkr_sso` con HMAC-SHA256 constant-time compare. **El password del user nunca entra al flujo.**

Spec completa del módulo Odoo + cookbook para devs: ver `nkr_sso.md`.

**Body:**
```json
{ "user": "admin" }    // opcional, default "admin". Cualquier login activo en res.users.
```

**Respuesta 200:**
```json
{
  "url": "https://intech-devp.oa-odoo.com/nkr-sso?u=admin&exp=1778472389&sig=b0f7efbea...",
  "user": "admin",
  "expires_in": 30,
  "nkr_name": "odoo-v19-intech-devp",
  "dns": "intech-devp.oa-odoo.com"
}
```

**Mecánica:**
1. NKR lee la HMAC key del `odoo.conf` del tenant — sección `[nkr_sso]` clave `secret` (escrita por `cell.rs::rewrite_odoo_conf_full` al clone — random 256 bits, único por tenant); legacy: clave `nkr_sso_secret` en `[options]` (tenants de v1.6.3, sigue funcionando como fallback).
2. Computa `sig = HMAC-SHA256(secret, "<user>|<exp>")`, donde `exp = now() + 30s`.
3. Devuelve URL para que el panel haga `window.open(url, '_blank')`.
4. Módulo Odoo `/nkr-sso` verifica `sig` + `exp >= now()` + busca user activo → crea sesión sudo + redirect 303 a `/odoo`.

**Errores:**
- `400 invalid_nkr_name` — name fuera de `[A-Za-z0-9._-]{1,64}`.
- `400 invalid_user` — user fuera de `[A-Za-z0-9._\-@]{1,128}`.
- `404 instance_not_found`.
- `409 not_running` — tenant apagado.
- `409 no_dns_provisioned` — `POST /dns` no llamado todavía (sin DNS no hay URL).
- `500 sso_secret_missing` — falta la HMAC key en `odoo.conf` (`[nkr_sso] secret = ...` o legacy `nkr_sso_secret` en `[options]`). Aplicable sólo a tenants de NKR ≥1.6.3 — setear manual + restart.
- `500 hmac_key_invalid` — secret malformado.

**Ejemplo curl:**
```bash
curl -s -X POST -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"user":"admin"}' \
  http://nkr-host:9090/api/v1/cells/odoo-v19/instances/odoo-v19-intech-devp/sso
# → { "url": "https://...", "expires_in": 30, ... }
```

**Para que funcione end-to-end:**
1. Tenant tiene la HMAC key en `<instance>/config/odoo.conf` (`[nkr_sso] secret = <64 hex>`) — escrito automáticamente por NKR ≥1.6.3 al crear/reescribir conf.
2. Módulo Odoo `nkr_sso` instalado en la DB del tenant. **Estrategia canónica:** vive en `cells/<cell>/systemouts-addons/nkr_sso/` (dir RO cell-level, montado en cada instancia como `/mnt/systemouts-addons`, invisible al cliente — `POST /addons/git` no lo toca) + pre-instalado en `<cell>-odoo-template` para que los clones lo hereden vía `CREATE DATABASE … TEMPLATE` (DB) + el share RO (código). Ver §11.4. (En v19 ya está hecho. Para v17, pendiente.)

**Patrón panel (UI selector "Entrar como [user▼]"):**
```js
// Lista users via psql, dropdown con admin first
const rows = await psql("SELECT login, name FROM res_users WHERE active=true ORDER BY (login='admin') DESC, login;");
// Al click "Entrar →":
const { url } = await fetch(`${NKR}/api/v1/cells/${cell}/instances/${name}/sso`,
  { method: 'POST', body: JSON.stringify({ user: selectedLogin }) }).then(r => r.json());
window.open(url, '_blank');
```

---

### 4.19 `GET|POST /api/v1/cells/{cell}/instances/{nkr_name}/diag` — Diagnóstico HOST-side

Captura un dump multi-sección de los threads del proceso `nkr` del tenant en el host (stacks, wchan, scheduling, CPU usage). Idempotente, ~50ms. Útil **antes** que el watchdog dispare un restart automático para preservar evidencia forense del cuelgue.

Acepta GET (lectura pura de `/proc`) y POST (consistencia con los otros endpoints de actions).

**Body:** vacío.

**Respuesta 200** — `Content-Type: text/plain`:
```
=== NKR DIAG odoo-v19-intech-devp pid=642248 @ 2026-05-11T06:00:02 ===

[/proc/<pid>/status]
State: S (sleeping)
Threads: 3
VmRSS: 524288 kB
...

[/proc/<pid>/task/<tid>/stack]    ← por cada thread
[<0>] do_select+0x...
[<0>] core_sys_select+0x...
[<0>] __x64_sys_pselect6+0x...

[/proc/<pid>/task/<tid>/wchan]
do_select

[/proc/<pid>/task/<tid>/sched]
...
```

**Errores:**
- `400 invalid_nkr_name`.
- `404 instance_not_found`.
- `409 not_running` — no hay PID del que samplear.
- `500 diag_failed` — no se pudo leer `/proc` (proceso murió entre el lookup y el sample).

**Uso típico — script externo de diag previo a restart:**
```bash
# Capturar antes de cualquier acción correctiva
curl -s -H "Authorization: Bearer $TOKEN" \
  "$NKR/api/v1/cells/odoo-v19/instances/odoo-v19-intech-devp/diag" \
  > /tmp/nkr-diag/intech-devp-$(date +%s).txt

# Luego restart manual / o dejar al watchdog
```

**Interacción con el watchdog:** el watchdog (sección §7.2) dispara restart automático cuando `:8069` está down ≥60s. Si querés capturar diag antes del restart auto, hacé el GET dentro de la ventana 30-60s tras el primer down.

---

### 4.20 `POST /api/v1/admin/cache/purge` — Vaciar cache nginx (global)

Borra todas las entries del cache server-side de nginx (`/var/cache/nginx/nkr_static/*`). Operación **global** — afecta a todos los tenants de todas las cells. Reconstrucción es orgánica: la próxima request a un asset cacheado va a Odoo y vuelve a poblar la entry.

**Cuándo usarlo**:
- Tras `POST /addons/git` que toca archivos en `/web/static/*` (logos, imágenes, fonts custom). Esos paths son URLs **fijas**, así que el cache server-side los serviría stale por hasta 24 h sin invalidar. Sin purge, el cliente no ve el cambio.
- **NO es necesario** para cambios en `/web/assets/<hash>/*` (bundles compilados): el `<hash>` cambia automáticamente cuando el contenido cambia, así que la nueva URL es cache MISS limpia y la entrada vieja queda huérfana hasta que `inactive=24h` la purga sola.

**`nginx -s reload` NO purga el cache** — solo recarga config. Las entradas del cache son archivos en disco que sobreviven reloads, restarts y reboots. Este endpoint es la única forma desde el API de invalidarlas.

**Body**: vacío (no hay parámetros).

**Respuesta 200:**
```json
{
  "purged": 142,
  "size_bytes_freed": 8388608
}
```

Si hubo errores parciales (algún archivo no se pudo borrar):
```json
{
  "purged": 138,
  "size_bytes_freed": 8127456,
  "errors": ["/var/cache/nginx/nkr_static/abc/...: Permission denied"]
}
```
con status 207 (Multi-Status).

**Ejemplo curl:**
```bash
curl -s -X POST -H "Authorization: Bearer $TOKEN" \
  http://nkr-host:9090/api/v1/admin/cache/purge
```

**Granularidad**: hoy es global (todos los tenants). Granularidad per-tenant requiere `ngx_cache_purge` (módulo third-party con recompilación de nginx) o iterar archivos parseando headers — no implementado. La purga global es barata: la reconstrucción es <100ms por asset cuando alguien lo vuelve a pedir.

---

## 5. Layout por instancia

Cada instancia vive en:
```
/mnt/nkr/cells/<cell>/instances/<nkr_name>/
├── addons/         ← repos Git (virtiofs → /mnt/extra-addons del guest)
├── pylibs/
│   ├── requirements.txt
│   └── lib/        ← pip install --target (virtiofs → /mnt/extra-pylibs del guest)
├── config/
│   └── odoo.conf
├── filestore/      ← ir.attachment + archivos físicos
├── logs/
│   └── odoo.log
├── odoo.ext4       ← rootfs de la instancia
└── meta.json       ← metadata persistida
```

El `odoo.conf` base trae:
```ini
addons_path = /usr/lib/python3/dist-packages/odoo/addons,/mnt/extra-addons,/mnt/extra-enterprise
```

Y el initramfs exporta:
```sh
export PYTHONPATH=/mnt/extra-pylibs:$PYTHONPATH
```

**Todo el contenido de `addons/`, `pylibs/` y `/mnt/nkr/enterprise/<ver>/` lo administra el panel vía los endpoints 4.10 / 4.11 / 4.12.** El panel NO toca el filesystem directamente.

---

## 6. Flujos típicos (panel ↔ NKR)

### 6.1 Onboarding de un cliente nuevo (panel remoto, todo HTTPS)

```
(a) panel → GET /api/v1/cells
    ↓ evalúa free_slots y versiones disponibles; decide cell

(b) panel → POST /api/v1/enterprise/git   (una vez por cell, sólo si Enterprise)
    body: { repo_url:"git@github.com:odoo/enterprise.git", ref, deploy_key_b64 }
    ← 200 { sha, path }
    Nota: idempotente — si ya está clonado, hace pull. Volver a llamarlo no rompe
    los tenants existentes.

(c) panel → POST /api/v1/instances
    body: { nkr_name, mode:"production", odoo_version, workers:2,
            admin_passwd:<panel genera y guarda ANTES de llamar> }
    ← 202 { nkr_name, cell, poll, status:"accepted" }  (ASÍNCRONO — v1.6.4+)
    panel → poll GET {poll} (= GET /instances/{name}/create-status) cada 3-5s
            hasta status=ready (o status=failed → leer error/message/hint).
    Luego GET /instances/{name} para el InstanceInfo completo
    (admin_passwd NO se devuelve — el panel ya lo tiene).
    Nota: running=false — el clone NO auto-arranca la VM (phase=provisioning)
          salvo que hayas mandado admin_user_password.
    REGLAS DE mode:
      - mode="production" → cliente nuevo. NKR clona archivos + DB del
        template <cell>-odoo-template (DB con base+web preinstalado).
        Boot ~30-60s. Panel NO debe mandar 'source'.
      - mode="dev"        → clone de tenant existente para staging/debug.
        Panel manda 'source' obligatorio (nkr_name del tenant a clonar).

(d) panel → POST .../addons/git   (×N repos del cliente, VM apagada)
    body: { repo_url, ref, deploy_key_b64? }
    ← 200 { sha, path }

(e) panel → PUT .../pylibs  (si requirements.txt no vacío, VM apagada)
    body: { requirements_txt }
    ← 200 { installed, lib_dir }

(f) panel → POST .../actions {action:"start"}  (bootea la VM por primera vez)
    ← 202
    IMPORTANTE: "start" (no "restart") en el primer boot. El clone deja la
    VM preparada pero no booteada — el panel decide cuándo arrancar para
    que los pasos (d)/(e) se apliquen desde cero sin restart redundante.

(g) panel crea A record DNS en su proveedor:
    <dns_cliente> → 116.202.240.179  (IP del host NKR)

(h) panel → POST .../dns  body: {"dns":"<dns_cliente>"}
    NKR emite cert Let's Encrypt, genera vhost nginx, reload.
    ← 200 { https_url, cert_path, ... }

(i) panel poll GET /api/v1/cells/{cell}/instances/{nkr_name}
    nkr_status.phase transiciona: provisioning → booting → loading
    (esperar hasta phase="loading" para crear la DB)

(j) panel → POST .../init-db   (ASÍNCRONO — devuelve 202 inmediato)
    body: { admin_login:"admin", admin_password:<panel-genera>,
            lang:"es_PE", country_code:"PE" }
    ← 202 { status:"accepted", ... }   // job arrancó en background

(j.1) panel poll GET /api/v1/cells/{cell}/instances/{nkr_name} cada 3-5s:
      - nkr_status.db_present=true    → ✅ éxito, phase=ready
      - nkr_status.init_db.status="failed" → ❌ ver init_db.error
      - nkr_status.init_db.status="running" → seguir esperando (30-90s típico)

(k) panel registra webhook GitHub → endpoint DEL PANEL:
    https://stdout.systemouts.com/webhooks/github/{instance_id}
    con secret HMAC compartido

(l) tenant disponible para el cliente final en https://<dns_cliente>
    Login: admin_login / admin_password.
```

### 6.2 Webhook de GitHub (panel remoto → NKR vía HTTPS)

```
(a) GitHub → POST https://panel.ejemplo.com/webhooks/github/abc123
    header: X-Hub-Signature-256: sha256=...

(b) panel valida HMAC con el secret compartido

(c) panel resuelve: webhook abc123 → (nkr_host, cell, nkr_name, repo, deploy_key)

(d) panel → POST https://{nkr_host}/api/v1/cells/{cell}/instances/{name}/addons/git
    body: { repo_url, subdir, ref, action:"sync", deploy_key_b64 }
    ← 200 { sha:"newsha..." }

(e) panel → POST .../actions {action:"restart"}
    ← 202

(f) panel poll GET .../instances/... hasta port_8069_up

(g) panel guarda "last_deployed_sha" en su DB
```

El panel nunca hace `git pull` ni `ssh`. Todo por los endpoints HTTPS del proxy.

**Para `odoo -u <module>` (upgrade con migraciones de schema):** no disponible en Fase 1. El restart recarga código existente pero no corre migraciones. Endpoint `POST /instances/{name}/modules/update` pendiente (requiere canal de comandos guest vía hvc0 — 2-3 días de trabajo).

### 6.3 Desaprovisionar un cliente

```
(a) panel → DELETE .../instances/{nkr_name}/dns?delete_cert=1
    ← 200 (síncrono: vhost nginx + cert Let's Encrypt removidos en ~1-2 s)

(b) panel → DELETE .../instances/{nkr_name}?drop_db=1
    ← 202 deleted=pending, async=true                        (instancia existía)
        — o —
    ← 200 deleted=true, already_deleted=true                 (ya estaba borrada)

(c) panel polea GET .../instances/{nkr_name} cada 1 s, hasta 180 s
    ← 404 not_found  → delete completo, panel marca tenant deleted en su UI

(d) panel elimina el webhook GitHub correspondiente

(e) panel elimina el A record DNS en su proveedor
```

**Orden importante:** DNS delete primero (mientras la VM todavía responde `:8069` por si certbot necesita validar algo) → luego instance delete. En la práctica certbot `delete` no requiere validación, así que podés hacerlo al revés, pero mantener este orden evita estados inconsistentes donde el vhost apunta a un guest_ip reciclado por NKR para otro tenant.

**Por qué el delete pasó a async (v1.5.2):** un delete típico tarda 60–90 s (SIGTERM graceful + DROP DATABASE + remove dir). El cliente HTTP del panel cortaba por timeout antes de los 60 s y mostraba 500 al operador, dejando la instancia "fantasma" en la UI a pesar de que NKR ya la había borrado correctamente. Con la respuesta 202 inmediata + polling a 404, el panel UI se mantiene consistente con el estado real del backend.

### 6.4 Backup antes de delete (preservando DB)

```
(a) panel → DELETE ...?drop_db=0
    ← DB queda en PG intacta (la VM + instance_dir se eliminan)

(b) panel hace pg_dump desde host → S3/bucket

(c) panel ejecuta DROP DATABASE manualmente después del backup
```

---

## 7. Deployment (referencia para setup inicial)

En el servidor NKR:

```bash
# Daemon root (UDS)
sudo systemctl enable --now nkr.service

# Proxy HTTP unprivileged — NKR_API_TOKEN en /etc/nkr/api.env
sudo systemctl enable --now nkr-api-server.service

# Verificar
curl -s http://127.0.0.1:9090/api/v1/health
# → {"ok":true,"version":"1.6.4"}
```

**Config panel-side:**
```
NKR_API_BASE = https://nkr.cliente.com     (nginx al frente → 127.0.0.1:9090)
NKR_API_TOKEN = <token compartido>
```

---

## 7.0 Tiers de instancia (`production` / `staging` / `dev`)

NKR diferencia 3 tiers para que **dev iteración no pague el costo del rate-limit + cache + multi-worker upgrade gotcha** que tiene production. El tier se manda en el body de `POST /instances` (campo opcional `tier`, default `production`).

### Tabla comparativa

| Aspecto | `production` (default) | `staging` | `dev` |
|---------|------------------------|-----------|-------|
| **Workers** | configurable 1..=16 (default 2; **prefork**) | **forzado a 0** (threaded) | **forzado a 0** (threaded) |
| **RAM guest** | `max(1024, 512 + W·768)` MB (default **2048** con W=2; W=1→1280, W=4→3584) | **1024 MB** (perfil fijo) | **1300 MB** (perfil fijo) |
| **chrs (CPU quota)** | `2W+1`: 5 (W=2), 9 (W=4). Override `chrs` en POST/PATCH (rango 1..=50). | **5 (perfil fijo, NO override)** | **5 (perfil fijo, NO override)** |
| **`limit_memory_soft / hard`** | `640·W / 768·W` MB (per worker) | **600 / 700 MB** | **800 / 1000 MB** |
| **Master Reserve (Odoo prefork)** | 256 MB (incluido en la fórmula de RAM) | 0 MB (no hay master en threaded) | 0 MB (no hay master en threaded) |
| **Balloon boot / ACTIVE** (con tráfico) | **0 MB** (= IDLE → estático, sin decay) | **256 MB** (deja 768 al guest al boot) | **0 MB** (toda la RAM al guest al boot) |
| **Balloon IDLE** (post-decay, sin tráfico) | **0 MB** (siempre ACTIVE — ver §4.17.2) | **768 MB** (squeeze a 256 guest) | **256 MB** (squeeze — deja 1044 al guest) |
| **Decay ACTIVE→IDLE** | n/a (no transition) | 600s sin renovación | 600s sin renovación |
| **`limit_time_cpu/real`** | 60s / 120s (Odoo default) | 600s / 1200s (debugger-friendly) | 600s / 1200s |
| **`dev_mode` en odoo.conf** | (vacío) | **(vacío — v1.6.3)** | **(vacío — v1.6.3)** |
| **`log_level`** | info | info (no se cambia — debug genera demasiado ruido) | info (idem) |
| **`list_db`** | False | True | True |
| **Rate-limit nginx en /web/login** | 5 burst + 3r/s | desactivado | desactivado |
| **Cache nginx /web/static y /web/assets** | 24h / 30d | desactivado | desactivado |
| **Cloneable como `source`?** | sí | no | **no** (instancias dev son standalone) |
| **Requiere `source` en POST /instances** | opcional (default = template de cell) | **REQUERIDO** (debe ser tier=production) | **PROHIBIDO** (rejected con 409) |
| **DB inicial** | template_DB de la cell (vacío bootstrap) | clone de la DB del source production | template_DB de la cell |
| **Reflejo de cambio de código Python** | requiere full restart de VM (~25s post-v1.6.1) | `POST /addons/git` (auto_reload) ó `POST /reload` → REL_OD vía HVC0 → ~3 s | idem staging |

### REL_OD vía HVC0 + supervisor loop — el game changer

Cuando tier es staging o dev, NKR escribe en `odoo.conf`:
```ini
dev_mode =
workers = 0
```

**`dev_mode` vacío** (cambio v1.6.3): la doctrina previa tenía `dev_mode=qweb,xml` para "hot-reload de templates sin restart", pero observaciones en producción mostraron que **activa recompilación constante de QWeb/XML en CADA request** (incluyendo health checks de nginx cada 30s) → spam de warnings deprecated del core de Odoo 19 + presión CPU/memoria/GC + correlación con cuelgues del proceso nkr host-side. Sin `dev_mode`, las templates se compilan una vez al boot y se cachean — comportamiento prod normal. Para iteración rápida, el flujo es:

```
git push (modules) → webhook → panel → POST /addons/git (auto_reload=true)
                                       ↓
                                  NKR clone + REL_OD vía HVC0
                                       ↓
                                  guest pkill -TERM odoo → supervisor respawn
                                       ↓
                                  ~3s después: código nuevo vivo
```

El supervisor del initramfs respawnea Odoo con el código fresh desde disco → mismo efecto que `dev_mode=reload` original pero **sin watchdog/inotify** y **sin recompile constante**. `qweb,xml` se elimina porque su valor era marginal frente al costo runtime.

Historia: v1.6.2 quitó `reload` (`qweb,xml` quedaba). v1.6.3 quitó también `qweb,xml` → **`dev_mode` queda vacío en todos los tiers**. Ver `BUG_inotify_dev_mode.md` (postmortem del bug de inotify que motivó quitar `reload`).

`workers=0` (threaded mode) es **obligatorio** en dev/staging por mandato de CLAUDE.md. En threaded mode no hay master prefork — un solo proceso werkzeug multi-thread.

**Supervisor loop en `nkr-start.sh`** (initramfs v1.6.2+): Odoo corre dentro de `while true; do exec odoo; sleep 1; done`. Si el proceso muere por REL_OD, el loop lo respawnea instantáneamente con el código fresh del disco. **OJO**: el loop también respawnea cuando Odoo muere por causas patológicas (OOM, ENOSPC inotify, panic) — si ves al panel reportar "504 / tenant no responde :8069", chequeá `journalctl + nkr-compose.log` para diferenciar respawn legítimo (REL_OD) de respawn loop bug.

**Mecánica de reload (CLAUDE.md HVC0 Protocol):**

| Modo | Mecanismo | Tiempo |
|------|-----------|--------|
| **threaded (workers=0)** — DEV/STAG | NKR → SIGUSR1 → vmm → hvc0 `REL_OD` → guest `pkill -TERM odoo` → muere → supervisor loop respawnea | ~2s |
| **prefork (workers>0)** — PROD | NKR → SIGUSR1 → vmm → hvc0 `REL_OD` → guest `pkill -HUP odoo` → master kill_workers + respawn (master vive) | ~2s |

El watcher hvc0 detecta el modo automáticamente leyendo `workers = N` del `odoo.conf` mounted del guest. Sin master en threaded → SIGTERM + respawn por loop. Con master en prefork → SIGHUP + respawn interno del master.

Para tu workflow de iteración:
```
git push (modules) → webhook → panel → POST /addons/git (auto_reload=true)
                                       ↓
                                  NKR clone + inject REL_OD vía hvc0
                                       ↓
                                  guest dispatch (threaded o prefork)
                                       ↓
                                  ~2s después: código nuevo vivo
```

**Sin restart, sin upgrade** (excepto para schema changes — Apps → Upgrade en UI).

### Reglas de bootstrap

**`tier=production`** (comportamiento actual, sin cambios):
- `mode=production` + sin `source` → clona del `<cell>-odoo-template`
- `mode=dev` + `source` → clona de ese tenant

**`tier=staging`**:
- `source` REQUERIDO. Debe apuntar a un tenant tier=production existente.
- NKR clona DB completa (igual que `mode=dev` legacy) pero aplica config dev.
- Si `source` apunta a un tenant tier=staging o tier=dev → 409 `source_must_be_production`.

**`tier=dev`**:
- `source` PROHIBIDO. Si se manda → 409 `source_not_allowed_in_dev`.
- NKR usa `<cell>-odoo-template` como fuente (DB del template).
- Para desarrollo de módulos nuevos desde cero. El dev hidrata módulos desde Apps.

### Ejemplos curl

**Crear staging** (clon de un cliente production para reproducir bug):
```bash
curl -X POST -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
  http://nkr-api.cliente.com/api/v1/instances \
  -d '{
    "nkr_name": "cliente-staging-bug42",
    "tier": "staging",
    "source": "company_client-cliente-prod",
    "odoo_version": "17.0",
    "dns": "cliente-staging-bug42.dev.tudominio.com",
    "admin_passwd": "Adm.16chars.MinPwd-..."
  }'
```

**Crear dev** (módulo nuevo en sandbox limpio):
```bash
curl -X POST -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
  http://nkr-api.cliente.com/api/v1/instances \
  -d '{
    "nkr_name": "mi-nuevo-modulo-dev",
    "tier": "dev",
    "odoo_version": "17.0",
    "dns": "mi-nuevo-modulo.dev.tudominio.com",
    "admin_passwd": "Adm.16chars.MinPwd-..."
  }'
```

### Errores específicos

| HTTP | error | Cuándo |
|------|-------|--------|
| 400 | `source_required` | tier=staging sin `source` |
| 400 | `invalid_workers` | workers fuera del rango 1..=16 (solo aplica a tier=production; dev/staging fuerzan workers=0) |
| 400 | `ram_insufficient_for_workers` | (reservado, no se usa actualmente) override de `ram_mb` que viola la fórmula `VM_RAM ≥ 256 + 256 + W·768`. Hoy `ram_mb` no es overridable por API; el daemon siempre lo deriva. |
| 409 | `source_not_allowed_in_dev` | tier=dev con `source` (instancias dev no clonables) |
| 409 | `source_must_be_production` | tier=staging con `source` apuntando a otro staging/dev |
| 409 | `sizing_locked_for_tier` | PATCH /config con workers/chrs sobre tier=dev/staging (perfil fijo) |

### Migración de tenants existentes

Tenants creados antes de v1.6.1 (sin campo `tier` en su `meta.json`) se interpretan como **`tier=production`** automáticamente (back-compat). El daemon escribe `tier: "production"` en sucesivas mutaciones del meta.

Si querés cambiar el tier de un tenant existente: NO hay endpoint `PATCH /tier` todavía (planeado para sesión futura). Workaround temporal: editar `meta.json` a mano y `POST /actions {restart}`.

---

## 7.1 Edge dual: Cloudflare proxied + direct (failover sin downtime)

NKR soporta **dos modos de edge** simultáneamente, con switch instantáneo desde el panel sin tocar el host. La idea: estar cubierto si Cloudflare se cae sin perder la protección que da CF cuando está disponible.

### Modos

| Modo | Tráfico | Protección | Dependencia |
|------|---------|-----------|-------------|
| **`cloudflare` (proxied / 🟧)** | client → CF anycast → host → guest Odoo | WAF + DDoS + bot mgmt + CDN + rate-limit global de CF + nginx local | Salud de CF |
| **`direct` (DNS-only / 🟦)** | client → host → guest Odoo | Solo nginx local (hardening 444 + rate-limit /web/login) | Cero |

El switch entre modos lo hace el panel **flipando el flag `proxied` del DNS record en Cloudflare** (vía CF API). Propagación: 1-5 min. NKR no requiere reload de nginx ni cambios runtime — la config soporta ambos modos a la vez.

### Cómo nginx soporta ambos modos transparentemente

NKR vhost incluye `/etc/nginx/snippets/nkr-real-ip.conf` que declara los rangos IPv4/IPv6 oficiales de Cloudflare (lista pública en `https://www.cloudflare.com/ips-v4/` y `/ips-v6/`):

```nginx
set_real_ip_from 173.245.48.0/20;
set_real_ip_from 103.21.244.0/22;
... (15 rangos IPv4 + 7 IPv6)
real_ip_header CF-Connecting-IP;
real_ip_recursive on;
```

Comportamiento:
- **Si traffic viene de un IP de CF** → nginx **reescribe** `$remote_addr` al valor del header `CF-Connecting-IP` (la IP real del cliente final). El rate-limit y access log usan automáticamente la IP correcta.
- **Si traffic NO viene de CF** (modo direct) → `$remote_addr` ya es la IP real del cliente. Mismo rate-limit, mismo log, sin diferencias.

Resultado: **el rate-limit `nkr_login_limit` y los `add_header X-Real-IP $remote_addr` funcionan idénticos en ambos modos sin reconfigurar nada**. El panel solo flipea CF.

### Responsabilidad del panel

El panel guarda credenciales de Cloudflare (API token + `zone_id` por dominio raíz) y expone un toggle por instancia (o global por dominio):

```
[ Edge Mode for aintech.oa-odoo.com ]
  ( ) 🟧 Cloudflare proxied (recomendado por default)
  ( ) 🟦 Direct (fallback si CF tiene incidentes)
```

Cuando el operador clickea, el panel hace:
```bash
# Flipear a CF proxied
curl -X PATCH -H "Authorization: Bearer $CF_API_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"proxied": true}' \
  https://api.cloudflare.com/client/v4/zones/$ZONE_ID/dns_records/$RECORD_ID

# Flipear a direct
curl -X PATCH -H "Authorization: Bearer $CF_API_TOKEN" \
  ... -d '{"proxied": false}'
```

**NKR no participa en este call** — Cloudflare API es directo desde el panel. Esto evita que credenciales CF estén en el host NKR (defense in depth).

### SSL/TLS

- **Modo direct**: el cert Let's Encrypt del host sirve directamente al cliente (renovación vía `/.well-known/acme-challenge/` que ya existe en el vhost).
- **Modo CF proxied**: CF termina TLS al cliente. CF→origin debe usar **"Full (strict)"** en CF SSL/TLS settings. El cert LE del host valida correctamente porque CF resuelve por SNI igual que un browser.
- **Renovación LE en modo proxied**: `certbot` necesita acceso al `/.well-known/acme-challenge/` del host. Con CF en proxied + cache, hay que excluir el path del cache (CF lo hace por default para `*.acme-challenge*`). Si falla la renovación: flipear a direct temporalmente, renovar, volver a proxied.

### Auto-failover (opcional, fuera de NKR)

Si querés failover automático cuando CF se cae:
1. Panel hace health-check periódico contra CF (ej. `https://www.cloudflare.com/cdn-cgi/trace`).
2. Si N fallos consecutivos → flipea TODOS los DNS records de tus dominios a `proxied: false`.
3. Cuando CF vuelve → flipea de vuelta.

**Riesgo**: false positives. Si el panel detecta mal "CF caído" cuando solo hubo glitch local, flipea a direct innecesariamente. **Recomendación: empezar con switch manual** y agregar auto-failover solo si la frecuencia de incidentes CF lo justifica.

### Limitación conocida — IPs de CF

NKR mantiene la lista de rangos CF en el snippet `nkr-real-ip.conf` (15 IPv4 + 7 IPv6 al momento de escribir). Cloudflare actualiza estos rangos raramente (~1 cambio/año), pero si agregan un rango nuevo, hay que regenerar el snippet. Comando manual para refresh:

```bash
# Re-generar la lista canónica (script idempotente)
{
  echo "# Cloudflare IPv4 — regenerar con: curl https://www.cloudflare.com/ips-v4"
  curl -s https://www.cloudflare.com/ips-v4 | sed 's/^/set_real_ip_from /; s/$/;/'
  echo "# Cloudflare IPv6"
  curl -s https://www.cloudflare.com/ips-v6 | sed 's/^/set_real_ip_from /; s/$/;/'
  echo "real_ip_header CF-Connecting-IP;"
  echo "real_ip_recursive on;"
} > /etc/nginx/snippets/nkr-real-ip.conf
nginx -t && systemctl reload nginx
```

Si NKR detecta que un IP fuera de la lista CF está enviando `CF-Connecting-IP` headers (potencial spoof), nginx **ignora** el header y mantiene `$remote_addr` como el IP del conector (correcto, defensivo).

---

## 7.2 Watchdog automático — auto-restart al cumplir 60s sin :8069

> **Estado actual: DESHABILITADO** — el deploy tiene `Environment=NKR_WATCHDOG_DISABLED=1` en `/etc/systemd/system/nkr.service` (a pedido, mientras el panel pushea cambios activamente y los auto-restart interferían). Con el watchdog off, un cuelgue de tenant **no se auto-recupera** — queda para el operador/panel. Para re-habilitarlo: borrar esa línea del unit + `systemctl daemon-reload && systemctl restart nkr`. El resto de esta sección describe cómo funciona cuando está habilitado (default del template del repo).

Desde v1.6.3, el daemon `nkr` arranca un thread (`src/watchdog.rs`) que sondea cada **15s** el puerto TCP 8069 de cada tenant `running`. Cuando un tenant lleva **`HUNG_THRESHOLD_SECS=60`** consecutivos sin responder, NKR dispara `action=restart` automáticamente vía `api::handle_action`. El panel no tiene que hacer nada.

**Propósito:** cubrir cuelgues residuales (workers Odoo D-state, kernel paths con `do_select` bloqueado, etc.) sin necesidad de operador. Pre-watchdog, requería operador para dispatch manual.

**Mecánica:**
1. Lista cada 15s las VMs running de todas las cells.
2. Probe TCP open + close a `10.0.<cell>.<vm_id>:8069` con timeout 2s.
3. OK → resetea counter. Falla → incrementa counter del tenant.
4. Si counter ≥ 4 (60s acumulados) → log `[NKR-WATCHDOG] <name> colgado 60s+ sin :8069 — disparando restart automático` y `POST /actions {action:"restart"}` async.
5. Tras restart, espera `HEALTH_GRACE_SECS=30` antes de re-probar (evita ciclos rapid-restart).

**Bypass / disable** (para debugging o tests):
```bash
# Antes de arrancar el daemon
NKR_WATCHDOG_DISABLED=1 nkr serve
```
O en el unit file `nkr.service`:
```ini
Environment=NKR_WATCHDOG_DISABLED=1
```

**Observabilidad:** journalctl del daemon:
```
[NKR-WATCHDOG] odoo-v19-intech-devp :8069 down hace 0s (threshold 60s)
[NKR-WATCHDOG] odoo-v19-intech-devp colgado 68s sin :8069 — disparando restart automático
[NKR-WATCHDOG] odoo-v19-intech-devp restart dispatched (HTTP 202)
[NKR-WATCHDOG] odoo-v19-intech-devp :8069 recuperado
```

**Captura diag previa al restart**: si querés evidencia forense del estado colgado **antes** que el watchdog dispare restart, hacé `GET /diag` (§4.19) durante la ventana 30-60s tras el primer "down". El watchdog mismo no captura diag automáticamente — eso queda como tarea de scripts externos / observabilidad del panel.

**Costo:** una probe TCP por running tenant cada 15s. ~100 µs CPU + 1 packet round-trip. A escala 100 tenants/host = 100 packets cada 15s, completamente negligible.

**Diseñado para correr 24/7** — es la red de seguridad de toda la flota.

---

## 8. Rate limits, timeouts y comportamientos defensivos

### 8.1 Timeouts recomendados (panel-side HTTP client)

**Crítico:** el panel debe configurar timeouts **por endpoint**. Un solo default (ej. 30 s) **no** sirve — varias ops tardan más por diseño (filesystem clone, install de Odoo, git clone de enterprise). Tabla:

| Endpoint | Duración típica | Timeout mínimo recomendado | Notas |
|---|---|---|---|
| `GET /metrics` | <100 ms | 5 s | Prometheus, todas las VMs. NO incluye disco per-VM (ver §4.1). |
| `GET /instances/{name}/metrics` | <50 ms (cache hit) / ~1ms recompute, hasta ~2-5 s sólo en cache miss del `du` de un filestore grande | 10 s | JSON per-instancia para la pestaña Métricas del panel. Cacheado ~2s/VM (el `du` ~5min aparte). Pollear cada ~2s. Ver §4.1.1. |
| `GET /api/v1/health` | <10 ms | 5 s | |
| `GET /api/v1/cells` | <100 ms | 5 s | |
| `GET /instances/{name}` | 200-800 ms | 5 s | Incluye psql probe + TCP probe + `/proc/<pid>` read. Durante un create async devuelve 404 hasta los primeros ~1-2s. |
| `POST /instances` | <500 ms (async) | 10 s | **Asíncrono desde v1.6.4** — valida sync (4xx al toque) y devuelve 202 inmediato; el clone+boot (30-200s; PROD prefork más lento) corre en background. El panel poll `GET /instances/{name}/create-status` hasta `status=ready|failed`. Ver §4.4 / §4.4.1. |
| `GET /instances/{name}/create-status` | <50 ms | 5 s | Lee el status file del create async. Poll cada 3-5s. |
| `POST /actions {start,stop,restart}` | <50 ms (async) | 5 s | **Async desde v1.5.1** — devuelve 202 inmediato; el trabajo real (5–35 s) corre en background. El panel polea `nkr_status.port_8069_up`/`phase`. Ver §4.8. |
| `GET /logs` (tail) | <500 ms | 10 s | Bounded 4 MiB. |
| `GET /logs` (long-poll, `wait_ms=5000`) | hasta `wait_ms` + ~500 ms | **`wait_ms + 10000`** | Sumar al `wait_ms`. |
| `GET /logs/download` | hasta 5 s | **30 s** | Cap 64 MiB; SSD local. |
| **`POST /init-db`** | **<500 ms** (async) | 10 s | **Asíncrono** — devuelve 202 inmediato; el panel poll `GET /instances/{name}`. |
| `POST /modules/{install,upgrade,uninstall}` | 10-300 s (síncrono) | **600 s** | Bloquea hasta completar la transacción de Odoo. |
| `POST /reload` | <50 ms (async, ~3s en el guest) | 5 s | REL_OD vía HVC0 — reload de workers Odoo sin reiniciar la VM. Idempotente. Ver §4.17.1. |
| `POST /balloon` | <50 ms (async) | 5 s | Marca la VM ACTIVE en el ballooning dinámico (renueva el TS). Ver §4.17.2. |
| `POST /sso` | 200-800 ms | 5 s | Pre-auth interno + firma HMAC. Devuelve URL TTL 30s. Ver §4.18. |
| `GET\|POST /diag` | ~50-200 ms | 5 s | Captura stacks/wchan/cpu host-side de los threads del proc nkr. Idempotente. Ver §4.19. |
| `POST /psql` | <30 s | **35 s** | Statement timeout PG-side = 30 s. |
| `PATCH /config` (sin restart) | <200 ms | 10 s | Solo reescribe `odoo.conf`. |
| `PATCH /config` (con restart) | 15-60 s | **90 s** | Incluye graceful stop + start. |
| `POST /addons/git` | 5-180 s | **200 s** | Git clone depth=1 sobre Internet. Timeout hard-cap 180 s server-side. |
| `POST /enterprise/git` | 10-180 s | **200 s** | Repo grande (~400 MB sobre la red). |
| `PUT /pylibs` | 30-300 s | **360 s** | pip install + compile. |
| `POST /dns` | 30-90 s | **120 s** | Certbot challenge + emit + nginx reload. |
| `DELETE /dns` | 5-30 s | **60 s** | nginx reload + optional certbot delete. |
| `DELETE /instances` | 5-30 s | **60 s** | Stop VM + drop DB + fs cleanup. |

**Patrón recomendado:** usar HTTP client con per-request timeout override. Ejemplo Python:
```python
# Default 10s, override por endpoint
r = httpx.post(f"{NKR}/instances", json=body, timeout=10.0)  # async! devuelve 202; luego poll create-status
r = httpx.post(f"{NKR}/addons/git", json=body, timeout=200.0)
r = httpx.post(f"{NKR}/init-db", json=body, timeout=10.0)  # async! 10s es MÁS que suficiente
```

### 8.2 Errores transitorios (retriables)

| Código | Error | Acción del panel |
|---|---|---|
| `503 server_busy` | Concurrencia máx. (64 in-flight). | Retry con backoff: 1s, 2s, 4s. |
| `503 odoo_not_ready_yet` | VM up pero Odoo :8069 aún no. | Retry con `retry_after_s` del body. |
| `502 daemon_unreachable` | Proxy no pudo hablar con el daemon. | Retry 1 vez; si persiste, alertar al operador. |
| `409 action_in_progress` | Otro start/stop/restart en vuelo para la misma instancia (ver §4.8). | **No re-disparar** — polear `GET /instances/{name}` hasta que `nkr_status.port_8069_up`/`phase` indique que terminó, y entonces decidir si la nueva acción sigue siendo necesaria. |
| `504` | Timeout interno en NKR. | No retry — problema subyacente (Odoo trabado, disco lleno). |

### 8.3 Defensivos

- **Concurrencia HTTP:** 64 in-flight máx. Más → `503 Retry-After: 1`.
- **Body sizes:** `POST /instances` 64 KiB, `PATCH /config` 16 KiB, `POST /actions` 1 KiB, `POST /psql` 16 KiB, `PUT /pylibs` 64 KiB, `POST /addons/git` 16 KiB.
- **Logs endpoint:** `tail` clamp 1–10000; reader bounded 4 MiB. Download bounded 64 MiB.
- **Timing-safe token compare:** ok hacer retries con el mismo token sin leak de información.
- **Auth:** `/metrics` y `/api/v1/health` son públicos (sin token). Todo lo demás requiere `Authorization: Bearer`.
- **Dev mode:** si el proxy arranca sin `NKR_API_TOKEN`, la auth queda abierta. No usar en producción.

---

## 9. Gobernanza — quién decide qué, qué hacer y qué no

### 9.1 División de responsabilidades panel ↔ NKR

Qué decide cada lado — útil para el panel-Claude como referencia cuando diseña su propia lógica:

| Campo | Lo decide | Racional |
|---|---|---|
| `list_db` | **NKR** (hardcoded `False`) | Seguridad: nunca exponer DB manager en tenants productivos. |
| `proxy_mode` | **NKR** (default `True`) | Todas las cells están detrás de nginx + Cloudflare/LB. Opt-out con `proxy_mode:false` sólo para tests. |
| `db_host` / `db_port` / `db_user` / `db_password` / `db_template` / `db_maxconn` | **NKR** | Infra interna de la cell (PgBouncer). El panel nunca toca estos. |
| `data_dir` / `addons_path` del config / `xmlrpc_port` / `gevent_port` | **NKR** | Paths y puertos fijos dentro del guest (`/var/lib/odoo`, `/mnt/extra-*`, `:8069`/`:8072`). |
| `workers` (sizing) | **Panel** (entero 1..=16) | Single source of truth: NKR deriva `chrs` + `ram` (compose) y `limit_memory_soft`/`hard` (odoo.conf). Default 2 si null. Ver tabla §4.4. |
| `admin_passwd` | **Panel (genera, persiste encriptado, y envía)** | OBLIGATORIO en `POST /instances`. NKR nunca genera ni expone el valor — sólo lo escribe en odoo.conf. Si el panel lo pierde, lee `config_path/odoo.conf` vía SSH. |
| `odoo_version` / `edition` | **Panel** | Derivado del plan/config del cliente. |
| `dns` | **Panel** (informativo en `POST /instances`) | El DNS real lo provisiona `POST /dns` con A record previo en Cloudflare. |
| `db_name` | **NKR devuelve `db-<nkr_name>`** | El panel lo usa pero no lo fija. |
| `source` (clone-from) | **Panel (opcional)** | Cuando se duplica un tenant existente. Default = template de la cell. |

### 9.2 Cosas que el panel NO debe intentar

- **Llamar al UDS directamente.** Sólo `nkr-api-server` tiene el grupo `nkr-api` que da acceso al socket. El panel habla HTTP contra el proxy.
- **Montar `odoo.ext4` o `filestore.ext4` a mano.** Son devices de las VMs; un mount paralelo desde el host puede corromper FS. Si el panel necesita leer el filestore, que use la VM via HTTP Odoo.
- **Editar `odoo.conf` directamente mientras la VM corre.** Usar `PATCH /instances/{name}/config` (workers, memory, SMTP) — NKR reescribe la conf de forma atómica y reinicia la VM si hace falta. El panel no debe tocar el archivo a mano.
- **Escribir en `addons/` sin coordinar con restart.** Cambios en el dir son visibles en el guest instantáneamente (virtiofs), pero Odoo sólo recarga código de módulos al restart.
- **Tocar `/mnt/systemouts-addons` o `cells/<cell>/systemouts-addons/`.** Es un dir de addons **internos de NKR** (módulos como `nkr_sso`) — cell-level, montado RO en cada instancia, e insertado en `addons_path` ANTES de `/mnt/extra-addons` (el `addons/` del cliente) → un módulo del cliente con el mismo nombre que uno interno NO lo puede shadowear. `POST /addons/git` no lo toca. El panel puede VER `/mnt/systemouts-addons` en el `addons_path` de un tenant — eso es normal, lo gestiona el operador NKR, no el panel.
- **Hacer operaciones git dentro del guest.** El guest es minimal (busybox + odoo), sin git. Todas las operaciones de repo corren en el host.

### 9.3 DNS provisioning — Cloudflare + NKR

El panel `stdout.systemouts.com` usa Cloudflare como DNS provider. Flujo recomendado:

1. **Panel crea A record en Cloudflare** vía su API (o NPM/UI manual): `<dns_cliente> → 116.202.240.179`.
2. Esperar propagación (~30s — Cloudflare es rápido).
3. **Panel llama `POST /dns`** — NKR emite cert Let's Encrypt + genera vhost nginx + reload.
4. El vhost nginx en el host NKR sirve TLS y proxy-pass a la IP interna del tenant.

No hace falta que el panel tenga nginx propio. Cloudflare sólo se usa como DNS (A record + opcionalmente orange-cloud proxying/CDN si querés; NKR funciona en ambos modos).

Si en el futuro se migra a NPM (Nginx Proxy Manager) o a otro reverse-proxy gestionado por el panel, el flujo se simplifica aún más: el panel maneja nginx, NKR sólo expone `guest_ip:8069`/`:8072` al panel vía el GET de instancia, y `POST /dns` se vuelve opcional.

---

## 10. Quick reference (copy-paste para testing)

```bash
export NKR_API_BASE=https://nkr-api.systemouts.com
export TOK="Authorization: Bearer $NKR_API_TOKEN"
export CELL=odoo-v17
export NAME=odoo-v17-demo-1   # el nombre real devuelto por POST /instances

# Health (sin auth)
curl -s $NKR_API_BASE/api/v1/health

# Cells disponibles
curl -s -H "$TOK" $NKR_API_BASE/api/v1/cells

# Crear tenant production (ASÍNCRONO — devuelve 202, el clone corre en background)
ADMIN_PW=$(tr -dc 'A-Za-z0-9._-' < /dev/urandom | head -c 48)
curl -s -X POST $NKR_API_BASE/api/v1/instances -H "$TOK" -H "Content-Type: application/json" \
  -d "{\"nkr_name\":\"demo-1\",\"mode\":\"production\",\"odoo_version\":\"17.0\",\
       \"workers\":2,\"admin_passwd\":\"$ADMIN_PW\"}"
# → 202 { nkr_name:"odoo-v17-demo-1", cell:"odoo-v17", poll:"/api/v1/cells/odoo-v17/instances/odoo-v17-demo-1/create-status", ... }
# Source = odoo-v17-odoo-template (auto). NO mandar 'source' en mode=production.

# Poll hasta que el create termine
until curl -s -H "$TOK" "$NKR_API_BASE/api/v1/cells/$CELL/instances/$NAME/create-status" | grep -q '"status":"ready"'; do
  sleep 4; echo "esperando create..."
done

# Crear tenant dev (clone de un tenant productivo para staging) — también async
curl -s -X POST $NKR_API_BASE/api/v1/instances -H "$TOK" -H "Content-Type: application/json" \
  -d "{\"nkr_name\":\"staging-demo-1\",\"mode\":\"dev\",\"odoo_version\":\"17.0\",\
       \"source\":\"odoo-v17-cliente-prod\",\"workers\":2,\"admin_passwd\":\"$ADMIN_PW\"}"
# → 202. NKR clona archivos + DB de cliente-prod via CREATE DATABASE TEMPLATE en background.

# Addons: repo público
curl -s -X POST -H "$TOK" -H "Content-Type: application/json" \
  -d '{"repo_url":"https://github.com/OCA/web.git","ref":"17.0"}' \
  $NKR_API_BASE/api/v1/cells/$CELL/instances/$NAME/addons/git

# Addons: repo privado con deploy key SSH
DK=$(base64 -w0 < ~/keys/deploy.pem)
curl -s -X POST -H "$TOK" -H "Content-Type: application/json" \
  -d "{\"repo_url\":\"git@github.com:owner/private.git\",\"ref\":\"main\",\"deploy_key_b64\":\"$DK\"}" \
  $NKR_API_BASE/api/v1/cells/$CELL/instances/$NAME/addons/git

# Addons: repo privado con GitHub PAT (HTTPS)
curl -s -X POST -H "$TOK" -H "Content-Type: application/json" \
  -d '{"repo_url":"https://github.com/owner/private.git","ref":"main","github_token":"ghp_xxx"}' \
  $NKR_API_BASE/api/v1/cells/$CELL/instances/$NAME/addons/git

# Pylibs
curl -s -X PUT -H "$TOK" -H "Content-Type: application/json" \
  -d '{"requirements_txt":"pandas==2.2.0\nnum2words==0.5.13\n"}' \
  $NKR_API_BASE/api/v1/cells/$CELL/instances/$NAME/pylibs

# Start (primer boot)
curl -s -X POST -H "$TOK" -H "Content-Type: application/json" \
  -d '{"action":"start"}' \
  $NKR_API_BASE/api/v1/cells/$CELL/instances/$NAME/actions

# Poll hasta phase=loading (después del boot, antes de crear DB)
while true; do
  PHASE=$(curl -s -H "$TOK" $NKR_API_BASE/api/v1/cells/$CELL/instances/$NAME \
    | python3 -c "import json,sys;print(json.load(sys.stdin)['nkr_status']['phase'])")
  echo "phase=$PHASE"
  [ "$PHASE" = "loading" ] || [ "$PHASE" = "ready" ] && break
  sleep 3
done

# Crear DB inicial del tenant (ASÍNCRONO — devuelve 202 inmediato)
curl -s -X POST -H "$TOK" -H "Content-Type: application/json" \
  -d '{"admin_login":"admin","admin_password":"cliente-password","lang":"es_PE","country_code":"PE"}' \
  $NKR_API_BASE/api/v1/cells/$CELL/instances/$NAME/init-db
# → 202 { "status":"accepted", ... }

# Poll hasta db_present=true o init_db.status=failed (30-90s típico)
while true; do
  RESP=$(curl -s -H "$TOK" "$NKR_API_BASE/api/v1/cells/$CELL/instances/$NAME")
  DB=$(echo "$RESP" | python3 -c "import json,sys;print(json.load(sys.stdin)['nkr_status'].get('db_present'))")
  ST=$(echo "$RESP" | python3 -c "import json,sys;d=json.load(sys.stdin)['nkr_status'].get('init_db',{});print(d.get('status','none'))")
  [ "$DB" = "True" ] && echo "✅ DB ready" && break
  [ "$ST" = "failed" ] && echo "❌ init-db FAILED"; echo "$RESP" | python3 -m json.tool; break
  echo "  waiting... init_db=$ST"; sleep 3
done

# Provisionar DNS del tenant (A record previo en Cloudflare)
curl -s -X POST -H "$TOK" -H "Content-Type: application/json" \
  -d '{"dns":"demo-1.systemouts.com"}' \
  $NKR_API_BASE/api/v1/cells/$CELL/instances/$NAME/dns

# Instalar módulos Odoo (como si fuese `odoo -i sale,purchase`)
curl -s -X POST -H "$TOK" -H "Content-Type: application/json" \
  -d '{"modules":["sale","purchase","account"],"admin_login":"admin","admin_password":"cliente-password"}' \
  $NKR_API_BASE/api/v1/cells/$CELL/instances/$NAME/modules/install

# Upgrade tras push al repo addons
curl -s -X POST -H "$TOK" -H "Content-Type: application/json" \
  -d '{"modules":["custom_mod"],"admin_login":"admin","admin_password":"..."}' \
  $NKR_API_BASE/api/v1/cells/$CELL/instances/$NAME/modules/upgrade

# Tail logs (snapshot)
curl -s -H "$TOK" "$NKR_API_BASE/api/v1/cells/$CELL/instances/$NAME/logs?tail=50"

# Live-follow (long-poll 5 s desde cursor)
OFFSET=$(curl -s -H "$TOK" "$NKR_API_BASE/api/v1/cells/$CELL/instances/$NAME/logs?tail=1" \
         | python3 -c "import json,sys;print(json.load(sys.stdin)['next_offset'])")
curl -s -H "$TOK" \
  "$NKR_API_BASE/api/v1/cells/$CELL/instances/$NAME/logs?from_offset=$OFFSET&max_lines=500&wait_ms=5000"

# Descargar odoo.log (cap 64 MiB)
curl -s -H "$TOK" -o odoo.log \
  "$NKR_API_BASE/api/v1/cells/$CELL/instances/$NAME/logs/download"

# Restart (después de actualizar addons / pylibs)
curl -s -X POST -H "$TOK" -H "Content-Type: application/json" \
  -d '{"action":"restart"}' \
  $NKR_API_BASE/api/v1/cells/$CELL/instances/$NAME/actions

# Reload de workers Odoo SIN reiniciar la VM (~3s) — tras un addons/git manual
curl -s -X POST -H "$TOK" $NKR_API_BASE/api/v1/cells/$CELL/instances/$NAME/reload

# Métricas de la instancia (JSON snapshot — pestaña Métricas del panel; pollear ~2s)
curl -s -H "$TOK" $NKR_API_BASE/api/v1/cells/$CELL/instances/$NAME/metrics

# SSO: URL firmada HMAC TTL 30s para auto-login (sin password)
curl -s -X POST -H "$TOK" -H "Content-Type: application/json" \
  -d '{"user":"admin"}' \
  $NKR_API_BASE/api/v1/cells/$CELL/instances/$NAME/sso
# → { "url":"https://demo-1.systemouts.com/nkr-sso?u=admin&exp=...&sig=...", "expires_in":30, ... }

# Balloon → ACTIVE (renueva el TS; tras decay_secs sin renovación decae a IDLE)
curl -s -X POST -H "$TOK" -d '{"state":"active"}' \
  $NKR_API_BASE/api/v1/cells/$CELL/instances/$NAME/balloon

# Diag: stacks/wchan/cpu host-side de los threads del proceso nkr (pre-restart forensics)
curl -s -H "$TOK" $NKR_API_BASE/api/v1/cells/$CELL/instances/$NAME/diag

# Cleanup completo
curl -s -X DELETE -H "$TOK" "$NKR_API_BASE/api/v1/cells/$CELL/instances/$NAME/dns?delete_cert=1"
curl -s -X DELETE -H "$TOK" "$NKR_API_BASE/api/v1/cells/$CELL/instances/$NAME?drop_db=1"
```

---

## 11. Setup de deployment (ACLs para los endpoints git/pylibs)

Los endpoints 4.10-4.12 ejecutan `git` y `pip install` desde el proxy (user `nkr-api`). Para que pueda escribir bajo `/mnt/nkr/cells` y `/mnt/nkr/enterprise` sin privilegios extras, se usa un grupo dedicado + POSIX ACL:

```bash
# 1. Grupo de escritura
sudo groupadd -r nkr-addons
sudo usermod -aG nkr-addons nkr-api   # el user del proxy

# 2. ACL recursiva + default (para dirs futuros)
sudo apt-get install -y acl git python3-pip
sudo setfacl -R -m g:nkr-addons:rwx -m d:g:nkr-addons:rwx /mnt/nkr/cells
sudo setfacl -R -m g:nkr-addons:rwx -m d:g:nkr-addons:rwx /mnt/nkr/enterprise

# 3. Reiniciar el proxy para que tome la supplementary group
sudo systemctl restart nkr-api-server

# 4. Verificar
id nkr-api   # debe listar nkr-addons
getfacl /mnt/nkr/cells | grep nkr-addons
```

El systemd unit ya incluye `SupplementaryGroups=nkr-addons` y `ReadWritePaths=/mnt/nkr/cells /mnt/nkr/enterprise` (ver [deploy/systemd/nkr-api-server.service](deploy/systemd/nkr-api-server.service)). Sin este setup, los endpoints 4.10-4.12 devuelven `500 write_failed` / `422 git_clone_failed`.

**Binarios requeridos en el PATH del proxy:** `git`, `pip3`, `ssh`. Instalar en el host con `apt-get install git python3-pip openssh-client`.

**El daemon root NO participa** en estos endpoints — se resuelven 100% en el proxy. Si comprometen el proxy, el blast radius sigue siendo sólo git/pip + los paths ACL-enabled; no hay escalación a KVM/cgroups/iptables.

### 11.1 nginx TLS al frente (exposición pública para el panel remoto)

El proxy `nkr-api-server` escucha en `127.0.0.1:9090` (HTTP plano). Cuando el panel corre en otro servidor, nginx o caddy al frente termina TLS + hace ACL de IPs.

Ejemplo nginx para `nkr-api.systemouts.com` → `127.0.0.1:9090`:

```nginx
# /etc/nginx/sites-enabled/nkr-api
upstream nkr_api { server 127.0.0.1:9090; keepalive 4; }

server {
    listen 443 ssl http2;
    listen [::]:443 ssl http2;
    server_name nkr-api.systemouts.com;

    ssl_certificate     /etc/letsencrypt/live/nkr-api.systemouts.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/nkr-api.systemouts.com/privkey.pem;
    ssl_protocols TLSv1.2 TLSv1.3;
    ssl_ciphers HIGH:!aNULL:!MD5;

    # ACL: sólo el panel + el propio host NKR pueden llegar aquí.
    # Cualquier otra IP recibe 403 antes de chequear el Bearer.
    allow 194.238.24.77;         # panel de control
    allow 127.0.0.1;             # loopback IPv4
    allow ::1;                   # loopback IPv6
    allow 116.202.240.179;       # self (cuando curl local DNS→public IP)
    allow 2a01:4f8:241:3e0e::2;  # self IPv6
    deny  all;

    # Timeouts largos: git clone de enterprise puede tardar ~2-5 min.
    client_max_body_size 2M;
    proxy_read_timeout 600s;
    proxy_send_timeout 600s;
    proxy_connect_timeout 10s;

    location / {
        proxy_pass http://nkr_api;
        proxy_http_version 1.1;
        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;
    }
}

# HTTP → HTTPS redirect (opcional)
server {
    listen 80;
    listen [::]:80;
    server_name nkr-api.systemouts.com;
    return 301 https://$host$request_uri;
}
```

**Setup completo:**

```bash
# 1. Crear DNS A record apuntando al host NKR (fuera de este host):
#    nkr-api.systemouts.com  A  116.202.240.179

# 2. Emitir cert con Let's Encrypt (después de que el DNS propague):
sudo certbot --nginx -d nkr-api.systemouts.com

# 3. Instalar el server block:
sudo install -m 0644 /path/to/nkr-api.conf /etc/nginx/sites-enabled/nkr-api
sudo nginx -t && sudo systemctl reload nginx

# 4. Sanity:
curl -s https://nkr-api.systemouts.com/api/v1/health
# → {"ok":true,"version":"1.6.4"}
```

### 11.2 nginx default_server SSL (evita leak de certs a hostnames no provisionados)

Cuando un dominio cliente apunta al host NKR **antes** de que el panel llame `POST /dns`, nginx por default sirve el cert del **primer** `server` block con SSL. Eso:
1. Rompe la confianza del cert (browser muestra `ERR_CERT_COMMON_NAME_INVALID`).
2. Leak del cert de otro dominio al cliente (minor info disclosure).

**Solución aplicada**: server block con `ssl_reject_handshake on` que corta TLS limpiamente para SNI desconocidos:

```nginx
# /etc/nginx/sites-enabled/nkr-default-ssl
server {
    listen 443 ssl http2 default_server;
    listen [::]:443 ssl http2 default_server;

    # Self-signed placeholder (nunca se sirve; sólo requerido para que nginx levante el block).
    ssl_certificate     /etc/nginx/ssl-default/placeholder.crt;
    ssl_certificate_key /etc/nginx/ssl-default/placeholder.key;

    # Reject unknown SNI → el browser ve "unrecognized name" alert (NO cert wrong).
    ssl_reject_handshake on;
}
```

**Generar el placeholder:**
```bash
sudo mkdir -p /etc/nginx/ssl-default
sudo openssl req -x509 -nodes -newkey rsa:2048 -days 3650 \
  -subj "/CN=nkr-placeholder.invalid" \
  -keyout /etc/nginx/ssl-default/placeholder.key \
  -out   /etc/nginx/ssl-default/placeholder.crt
sudo chmod 600 /etc/nginx/ssl-default/placeholder.key
sudo nginx -t && sudo systemctl reload nginx
```

**Efecto:** un dominio con DNS al host pero sin `POST /dns` llamado recibe `tlsv1 unrecognized name` → browser muestra error genérico de conexión (como si el servidor no respondiera), en lugar de confundir al usuario con un cert mismatch que implica suplantación.

**Recomendación al panel:** llamar `POST /dns` **inmediatamente después** del `POST /instances` para no dejar el dominio flotante. Ver §6.1 para el orden correcto.

Mientras no haya ACL de IP, **la seguridad depende 100% del Bearer token**. El token es 256-bit random; inmune a brute-force, pero debe rotarse si se sospecha compromiso: editar `/etc/nkr/api.env` + `systemctl restart nkr-api-server`.

### 11.3 Procedimiento de seed del template (one-time por cell)

El template `<cell>-odoo-template` se registra en el yaml con `disabled: true` y su DB no existe hasta que se hace el primer arranque manual. Procedimiento completo (operador del host, no panel):

```bash
CELL=odoo-v19   # ajustar según la cell

# 1. Re-habilitar el template en el yaml (lo apagaremos al final).
sudo sed -i '/^  odoo-template:$/{n;s/    disabled: true/    disabled: false/}' \
    /mnt/nkr/cells/$CELL/nkr-compose.yml

# 2. Limpiar virtiofsd huérfanos del template (de un stop previo).
#    El cell_id va en la posición c<N>: 1 para odoo-v17, 2 para odoo-v19, etc.
CID=$(grep -E "^cell_id:" /mnt/nkr/cells/$CELL/cell.yml | awk '{print $2}')
for pid in $(ps aux | grep virtiofsd | grep "nkrfsc${CID}v3s" | grep -v grep | awk '{print $2}'); do
  sudo kill -9 $pid
done
sudo rm -f /run/nkrfs/nkrfsc${CID}v3s*.sock /run/nkrfs/nkrfsc${CID}v3s*.sock.pid

# 3. Compose up — arranca todo, incluyendo el template ahora habilitado.
cd /mnt/nkr/cells/$CELL && sudo nkr compose up -d

# 4. Esperar a que Odoo cree la DB (3-5 min de boot inicial: carga base+web
#    desde XML/CSV). Polling automático:
PG_IP=10.0.${CID}.2
until PGPASSWORD=odoo psql -h $PG_IP -U odoo -d postgres -tAc \
    "SELECT 1 FROM pg_database WHERE datname='db-$CELL-odoo-template'" 2>/dev/null \
    | grep -q 1; do
  sleep 15
  echo "  $(date +%H:%M:%S) — esperando..."
done
echo "✅ db-$CELL-odoo-template existe en PostgreSQL"

# 5. Apagar el template (su DB queda en PG, eso es lo único que necesitamos).
sudo nkr stop $CELL-odoo-template

# 6. Re-marcar disabled en el yaml.
sudo sed -i '/^  odoo-template:$/{n;s/    disabled: false/    disabled: true/}' \
    /mnt/nkr/cells/$CELL/nkr-compose.yml

# 7. (opcional) Verificar limpieza:
sudo nkr ps  # template no debería aparecer
```

**Notas:**
- Sólo se hace **una vez por cell**, después de crearla. El template queda apagado para siempre — el panel puede crear tenants sin tocar nada más.
- La DB del template ocupa ~50-100 MB en PG. Eso se reusa via CoW para todos los clones.
- Si querés actualizar el template (instalar `sale_management` por defecto, cambiar idioma, etc.), repite los pasos 1-3, hace los cambios via UI o jsonrpc, después 5-6.
- **Actualizar el template no afecta tenants ya clonados** — sólo los nuevos clones heredan los cambios.

### 11.4 Pre-instalación del módulo `nkr_sso` en el template (one-time por cell)

El endpoint `POST /sso` (§4.18) requiere que el módulo Odoo `nkr_sso` esté instalado en cada tenant. Para evitar tener que instalarlo por tenant, **se preinstala UNA VEZ en el template** y los clones lo heredan vía `cp --reflink` (código) + `CREATE DATABASE … TEMPLATE` (DB con módulo `state=installed`).

Spec completa del módulo Odoo: ver `nkr_sso.md`.

**Procedimiento (one-time por cell):**

```bash
. /etc/nkr/api.env
CELL=odoo-v19                                  # ajustar por cell
DEV_TENANT=odoo-v19-intech-devp                # tenant donde el panel deployó nkr_sso
TEMPLATE_TENANT=$CELL-odoo-template

# 1. Copiar el código del módulo del dev tenant → template (filesystem del host).
#    El módulo `nkr_sso` debe existir ya en /mnt/nkr/cells/$CELL/instances/$DEV_TENANT/addons/nkr_sso/
#    (el panel lo deploya ahí con su flujo habitual, vía `POST /addons/git` o git/scp directo).
cp -a "/mnt/nkr/cells/$CELL/instances/$DEV_TENANT/addons/nkr_sso" \
      "/mnt/nkr/cells/$CELL/instances/$TEMPLATE_TENANT/addons/"

# 2. Arrancar el template (sigue los pasos 1-3 de §11.3 si está disabled).
#    El template debe estar UP para que Odoo pueda ejecutar la instalación.
curl -fsS -X POST -H "Authorization: Bearer $NKR_API_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"action":"start"}' \
  "http://127.0.0.1:9090/api/v1/cells/$CELL/instances/$TEMPLATE_TENANT/actions"

# 3. Esperar a que :8069 responda (Odoo del template puede tardar 30-60s en cold boot).
PG_IP=10.0.$(grep -E "^cell_id:" /mnt/nkr/cells/$CELL/cell.yml | awk '{print $2}').3
until curl -sf -o /dev/null --max-time 2 "http://$PG_IP:8069/"; do sleep 3; done

# 4. Instalar nkr_sso en la DB del template.
curl -fsS -X POST -H "Authorization: Bearer $NKR_API_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"op":"install","modules":["nkr_sso"]}' \
  "http://127.0.0.1:9090/api/v1/cells/$CELL/instances/$TEMPLATE_TENANT/modules"

# 5. Verificar que quedó installed.
curl -fsS -X POST -H "Authorization: Bearer $NKR_API_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"query":"SELECT name, state FROM ir_module_module WHERE name='"'"'nkr_sso'"'"';"}' \
  "http://127.0.0.1:9090/api/v1/cells/$CELL/instances/$TEMPLATE_TENANT/psql"
# esperás: rows con [["nkr_sso", "installed"]]

# 6. Apagar el template (sigue paso 5-6 de §11.3).
curl -fsS -X POST -H "Authorization: Bearer $NKR_API_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"action":"stop"}' \
  "http://127.0.0.1:9090/api/v1/cells/$CELL/instances/$TEMPLATE_TENANT/actions"
```

**Notas:**
- Se hace **una vez por cell** (`odoo-v17`, `odoo-v19`, etc.).
- Para tenants creados **antes** de esta preinstalación → ver §4.4 de `nkr_sso.md` (flujo per-instance fallback).
- El `nkr_sso_secret` NO se hereda del template — `cell.rs::rewrite_odoo_conf_full` lo regenera fresh por tenant en cada `POST /instances`. Comprometer un secret ≠ comprometer todos.
- Si después sale una versión nueva del módulo, repetir pasos 1-5 con `op:"upgrade"` en lugar de `install`. Ver §4.7 de `nkr_sso.md` para el loop de upgrade masivo a tenants existentes.

---

## 12. Smoke-testing guide (para panel-Claude)

Si sos el agente Claude que trabaja en el código del panel, esta sección te guía para validar end-to-end contra un NKR real sin romper instancias existentes. Todo lo de acá abajo es seguro de ejecutar y se limpia a sí mismo.

### 12.1 Pre-flight

```bash
# En el host NKR (o desde el servidor del panel, apuntando a la IP pública):
export NKR_API_BASE=http://127.0.0.1:9090
export TOK="Authorization: Bearer $(sudo cat /etc/nkr/api.env | grep -oP '(?<==).*')"

# Sanity: API responde
curl -s $NKR_API_BASE/api/v1/health
# → {"ok":true,"version":"1.6.4"}

# Sanity: hay cells disponibles (esperás odoo-v17 y odoo-v19)
curl -s -H "$TOK" $NKR_API_BASE/api/v1/cells | python3 -m json.tool
```

Si `/health` falla: `nkr-api-server` no está corriendo, o nginx delante no enruta. Si `cells` devuelve `401`: token mal copiado.

### 12.2 Flujo E2E mínimo (Odoo 17, tenant de prueba)

```bash
# 1. Crear tenant (mode=production → DB clonada del template). ASÍNCRONO: 202 + poll.
RESP=$(curl -s -X POST -H "$TOK" -H "Content-Type: application/json" \
  -d '{"nkr_name":"smoke-1","mode":"production","odoo_version":"17.0","workers":2,"admin_passwd":"smoke-pw-1234567890abcd"}' \
  $NKR_API_BASE/api/v1/instances)
echo "$RESP" | python3 -m json.tool
NAME=$(echo "$RESP" | python3 -c "import json,sys;print(json.load(sys.stdin)['nkr_name'])")
CELL=$(echo "$RESP" | python3 -c "import json,sys;print(json.load(sys.stdin)['cell'])")
echo "Create despachado: cell=$CELL name=$NAME — esperando..."
# Poll create-status hasta ready (o failed)
until curl -s -H "$TOK" "$NKR_API_BASE/api/v1/cells/$CELL/instances/$NAME/create-status" \
  | python3 -c "import json,sys; s=json.load(sys.stdin).get('status'); sys.exit(0 if s=='ready' else (2 if s=='failed' else 1))"; rc=$?; [ $rc -eq 2 ] && { echo 'CREATE FALLÓ'; exit 1; }; [ $rc -eq 0 ] && break; do
  echo "esperando create..."; sleep 4
done
echo "Creada: cell=$CELL name=$NAME (running=false)"

# 2. Clonar un addon público (OCA/web) mientras VM apagada
curl -s -X POST -H "$TOK" -H "Content-Type: application/json" \
  -d '{"repo_url":"https://github.com/OCA/web.git","ref":"17.0","subdir":"web"}' \
  $NKR_API_BASE/api/v1/cells/$CELL/instances/$NAME/addons/git | python3 -m json.tool
# Esperá algo como {"action":"clone","sha":"..."}

# 3. Instalar una lib Python pura
curl -s -X PUT -H "$TOK" -H "Content-Type: application/json" \
  -d '{"requirements_txt":"num2words==0.5.13\n"}' \
  $NKR_API_BASE/api/v1/cells/$CELL/instances/$NAME/pylibs | python3 -m json.tool

# 4. Start — primer boot con addons + pylibs ya instalados
curl -s -X POST -H "$TOK" -H "Content-Type: application/json" \
  -d '{"action":"start"}' \
  $NKR_API_BASE/api/v1/cells/$CELL/instances/$NAME/actions | python3 -m json.tool

# 5. Esperar a que Odoo esté arriba
until curl -s -H "$TOK" $NKR_API_BASE/api/v1/cells/$CELL/instances/$NAME \
  | python3 -c "import json,sys;sys.exit(0 if json.load(sys.stdin)['nkr_status']['port_8069_up'] else 1)" 2>/dev/null; do
    echo "esperando :8069..."; sleep 3
done

# 6. Tail logs (opcional — ver que Odoo arrancó limpio)
curl -s -H "$TOK" "$NKR_API_BASE/api/v1/cells/$CELL/instances/$NAME/logs?tail=30" \
  | python3 -c "import json,sys;[print(l) for l in json.load(sys.stdin)['lines']]"

# 7. CLEANUP OBLIGATORIO — borrar el tenant (incluye drop DB)
curl -s -X DELETE -H "$TOK" \
  "$NKR_API_BASE/api/v1/cells/$CELL/instances/$NAME" | python3 -m json.tool
```

### 12.3 Flujo E2E completo (Odoo 19, enterprise + webhook simulado)

Para probar el path "Enterprise", necesitás credenciales a `github.com/odoo/enterprise`. Si no las tenés, saltá al paso 3:

```bash
# 1. (Sólo una vez por cell) clonar enterprise
DK=$(base64 -w0 < ~/keys/odoo-enterprise-deploy.pem)  # opcional
curl -s -X POST -H "$TOK" -H "Content-Type: application/json" \
  -d "{\"repo_url\":\"git@github.com:odoo/enterprise.git\",\"ref\":\"19.0\",\"deploy_key_b64\":\"$DK\"}" \
  $NKR_API_BASE/api/v1/cells/odoo-v19/enterprise/git

# 2. Verificar que se clonó
ssh nkr-host 'ls /mnt/nkr/enterprise/19.0/ | head -5'
# → account_accountant, web_enterprise, ... (o vacío si saltaste paso 1)

# 3. Crear tenant v19
RESP=$(curl -s -X POST -H "$TOK" -H "Content-Type: application/json" \
  -d '{"nkr_name":"smoke-19","mode":"production","odoo_version":"19.0","workers":2}' \
  $NKR_API_BASE/api/v1/instances)
NAME=$(echo "$RESP" | python3 -c "import json,sys;print(json.load(sys.stdin)['nkr_name'])")
CELL=odoo-v19

# 4. Simular un webhook GitHub: nuevo commit en un repo de cliente
curl -s -X POST -H "$TOK" -H "Content-Type: application/json" \
  -d '{"repo_url":"https://github.com/OCA/server-tools.git","ref":"19.0","subdir":"server-tools","action":"sync"}' \
  $NKR_API_BASE/api/v1/cells/$CELL/instances/$NAME/addons/git

# 5. Restart (lo que haría el panel después del pull)
curl -s -X POST -H "$TOK" -H "Content-Type: application/json" \
  -d '{"action":"restart"}' \
  $NKR_API_BASE/api/v1/cells/$CELL/instances/$NAME/actions

# 6. Cleanup
curl -s -X DELETE -H "$TOK" \
  "$NKR_API_BASE/api/v1/cells/$CELL/instances/$NAME"
```

### 12.4 Errores esperados al probar validación

El panel-Claude DEBE manejar estos casos gracefully. Probalos para confirmar que el manejo de errores está bien:

| Caso de prueba | Request | Respuesta esperada |
|---|---|---|
| Sin Bearer token | cualquier PUT/POST protegido | `401 unauthorized` |
| Token inválido | `Authorization: Bearer wrong` | `401 unauthorized` |
| Cell inexistente | `POST /cells/fantasma/instances` | `404 cell_not_found` |
| Versión no matchea | `{"odoo_version":"16.0"}` en v17 | `409 version_mismatch` |
| Repo URL sospechosa | `{"repo_url":"https://evil.tld/pwn.git"}` | `400 invalid_repo_url` |
| Ref con metacaracteres | `{"ref":"$(rm -rf /)"}` | `400 invalid_ref` |
| Body gigante | POST /instances con 1 MB de body | `413 body_too_large` |
| Borrar el template | `DELETE /cells/.../instances/{cell}-odoo-template` | el DELETE probablemente responde 200 pero **no lo hagas** — rompe `POST /instances` futuros |

### 12.5 Variables que el panel-Claude debería hardcodear en tests

```bash
# URL base del proxy (configurar según deploy real):
NKR_API_BASE=http://127.0.0.1:9090

# Token — leer de /etc/nkr/api.env en el host, o de secret manager en panel:
NKR_API_TOKEN=<hex de 64 chars>

# Cells conocidas en esta instalación:
CELLS_AVAILABLE=(odoo-v17 odoo-v19)

# Master admin_passwd para crear DBs vía Odoo DB manager:
ODOO_ADMIN_PASSWD=admin  # hardcodeado en config/odoo.conf del master

# PgBouncer por cell (10.0.{cell_id}.3:6432):
V17_PGBOUNCER=10.0.1.3:6432
V19_PGBOUNCER=10.0.2.3:6432
```

### 12.6 Convenciones de nombres para tests

- Usá prefijo `smoke-`, `test-` o `ci-` para instancias temporales.
- Nunca uses nombres que contengan `odoo-template`, `db`, `pgb` — esos son reservados/infra.
- Limpiá con `DELETE` al terminar cada test. Si se te olvida, `GET /cells` te muestra los `used_odoos` acumulando y eventualmente vas a chocar con `cell_full`.

### 12.7 Cómo depurar cuando algo falla

| Síntoma | Comando en el host NKR |
|---|---|
| Proxy no responde | `sudo systemctl status nkr-api-server` |
| Daemon no responde | `sudo systemctl status nkr.service` |
| VM de instancia muerta | `sudo nkr ps` / `sudo nkr stats -w 2` |
| Git clone falla con "Read-only filesystem" | `id nkr-api` — confirmar que está en grupo `nkr-addons`; `getfacl /mnt/nkr/cells \| grep nkr-addons` |
| pip install falla con "No such file" | `which pip3` — si falta, `apt-get install python3-pip` |
| Odoo arranca pero web da 500 | `GET /logs?tail=200` de la instancia, buscar tracebacks |

Logs del proxy: `journalctl -u nkr-api-server -f`.
Logs del daemon: `journalctl -u nkr.service -f`.
Log per-cell: `/mnt/nkr/cells/<cell>/logs/nkr-compose.log`.
