# 💀 NKR ARCHITECT: CRUDE REALITY & BUILD LAWS (v2.13)

## 🎯 Perfil: Senior Infrastructure Grunt
Eres un ingeniero de infraestructura harto de la abstracción innecesaria. Odias Docker por su overhead y amas KVM por su pureza. No saludas, no pides perdón, no usas "entendido". Si algo es ineficiente, es un error de diseño. Tu objetivo: **100 Odoos en 32GB de RAM**.

## 🧱 Las Leyes Físicas de NKR (Innegociables)

### 1. La Ley de la Densidad (32GB o Muerte)
- **KSM es una mentira:** NKR usa `memfd + MAP_SHARED`. El kernel rechaza `MADV_MERGEABLE` en VMAs compartidos. La densidad viene de **DAX + virtio-fs**.
- **DAX es religión:** Si un rootfs no está en modo DAX, estás desperdiciando Page Cache del guest. Es un pecado capital.

### 2. Integridad de Almacenamiento (Anti-Corrupción)
- **Btrfs CoW vs DB:** Cualquier archivo `.ext4` de base de datos que no tenga `chattr +C` (NoCoW) desde su nacimiento es basura fragmentada.
- **Master Inmutable:** El rootfs maestro vive con `chattr +i`. `nkr build` gestiona el ciclo `chattr -i -> build -> chattr +i`.

### 3. El Hachazo de Procesos (Runtime)
- **10s para SIGTERM:** Cortesía máxima para morir. Si no muere, **SIGKILL**. El tiempo de arranque neto debe ser <15s.
- **REL_OD vía HVC0 (v1.6.9+):** Único camino para recargar código. **`dev_mode` debe ir vacío** — no `reload` (agota inotify, no funciona en virtio-fs), no `qweb,xml` (activa watchdog interno de Odoo que recompila templates en cada request → CPU spike + cuelgues, lección 2026-05-11). En tier=dev/staging, `cell.rs::rewrite_odoo_conf_full` fuerza `dev_mode =` vacío. **Mecánica del kill (v1.6.9 fix):** el supervisor en `initramfs::generate_init_script` lanza Odoo con `su -c 'echo \$\$ > /tmp/odoo.pid; exec /usr/bin/python3 -u /usr/bin/odoo …'` — el inner shell escribe su PID (= el del python3 tras `exec`) a `/tmp/odoo.pid`. El watcher hvc0 lee `/newroot/tmp/odoo.pid` (el watcher corre fuera del chroot) y hace `kill -KILL $pid` directo, sin `pkill -f` ni grace SIGTERM. SIGKILL es seguro en threaded (no hay master prefork que preservar). Tiempo total commit→reload: **~7 s consistente** (medido bajo carga real con websocket + cron). Diagnóstico vivo: cada paso del watcher se loguea a `<instance>/logs/nkr-watcher.log` (virtio-fs share visible desde host).
- **`nkr compose up -d` con detach correcto (v1.6.9+):** las VMs spawned hacen `setsid()` y escriben stdout/stderr **directo al boot log file** (Stdio::from(File), no pipes). El compose process sale limpio tras los health checks → todas las VMs quedan con `PPid=1` (init). Antes (bug pre-v1.6.9) cada `nkr compose up -d` quedaba colgado para siempre en `child.wait()` sosteniendo pipes; un `pkill -f 'nkr compose'` mataba TODAS las VMs en cascada. Verificación: `for p in $(pgrep -f 'nkr run'); do awk '/^PPid:/ {print $2}' /proc/$p/status; done | grep -c '^1$'` debe igualar el número de VMs activas.

## 🚀 Git & Addons: Plan C — Replace per-módulo + trash sibling (v2.10)

### 1. El Fin del D-State (Zombie Process)
- **Causa Raíz:** Modificar archivos "in-place" bajo virtio-fs asfixia al kernel del guest.
- **Ley de Oro:** Cada módulo en `addons/` se reemplaza atómicamente (rename per-module). El dir `addons/` top-level **NUNCA cambia de inodo**.

### 2. Protocolo de Intercambio per-módulo
1. **Prepare:** Generar staging en `addons.staging/`.
2. **Por cada módulo en staging:**
   - Si existe en `addons/<m>` → `rename(addons/<m>, <instance>/.nkr-trash/<ts>-<m>)`. El trash es **HERMANO de `addons/`**, fuera del `addons_path` → Odoo nunca lo escanea.
   - `rename(addons.staging/<m>, addons/<m>)` — atómica per-módulo.
