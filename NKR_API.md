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
  # → {"ok":true,"version":"1.3.0"}
  ```

### Conceptos mínimos

- **Cells hoy disponibles:** `odoo-v17` (PG16 + PgBouncer, Odoo 17.0) y `odoo-v19` (PG16 + PgBouncer, Odoo 19.0). Ambas con 19 slots libres (1 ocupado por el template).
- **Patrón mental:** cell = rack (versión fija + infra compartida), instancia = tenant.
- **Cada cell tiene un `<cell>-odoo-template` reservado** como source automático de `mode=production`. Está apagado por default — su DB vive en PG y los archivos en disco; eso basta para clonar. Ver §4.4 "Convención del template".
- **`POST /instances` deja la VM preparada pero APAGADA** (`running: false`). Secuencia del panel:
  1. `POST /instances` con `mode=production` (cliente nuevo) o `mode=dev + source` (clone de tenant existente). NKR clona archivos + DB.
  2. `POST /addons/git` / `PUT /pylibs` — opcional, con VM apagada.
  3. `POST /actions {action:"start"}` — devuelve `202` en <50 ms (async desde v1.5.1). El boot real tarda ~30-60 s; el panel **debe polear** `GET /instances/{name}` → `nkr_status.phase` hasta `loading|ready`.
  4. Updates posteriores (git pull de addons, pip install nuevo) → `POST /actions {action:"restart"}` (también async — ver §4.8).
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
| `201`  | Instancia creada |
| `202`  | Acción aceptada (start/stop/restart) |
| `400`  | JSON inválido, identificador inválido, action desconocida |
| `401`  | Bearer token mal o ausente |
| `404`  | Instancia no existe |
| `409`  | Conflicto: `cell_full`, `version_mismatch`, `no_cell_available`, `no_template_source`, `action_in_progress` |
| `413`  | Body excede el límite |
| `500`  | Error interno (clone_failed, etc.) |
| `502`  | Daemon UDS no responde (`daemon_unreachable`) |
| `503`  | Server busy — `retry_after: 1s` |

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

Formato `text/plain` exposition 0.0.4. Usar para Grafana/Prometheus scraping.

### 4.2 `GET /api/v1/health` — Health check (sin auth)

```bash
curl -s http://nkr-host:9090/api/v1/health
# → {"ok":true,"version":"1.3.0"}
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

El panel sólo conoce la versión de Odoo que el cliente necesita. NKR elige la cell menos llena que matchee.

**Body:**
```json
{
  "nkr_name": "cliente-42",
  "mode": "dev",
  "odoo_version": "17.0",
  "admin_passwd": "panel-lo-genera-y-guarda-encriptado-16-a-128-chars",
  "workers": 2,
  "dns": "cliente-42.systemouts.com",
  "edition": "community",
  "proxy_mode": null,
  "source": null,
  "cell": null,
  "addons_path": null,
  "pg_version": null,
  "python_libs": [],
  "balloon_mb": null
}
```

**Campos obligatorios:** `nkr_name`, `mode`, `odoo_version`, `admin_passwd`. Todo lo demás es opcional.

**Sizing — `workers` es la única input.** El panel sólo manda `workers` (entero). NKR deriva automáticamente el resto:
- **Compose (VM-level):** `chrs` (CPU quota cgroup, 1 chr = 20 % de un core), `ram` (MB de RAM física que KVM asigna al guest), `balloon_mb` (MB que el guest devuelve al host vía VirtIO-Balloon en el boot).
- **odoo.conf (proceso-level):** `workers`, `limit_memory_soft`, `limit_memory_hard`.

