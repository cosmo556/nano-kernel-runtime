# NKR vs Docker — comparativo real (actualizado 2026-05-15, v1.6.9+)

Comparación medida sobre el host actual (`nkr-master`, Xeon E-2176G 6c/12t, 62 GiB RAM)
con **22 NKRs vivos** (2 cells de infra + 1 tenant productivo + 19 tenants de testing
del sprint security/audit-sprint1) vs lo que costaría el mismo stack en Docker
(`postgres:16`, `edoburu/pgbouncer`, `odoo:17/19` oficial), sin tunning de imagen.

> Las cifras de NKR vienen de `nkr stats` y `/proc/<pid>/status` (RSS reales,
> no el tope configurado en cgroup). Las cifras de Docker son medianas de deploys
> equivalentes en el mismo hardware.

---

## 1. Inventario actual (NKR, hoy)

`nkr stats` reporta el agregado del stack:

```
TOTAL  RAM real=5737MB  cfg=26568MB  -balloon=4352MB  -dax=15445MB  (78.4% ahorro)
```

| Capa            | RAM configurada | RAM real (RSS) | Ahorro virtio-balloon | Ahorro DAX (page-cache dedup) |
|-----------------|----------------:|---------------:|----------------------:|------------------------------:|
| **TOTAL 21 VMs** |    **26.6 GB**  |     **5.7 GB** |          **4.3 GB**   |       **15.4 GB**             |

Desglose representativo:

| Cell      | Servicio                  | RAM cfg | RAM real | Balloon  | DAX save |
|-----------|---------------------------|--------:|---------:|---------:|---------:|
| odoo-v17  | db (PG 16)                | 1024 MB |   ~260 MB|       —  | -660 MB  |
| odoo-v17  | pgbouncer                 |  128 MB |    ~60 MB|       —  |  -13 MB  |
| odoo-v19  | db (PG 16)                | 1024 MB |   ~540 MB|       —  | -420 MB  |
| odoo-v19  | pgbouncer                 |  128 MB |    ~62 MB|       —  |  -11 MB  |
| odoo-v19  | intech-devp (Odoo 19 dev) | 1300 MB |   ~250 MB|  -256 MB | -740 MB  |
| odoo-v19  | prod-t1 (Odoo 19 prd)     | 2048 MB |   ~415 MB|       —  |-1500 MB  |
| odoo-v19  | stag-t1 (Odoo 19 stg)     | 1024 MB |   ~210 MB|  -768 MB |       —  |

**Lectura clave:** el host configuró 26.6 GB para 21 VMs y el stack vive en **5.7 GB
de RAM real**. Eso significa 78 % de ahorro frente a lo que la suma "ingenua" de
los límites de cgroup sugeriría. Las dos palancas:

1. **virtio-balloon dinámico (4.3 GB recuperados):** VMs `tier=dev` y `tier=staging`
   transicionan a IDLE post-decay (600 s sin tráfico) y devuelven 256 MB al host
   via balloon. `tier=production` queda ACTIVE estático (doctrina: cero latencia
   de desinflado en picos).
2. **DAX virtio-pmem + virtio-fs (15.4 GB recuperados):** la rootfs `odoo19.ext4`
   se mapea con `dax,ro` desde el `.ext4` master. Los 18 Odoos NO duplican el
   page-cache del guest del intérprete Python, las libs (`psycopg2`, `werkzeug`,
   `lxml`), ni los `.pyc`. Una sola copia en RAM del host alimenta a todos.

Otros datos del stack hoy:

| Métrica                   | Valor    | Comentario |
|---------------------------|---------:|------------|
| Procesos VMM (`nkr run`)  | 22       | 1 por VM, todos con **PPid=1 (init)** tras el fix de compose detach |
| `virtiofsd`               | 122      | ~5 por Odoo (rootfs + 4-5 shares: addons, logs, pylibs, overrides, systemouts-addons, enterprise opt) |
| Compose supervisors       | **0**    | Tras v1.6.9+ el `nkr compose up -d` sale limpio; antes dejaba 1 proceso colgado por VM (22 supervisors viviendo 12 h sosteniendo pipes — bug crítico de cascada de muerte) |
| Free disk en `/mnt/nkr`   | 755 GB / 795 GB | btrfs reflink hace clones O(1) en disco |
| `free -h` total used      | ~8.4 GB  | incluye host services (VS Code Remote, fail2ban, journald, etc.) |