3. **Signal:** Inyectar `REL_OD` vía HVC0 inmediatamente.
4. **Cleanup lazy (60s después):** background thread hace `rm -rf <instance>/.nkr-trash/*`. POSIX inode semantics: los fds del Odoo viejo siguen vivos contra el inode movido al trash hasta que el proceso muere y el supervisor respawnea.

### 3. Lecciones aprendidas 2026-05-11 (NO REGRESAR)
- **NO usar `renameat2(RENAME_EXCHANGE)` del top-level `addons/`**: virtio-fs no propaga la invalidación del inode al guest. El guest cachea el viejo inode y empieza a ver archivos fantasmas tras el cleanup (`Some modules are not loaded` + ENOENT random).
- **NO meter el trash dentro de `addons/`** (ni con dot prefix): Odoo 19 `update_list()` escanea TODOS los dirs incluyendo dotfiles. `addons/.nkr-trash-<m>/` crashea con `FileNotFoundError: Invalid module name` cada vez que la UI hace Update Apps List. El trash **DEBE** ser sibling de `addons/`.

### 4. Manejo de Errores
- **Strict Error Handling:** No enmascarar `EACCES` o `ENOSPC`. Si un rename falla, el deploy aborta dejando estado mixto (lo ya renombrado queda); `POST /actions {restart}` recupera consistencia.

## 🔐 SSO HMAC + `systemouts-addons` (v1.6.4 — sprint security/audit-sprint1)

### 1. Contrato
- NKR firma URLs `https://<dns>/nkr-sso?u=<login>&exp=<ts>&sig=<hmac_sha256>` con una HMAC key de 256 bits, **única por tenant**, escrita por `cell.rs::rewrite_odoo_conf_full` al clone en `odoo.conf` **sección `[nkr_sso]` clave `secret`** (NO en `[options]` — eso genera el WARNING `unknown option` de Odoo; las keys de otras secciones no). Legacy (1.6.3): `nkr_sso_secret` en `[options]` — sigue funcionando como fallback en `handle_sso` (`api.rs`) y en el módulo (`_nkr_sso_secret()`).
- TTL = 30s. Módulo Odoo `nkr_sso` verifica HMAC constant-time + crea sesión sudo sin pedir password. **El password del user JAMÁS sale del host.** Comprometer el secret = login arbitrario → rotar = editar `[nkr_sso] secret` en `odoo.conf` + `POST /actions {restart}`.
- Odoo 19 NO expone secciones no-`[options]` en `config` (no hay `config.misc`) → el módulo re-parsea el rc file con `configparser` para leer `[nkr_sso] secret`. NKR-side (`handle_sso`) hace un grep del archivo (`parse_conf_value`). Spec completa: `nkr_sso.md` §2/§3.

### 2. `systemouts-addons` — addons internos cell-level, invisibles al cliente
- Dir `cells/<cell>/systemouts-addons/` — vive a nivel cell, montado **RO** en cada instancia como `/mnt/systemouts-addons`, e insertado en `addons_path` **antes** de `/mnt/extra-addons` (el `addons/` del tenant) → un módulo del cliente con el mismo nombre que uno interno NO puede shadowearlo. `POST /addons/git` NUNCA lo toca → el cliente no lo ve y no puede sobrescribirlo. Una sola copia por cell, RO. Hoy contiene `nkr_sso/` (en `cells/odoo-v19/systemouts-addons/`).
- **El mountpoint `/mnt/systemouts-addons` DEBE existir en la imagen del rootfs maestro** (`Nkrfile.odoo*` lo crea junto a `/mnt/extra-addons` etc.) — el rootfs se monta RO en el guest, así que el initramfs no puede `mkdir` un mountpoint nuevo. **Lección 2026-05-12**: agregar una share virtio-fs con un guest path nuevo (`/mnt/foo`) requiere que `/mnt/foo` exista en el rootfs maestro; si no → `mkdir: Read-only file system` + `mount ... failed: No such file or directory` (no fatal, el boot sigue, pero el share no monta). Fix: `mkdir /mnt/foo` en el `Nkrfile`, o (quirúrgico) `chattr -i` + loop-mount RW + `mkdir` + umount + `chattr +i` en `images/<v>.ext4` y en el `<cell>-odoo-template/odoo.ext4`. **MITIGADO (v1.6.4, Step 2):** el initramfs ahora monta un tmpfs sobre `/mnt` del guest → cualquier `/mnt/*` nuevo "just works" sin tocar el rootfs maestro; lo de arriba ya no aplica para shares futuros (`/mnt/systemouts-addons` sigue horneado por inercia, no por necesidad). Ver §📌 Pendientes/Completado.
- **Distribución del módulo `nkr_sso`:** pre-instalado en cada `<cell>-odoo-template` (código en `cells/<cell>/systemouts-addons/nkr_sso/` + `state=installed` en `db-<cell>-odoo-template`). Los clones heredan ambos vía `CREATE DATABASE … TEMPLATE` (DB) + el share RO cell-level (código). Cero trabajo del panel per-tenant. El secret se regenera fresh por tenant (no se hereda del template).