| Campo | Valor | Efecto |
|-------|-------|--------|
| `mode` | `"production"` | **Tenant fresh para cliente nuevo.** NKR clona archivos + DB del template `<cell>-odoo-template` (DB con base+web preinstalados, sin datos). `source` no se debe mandar — NKR lo fuerza al template. Boot rápido (~30-60 s). |
| `mode` | `"dev"` | **Clone de un tenant existente** (typical: clonar producción para staging). `source` **obligatorio** (nkr_name del tenant a clonar). NKR hace `CREATE DATABASE ... TEMPLATE` desde la DB del source. |
| `cell` | `null` | Auto-selecciona la cell con `odoo_version` match y menos `used_odoos`. |
| `cell` | `"foo"` | Fuerza esa cell; 409 si versión no matchea o está llena. |
| `source` | `null` (con `mode=production`) | NKR usa `<cell>-odoo-template` automáticamente. **No mandar source en mode=production**: si lo mandás, 409 `source_not_allowed_in_production`. |
| `source` | `"<tenant>"` (con `mode=dev`) | **Obligatorio.** Tenant a clonar (mismo cell). Si falta en `mode=dev` → 400 `source_required`. |
| `edition` | `"enterprise"` \| `"community"` \| `null` | **Determina si la instancia monta `/mnt/extra-enterprise`** (share del repo enterprise descargado vía 4.11). `community` (o `null`): no se monta y no se incluye en `addons_path` → tenant 100% community. `enterprise`: se monta y se incluye → tenant ve los módulos enterprise. **Cambio post-creación**: para upgrade community→enterprise, el panel debe llamar `PATCH /config` con `addons_path` extendido manualmente más restart; el remount automático no está implementado todavía. |
| `admin_user_password` | string `[A-Za-z0-9._-]{8,128}` \| `null` | **OPCIONAL.** Password de login del user `admin` del tenant (login web, distinta del `admin_passwd` master). Si se manda, NKR garantiza el flujo completo antes del 201: (1) `nkr compose up -d` para arrancar el tenant, (2) polling :8069 hasta TCP up (max 120 s), (3) JSON-RPC login `admin/admin` (la default heredada del template) → `res.users.change_password`. Si todo OK, devuelve 201 con tenant arrancado y password seteada. Si falla algún paso, 503 `admin_password_setup_failed` (tenant queda arrancado pero sigue con `admin/admin`). Si se omite, comportamiento legacy: NKR sólo clona — el panel debe luego `POST /actions {start}` y opcionalmente cambiar la password vía JSON-RPC manual. **Recomendado siempre mandarla en producción** para cerrar la ventana de `admin/admin`. |
| `python_libs` | `[]` | Si no vacío: 500 hoy (requiere rebuild del master ext4 — pendiente). |
| `workers` | int `1..=16` \| `null` | **Single source of truth de sizing.** Default `2` si null. NKR deriva chrs+ram+balloon (compose) y limit_memory_soft/hard (odoo.conf) automáticamente — ver tabla abajo. |
| `balloon_mb` | int `0..` \| `null` | **OPCIONAL.** Override del VirtIO-Balloon. Si `null` (recomendado), NKR aplica el default derivado de `workers` (ver tabla). Si se manda valor explícito, ese valor reemplaza al default. `0` desactiva el balloon (no recomendado para Odoo en cells densas). Subirlo más allá del default agresivo (40 % de la RAM) puede provocar OOM-killer del guest bajo picos de carga (imports, generación de PDF, install/upgrade de módulos). |
| `admin_passwd` | string `[A-Za-z0-9._-]{16,128}` | **OBLIGATORIO.** Master password del Odoo del tenant. El panel es única fuente — lo genera, lo guarda encriptado, y lo manda en este body. NKR nunca lo genera ni lo devuelve. Se persiste sólo en `odoo.conf` del tenant. Si se omite → `400 admin_passwd_required`. |
| `proxy_mode` | `true` (default) \| `false` | Productivo SIEMPRE `true`. `false` sólo para tests locales sin nginx. |
| `list_db` | cualquier valor | **Ignorado**. NKR fuerza `False` en cells productivas (ver §9.1). |
| `addons_path` | cualquier valor | Se inyecta en `odoo.conf`. |

**Tabla de derivación de recursos:**

| `workers` | compose `chrs` | compose `ram` (MB) | compose `balloon_mb` | odoo.conf `limit_memory_soft` | odoo.conf `limit_memory_hard` | Uso |
|-----------|----------------|--------------------|----------------------|-------------------------------|-------------------------------|-----|
| 1 | 3 | 1024 (1 GB) | 0 (disabled) | 400 MB (419430400 b) | 750 MB (786432000 b) | dev / test / single-user |
| 2 (default) | 5 | 2048 (2 GB) | 292 | 800 MB (838860800 b) | 1500 MB (1572864000 b) | producción típica |
| 3 | 7 | 3072 (3 GB) | 566 | 1200 MB | 2250 MB | uso medio |
| 4 | 9 | 4096 (4 GB) | 840 | 1600 MB | 3000 MB | retail / muchos usuarios |
| 5 | 11 | 5120 (5 GB) | 1114 | 2000 MB | 3750 MB | alta carga |
| 8 | 17 | 8192 (8 GB) | 1936 | 3200 MB | 6000 MB | tenants pesados / multi-empresa |
| N | 2N+1 | 1024·N | max(0, 1024·N − 750·N − 256) ≈ 274·N − 256 | 400·N MB | 750·N MB | — |

