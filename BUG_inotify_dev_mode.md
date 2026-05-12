# Bug: `dev_mode=reload` agota inotify watches del guest → loop de respawn

**Fecha**: 2026-05-10
**NKR version**: 1.6.2
**Severidad**: Alta — rompe creación de cualquier tenant con `tier=dev` o `tier=staging`
**Status**: Diagnosticado, pendiente de fix

---

## Síntoma

`POST /api/v1/instances` con `tier=dev` (o `tier=staging`) timeoutea con 504 (o el panel reporta "tenant no responde :8069 en 120s"). El log del daemon muestra:

```
[API] admin_password_setup_failed: tenant 10.0.2.5 no respondió :8069 en 120s
[NKR-HEALTH] 'intech-dev' — intento 20/20 fallido
```

La VM está corriendo (visible en `nkr ps`), pero el puerto 8069 nunca levanta.

---

## Causa raíz

El `odoo.conf` que NKR escribe para tenants con `tier=dev`/`tier=staging` incluye:

```ini
dev_mode = reload,qweb,xml
```

Odoo activa entonces el watcher `watchdog` que llama recursivamente a `inotify_add_watch` sobre `/usr/lib/python3/dist-packages/odoo/addons` — el árbol de módulos del core de Odoo 19, **cientos de subdirectorios**.

El guest minimal de NKR (`nanolinux` initramfs) levanta con el default de kernel:

```
fs.inotify.max_user_watches = 8192
```

Odoo agota ese cupo durante el escaneo recursivo y `inotify_add_watch` retorna `ENOSPC` (errno 28).

Traceback exacto del log de Odoo (`logs/nkr-compose.log` del compose en cuestión):

```
File "/usr/lib/python3/dist-packages/odoo/cli/server.py", line 118, in main
  rc = server.start(preload=config['db_name'], stop=stop)
File "/usr/lib/python3/dist-packages/odoo/service/server.py", line 1622, in start
  watcher.start()
File "/usr/lib/python3/dist-packages/odoo/service/server.py", line 359, in start
  self.observer.start()
File "/usr/lib/python3/dist-packages/watchdog/observers/api.py", line 261, in start
  emitter.start()
File "/usr/lib/python3/dist-packages/watchdog/observers/inotify.py", line 119, in on_thread_start
  self._inotify = InotifyBuffer(path, self.watch.is_recursive)
File "/usr/lib/python3/dist-packages/watchdog/observers/inotify_buffer.py", line 37, in __init__
  self._inotify = Inotify(path, recursive)
File "/usr/lib/python3/dist-packages/watchdog/observers/inotify_c.py", line 179, in __init__
  self._add_dir_watch(path, recursive, event_mask)
File "/usr/lib/python3/dist-packages/watchdog/observers/inotify_c.py", line 402, in _add_dir_watch
  self._add_watch(full_path, mask)
File "/usr/lib/python3/dist-packages/watchdog/observers/inotify_c.py", line 416, in _add_watch
  Inotify._raise_error()
File "/usr/lib/python3/dist-packages/watchdog/observers/inotify_c.py", line 428, in _raise_error
  raise OSError(errno.ENOSPC, "inotify watch limit reached")
OSError: [Errno 28] inotify watch limit reached
```

El proceso muere con rc=1. El supervisor loop de `nkr-start.sh` (introducido en v1.6.1 para `workers=0` threaded) lo respawnea inmediatamente:

```
[NKR-INTECH-DEV] Odoo salió rc=1 — respawn en 1s
[NKR-INTECH-DEV] Lanzando Odoo (supervisor): su -p -s /bin/sh odoo -c 'exec /usr/bin/python3 -u /usr/bin/odoo -c /tmp/nkr-overrides/etc/odoo/odoo.conf'
```

→ Ciclo infinito. Como cada intento muere antes de bind del puerto 8069, el `[NKR-HEALTH]` falla los 20 intentos × 5s = 100s, el `nkr compose` retorna timeout, el daemon devuelve 504 al panel.

---

## Por qué es incompatible con la arquitectura NKR

`dev_mode=reload` está diseñado para "auto-recargar código Python cuando un archivo del addons_path cambia en disco". Para detectar el cambio, Odoo hace inotify watch recursivo.

NKR comparte `addons/` con el guest **vía virtio-fs**. virtio-fs es una limitación conocida del kernel Linux:

> **virtio-fs no propaga eventos inotify del host al guest** (limitación FUSE).

Aunque inotify funcionara dentro del guest, el watcher **NO recibiría eventos** cuando el host modifica los addons (que es el caso real: `POST /addons/git` escribe en host, el guest tiene que enterarse). Por eso v1.6.1 introdujo el protocolo **REL_OD vía HVC0**: NKR manda SIGUSR1 a la VM → vmm inyecta `REL_OD\n` por hvc0 → guest hace `pkill -TERM odoo` → supervisor respawnea con código fresh.