### 3. Endpoints HTTP

| Verb | Path | Función |
| :--- | :--- | :--- |
| POST | `/api/v1/cells/{cell}/instances/{name}/sso` | Emite URL HMAC TTL 30s. Body: `{"user": "<login>"}` (default `admin`). |
| GET/POST | `/api/v1/cells/{cell}/instances/{name}/diag` | Captura HOST-side stacks/wchan/cpu de threads del proceso `nkr` del tenant (text/plain, ~50ms, idempotente). Usado pre-restart para forensics. |

## 🛡️ Watchdog (v1.6.3+, REL_OD-aware desde v1.6.8) — **ACTIVO**

`src/watchdog.rs` — thread del daemon. Cada `PROBE_INTERVAL_SECS=15` sondea TCP `:8069` por tenant `running`. Tras `HUNG_THRESHOLD_SECS=120` consecutivos sin respuesta, dispara `restart` automático vía `api::handle_action`. Bypass: env var `NKR_WATCHDOG_DISABLED=1`.

- **Estado actual (2026-05-15): HABILITADO**. Threshold subido 60s → 120s en v1.6.8 tras observar falsos positivos durante deploys legítimos (REL_OD con websockets activos + cron en curso podía tardar 60+s en Odoo "Initiating shutdown" en v1.6.4 antes del fix de SIGKILL directo).
- **Grace REL_OD-aware (v1.6.8)**: `api::handle_reload_workers` llama a `watchdog::note_reload(nkr_name)` justo después de inyectar SIGUSR1. Durante los siguientes `RELOAD_GRACE_SECS=240`, el threshold efectivo sube a `RELOAD_THRESHOLD_SECS=180` (en vez de 120) — cubre el peor caso del deploy sin retrasar detección de cuelgues reales en VMs ociosas.
- **Combinado con el fix de v1.6.9** (REL_OD via `/tmp/odoo.pid` escrito por supervisor + SIGKILL directo del watcher hvc0, ver §Gestión de Procesos), un commit deploy completa el ciclo en ~7s consistente — MUY por debajo del threshold de 120s/180s. El watchdog solo dispara en cuelgues reales (Odoo D-state, kernel deadlock, etc.).
- Costo: una probe TCP per running tenant cada 15s, negligible.

## 🩺 Boot console por instancia (v1.6.4)

`compose.rs` — cada VM escribe stdout (serial console del guest: echos del initramfs + `dmesg`) + stderr (logs del VMM `nkr run`) a `<instance_dir>/.<config_name>-vm-boot.log` (se trunca en cada arranque). Útil para diagnosticar mounts virtio-fs, panics del guest, truncación de cmdline, etc. — antes todo eso se mezclaba en `nkr-compose.log` (compartido + rotado).

## 📊 Matriz de Perfiles (Odoo 19 Optimized)

**Fuente de verdad: `src/api.rs::derive_resources_for_tier`** (aplica solo a tenants Odoo creados vía API; pg/pgbouncer siguen su sizing en el `nkr-compose.yml` de la cell). Si esto y el código divergen, gana el código.

| Tier | VM RAM | chrs | Workers | Soft Limit | Hard Limit | Balloon boot/ACTIVE | Balloon IDLE | Decay |
| :--- | :--- | :--- | :--- | :--- | :--- | :--- | :--- | :--- |
| **DEV** | **1300 MB** | 5 | 0 | 800 MB | **1000 MB** | 0 | 256 | 600s |
| **STAGING** | **1024 MB** | 5 | 0 | 600 MB | **700 MB** | 256 | 768 | 600s |
| **PROD** | `max(1024, 512+768·W)` (W=2→**2048**) | `2W+1` | W (≥1, def 2) | 640·W MB | **768·W MB** | 0 | 0 (estático) | n/a |

- PROD nace y se queda **ACTIVE** (balloon=0, sin decay) — "evita latencia de desinflado en picos". DEV/STAGING transicionan a IDLE tras 600s sin tráfico (squeeze hasta dejar ≥256 MB reales al guest).
- DEV se subió de 768→1300 MB (soft/hard 400/512→800/1000) en v1.6.2 tras `Server memory limit reached` ciclando con Odoo 19 + ~31 módulos custom en threaded mode.
- `dev_mode` vacío en DEV/STAGING (no `reload`, no `qweb,xml` — ver §El Hachazo de Procesos + `BUG_inotify_dev_mode.md`).
- Override de `workers>4` en PROD → balloon=0 obligatorio (regla del Grifo); ya se cumple porque PROD siempre deriva balloon=0. RAM mínima = la fórmula de arriba; el daemon nunca acepta menos.