**Fórmulas:**
- compose `chrs` = `2·workers + 1`
- compose `ram_mb` = `1024·workers`
- compose `balloon_mb` = `max(0, ram_mb − limit_memory_hard_mb − 256)`, **floor a 0 si el resultado es < 64 MB** (un balloon muy pequeño consume más overhead del que ahorra).
- odoo.conf `limit_memory_soft` (bytes) = `400 · workers · 1 MiB`
- odoo.conf `limit_memory_hard` (bytes) = `750 · workers · 1 MiB`

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

**Respuesta 201:**
```json
{
  "nkr_name": "odoo-v17-cliente-42",
  "cell": "odoo-v17",
  "vm_id": 6,
  "guest_ip": "10.0.1.7",
  "dns": "cliente-42.systemouts.com",
  "db_name": "db-odoo-v17-cliente-42",
  "addons_path": "/mnt/nkr/cells/odoo-v17/instances/odoo-v17-cliente-42/addons",
  "logs_path":   "/mnt/nkr/cells/odoo-v17/instances/odoo-v17-cliente-42/logs/odoo.log",
  "config_path": "/mnt/nkr/cells/odoo-v17/instances/odoo-v17-cliente-42/config/odoo.conf",
  "instance_dir":"/mnt/nkr/cells/odoo-v17/instances/odoo-v17-cliente-42",
  "meta": { ... },
  "nkr_status": {
    "running": false,
    "port_8069_up": false,
    "phase": "provisioning",
    "db_present": false,
    "odoo_version_running": "17.0"
  }
}
```

**Importante:** el panel envía `admin_passwd` en el body y es responsable de persistirlo encriptado **antes** de llamar. NKR lo escribe en `config_path/odoo.conf` y no lo devuelve en ninguna respuesta (ni en la 201 ni en GETs futuros). Si el panel pierde el valor, la única forma de recuperarlo es leer `config_path/odoo.conf` vía SSH al host NKR (fuera de la API).

**Notas importantes para el panel:**
- `nkr_name` devuelto puede tener prefijo de cell auto-añadido (si pediste `"cliente-42"` en una cell llamada `"odoo-v17"`, recibís `"odoo-v17-cliente-42"`). **Siempre usar el nombre devuelto**, no el que enviaste.
- `addons_path` es el path HOST (no del guest). El panel NO lo toca directamente; todo va por los endpoints §4.10 / §4.12.
- `port_8069_up` puede ser `false` al momento de la respuesta; poll `GET /instances/{name}` hasta que sea `true` antes de marcar "ready".

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

**Por qué `production` es ahora rápido:** NKR clona la DB del template via `CREATE DATABASE ... TEMPLATE` (CoW a nivel filesystem PG, ~5 segundos). Cuando Odoo arranca, encuentra la DB ya inicializada y sólo carga workers — boot completo en ~30-60 s. Si en cambio `mode=production` no copiara la DB, Odoo arrancaría contra una DB vacía y se autoinicializaría (cargando `base` desde XML/CSV) → 3-5 minutos. Ese era el comportamiento previo a v1.6 y la causa del 504 en boots iniciales.

**Errores específicos:**
```json
// 409 — panel mandó source en mode=production
{ "error":"source_not_allowed_in_production",
  "message":"mode=production siempre clona del template de la cell. Para clonar de otro tenant usá mode=dev con 'source' explícito." }

// 400 — panel no mandó source en mode=dev
{ "error":"source_required",
  "message":"mode=dev requiere 'source' explícito (nkr_name del tenant fuente). Para crear un tenant fresh usá mode=production." }

// 409 — versión no matchea en la cell forzada
{ "error":"version_mismatch", "cell":"foo", "cell_version":"17.0", "requested_version":"16.0" }

// 409 — no hay cell con esa versión
{ "error":"no_cell_available", "message":"...", "requested_version":"19.0" }

// 409 — cell llena
{ "error":"cell_full", "cell":"odoo-v17", "used":20, "max":20 }

// 409 — el template no existe (operador debe recrearlo)
{ "error":"cell_template_missing", "cell":"foo", "expected_template":"foo-odoo-template" }

// 500 — clone falló
{ "error":"clone_failed", "message":"...", "cell":"...", "source":"...", "nkr_name":"..." }
```

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
| `stop` | 1–30 s (típ. 2–5) | SIGTERM + drain de workers + cleanup. PG con checkpoint puede subir a 60 s. |
| `restart` | 5–35 s | stop + 200 ms + start. `skip_warmup=true` en clones evita los 55 s de warmup HTTP. |