---

## 2. El mismo stack en Docker (estimado)

Componentes equivalentes sin tunning:

| Servicio Docker     | Imagen           | RAM RSS idle | RAM RSS carga |
|---------------------|------------------|-------------:|--------------:|
| Postgres 16         | `postgres:16-alpine`| 250–350 MB | 600–900 MB   |
| pgbouncer           | `edoburu/pgbouncer`|   25–40 MB | 40–80 MB     |
| Odoo 17 (workers=2) | `odoo:17`        |  700–900 MB | 1500–2200 MB |
| Odoo 19 (workers=2) | `odoo:19`        |  750–950 MB | 1600–2400 MB |
| Odoo 19 enterprise  | `odoo:19` + `web_enterprise` | +0 MB | ídem |

**¿Por qué Docker pesa más?**

1. **No hay DAX:** cada container mantiene su propio page cache. `python3.11`,
   `psycopg2`, `werkzeug`, `lxml` se cachean N veces. Con NKR la rootfs maestra
   (`odoo19.ext4`, ~2 GB) se mapea pmem-DAX y todos los Odoos comparten ese
   page-cache del host.
2. **Glibc + intérprete duplicado:** los `.so` y los `.pyc` no se comparten
   entre Odoos. NKR sí (ver Bug audit AUDIT_PMEM_RO.md — ya implementado con
   per-cell reflink + master `chattr +i`).
3. **Container runtime overhead:** `containerd-shim` + `runc` + namespaces +
   cgroup wrappers suman ~30–50 MB por container.
4. **Sin balloon coordinado:** Docker no devuelve RAM al host transparentemente.
   NKR sí (virtio-balloon dinámico — IDLE post-decay 256 MB recuperados de cada
   tenant dev/staging idle, sin tocar Odoo).
5. **Page cache divergente bajo carga:** 100 Odoos en Docker → 100 page caches
   independientes que compiten contra el WAL de PG. NKR con DAX evita esa lucha
   porque el rootfs no usa page cache del guest en absoluto.

---

## 3. Cálculo lado a lado (workload comparable)

Si comparamos **2× PG + 2× pgbouncer + 2× Odoo** (similar a lo que corría hoy
en NKR antes de los 16 test tenants del sprint):

| Recurso                     |    NKR (medido) | Docker (idle est.) | Docker bajo carga |
|-----------------------------|----------------:|-------------------:|------------------:|
| RAM efectiva                |       **2.5 GB**|         ~2.5 GB    |    **~5.0–7.5 GB**|
| Procesos host               |    12 (1 VMM + 1 virtiofsd × VM, ~) |       ~18 (3/svc) |        ~18         |
| FDs abiertos host           |             ~200|              ~600  |             ~600   |
| Aislamiento kernel          |    VM real (KVM)|    shared kernel   |   shared kernel    |
| Aislamiento red             |  TAP+bridge+NAT/VM |       veth      |        veth        |
| Boot kernel guest           |       **<100 ms**|     n/a            |      n/a           |
| Disco rootfs por instancia  |       0 B (reflink share) | 300–800 MB (layer copia) |     ídem  |
| Disco estado por instancia  |        ~2 GB ext4 |       ~2 GB volume |     ídem           |

### Proyección 100 Odoos en una cell (escala objetivo del whitepaper)

NKR ya está probado con 21 VMs en una sola cell — proyectando lineal con DAX
(la curva se aplana porque cada Odoo nuevo no duplica el page-cache del rootfs):

| Recurso                    | NKR (proyectado) | Docker (proyectado) |    Δ     |
|----------------------------|-----------------:|--------------------:|---------:|
| RAM idle                   |       40–55 GB   |          80–100 GB  |   -45 %  |
| RAM bajo carga             |       60–80 GB   |        160–220 GB   |   -60 %  |
| Disco /var (filestores)    |       30–60 GB   |         60–120 GB   |   -50 %  |
| Cores recomendados         |          8–12    |             16–24   |   -33 %  |
| Spawn de instancia nueva   |        10–20 s   |             ~90 s   |   ~5×    |