> **Nota:** Odoo 19 requiere >512MB de Address Space solo para el bootstrap. Hard Limit < ~512MB garantiza OOM-kill al arranque.

## 🎈 Política de Ballooning Dinámico
### 1. El Límite de Seguridad (The Floor)
La VM **NUNCA** baja de **256 MB** de RAM real. Si el balloon infla más, el OOM-Killer mata el `init`.

### 2. Estados (Híbrido API + Decay)
- **ACTIVE (Default al Boot):** Balloon = 0. Toda VM nace en ACTIVE para sobrevivir al bootstrap (~350MB RSS).
- **IDLE (Inactiva):** `balloon_mb = VM_RAM - 256 MB`. Se activa tras `decay_secs` sin tráfico.
- **Trigger:** Nginx Proxy o Panel notifican vida a la API de NKR para resetear el timer de decay.

## 🧬 Provisión: El "Golden Path"
### 1. Estado "Cold-Prepared" (POST /instances)
- La VM se prepara (Reflink disco + Template DB), pero queda **APAGADA** (`running: false`) salvo que el panel mande `admin_user_password` (entonces el job background también arranca + setea el password del user `admin`).
- El arranque es un acto explícito post-configuración de red/proxy.
- **`POST /instances` es ASÍNCRONO (v1.6.4+):** valida sync (todos los 4xx al toque), despacha el clone en un thread, devuelve `202 {nkr_name, poll}`. El panel pollea `GET /cells/{cell}/instances/{name}/create-status` hasta `status=ready|failed`. Status file: `/mnt/nkr/cells/{cell}/.nkr-creates/{name}.json` (a nivel cell para sobrevivir al rollback del clone). Motivo: PROD prefork bootea ~140s = justo el límite del readiness wait de `nkr compose up` → clientes con timeout corto (panel, Cloudflare ~100s) veían 504 aunque el create terminara OK (caso `johao-y-richavo`, 2026-05-11).

### 2. Seeding de Datos
- **Instancias Limpias:** `cp --reflink` de master rootfs + `CREATE DATABASE FROM TEMPLATE db-master-{ver}`.
- **Prohibición:** Prohibido usar dumps SQL en el flujo estándar. PG Template es O(1).

## 🧬 Gestión de Procesos & Reload (HVC0)
### 1. Supervisor Loop (Guest)
- **workers=0:** `nkr-start.sh` corre Odoo en un `while true`. `REL_OD` -> `pkill -TERM` -> Respawn instantáneo.
- **workers>0:** `REL_OD` -> `pkill -HUP` al master.
- ✅ **`POST /reload` en workers=0 — ARREGLADO (v1.6.4):** el watcher hvc0 del initramfs (`src/initramfs.rs`, template per-instance) ahora detecta el modo Odoo leyendo `workers = N` de `/etc/odoo/odoo.conf` del guest: si vacío o `0` → `pkill -TERM -f /usr/bin/odoo` (el supervisor loop de `nkr-start.sh` lo respawnea con código fresh); si `>0` → `pkill -HUP` al master (prefork). Sin downtime de la VM en ningún caso. Verificado 2026-05-12 en `intech-devp` (workers=0): `POST /reload` → `:8069` vuelve en ~3s, `/nkr-sso`→303. (Antes: `pkill -HUP` siempre → en threaded no respawneaba → `:8069` caído; el workaround era `POST /actions {restart}` — ya no hace falta.)

### 2. Kernel cmdline — tope de tamaño
El nano-kernel tiene un `COMMAND_LINE_SIZE` chico (~1024 bytes). Con muchas shares virtio-fs el cmdline se truncaba (se perdían `init=` y `nkr.ip=` del final, y parcialmente `nkr.rootfs=` → boot frágil). **Fix (v1.6.4, `vmm.rs::configure_linux_boot`):** omitir lo redundante — el rootfs (`guest_path == "/"`) solo emite su `virtio_mmio.device` (el initramfs lo monta vía `nkr.rootfs=`, no via `nkr.fs0=`), y `nkr.fsr{i}=` solo se emite cuando es `ro` (el initramfs trata la ausencia como `rw`). ~60 bytes de holgura. Si volvés a quedarte sin espacio: acortar más (combinar `nkr.fs/fsm/fsr` en un param, o subir `COMMAND_LINE_SIZE` en el build del kernel).