**Cómo el panel debe orquestar un webhook (`git push` → restart):**
```
1. POST /addons/git { action:"sync" }      → 200 con sha nuevo
2. POST /actions    { action:"restart" }   → 202 instantáneo (async:true)
3. while (true):                              ← polling
     info = GET /instances/{name}
     if info.nkr_status.port_8069_up: break
     if elapsed > 180s: alert; break
     sleep 1s
4. log "[panel] restart OK en Xs, sha=…"
```

Si entre (2) y (3) el panel mete otro `POST /actions` (típico en CI con re-runs), el segundo recibe `409 action_in_progress` — **no es un fallo**, simplemente hay que polear hasta que el primero termine y luego decidir si re-disparar.

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

**Idempotencia:** cualquier `action` (`clone`, `pull`, `sync`) ejecuta un re-clone fresco a un tmp dir efímero (`addons/.nkr-tmp-<subdir>/`) y luego mueve los módulos. El `.git` no se preserva — para "actualizar" el panel vuelve a llamar al endpoint y NKR re-clona el árbol completo. Cada módulo movido lleva un tracker `addons/<m>/.nkr-source` (`repo_url=`, `ref=`, `sha=`) que sirve para distinguir overwrite legítimo (mismo repo) vs conflicto real (otro repo con módulo del mismo nombre → `409 module_conflict`).

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
               "account_fiscal_year"]
}
```

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

**Para que Odoo vea los módulos nuevos:** después del clone el panel debe (1) `POST /actions {action:"restart"}` para que Odoo recargue manifests y los módulos aparezcan en la tabla `ir_module_module`, o (2) en la UI ir a Apps → Update Apps List. **Sin restart o Update Apps List los módulos siguen invisibles aunque estén físicamente en `addons/`.**

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

**Después de llamar**: `POST /actions {action:"restart"}` para que Odoo recoja las libs nuevas.

**Errores:**
- `400 missing_or_invalid_requirements_txt` — body sin campo o vacío.
- `404 instance_not_found` — instancia no existe.
- `422 pip_install_failed` — pip falló. Body incluye `log_tail`. Causas típicas: wheel no disponible para el Python del guest + falta gcc, versión de dep no existe en PyPI, nombre typo.
- `500 write_requirements_failed` — ACL mal configurada (ver §11).

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
POST /instances           ← clone listo, VM aún apagada
POST /addons/git          ← repos del cliente (con github_token / deploy_key_b64)
POST /actions {start}     ← VM bootea
GET  /instances/{name}    ← poll hasta phase="loading"
POST /init-db             ← crea DB (async, devuelve 202)
GET  /instances/{name}    ← poll hasta nkr_status.db_present=true
POST /dns                 ← cert + vhost + (NEW) sella web.base.url
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

- **`workers`** → re-deriva `ram` + `chrs` + `balloon_mb` en `nkr-compose.yml` Y `workers` + `limit_memory_soft` + `limit_memory_hard` en `odoo.conf`. **Single input → 6 outputs coherentes** (ver tabla en §4.4). El balloon se recalcula en cada cambio de workers porque depende de `ram - limit_memory_hard`; mantenerlo desincronizado desperdicia densidad (RAM nueva sin donar al host) o starvea Odoo (RAM vieja donada cuando workers creció).
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

### 4.18 `POST /api/v1/admin/cache/purge` — Vaciar cache nginx (global)

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
    ← 201 con InstanceInfo (admin_passwd NO se devuelve — el panel ya lo tiene).
    Nota: running=false — clone NO auto-arranca la VM (phase=provisioning).
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
# → {"ok":true,"version":"1.3.0"}
```

**Config panel-side:**
```
NKR_API_BASE = https://nkr.cliente.com     (nginx al frente → 127.0.0.1:9090)
NKR_API_TOKEN = <token compartido>
```

---

## 8. Rate limits, timeouts y comportamientos defensivos

### 8.1 Timeouts recomendados (panel-side HTTP client)