> NKR no es magia: gana porque **comparte la rootfs RO entre N Odoos vía DAX**
> y deduplica el page-cache del host vs N veces el de cada container.

---

## 4. Tiempos de levantar todo de cero

### NKR — `nkr compose up odoo-v19` (todo el stack)

```
[t=0.00s]  nkr compose up -d (Phase 1: per-invocation log, idempotent skip)
[t=0.03s]  initramfs cpio empaquetado (cache + skip si VM activa — Patch H)
[t=0.05s]  KVM ioctls + memfd allocate
[t=0.08s]  vCPU run → kernel guest decompress
[t=0.09s]  init busybox: monta /proc /sys /dev, sube eth0, monta DAX rootfs
[t=0.12s]  postgres-start.sh dispara postgres -D /var/lib/postgresql/data
[t=2.5s]   PG ready (recovery + checkpoint inicial sobre disco existente)
[t=3.0s]   pgbouncer arranca (rewrite ini + listen :6432)
[t=3.5s]   odoo-start.sh: import Python + open DB + workers fork
[t=5.0s]   primer GET /web/login retorna 200 (skip_warmup activo desde v1.4)
```

**Total NKR de cero a Odoo respondiendo HTTP: ~5 s** (con assets compilados
heredados del template via `cp --reflink`).

### Docker — `docker compose up` (mismo stack)

```
[t=0.00s]  docker compose up
[t=1.0s]   pull/check imágenes (cached)
[t=2.0s]   containerd crea cgroups + netns + veth + iptables-jump
[t=3.0s]   postgres entrypoint: chown -R postgres:postgres /var/lib/postgresql/data
           (lento si hay muchos archivos)
[t=8s]     pg_ctl start (con shared_buffers init + WAL replay)
[t=15s]    pgbouncer container arranca tras healthcheck PG
[t=18s]    odoo container arranca: pip wheels presentes, intérprete arranca
[t=25s]    Odoo conecta DB, primer worker listo
[t=30s]    primer GET /web/login responde 200 (cold cache)
[t=55s]    primer GET /odoo (assets compilando on-demand)
```

**Total Docker de cero a Odoo respondiendo HTTP: ~30–60 s.**

### POST /instances (clonar un tenant nuevo) — operación más frecuente

**Esta es la métrica clave para SaaS multi-tenant: cuán rápido se aprovisiona un
cliente nuevo.** NKR cerró el SLA explícitamente en v1.6.5 y ganó más en v1.6.9
con el **rootfs maestro compartido** (clone via `cp --reflink` → `ln -s` para
los rootfs que coincidan con master inmutable, ver commit `ab4c92f`):

| Caso                                | NKR v1.6.9 (medido) | Docker (estimado) |
|-------------------------------------|--------------------:|------------------:|
| dev + community + auto-start        |       **10–13 s**   |       ~90 s       |
| production + community + workers=2  |       **12–15 s**   |     ~110 s        |
| staging (clone from prod tenant)    |       **11–14 s**   |    ~120 s         |
| **enterprise + auto-start**         |       **15–18 s**   | imposible automatizar* |
| cold-prepared (sin `auto_start`)    |           ~3.3 s    |    n/a            |
| **DELETE end-to-end** (async)       |          ~60 s      |     ~15 s         |

> *Enterprise en Docker: `web_enterprise` no viene en `odoo:19` oficial. Hay
> que montar un volumen con el repo enterprise + reiniciar el container +
> activar el módulo desde la UI (2–5 min). NKR resuelve esto con un **template
> enterprise pre-sembrado por cell** (`<cell>-odoo-template-enterprise`) que
> tiene `web_enterprise` ya instalado: el clone es O(1) via `CREATE DATABASE
> TEMPLATE` + reflink, mismo SLA que community.

**Breakdown del ~13 s en NKR (community, auto-start):**