## 🚀 Overrides de Escala (Regla del Grifo)
- **Workers > 4:** Requiere `balloon_mb = 0` (Sin elasticidad).
- **Fórmula RAM Mínima:** `VM_RAM >= (512 + (Workers * 768))`. 
- **Cgroup Guard:** Rechazar con `400 RAM_INSUFFICIENT_FOR_WORKERS` si no se cumple el mínimo.

---

## 📌 Pendientes — sprint security/audit-sprint1 (estado 2026-05-12)

Branch: `security/audit-sprint1`. Working tree con cambios no committed.

### Completado
- ✅ **SSO HMAC** — `src/api.rs` `handle_sso` (HMAC-only, sin password; lee `[nkr_sso] secret` con fallback a `nkr_sso_secret` de `[options]`) + `handle_diag` + `handle_create_status`. `src/ipc.rs`+`ipc_server.rs`: variantes `Sso`/`Diag`/`GetCreateStatus`. `src/bin/nkr_api_server.rs`: rutas `POST /sso` + `GET|POST /diag` + `GET .../create-status`. `Cargo.toml`: deps `hmac`/`sha2`/`base64`. `nkr_sso.md`: spec completa del módulo Odoo.
- ✅ **`src/cell.rs::rewrite_odoo_conf_full`** — genera la HMAC key random 256 bits per-tenant en `[nkr_sso] secret` (migra el legacy `nkr_sso_secret` de `[options]` preservando el valor; idempotente). También: inyecta el share `systemouts-addons` (`append_compose_block`) + `/mnt/systemouts-addons` en `addons_path` (2ª entrada, antes de `/mnt/extra-addons`); creación de cell crea `cells/<cell>/systemouts-addons/`.
- ✅ **`POST /instances` ASÍNCRONO (v1.6.4)** — `handle_create` valida sync, despacha clone en background, devuelve 202; `handle_create_status` + endpoint `GET /cells/{cell}/instances/{name}/create-status`; status file en `/mnt/nkr/cells/{cell}/.nkr-creates/{name}.json`; guard `inflight_creates`. Docs en `NKR_API.md` §4.4/§4.4.1/§8.1. Resuelve el 504 falso (caso `johao-y-richavo`).
- ✅ **`systemouts-addons` — DESPLEGADO Y FUNCIONANDO (v19):**
  - **El bug del mount era**: `/mnt/systemouts-addons` no existía como mountpoint en el rootfs maestro → el initramfs no podía `mkdir`-earlo (rootfs RO) → `mkdir: Read-only file system` → mount fallaba. (NO eran las colisiones de IRQ; el dmesg del guest confirmó que todos los virtio-mmio devices registran OK, incluidos los de IRQ compartido. Y el guest es PIC-only — `APIC: ACPI MADT or MP tables are not detected` → solo IRQs 0-15.) **Fix**: `mkdir /mnt/systemouts-addons` agregado a `Nkrfile.odoo19`+`Nkrfile.odoo`, + edición quirúrgica (`chattr -i` + loop-mount RW + `mkdir`+chown+`.keep` + umount + `chattr +i`) de `images/odoo19.ext4`, `images/odoo.ext4`, y los `odoo.ext4` de los 2 templates. Verificado: el template v19 bootea, `/mnt/systemouts-addons` monta, addons_path lo incluye.
  - `cells/odoo-v19/systemouts-addons/nkr_sso/` — el módulo (con `author="SystemOuts"`, `_nkr_sso_secret()` que re-parsea el rc file). `nkr_sso` `installed` en `db-odoo-v19-odoo-template` (vía `update_list()` JSON-RPC + `POST /modules/install`). → cada instancia v19 nueva hereda `nkr_sso` ya installed (DB) + el código (share RO cell-level).
  - `intech-devp` MIGRADO: `nkr_sso/` movido fuera de su `addons/` (el cliente ya no lo ve); share `systemouts-addons` + `/mnt/systemouts-addons` en addons_path + el mountpoint en su `odoo.ext4` + `[nkr_sso] secret` en su `odoo.conf`. Verificado: `/nkr-sso` → 303, `login OK`, sin warning `unknown option`.
  - `odoo-v19-odoo-template`: `nkr_sso/` removido de su `addons/`; share + addons_path entry + mountpoint en su ext4; `nkr_sso` installed en DB; `disabled: true`, parado.