**Crítico:** el panel debe configurar timeouts **por endpoint**. Un solo default (ej. 30 s) **no** sirve — varias ops tardan más por diseño (filesystem clone, install de Odoo, git clone de enterprise). Tabla:

| Endpoint | Duración típica | Timeout mínimo recomendado | Notas |
|---|---|---|---|
| `GET /metrics` | <100 ms | 5 s | |
| `GET /api/v1/health` | <10 ms | 5 s | |
| `GET /api/v1/cells` | <100 ms | 5 s | |
| `GET /instances/{name}` | 200-800 ms | 5 s | Incluye psql probe + TCP probe + `/proc/<pid>` read. |
| `POST /instances` | 30-45 s | **90 s** | `cp -a` del master + `e2fsck` + compose block write. |
| `POST /actions {start,stop,restart}` | <50 ms (async) | 5 s | **Async desde v1.5.1** — devuelve 202 inmediato; el trabajo real (5–35 s) corre en background. El panel polea `nkr_status.port_8069_up`/`phase`. Ver §4.8. |
| `GET /logs` (tail) | <500 ms | 10 s | Bounded 4 MiB. |
| `GET /logs` (long-poll, `wait_ms=5000`) | hasta `wait_ms` + ~500 ms | **`wait_ms + 10000`** | Sumar al `wait_ms`. |
| `GET /logs/download` | hasta 5 s | **30 s** | Cap 64 MiB; SSD local. |
| **`POST /init-db`** | **<500 ms** (async) | 10 s | **Asíncrono** — devuelve 202 inmediato; el panel poll `GET /instances/{name}`. |
| `POST /modules/{install,upgrade,uninstall}` | 10-300 s (síncrono) | **600 s** | Bloquea hasta completar la transacción de Odoo. |
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
r = httpx.post(f"{NKR}/instances", json=body, timeout=90.0)
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

# Crear tenant production (NKR auto-clona DB del template, boot ~30-60s)
ADMIN_PW=$(tr -dc 'A-Za-z0-9._-' < /dev/urandom | head -c 48)
curl -s -X POST $NKR_API_BASE/api/v1/instances -H "$TOK" -H "Content-Type: application/json" \
  -d "{\"nkr_name\":\"demo-1\",\"mode\":\"production\",\"odoo_version\":\"17.0\",\
       \"workers\":2,\"admin_passwd\":\"$ADMIN_PW\"}"
# → 201. NKR derivó: compose ram=1536/chrs=5, odoo.conf soft=600M/hard=1000M
# Source = odoo-v17-odoo-template (auto). NO mandar 'source' en mode=production.

# Crear tenant dev (clone de un tenant productivo para staging)
curl -s -X POST $NKR_API_BASE/api/v1/instances -H "$TOK" -H "Content-Type: application/json" \
  -d "{\"nkr_name\":\"staging-demo-1\",\"mode\":\"dev\",\"odoo_version\":\"17.0\",\
       \"source\":\"odoo-v17-cliente-prod\",\"workers\":2,\"admin_passwd\":\"$ADMIN_PW\"}"
# → 201. NKR clonó archivos + DB de cliente-prod via CREATE DATABASE TEMPLATE.

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
# → {"ok":true,"version":"1.3.0"}
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
# → {"ok":true,"version":"1.3.0"}

# Sanity: hay cells disponibles (esperás odoo-v17 y odoo-v19)
curl -s -H "$TOK" $NKR_API_BASE/api/v1/cells | python3 -m json.tool
```

Si `/health` falla: `nkr-api-server` no está corriendo, o nginx delante no enruta. Si `cells` devuelve `401`: token mal copiado.

### 12.2 Flujo E2E mínimo (Odoo 17, tenant de prueba)

```bash
# 1. Crear tenant (mode=production → sin DB clone; seguro en cells vírgenes)
#    Nota: la VM queda preparada pero NO arranca automáticamente.
RESP=$(curl -s -X POST -H "$TOK" -H "Content-Type: application/json" \
  -d '{"nkr_name":"smoke-1","mode":"production","odoo_version":"17.0","workers":2}' \
  $NKR_API_BASE/api/v1/instances)
echo "$RESP" | python3 -m json.tool
NAME=$(echo "$RESP" | python3 -c "import json,sys;print(json.load(sys.stdin)['nkr_name'])")
CELL=$(echo "$RESP" | python3 -c "import json,sys;print(json.load(sys.stdin)['cell'])")
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