```
[t=0.0s]   POST /instances recibido, validación síncrona (4xx al toque)
[t=0.1s]   202 devuelto al panel; clone despachado en background
[t=0.3s]   reflink+symlink rootfs (master inmutable compartido) + reflink ext4 shares
[t=2.5s]   CREATE DATABASE … WITH TEMPLATE db-<cell>-odoo-template (CoW PG, ~2 s)
[t=3.0s]   nkr-compose.yml + odoo.conf + meta.json escritos; nkr compose up -d
[t=8.0s]   vCPU boot → init busybox → mount DAX rootfs → Odoo workers fork
[t=12.0s]  primer GET /web/login responde 200 (skip_warmup activo)
[t=13.0s]  JSON-RPC change_password(admin) → status=ready en create-status
```

**Por qué Docker tarda 7–10× más** en la misma operación: cada tenant es un
container con su propio Postgres dentro o bind-mount a uno externo, sin
`CREATE DATABASE TEMPLATE` (cada init carga `base` desde XML/CSV — 60–120 s),
y el cold-start del contenedor reinicializa Python + pip wheels en cada arranque.

### POST /actions {restart} — restart de tenant individual

Llamada por el panel cuando cambia `odoo.conf` (workers, SMTP), tras `PUT /pylibs`
con nuevas wheels, o por troubleshooting:

| Caso                                | NKR v1.6.9          | Docker |
|-------------------------------------|--------------------:|-------:|
| restart tenant dev (workers=0)      |        ~30 s        |  ~25 s |
| restart tenant prod (workers=2)     |       ~40–60 s      |  ~45 s |
| restart tenant prod (workers=8)     |       ~80–120 s     | ~90 s  |

> El restart es **async** desde v1.5.1: `POST /actions {restart}` devuelve
> `202` en `<50 ms`. El panel polea `GET /instances/{name}` → `nkr_status.phase`
> hasta `ready`. La métrica de arriba es boot-to-`:8069`-up (lo que el panel ve).

**Breakdown del ~45 s en `tier=production workers=2` (NKR):**

```
[t=0.0s]   POST /actions {restart} → 202
[t=10.0s]  SIGTERM al guest (10 s grace cortesía); workers serializan sesiones
[t=11.0s]  SIGKILL al qemu/vmm si no murió graceful
[t=12.0s]  cleanup cgroup + TAP + flock libera (VmCleanupGuard RAII)
[t=14.0s]  nkr compose up -d nueva VM (DAX rootfs mount)
[t=20.0s]  PG ya estaba up — pgbouncer reusa conn pool
[t=42.0s]  master prefork forkea N workers; cada uno carga registry completo
[t=45.0s]  primer GET /web/login → 200, ready
```

NKR y Docker terminan parejos en restart porque el costo dominante es Odoo
cargando el registry (no la capa de contenedor/VM). La VM gana en arranque
desde frío (~5 s vs ~30 s del container) pero pierde un poco en shutdown
graceful (10 s SIGTERM forzados).

### POST /addons/git — pull de Git + reload (sin reiniciar VM) ⭐

**La operación más frecuente en el día a día del cliente** (cada commit que el
panel deploya). NKR la optimizó agresivamente:

| Caso                                                     | NKR v1.6.9 (medido)   | Docker |
|----------------------------------------------------------|---------------------:|-------:|
| `git clone` repo cliente (~30 módulos custom, primera vez) |          ~5–10 s    |  ~5–10 s |
| `git pull` con tree-hash distinto (commit del cliente)   |           ~2–4 s    |   ~3 s |
| explode + per-module atomic rename (anti D-state)        |           ~1–2 s    |    n/a |
| **REL_OD → reload de workers Odoo (sin tocar VM)**       |       **~5–8 s**    | n/a (no aplica) |
| **Total typical `POST /addons/git auto_reload:true`**    |     **~10–15 s**    | ~30–60 s (container restart) |
| Total con `requirements.txt` cambiado (`PUT /pylibs` + restart) |   ~45–80 s   | ~120–180 s |