REL_OD reemplaza completamente `dev_mode=reload` para nuestro caso de uso. Mantener `dev_mode=reload` activo:
- **No aporta nada** (los eventos no se propagan via virtio-fs)
- **Rompe el bootstrap** (agota inotify max_user_watches del kernel guest, ENOSPC, loop)

---

## Fixes posibles

### Opción A — Eliminar `dev_mode=reload` por completo (recomendado)

Quitar la línea de `cell.rs::rewrite_odoo_conf_full` para todos los tiers. REL_OD via HVC0 cubre el caso de uso. `qweb,xml` se pueden mantener (no usan inotify; activan recompile de QWeb templates al servir cada request en dev mode).

**Diff conceptual** (`src/cell.rs`):

```diff
-    config.push_str("dev_mode = reload,qweb,xml\n");
+    config.push_str("dev_mode = qweb,xml\n");
```

**Pros**: Cero riesgo. REL_OD ya hace el trabajo. El bug desaparece.
**Contras**: Ninguno técnico. Hay que actualizar la doctrina en CLAUDE.md (sección "dev_mode = reload,qweb,xml — el game changer") porque el game changer real es REL_OD, no `reload`.

### Opción B — Subir `fs.inotify.max_user_watches` en el initramfs

Agregar al `nkr-start.sh` antes de lanzar Odoo:

```sh
echo 524288 > /proc/sys/fs/inotify/max_user_watches
echo 1024   > /proc/sys/fs/inotify/max_user_instances
```

**Pros**: Mantiene el `reload` y permite cualquier otra cosa que use inotify dentro del guest.
**Contras**:
- `dev_mode=reload` igual no funciona via virtio-fs (no resuelve el problema de fondo, solo evita el ENOSPC)
- Cada tenant existente tiene su propio initramfs (`/mnt/nkr/initramfs/<name>.cpio.gz`) — cambios al `initramfs.rs` requieren regen del cpio por tenant, no se aplican a VMs ya creadas hasta el próximo `regen + restart`

### Opción C — Híbrido

Aplicar A (la solución correcta arquitectónicamente) **y** B (para defender contra otros consumidores de inotify dentro del guest, ej. fail2ban, systemd-journald, futuros daemons).

---

## Workaround manual para tenants ya rotos

Para una instancia existente atrapada en el loop de respawn:

```bash
# 1. Editar el odoo.conf del tenant
sed -i 's/^dev_mode = reload,qweb,xml/dev_mode = qweb,xml/' \
  /mnt/nkr/cells/<cell>/instances/<nkr_name>/config/odoo.conf

# 2. Restart la VM
curl -X POST -H "Authorization: Bearer $TOKEN" \
  -d '{"action":"restart"}' \
  http://127.0.0.1:9090/api/v1/cells/<cell>/instances/<nkr_name>/actions
```

O, más simple, **DELETE + retry post-fix**:

```bash
curl -X DELETE -H "Authorization: Bearer $TOKEN" \
  "http://127.0.0.1:9090/api/v1/cells/<cell>/instances/<nkr_name>?drop_db=true"
```

---

## Lecciones

1. **virtio-fs + inotify es una incompatibilidad con consecuencias colaterales**: no es solo "los eventos no se propagan", el watcher mismo puede romper el guest si recurse sobre filesystems virtio-fs montados (escenarios de FS no-watcheable) o agotar `max_user_watches` (caso real).
2. **`dev_mode=reload` está obsoleto bajo NKR**: REL_OD via HVC0 es la solución arquitectónicamente correcta. La doctrina v2.2 que lo introdujo lo dejó claro pero `cell.rs` siguió escribiendo `reload` por inercia.
3. **El supervisor loop ofusca el síntoma**: sin él, Odoo moriría una sola vez con rc=1 y el log se vería claramente. El loop hace que el problema parezca "VM tarda mucho" cuando en realidad es "VM crashea cada 7s y nunca levanta el puerto". Vale la pena loguear `[NKR-SUPERVISOR] respawn count: N` para detectar este patrón antes.

---

## Archivos involucrados

- `src/cell.rs::rewrite_odoo_conf_full` — escribe `dev_mode=reload,qweb,xml` para tier dev/staging
- `src/initramfs.rs` — el `nkr-start.sh` con el supervisor loop (v1.6.2) que ofusca el síntoma
- `CLAUDE.md` §"dev_mode" — doctrina que describe la feature como key (necesita actualización)
- `NKR_API.md` §7.0 (tier system table) — fila "dev_mode en odoo.conf" → cambiar `reload,qweb,xml` a `qweb,xml`