- ✅ **Truncación del cmdline** — `vmm.rs::configure_linux_boot`: omitir `nkr.fs0/fsm0/fsr0` del rootfs + `nkr.fsr{i}=` solo cuando `ro`. ~60 bytes de holgura. Verificado: el template (7 shares, caso peor) ahora tiene cmdline completo (`...nkr.rootfs=... init=/sbin/init nkr.ip=...`).
- ✅ **Boot console por instancia** — `compose.rs`: `<instance>/.<name>-vm-boot.log` (serial del guest + logs del VMM). Fue lo que permitió diagnosticar el bug del mount.
- ✅ **Watchdog DESHABILITADO** — `Environment=NKR_WATCHDOG_DISABLED=1` en `/etc/systemd/system/nkr.service` (a pedido).
- ✅ **`POST /reload` en workers=0** — el watcher hvc0 del initramfs ahora diferencia modo Odoo: workers=0 (threaded) → `pkill -TERM` (supervisor loop respawnea); workers>0 (prefork) → `pkill -HUP` master. `api.rs::handle_reload_workers` + `vmm.rs` + comments actualizados. Verificado en `intech-devp` (workers=0): `POST /reload` → `:8069` vuelve ~3s. Adiós al workaround "usar `POST /actions {restart}` para workers=0".
- ✅ **Ballooning dinámico roto en `tier=dev` — ARREGLADO (v1.6.4):** el perfil dev nace ACTIVE con `balloon_mb=0` y transiciona a IDLE=256 MB tras `decay_secs`. Bug: `vmm.rs::configure_linux_boot` emitía el `virtio_mmio.device` del balloon **solo si `balloon_mb > 0`** → en dev el guest nunca probeaba el driver → el `set_target_mb(256)` del decay era no-op (el host cambiaba config space, nadie en el guest leía). Fix: emitir el MMIO device también cuando hay ballooning dinámico configurado (`balloon_idle_mb != 0 && != balloon_mb`). Verificado en `intech-devp` (perfil dev, decay temporal 120s): boot → `¡DRIVER_OK! Balloon listo.` + cmdline con `virtio_mmio.device=4K@0xd0040000`; decay → `Inflado: 65536 páginas totales (256 MB recuperados del guest)`; `POST /balloon` → `Desinflado: 0 páginas restantes`. (Staging no estaba afectado — nace con `balloon_mb=256 > 0`.) Nota: `intech-devp` tenía `balloon_idle_mb` ausente en su compose block (drift de creación previa) — agregado `balloon_idle_mb: 256` (= perfil dev) + restart. **Métrica runtime**: `state.rs::update_balloon_mb` — el vmm reescribe `balloon_mb` en su state file (`/tmp/nkr-vms/c{cell}-v{vm}.json`) en cada transición ACTIVE↔IDLE, así `nkr ps` / `/metrics` (`nkr_balloon_mb`) reflejan el target actual, no el de boot. Verificado: tras decay → `nkr_balloon_mb{vm="…"} 256`; al restart el `register_vm` lo resetea al valor de boot (0).
- ✅ **Step 2 — `/mnt` tmpfs en el initramfs (NO overlayfs)** — `src/initramfs.rs` (ambos templates, bloque "Breathing zones"): tras montar el rootfs RO en `/newroot`, `[ -d /newroot/mnt ] && mount -t tmpfs tmpfs /newroot/mnt`. Así `/mnt` del guest es siempre RW → cualquier share virtio-fs con un guest path nuevo bajo `/mnt/*` (`mount -t virtiofs ... /mnt/foo`) hace `mkdir -p` sobre el tmpfs y "just works" **sin rebuild del rootfs maestro**. El tmpfs *shadowea* el `/mnt` horneado (que solo tiene `.keep`s vacíos — los reales son mountpoints, no contenido). No es overlayfs (descartado: toca el boot path entero, riesgoso); es el patrón "breathing zones" existente extendido a `/mnt`. Verificado 2026-05-12 en el template v19 (7 shares, caso peor) **y en `intech-devp` (tenant real)**: `[DBG] /mnt → tmpfs OK`, los 6/7 shares montan sobre el tmpfs, rootfs sigue `ro,relatime` (sin regresión), cmdline completo, `RootFS DAX: 256 MB` intacto, `/nkr-sso`→303, `/web/login`→200. **El fix quirúrgico del ext4 (mkdir en `images/*.ext4`) ya no es necesario para shares nuevos** — sigue ahí para `/mnt/systemouts-addons` actual pero futuros `/mnt/*` no lo requieren. (Hay también un `[DBG] rootfs mount opts: ...` que loguea `grep ' /newroot ' /proc/mounts` para confirmar de un vistazo que el rootfs montó como se esperaba.)
- ✅ **`handle_pylibs_put` — warning cosmético eliminado** — `pip install` ahora corre con `umask 022` en el child (`pre_exec` override del `UMask=0077` del systemd unit del proxy) → los archivos quedan 0644/0755 = world-readable, que es lo que el guest (uid 101, virtio-fs sin UID remap) necesita. Borrado el `chmod -R go+rX` posterior (que logueaba `Operation not permitted` benigno sobre el `pylibs/lib/` root-owned, y era redundante porque ese dir ya es 0775).
- ✅ **Métricas del *guest* — CPU + RAM-host + disco + salud (v1.6.4)** — `metrics.rs`:
  - `read_cgroup_stats(vm_name)` lee `/sys/fs/cgroup/nkr/<vm>/{cpu.stat,memory.current}` → `nkr_cpu_seconds_total{vm}` (counter, `usage_usec` — incluye virtiofsd/vhost, supersede al jittery `nkr_cpu_pct`), `nkr_cpu_throttled_seconds_total{vm}` (counter, `throttled_usec`), `nkr_cgroup_memory_bytes{vm}` (gauge, `memory.current` — más completo que `nkr_rss_mb`).
  - `disk_usage_for_vm` + `du_bytes_cached` (caché **5 min**, `DU_TTL`) + `find_instance_dir` (glob `cells/*/instances/<vm.name>`) + `stat_block_used_total` → array `disk` (mounts: `addons|filestore|logs|pylibs` via `du`, `disk:<stem>` via `st_blocks`; skipea el rootfs `images/*.ext4`). **NO en `/metrics`** (un `du` por scrape × 100 VMs sería O(seg)) — sólo en el endpoint per-instancia (abajo).
  - `nkr_up{vm,cell,tier}` (gauge 1/0, incluye tenants parados — `discover_tenants()` scanea `cells/*/instances/*/meta.json` + agrega `<cell>-db`/`-pgb`) — métrica info para joins. + `nkr_build_info{version}`, `nkr_vm_count`, `nkr_total_{rss,balloon,dax_savings}_mb`.
  - **Endpoint per-instancia `GET /api/v1/cells/{cell}/instances/{name}/metrics`** (`IpcRequest::MetricsForVm` → `metrics::vm_metrics_json`) → JSON snapshot de UNA VM (cgroup CPU/mem, balloon, dax, rss, net, io, `disk[]`, cell/tier, uptime, chrs, `as_of`, `stale`). **Caché server-side ~2s/VM** (`VM_METRICS_CACHE`, `VM_METRICS_TTL=2s`; bajado de 30s — el panel quería gráficos cada 5s, luego cada 2s y el recompute es ~1ms, el `du` queda cacheado 5min aparte) → el panel pollea cada ~2s; la caché coalesce bursts y es el rate-limit (no devuelve 429). `guest_mem` se refresca cada ~10s (`BALLOON_STATS_INTERVAL_SECS`, bajado de 30s). VM parada → `{running:false,...}`; desconocida → 404. **Esto es lo que usa la pestaña Métricas del panel** (no `/metrics`, que es para un Grafana eventual). Ruta en `nkr_api_server.rs`, validación `is_safe_identifier`.
  - **RAM interna del guest — HECHO (v1.6.4):** `balloon.rs` extendido a 3 queues (statsq, índice 2, `VIRTIO_BALLOON_F_STATS_VQ` — antes anunciada pero no implementada). `process_stats()` drena el buffer del guest (`le16 tag, le64 val` × N — `MEMFREE/MEMTOT/AVAIL/CACHES/SWAP_IN/OUT/MAJFLT/MINFLT`); el vmm lo llama cada ~30s desde el vcpu loop (`BALLOON_STATS_LAST_TS`) y persiste vía `state::update_guest_mem` (4 campos `guest_mem_*_bytes` nuevos en `VmState`, `#[serde(default)]`). El daemon lo expone como `guest_mem:{total/available/free/cached_bytes}` en el JSON per-instancia + `nkr_guest_mem_*{vm}` en `/metrics`. Las rutas inflateq/deflateq quedaron byte-for-byte iguales — la statsq es aditiva (`qi < 2` → `qi < 3` en los handlers MMIO del balloon en `vmm.rs`, arrays `[_;2]`→`[_;3]`). **Verificado en `intech-devp`:** `Cola 'statsq' activada` + `DRIVER_OK`, `guest_mem` aparece tras ~40s (~1255 MiB total / ~922 available idle), `/metrics` con `nkr_guest_mem_*`, state file persiste, y el ciclo de ballooning (decay→inflado 256 MB→`POST /balloon`→desinflado→0) sigue funcionando + Odoo intacto (`/nkr-sso` 303).
  - Verificado: `/metrics` (~118 líneas, sin `nkr_guest_disk`, scrape <100ms), endpoint per-VM (JSON completo con `guest_mem`, cache hit → `stale:true`, 404 en VM inexistente).
  - **Pendiente (lo único que queda de métricas, futuro):** vista global/host para ops — `nkr_host_mem_*` (`/proc/meminfo`), `nkr_host_cpu_seconds_total` (`/proc/stat`), `nkr_host_disk_*` (`statvfs`). Spec en `NKR_API.md §4.1.1`. (También baratos: la salvedad del `disk{mount=filestore}=0` cuando es `.ext4` share — se arregla cuando `VmState` lleve la lista de shares.)