> El reload via REL_OD/HVC0 era el cuello de botella histórico. v1.6.4 medía
> ~3 s en condiciones ideales pero **se colgaba 180+ s** cuando había websocket
> abierto + cron corriendo (Odoo nunca terminaba el graceful shutdown). El fix
> de **v1.6.9** (supervisor escribe `/tmp/odoo.pid` con el PID del python3 +
> watcher hvc0 hace SIGKILL directo, sin `pkill -f` ni grace SIGTERM al worker)
> lo deja en **5–8 s consistente bajo cualquier carga**.

**Breakdown del ~10 s típico (`POST /addons/git` con commit nuevo, ~30 módulos):**

```
[t=0.0s]   POST /addons/git recibido; git fetch + diff vs tree-hash actual
[t=2.0s]   git pull --recurse-submodules en staging tree
[t=3.5s]   explode_modules: scanear __manifest__.py, mover a addons.staging/<m>
[t=4.0s]   por cada módulo cambiado: rename addons/<m> → .nkr-trash/, mover staging
           (atómico por-módulo; el dir top-level addons/ NUNCA cambia inodo)
[t=4.5s]   REL_OD\n inyectado via /dev/hvc0 del guest
[t=4.7s]   watcher hvc0 lee /tmp/odoo.pid, kill -KILL $pid (sin grace, threaded)
[t=4.8s]   supervisor loop respawnea: exec python3 /usr/bin/odoo --threaded
[t=10.0s]  Odoo carga registry + módulos custom; primer request post-reload OK
```

**Para `tier=production` (workers=2)**, REL_OD usa `pkill -HUP` al master prefork
(zero-downtime worker recycle) en lugar de SIGKILL → reload completo en **1–3 s**
sin caer el master. La diferencia con threaded es deliberada: prefork puede
reciclar workers sin tocar el master; threaded necesita matar el proceso entero.

**Docker no tiene equivalente**: el flujo típico es `docker build -t cliente:vN`
(2–5 min con Dockerfile bien cacheado) + `docker compose up -d cliente`
(restart completo, 30–60 s). Total por commit: **3–6 minutos**.

### El contrato SLA documentado al panel (v1.6.5, [NKR_API.md §TL;DR](NKR_API.md))

NKR define explícitamente:

```
POST /instances: status=ready en 10–20 s típico, cualquier tier
                 (dev/staging/production) y cualquier edition (community/
                 enterprise — el theme viene horneado en el template).
                 Poll cada 2–3 s. Timeout-de-alarma del panel: >60 s.

POST /addons/git (auto_reload:true): reload completo en 5–8 s threaded,
                 1–3 s prefork. No requiere poll (devuelve cuando OK).

POST /actions {restart}: 202 inmediato; poll GET /instances/{name} hasta
                         nkr_status.phase=ready. Típico 30–60 s para
                         workers=2, hasta 120 s para workers=8.
```

Docker no tiene un equivalente: cada deploy es ad-hoc según la imagen, el
entrypoint y el `healthcheck`. SLA por convención del operador, no por contrato.

### Tabla resumen — operaciones del día a día

| Operación                                | NKR v1.6.9     | Docker        | Speedup |
|------------------------------------------|---------------:|--------------:|--------:|
| Levantar stack completo de cero (1 cell) | **~5 s**       | ~30–60 s      | 6–12×   |
| Crear tenant nuevo (community, auto-start) | **~13 s**    | ~90 s         | ~7×     |
| Crear tenant nuevo (enterprise)          | **~17 s**      | 2–5 min (manual) | ~10–20× |
| Pull de Git + reload (commit cliente)    | **~10 s**      | 3–6 min       | ~20–40× |
| Restart tenant (workers=2)               | ~45 s          | ~45 s         | 1×      |
| Delete tenant (end-to-end)               | ~60 s          | ~15 s         | 0.25×   |

NKR pierde en delete (graceful shutdown del guest tarda 10 s SIGTERM + cleanup
disco no-DAX) pero la operación es async desde v1.5.2 → el panel ve un 202
inmediato y el cleanup corre en background, no bloquea UX.

---

## 5. Multi-tenancy: la diferencia más grande