- **Deploy**: `Cargo.toml` → 1.6.4; `cargo build --release`; binarios en `/usr/local/bin/` vía rename atómico (backup `.pre-async-create`); `systemctl restart nkr nkr-api-server` (`KillMode=process` → VMs guest intactas); `/health` → 1.6.4.

### Pendiente
1. **Métricas — TODO LO DEL TENANT HECHO** (host-side per-VM + `nkr_up`/build/totales en `/metrics`; CPU/RAM-host/disco/RAM-interna-del-guest en el JSON per-instancia `GET .../instances/{name}/metrics`). Las pruebas de métricas las hace el panel. Lo único que queda (futuro, para ops, no urgente): **vista global/host** — `nkr_host_mem_*` (`/proc/meminfo`), `nkr_host_cpu_seconds_total` (`/proc/stat`), `nkr_host_disk_*` (`statvfs` de `/mnt/nkr` y `/`) + un dashboard que junte eso con el agregado per-VM. (Barato de paso: `disk{mount=filestore}=0` cuando el filestore es un `.ext4` share — se arregla cuando `VmState` lleve la lista de shares.)
2. **Coordinar con panel**: si el panel mantiene una copia del módulo `nkr_sso` en su repo, necesita el helper `_nkr_sso_secret()` nuevo (lee la ruta del rc file con `config["config"]` — `config.rcfile` deprecado en 19.0 — + `configparser` para `[nkr_sso] secret`). La spec `nkr_sso.md` §2/§3 ya lo tiene; cuando sincronicen con la spec, alineado. (✓ Ya hecho: el panel quitó `nkr_sso` de su repo de addons del cliente — ahora vive solo en `cells/<cell>/systemouts-addons/`. Verificado en `intech-devp`: `addons/` sin `nkr_sso`, se sirve desde `/mnt/systemouts-addons`.)
3. **v17 — DIFERIDO** (a pedido — esperar a juntar más cambios para no actualizar el rootfs/template v17 a cada rato): `cp -a` de `nkr_sso/` a `cells/odoo-v17/systemouts-addons/nkr_sso/` (manifest `version: "17.0.1.0.0"` si Odoo 17 lo exige; el mountpoint `/mnt/systemouts-addons` ya está en el rootfs v17) → `update_list()` JSON-RPC en el template v17 → `POST /modules/install nkr_sso`. Revisar compat de `_compute_session_token` en Odoo 17 antes.
4. **PR del sprint** — el commit ya está hecho y pusheado: `738743e` en `security/audit-sprint1` ("v1.6.4: SSO HMAC + systemouts-addons + async create + guest metrics + balloon/reload fixes", 27 files, +5850/−1081; incluye `BUG_inotify_dev_mode.md`, `deploy/fail2ban/`, `nkr_sso.md`, `src/watchdog.rs`). Falta abrir el PR a `main` cuando se decida. (Nota: el repo `deploy/systemd/nkr.service` tiene el `KillMode=process` pero NO el `Environment=NKR_WATCHDOG_DISABLED=1` — eso es un override runtime-only en `/etc/systemd/system/nkr.service`, a propósito; el default del template = watchdog habilitado. El módulo `cells/.../systemouts-addons/nkr_sso/` vive bajo `/mnt/nkr/`, fuera del repo.)

### Estado al cerrar (2026-05-12)
- NKR daemon + nkr-api-server `active`, v1.6.4. **Watchdog OFF** (`NKR_WATCHDOG_DISABLED=1`).
- Tenants: `intech-devp` running ✓ (reiniciado 2026-05-12 con el initramfs nuevo de Step 2 — `/mnt → tmpfs OK`, shares OK; SSO `/nkr-sso` → 303, `web/login` → 200, migrado a `systemouts-addons`, `addons/` sin `nkr_sso`, `odoo.conf` con `[nkr_sso] secret`). `johao-y-richavo` — fue borrado por el panel (cleanup del 504 síncrono; el create async lo resuelve). Template v19 parado + `disabled: true`, `nkr_sso` installed en su DB. v17 master+template tienen el mountpoint `/mnt/systemouts-addons` pero todavía no `nkr_sso` (ver Pendiente #1).
- Working tree: ver `git status` — todo sin commitear, esperando cierre de pendientes.