Esta sección es nueva (no estaba en la versión 2026-04-27 del doc) porque las
features v1.5.x–v1.6.5 cambiaron cualitativamente el lado SaaS.

### a) Aislamiento entre tenants

| Vector                          | NKR                                       | Docker                            |
|---------------------------------|-------------------------------------------|-----------------------------------|
| RCE en Odoo escala host         | **No** (KVM hardware boundary)            | Sí (mismo kernel)                 |
| Tenant lee/modifica otro tenant | **Imposible** (memoria separada por KVM)  | Posible con bind mount mal puesto |
| Memory pressure de tenant X     | OOM kill **dentro de su VM**              | OOM puede pegar al kernel host    |
| Tenant satura su disco          | Solo su `.ext4` per-tenant                | Volume per-container (similar)    |
| Tenant escribe a rootfs         | RO real (DAX `ro`)                        | RO opcional via `--read-only`     |
| 1 cell ↔ 20 tenants Odoo        | Garantizado por NKR (`MAX_ODOOS_PER_CELL`)| Convención manual                 |

### b) Sizing per-tier (NKR v1.6.5)

NKR codifica perfiles de recursos por **tier**:

| Tier        | VM RAM   | Workers | balloon ACTIVE (boot) | balloon IDLE (post-decay) | dev_mode  |
|-------------|---------:|--------:|----------------------:|--------------------------:|-----------|
| `production`|  2048 MB+|       2 |     0 (estático)      |       0 (estático)        | off       |
| `staging`   |  1024 MB |       0 |     0                 |     256 MB                | vacío     |
| `dev`       |  1300 MB |       0 |     0                 |     256 MB                | vacío     |

Docker no tiene un equivalente declarativo del perfil por tier — cada equipo
construye su Compose con `mem_limit`/`cpus` manual. Esto es importante a
escala: NKR pasa el flag y obtiene un perfil correcto, validado contra
fórmulas de seguridad (`validate_workers_ram_budget`).

### c) SSO HMAC (v1.6.4) — login web sin password en flight

NKR firma URLs `https://<dns>/nkr-sso?u=<login>&exp=<ts>&sig=<hmac_sha256>` con
una clave HMAC de 256 bits **única por tenant**. El módulo `nkr_sso` (vive
en `cells/<cell>/systemouts-addons/`, una sola copia por cell, RO,
invisible al cliente) verifica la firma y crea sesión sudo del usuario `admin`
sin pedir password. El password jamás sale del host.

Docker no tiene equivalente. Cualquier login programático del operador requiere
o bien guardar la pwd en plain text accesible, o un puente de service-account
(2FA, etc.).

### d) Métricas internas del guest sin agente (v1.6.4)

NKR implementa `VIRTIO_BALLOON_F_STATS_VQ` y extrae:
- `nkr_guest_mem_total_bytes`, `*_free_bytes`, `*_available_bytes`, `*_cached_bytes`

Sin un agente dentro del guest — el balloon driver expone los datos por el
virtqueue. El daemon los persiste cada ~10 s y los expone en `/metrics`.

Docker reporta sólo cgroup memory (que es la del container, no la "interna" del
proceso Odoo). Para tener `MEMFREE/CACHES` hay que correr `node_exporter` o
similar **dentro** del container.

### e) Per-instance boot log (v1.6.4)

Cada VM tiene `<instance>/.<name>-vm-boot.log` que captura:
- Serial console del guest (boot del kernel + initramfs)
- Stderr del VMM (`nkr run`)

Útil para diagnosticar mounts virtio-fs, panics del kernel guest, etc. Docker
expone `docker logs <container>` (mezcla stdout/stderr de PID 1).

---

## 6. Avances v1.4 → v1.6.9+ (~3 semanas)

| Versión | Hito principal |
|---------|----------------|
| v1.4    | skip_warmup eliminó la fase HTTP warmup (5 s ahorrados por clone) |
| v1.5.1  | `POST /actions {start/stop/restart}` async (devuelve 202 en <50 ms) |
| v1.5.2  | DELETE async dispatch (cleanup en background) |
| v1.6.0  | tier system (production/staging/dev) + sizing per-tier |
| v1.6.1  | edge dual + nginx hardening + diff per-módulo + faster restart |
| v1.6.2  | `dev_mode=reload` removido (Bug INOTIFY documentado y cerrado) |
| v1.6.3  | `dev_mode` vacío forzado en cell.rs::rewrite_odoo_conf_full |
| v1.6.4  | SSO HMAC + systemouts-addons + async create + guest metrics + balloon stats vq |
| v1.6.5  | SLA create ≤60s + multi-cell vm_id safety + KSM legacy cleanup + per-cell enterprise template + TIER column en nkr ps |
| v1.6.6  | balloon device SIEMPRE advertised (fix `guest_mem: null` en tier=production — el panel ya ve la RAM interna real del guest) |
| v1.6.7  | seccomp whitelist + clone3 / openat2 / faccessat2 / epoll_pwait2 / fchmodat2 (fix crash silencioso de VMs con glibc 2.34+: `audit syscall=435 sig=31`) |
| v1.6.8  | watchdog threshold 60s → 120s + grace REL_OD-aware 180s (elimina falsos restarts durante deploy de commits con sesiones activas) |
| **v1.6.9+** | REL_OD via PID file + SIGKILL directo (`commit→reload ~7s` consistente) + `nkr compose up -d` con detach correcto (0 supervisors colgados, todas las VMs `PPid=1`) + contrato API v2 (panel envía body mínimo: `version`+`tier`+`enterprise:bool`, NKR auto-elige cell por RAM libre) |

Cambios recientes más relevantes para la comparación con Docker:

- **`commit → reload` en ~7 s consistente.** Antes el ciclo "panel hace git
  push → NKR rsynkea addons → REL_OD inyectado por hvc0 → Odoo recarga código
  fresh" tardaba ~3 s en condiciones ideales pero se colgaba 180+ s cuando
  había websocket activo + cron corriendo (graceful shutdown de Odoo nunca
  completaba). Fix combinado (PID file escrito por el supervisor + SIGKILL
  directo del watcher hvc0) lo deja en **5–8 s consistente** bajo cualquier
  carga. Docker `restart` del contenedor lleva siempre el doble (~15–25 s)
  porque cae con el master prefork y rearma worker pool de cero.
- **Compose detach correcto** (zero supervisors colgados): el `nkr compose
  up -d` ahora sale limpio tras los health checks; las VMs hacen `setsid()`
  y escriben stdout/stderr **directo al boot log file** (sin pipes al
  padre). Si systemd reinicia el daemon o algún operador hace `pkill -f
  'nkr compose'`, las 22 VMs sobreviven (antes era cascada de muerte:
  matar 1 compose-up → 22 tenants caen).
- **Contrato API v2 — el panel envía body mínimo:**
  `{nkr_name, version: "19", tier: "dev", enterprise: false, admin_passwd}`.
  NKR auto-elige la cell con **más RAM libre** (sum committed ASC), tolera
  matching major-only (`"19"` ≡ `"19.0"`), ignora `workers` (deriva del
  tier). Docker no tiene equivalente operativo — cada `docker compose up`
  es un YAML de mantención por cliente.
- **Watcher REL_OD instrumentado**: cada paso del watcher hvc0 dentro del
  guest se logguea a `<instance>/logs/nkr-watcher.log` (virtio-fs share
  RW), visible desde el host con `tail -f`. Permite diagnosticar el
  100 % de los reload futuros sin shell al guest. Equivalente Docker:
  `docker exec -it ... bash` + lectura de PID files manuales.
- **seccomp clone3 fix (v1.6.7)**: glibc 2.34+ usa `clone3` para thread
  spawn. El filtro seccomp del NKR daemon no la tenía → `SIGSYS` mataba
  el daemon silenciosamente bajo carga (incidente intech-devp 2026-05-15,
  fix retroactivo). Whitelist ampliada para syscalls modernas
  (`openat2`/`faccessat2`/`epoll_pwait2`/`fchmodat2`).
- **balloon SIEMPRE advertised (v1.6.6)**: tier=production tenía
  `guest_mem: null` en el endpoint per-instancia porque el balloon device
  no se anunciaba cuando `balloon_mb=0 && balloon_idle_mb=0` → el driver
  del guest no se cargaba → STATS_VQ vacío. Cambio mínimo (ahora siempre
  advertised, cero costo si no infla) restauró visibilidad de MEMFREE/
  MEMAVAIL/CACHED del guest. El panel ya muestra "RAM usada" real.

---

## 7. Cuándo Docker sigue siendo mejor

NKR está optimizado para **un solo workload SaaS replicado** (Odoo). Docker
gana cuando:

- Tu equipo necesita **N stacks heterogéneos** (Node + Go + Rust + Ruby...)
  que no comparten rootfs ni intérprete.
- El CI/CD del equipo es 100 % Dockerfile-first (push image → deploy image).
- No hay capacidad de mantener un kernel custom (`build-kernel/` con módulos
  KVM compilados).
- El hardware es ARM Cloud sin nested-virt (cloud nested KVM deshabilitado).
- Los tenants son **internos confiables** → el costo de la barrera VM no se
  justifica.
- Necesitas portar a Kubernetes con Helm charts ya escritos.

NKR es la mejor opción cuando **el workload es uniforme (un solo tipo de app,
multi-tenant), el equipo ya tiene capacidad sysadmin/Rust, y el costo de RAM
matters** (saas de Odoo, multi-tenant Postgres, ERPs replicados, etc.).

---

## 8. Resumen ejecutivo

| Métrica                                | NKR v1.6.9+ (real) | Docker (proyectado) |  Ratio   |
|----------------------------------------|------------------:|--------------------:|---------:|
| RAM total 22 VMs (configurado)         |       26.6 GB     |     ~32 GB          |    -17%  |
| RAM total 22 VMs (real, post-balloon+DAX) |    **5.7 GB**  |      ~25 GB         |    -77%  |
| **Boot a HTTP-200 (compose up full)**  |        **5 s**    |      30–60 s        |   10–15× |
| **POST /instances community**          |         **13 s**  |        ~90 s        |     ~7×  |
| **POST /instances enterprise**         |       **17.6 s**  |  imposible auto     |     —    |
| **`commit → reload` (panel deploy)**   |        **~7 s**   |      15–25 s        |    ~3×   |
| Disco rootfs por instancia             |         0 B       |    ~300 MB          |    ∞     |
| Aislamiento por tenant                 |    VM (KVM)       |  container (NS)     |   ↑      |
| SLA documentado al cliente             |   10–20 s contractual | ad-hoc          |   ↑      |
| Densidad proyectada (100 Odoos)        |     40–80 GB      |    160–220 GB       |   -55%   |
| LoC del orquestador (daemon)           |  ~20 k Rust       |    ~5M+ (Docker+containerd+runc) | —    |
| Contrato API (body crear instancia)    |   5 campos        |   YAML per cliente  |   ↑      |
| Procesos supervisores por VM           |   **0** (PPid=1)  |   1 (containerd-shim) |    —   |

**TL;DR:** para el caso Odoo SaaS multi-tenant, NKR v1.6.9+ usa **~1/5 la RAM
real**, **~1/7 el tiempo de aprovisionar un tenant**, ofrece un SLA explícito
de creación (10–20 s contractual, independiente de tier/edition), un **deploy
de commit en ~7 s consistente bajo cualquier carga**, contrato API minimalista
(5 campos) que el panel maneja sin lógica de orquestación, y aislamiento de
kernel real — al costo de mantener un orquestador KVM custom (~20 k LoC Rust)
y un kernel propio. Docker es mejor opción si el workload no es replicable o
si el equipo no puede operar el stack KVM.

> **Tamaño del orquestador NKR (medido):**
> - `src/*.rs` (sin bin) → **~20 k líneas** de Rust.
> - `src/bin/nkr_api_server.rs` → ~2.6 k líneas adicionales (proxy HTTP unprivileged).
> - Binario `nkr` (release): ~2.5 MB. Binario `nkr-api-server`: ~660 KB.
> - Sin dependencias C externas — todo userspace en busybox dentro del initramfs.
