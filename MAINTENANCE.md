# NKR — Guía de mantenimiento del equipo

Documento generado 2026-05-15 post-audit completo (~23k LoC Rust auditadas).
Va dirigido a desarrolladores que vayan a tocar el código o diagnosticar
incidentes en producción. Asume familiaridad con Rust + Linux + KVM.

---

## 1. Mapa de módulos — qué hace cada `src/*.rs`

| Módulo                 | LoC   | Responsabilidad                                                       | Heat |
|------------------------|------:|-----------------------------------------------------------------------|:----:|
| `api.rs`               | 4099  | Handlers HTTP: create/delete/start/stop/restart/reload/sso/diag/dns/init-db/modules/addons/balloon | 🔴 |
| `vmm.rs`               | 2888  | KVM setup, virtio device wiring, vcpu loop, signal handlers, cgroup, TAP | 🔴 |
| `cell.rs`              | 2996  | CellRegistry, instance dirs, compose.yml de la cell, clone scratch, tier matrix | 🔴 |
| `compose.rs`           | 1735  | `nkr compose up/down`, health checks, boot log capture per-instance | 🟠 |
| `initramfs.rs`         | 1135  | Generación del init.sh + watcher hvc0 + supervisor loop de Odoo | 🟠 |
| `bin/nkr_api_server.rs`| 2621  | Proxy HTTP unprivileged (token bearer auth) → IPC al daemon | 🟠 |
| `metrics.rs`           | 888   | `/metrics` Prometheus + endpoint per-VM JSON | 🟡 |
| `virtio_fs.rs`         | 685   | Wrapper de virtiofsd, shares montados en cada VM | 🟡 |
| `state.rs`             | 540   | State files per-VM en `/tmp/nkr-vms/` (PID, balloon, guest_mem) | 🟡 |
| `main.rs`              | 481   | CLI dispatch (`nkr run/ps/stop/restart/compose/...`) | 🟢 |
| `net.rs`               | 366   | Setup de bridge + iptables + tc + cBPF anti-spoofing | 🟢 |
| `janitor.rs`           | 349   | Cleanup periódico (5min) de state files huérfanos, locks viejos | 🟢 |
| `cli.rs`               | 349   | Parsing clap del CLI | 🟢 |
| `fsutil.rs`            | 299   | Helpers de ext4 (loop mount, chattr, etc.) | 🟢 |
| `ipc.rs` / `ipc_server.rs` | 551 | Protocolo UDS daemon↔api-server (request/response framing) | 🟢 |
| `registry.rs`          | 284   | IPRegistry (mapping cell_id/vm_id → guest_ip) | 🟢 |
| `pmem.rs`              | 277   | virtio-pmem con DAX (rootfs zero-copy) | 🟢 |
| `pull.rs`              | 195   | `nkr pull` (descarga imagen OCI → ext4) | 🟢 |
| `console.rs`           | 163   | virtio-console (hvc0): inyección host→guest de REL_OD/SHUTDOWN | 🟢 |
| `seccomp.rs`           | 149   | BPF filter del daemon: whitelist de syscalls (incluye clone3/openat2) | 🟢 |
| `balloon.rs`           | ~280  | virtio-balloon: inflate/deflate + STATS_VQ (guest_mem reporting) | 🟢 |
| `watchdog.rs`          | 194   | Thread del daemon: TCP probe :8069 → restart si colgado | 🟢 |
| `netlock.rs`           | 85    | flock inter-proceso para serializar netlink/iptables | 🟢 |

**Heat key**: 🔴 = riesgo alto (toca aquí con extremo cuidado, test exhaustivo)
🟠 = riesgo medio · 🟡 = bajo · 🟢 = mínimo.

---

## 2. Flujos críticos — diagramas de control

### 2.1 `POST /api/v1/instances` (create async)

```
panel → nkr-api-server (HTTP/9090 + bearer)
        │
        └→ IPC UDS → nkr daemon (api.rs::handle_create)
              │
              ├─ Validación sync (charset, admin_passwd ≥16, etc.) → 400 si falla
              ├─ Auto-cell-select (cell.rs::select_cell_for_version)
              │   por RAM committed ASC, tie-break cell_id ASC
              ├─ `inflight_creates` set: 409 si ya hay create con ese name
              ├─ Status file: `cells/<cell>/.nkr-creates/<name>.json {phase:"queued"}`
              ├─ thread::spawn → flujo async:
              │     1. CloneScratch RAII (rollback automático en panic)
              │     2. `cp --reflink` rootfs maestro → <instance>/odoo.ext4
              │     3. `CREATE DATABASE … TEMPLATE db-<cell>-odoo-template`
              │     4. Compose block en cell.yml (disabled si sin admin_pwd)
              │     5. Si admin_user_password presente:
              │           compose up → wait :8069 → JSON-RPC login + change_password
              │     6. status file → phase:"ready"|"failed"
              └─ Response sync: 202 {nkr_name, poll:"GET …/create-status"}
```

**Latencia esperada**: 13–17s end-to-end (medido en intech-devp).

### 2.2 `POST /reload` (REL_OD via hvc0)

```
panel → POST /reload
   ↓
api.rs::handle_reload_workers
   ├─ pid_is_nkr_vmm(pid) guard → 409 si PID reusado por otro proceso
   ├─ libc::kill(vmm_pid, SIGUSR1)
   ↓
vmm.rs::sigusr1_handler
   └─ RELOAD_REQUESTED.store(true) [async-signal-safe: solo atomic]
   ↓
vcpu_loop (vmm.rs)
   └─ swap(RELOAD_REQUESTED) → true → console_dev.try_inject("REL_OD\n")
   ↓
console.rs::inject_to_receiveq
   ├─ Escribe en descriptor disponible del receiveq
   ├─ Avanza used.idx + IRQ al guest
   └─ Log: "[NKR-CTL] 'REL_OD' inyectado en /dev/hvc0 del guest"
   ↓
GUEST kernel: virtio-console IRQ → driver → /dev/hvc0 buffer
   ↓
Watcher subshell (initramfs.rs):
   while true; do
     read -r _nkr_cmd < /dev/hvc0
     if "$_nkr_cmd" = "REL_OD"; then
       _odoo_pid=$(cat /newroot/tmp/odoo.pid)
       if workers=0 (threaded): kill -KILL $_odoo_pid    # DEV/STAG
       if workers>0 (prefork):  kill -HUP  $_odoo_pid    # PROD
     fi
   done
   ↓
nkr-start.sh supervisor (en threaded): eval $COMMAND retorna → sleep 1 → respawn
nkr-start.sh supervisor (en prefork):  master sobrevive, respawnea workers internamente
```

**Latencia esperada**:
- DEV/STAGING (threaded): **~5–8s** (SIGKILL + supervisor + Odoo boot 4s + módulos)
- PRODUCTION (prefork): **~1–2s** (master vivo, sólo workers respawnean)

### 2.3 `nkr compose up -d` (post v1.6.9 detach correcto)

```
operador/daemon → nkr compose up -d
   ↓
compose.rs::compose_up(yaml, detach=true)   [MODE 1]
   ├─ Spawnea self con `nkr compose up -f` + setsid() + Stdio→log_file
   ├─ Tail loop del log buscando "[NKR-READY]"
   └─ Exit cuando ready (o timeout)
   ↓                            [MODE 2 corre en background, session leader]
compose.rs::compose_up(yaml, detach=false)
   ├─ Para cada servicio en yaml:
   │     ├─ cmd.stdout/stderr = boot_log_file directo (Stdio::from(File))
   │     ├─ cmd.pre_exec → libc::setsid()
   │     ├─ cmd.spawn() → handles.push(child)
   ├─ Health checks paralelos (TCP :8069 probe + custom)
   ├─ Si all_ok: log "[NKR-READY] Todos los servicios listos: ..."
   ├─ try_wait() no-blocking (sólo reapea dummies "true" de skip-launch)
   └─ Mode 2 sale limpio → VMs reparentadas a init (PID 1)
```

**Verificación post-deploy**: `pgrep -af 'nkr compose up'` debe retornar **vacío**.
Cada VM debe tener `PPid: 1` en `/proc/<pid>/status`.

---

## 3. Invariantes que NUNCA pueden romperse

| Invariante | Por qué importa | Cómo verificarla |
|---|---|---|
| Cada VM en `nkr ps` tiene PPid=1 | Sino, mata-cascada (ver v1.6.9 fix) | `for p in $(pgrep -f 'nkr run'); do awk '/^PPid:/{print $2}' /proc/$p/status; done \| grep -c '^1$'` debe igualar `nkr ps \| wc -l` (menos header) |
| `KillMode=process` en nkr.service | Sino, systemd mata el cgroup entero al stop del daemon → mata VMs | `grep KillMode /etc/systemd/system/nkr.service` |
| `/newroot/tmp/odoo.pid` existe en VM corriendo | Sino, REL_OD cae al fallback pkill -f (menos confiable) | watcher.log debe mostrar `PID … vivo, enviando SIGKILL` no `pkill -f (fallback)` |
| state files atómicos | Sino, `list_vms()` puede leer VM-fantasma o perder VM real | `ls /tmp/nkr-vms/*.tmp 2>/dev/null` debe ser vacío |
| cell-registry.json es JSON válido | Sino, `load()` devuelve Default → mapeo de cells perdido | `cat /mnt/nkr/cell-registry.json \| python3 -m json.tool >/dev/null` |
| Sólo una VM corriendo por (cell_id, vm_id) | Sino, IP colision + state confuso | `pgrep -af 'nkr run' \| grep -oP -- '--id \d+'` agrupado por cell_id = sin duplicados |

---

## 4. Runbook de incidentes comunes

### 4.1 "Una VM se cuelga, :8069 no responde"

1. **Verificar que sea realmente cuelgue del tenant** (no del host):
   ```bash
   curl -sf --max-time 5 http://<guest_ip>:8069/web/login   # 502/timeout?
   nkr stats | grep <nkr_name>                              # CPU > 90% sostenido?
   ```
2. **Capturar diagnóstico ANTES de restartear**:
   ```bash
   curl -s "http://127.0.0.1:9090/api/v1/cells/<cell>/instances/<nkr_name>/diag" \
     -H "Authorization: Bearer $TOKEN" > /tmp/diag-<nkr_name>-$(date +%s).txt
   tail -200 <instance_dir>/logs/odoo.log
   tail -50 <instance_dir>/logs/nkr-watcher.log
   tail -50 <instance_dir>/.<config_name>-vm-boot.log
   ```
3. **Restart manual**:
   ```bash
   curl -X POST "http://127.0.0.1:9090/api/v1/cells/<cell>/instances/<nkr_name>/actions" \
     -H "Authorization: Bearer $TOKEN" -d '{"action":"restart"}'
   ```
4. **Si el watchdog ya lo restarteó automáticamente**: revisar el journal para ver
   si fue un REL_OD legítimo que se atascó (algo a investigar) o si Odoo
   crashed por otra causa (OOM, deadlock interno).

### 4.2 "Commit deploy lento — REL_OD tarda más de 30s"

1. **Verificar el watcher log**:
   ```bash
   tail -30 <instance_dir>/logs/nkr-watcher.log
   ```
   Buscar:
   - `iter=N: read OK, _nkr_cmd='REL_OD'` ← llegó OK
   - `REL_OD: leyendo /newroot/tmp/odoo.pid → 'PID'` ← PID leído
   - `kill -KILL rc=0` (threaded) o `kill -HUP rc=0` (prefork) ← señal enviada
2. **Si watcher se atascó en `iter=N: bloqueando en read`** (sin recibir REL_OD):
   - Race conocida del busybox + virtio-console (rara, ~1×/sesión heavy).
   - Workaround inmediato: `POST /actions {action:"restart"}` (recupera limpio).
   - El watchdog también lo detecta y restartea a los 180s automáticamente.
3. **Si watcher logueó SIGKILL/SIGHUP rc=0 pero :8069 no vuelve**:
   - Revisar `<instance_dir>/logs/odoo.log` — Odoo está booteando?
   - Verificar `<instance_dir>/.<config_name>-vm-boot.log` — supervisor relanzó?
   - Si `Odoo salió rc=137` aparece pero NO viene un nuevo "Lanzando Odoo": el
     supervisor murió (bug raro), restartear la VM.

### 4.3 "Cell registry corrupto / cells.json vacío"

```bash
cat /mnt/nkr/cell-registry.json
# Si está vacío o malformado:
ls -la /mnt/nkr/cell-registry.json*    # buscar .tmp orfanado
# Reconstrucción manual (último recurso):
cat > /mnt/nkr/cell-registry.json <<'EOF'
{ "entries": { "odoo-v17": 1, "odoo-v19": 2 } }
EOF
systemctl restart nkr
```

**Prevención**: el daemon v1.6.9+ usa flock + tmp+rename — esto NO debería pasar.
Si pasa, hay un bug a investigar.

### 4.4 "VM huérfana corriendo sin entrada en `nkr ps`"

```bash
# Inventario:
pgrep -af 'nkr run' > /tmp/running.txt
nkr ps > /tmp/listed.txt
# Comparar — si hay PID en running.txt que no aparece en listed.txt:
# Es una VM sin state file. Inspeccionar:
ps -o pid,ppid,etime,cmd -p <pid>
# Si es legítima pero state file falta, restartear la cell:
cd /mnt/nkr/cells/<cell> && nkr compose down && nkr compose up -d
# Si es zombie real (proceso muerto que aparece como D-state), reboot del host.
```

### 4.5 "Watchdog dispara restart innecesario en deploy"

Síntoma: el log muestra `[NKR-WATCHDOG] X colgado 180s sin :8069 — disparando restart`
JUSTO cuando el panel hizo un POST /reload o addons/git.

**v1.6.8+** ya maneja esto: `handle_reload_workers` llama a `watchdog::note_reload`
que sube el threshold efectivo a 180s durante 240s post-reload.

Si aún dispara: el reload tomó >180s. Investigar Odoo log para ver si el tenant
está bajo carga anómala (cron infinito, websocket loop, etc.).

---

## 5. Cómo agregar features sin romper invariantes

### 5.1 Agregar un nuevo endpoint HTTP

1. **Definir IPC request** en `src/ipc.rs` (variant del enum `IpcRequest`).
2. **Implementar handler** en `src/api.rs` retornando `IpcResponse`.
3. **Routear** en `src/ipc_server.rs::dispatch_request`.
4. **Exponer HTTP** en `src/bin/nkr_api_server.rs::handle_request`.
5. **Documentar** en `NKR_API.md` con: body, response, latencia esperada, errors, code references.

**Checklist obligatorio**:
- [ ] Validar TODOS los identifiers con `is_safe_identifier` o `is_safe_dns`.
- [ ] Body size limit (max 64KB típicamente).
- [ ] Si modifica estado mutable → tomar `inflight_actions` guard.
- [ ] Si envía señales a un VMM → guard `pid_is_nkr_vmm(pid)` antes del `kill`.
- [ ] Errores informativos sin leak de internals.
- [ ] Smoke test del endpoint + restart de daemon.

### 5.2 Agregar un share virtio-fs nuevo

Caso: querés montar `/mnt/extra-foo:ro` en cada tenant.

1. **Mountpoint debe existir en el rootfs maestro** (`Nkrfile.odoo19` o vía mkdir + chattr en el ext4). Sin esto, el initramfs no puede crear el mountpoint (rootfs RO). **Excepción**: bajo `/mnt/*` ya hay un tmpfs (v1.6.4+) — paths nuevos bajo `/mnt/foo` "just work".
2. **Append al compose block** del tenant en `cell.rs::append_compose_block`.
3. **Agregar a `addons_path`** del `odoo.conf` si el share contiene addons Python.
4. **Initramfs**: el VirtIO-FS mount es automático (lee `nkr.fsN=`/`nkr.fsmN=` del kernel cmdline).
5. **Tener cuidado con el tamaño del cmdline** (~1024 bytes, ver `vmm.rs::configure_linux_boot`). Si te quedás sin espacio, omitir param redundantes.

### 5.3 Cambiar el sizing per-tier

Editar **únicamente** `src/api.rs::derive_resources_for_tier`. **NO duplicar la matriz** en otros lugares — la doc en CLAUDE.md y NKR_API.md §7.0 referencia esa función.

Después del cambio:
1. Bump version en `Cargo.toml` solo si el cambio es **significativo** (memory check del usuario: no bumpear por tweaks).
2. Restartear daemon — los tenants existentes mantienen su sizing del compose.yml (no se re-deriva runtime).
3. Para que un tenant existente reciba el sizing nuevo: `nkr restart <name>` regenera el compose block.

---

## 6. Build, deploy, rollback

### 6.1 Build & deploy estándar

```bash
cd /root/dev/nano-kernel-runtime
cargo build --release                                  # ~32s
install -m 0755 target/release/nkr /usr/local/bin/nkr
install -m 0755 target/release/nkr-api-server /usr/local/bin/nkr-api-server
systemctl restart nkr nkr-api-server                  # NO toca VMs por KillMode=process
```

**Smoke test post-deploy** (debe pasar antes de cerrar la ventana):

```bash
systemctl is-active nkr nkr-api-server                 # ambos "active"
nkr ps | grep -c "^odoo"                               # esperado: número de VMs running
for ip in $(nkr ps | awk 'NR>2 && NF>5 {print $7}' | grep -v "—"); do
  curl -sf --max-time 3 "http://$ip:8069/web/login" >/dev/null && echo "OK $ip" || echo "FAIL $ip"
done
pgrep -af 'nkr compose up' | grep -v "claude\|/bin/bash" | wc -l    # esperado: 0
```

### 6.2 Rollback rápido

```bash
# Si hay binarios .pre-<version> guardados (recomendado: hacerlo antes de cada install):
cp /usr/local/bin/nkr.pre-1.6.9 /usr/local/bin/nkr
systemctl restart nkr nkr-api-server
# VMs siguen vivas (KillMode=process)
```

Si no hay backup, recompilar desde commit anterior:
```bash
git checkout <commit-hash-bueno>
cargo build --release
install -m 0755 target/release/nkr* /usr/local/bin/
systemctl restart nkr nkr-api-server
```

### 6.3 Regenerar initramfs de UN tenant

Necesario cuando cambias `src/initramfs.rs` y querés que un tenant existente reciba el código nuevo SIN restart de la VM (es bait — siempre requiere restart de VM):

```bash
# Stop limpio (no rompe nada):
nkr stop odoo-v19-<nkr_name>
rm /mnt/nkr/initramfs/<nkr_name>.cpio.gz
cd /mnt/nkr/cells/odoo-v<X>
nkr compose up -d                                     # regenera initramfs automático
# Wait :8069 (~5s para tenants normales)
```

### 6.4 Regenerar initramfs de TODOS los tenants (rollout de hot fix)

Después de un cambio crítico en `initramfs.rs` (watcher, supervisor, etc.):

```bash
# Una VM a la vez, ~10s downtime cada una, total ~5min:
for vm in $(nkr ps | awk 'NR>2 && NF>5 {print $3}'); do
  cell=$(nkr ps | awk -v v="$vm" '$3==v {print $1}')
  cd /mnt/nkr/cells/$cell
  nkr stop "$vm" && sleep 1 && nkr compose up -d
  # Verificar antes de seguir al próximo:
  ip=$(nkr ps | awk -v v="$vm" '$3==v {print $7}')
  if [ "$ip" != "—" ]; then
    for i in $(seq 1 30); do
      curl -sf --max-time 2 "http://$ip:8069/web/login" >/dev/null && break
      sleep 1
    done
  fi
done
```

---

## 7. Debugging cheatsheet

| Síntoma                              | Lo primero a revisar                                              |
|--------------------------------------|-------------------------------------------------------------------|
| Daemon crash / no arranca            | `journalctl -u nkr -n 100` + check `/etc/nkr/api.env` válido      |
| VM no arranca (boot timeout)         | `<instance>/.<name>-vm-boot.log` — virtiofsd error? IRQ conflict? |
| Odoo no responde :8069               | `<instance>/logs/odoo.log` — buscar "modules loaded" + "Registry loaded" |
| REL_OD silencioso                    | `<instance>/logs/nkr-watcher.log` — buscar `iter=N: read OK`      |
| Reload tarda >30s                    | Watcher log + check si Odoo respawneó (nuevo PID en boot log)     |
| guest_mem null en /metrics           | balloon device advertised? `dmesg` del guest grep "balloon"       |
| 502 desde nginx                      | `<instance>/logs/odoo.log` — Odoo arrancando o crash?             |
| Watchdog dispara restart inesperado  | Journal del daemon — buscar `[NKR-WATCHDOG] X colgado Ns`         |

**Logs claves por incidente**:

```
/var/log/syslog                                              # systemd boot/stop del daemon
journalctl -u nkr                                            # daemon nkr (incluye watchdog, janitor, API calls)
journalctl -u nkr-api-server                                 # HTTP frontend (auth failures, panic en handler)
/mnt/nkr/cells/<cell>/instances/<name>/logs/odoo.log         # Odoo aplicación
/mnt/nkr/cells/<cell>/instances/<name>/logs/nkr-watcher.log  # Watcher hvc0 del initramfs (DIAGNÓSTICO PRINCIPAL DE REL_OD)
/mnt/nkr/cells/<cell>/instances/<name>/.<name>-vm-boot.log   # Boot log del guest (kernel + initramfs)
/var/log/nkr-psql-audit.log                                  # Audit de POST /psql (queries + status ACCEPT/REJECT)
/mnt/nkr/cells/<cell>/logs/nkr-compose-*.log                 # Per-invocation compose log
/var/log/letsencrypt/letsencrypt.log                         # certbot (POST /dns)
```

---

## 8. Pitfalls conocidos (lecciones del audit + sesión 2026-05-15)

### 8.1 Subshell de busybox: stdout = pipe muerto

Cuando un subshell se backgrounea con `( ... ) &` en busybox sh, **su fd 1
NO es necesariamente `/dev/ttyS0`** — puede ser un pipe interno que nadie
lee. Resultado: todos los `echo` del subshell desaparecen en el void.

**Solución**: logueá a un archivo en una virtio-fs share RW (e.g.,
`/newroot/var/log/odoo/nkr-watcher.log`), no a stdout. Visible desde el
host vía `tail -f <instance>/logs/nkr-watcher.log`.

### 8.2 chroot context: `/tmp/X` no es lo mismo desde fuera

El watcher hvc0 corre en el initramfs **outer context** (sin chroot). El
supervisor de Odoo corre **dentro del chroot `/newroot`**. Si el supervisor
hace `echo $$ > /tmp/odoo.pid`, escribe en `/newroot/tmp/odoo.pid` (visto
desde el watcher). El watcher debe leer `/newroot/tmp/odoo.pid`, **no
`/tmp/odoo.pid`**.

Mismo cuidado con cualquier mount, env var, file path que cruce el límite
chroot ↔ initramfs.

### 8.3 `pkill -f` en busybox/initramfs es inconsistente

El matching de cmdline con `pkill -f '<pattern>'` no siempre matchea como
esperás (puede mirar `/proc/.../comm` solo, o tener bugs específicos de la
versión de busybox). **Siempre preferir kill al PID exacto** leído de un
PID file que el supervisor escribe pre-`exec`.

### 8.4 Reabrir hvc0 en cada iteración del watcher: race rara

`read -r ... < /dev/hvc0` abre+lee+cierra hvc0 cada vez. Hay un evento raro
(~1×/sesión de testing intensivo) donde el segundo REL_OD consecutivo
queda atascado en el `read`. **NO se conoce la causa raíz** (probamos
`exec 3< /dev/hvc0` persistente y rompió todos los reloads en testing).

**Workaround actual**: watchdog v1.6.8+ con grace REL_OD-aware (180s)
detecta el cuelgue y restartea automáticamente. Costo: ~64s downtime cada
~N reloads. Aceptable mientras se investiga una solución mejor (idea:
reemplazar el subshell por un binario Rust dedicado para el watcher).

### 8.5 Memory orderings inconsistentes en vmm.rs

El archivo mezcla `Ordering::Relaxed` y `Ordering::SeqCst` sin patrón.
Funcionalmente OK en x86 (TSO), pero pesadilla de auditoría. **Si tocás
una variable atómica nueva**, usá `SeqCst` por defecto (costo zero en x86).

### 8.6 `fs::write` no es atómico

Para state files mutables (que pueden ser leídos por otro proceso/thread),
**usar tmp+rename**:

```rust
let tmp = path.with_extension("json.tmp");
{ let mut f = File::create(&tmp)?; f.write_all(json.as_bytes())?; f.sync_data().ok(); }
fs::rename(&tmp, &path)?;
```

`state.rs::atomic_write_json` y `cell.rs::CellRegistry::save` ya lo hacen.
Cualquier nuevo state file debe seguir el patrón.

### 8.7 `panic!` en setup path: cuidado con resource leaks

`vmm.rs::run()` tiene varios `panic!` durante setup (cgroup, irqfd, tap).
Si panic'ea entre la creación de un recurso (TAP, virtiofsd, cgroup) y su
registro en `state`, el recurso queda huérfano. **Idealmente** wrappear
con RAII (`Drop`) o `scopeguard`. Hoy es manual y frágil.

### 8.8 `kill` a PIDs no validados es peligroso

`kill(0, …)` envía al process group ENTERO del caller (incluye al daemon).
`kill(1, …)` envía a init. Siempre `filter(|p| p > 1)` antes de cualquier
loop que envíe señales a PIDs leídos de un archivo o `/proc`.

### 8.9 Validar /proc/<pid>/comm antes de kill cross-process

Race típica: leés PID del state file → el VMM muere → kernel recicla el PID
para otro proceso del host → tu `kill(reused_pid, SIGUSR1)` mata un servicio
inocente. **Antes de cualquier signal a un PID externo**: leer
`/proc/<pid>/comm` y verificar que sea el binario esperado.

Helper `pid_is_nkr_vmm(pid)` en `api.rs` ya lo hace para los handlers de
reload/balloon. Aplicar a futuros call sites.

---

## 9. Testing

### 9.1 Tests unitarios actuales

```bash
cargo test --release          # ~2-3s para los 50 tests
```

Cobertura por módulo:
- `bin/nkr_api_server.rs`: 18 tests (rutas, validation) — bueno
- `cell.rs`: 9 tests (CloneScratch, parsing) — moderado
- `fsutil.rs`: 4 tests (ext4 helpers) — bueno
- `api.rs`: 4 tests (validators, conf_parser_tests) — escaso
- `balloon.rs`: 2 tests — escaso
- **`vmm.rs`, `compose.rs`, `state.rs`, `watchdog.rs`, `janitor.rs`: 0 tests** — gap

### 9.2 Smoke test post-deploy

Ver §6.1. Ejecutar SIEMPRE tras cualquier deploy a producción.

### 9.3 Test manual del REL_OD por tier

```bash
# DEV/STAGING (threaded):
TOK=$(grep -oP 'NKR_API_TOKEN=\K\S+' /etc/nkr/api.env)
curl -X POST "http://127.0.0.1:9090/api/v1/cells/<cell>/instances/<name>/reload" \
  -H "Authorization: Bearer $TOK"
# Esperado: ~5-8s para que :8069 vuelva. Watcher log muestra SIGKILL PID exacto.

# PROD (prefork):
# Esperado: ~1-2s. Watcher log muestra SIGHUP master PID exacto.
```

---

## 10. Roadmap de mejoras pendientes (post-audit 2026-05-15)

### Prioridad alta — para próximo sprint

1. **Split `vmm.rs`** en 5 módulos (signals, cgroup, network, boot, mmio_dispatch).
   Ver recomendación en findings del audit.
2. **Split `api.rs`** en 8 módulos bajo `src/api/` (create, actions, dns, db, sso, modules, observability, tier).
3. **`InstanceLock` per-(cell, nkr_name)** que serialice start/stop/restart/reload/delete/
   watchdog-restart. Patrón existe en `netlock.rs`/`registry.rs`, falta call site disciplinado.
4. **Watchdog backoff exponencial** en restart failures (evitar restart loop infinito si la VM no
   se recupera). Hoy retry cada 60s sin límite.
5. **Cleanup-on-panic en `vmm.rs::run`** vía RAII guards para TAP/cgroup/virtiofsd. Hoy panic
   en setup = recurso huérfano.

### Prioridad media

6. **`exec 3< /dev/hvc0` o equivalente robusto** para watcher hvc0 — investigar por qué el fd
   persistente rompió reloads (busybox + virtio-console). Posible: reemplazar subshell por
   binario Rust dedicado.
7. **`rustfmt`** sobre todo el repo (974 diffs hoy).
8. **`cargo clippy --fix`** sobre los 130 warnings idiomáticos.
9. **Atomic itimer per-purpose** en `vmm.rs` — hoy SIGALRM lo comparten balloon y shutdown,
   last-writer-wins.
10. **Tests integration** para vmm.rs/compose.rs/state.rs (subprocess-based) — cero coverage hoy.

### Prioridad baja (mejoras futuras)

11. Memory orderings consistentes (todo SeqCst o todo Acquire/Release).
12. `extern "C" fn` signal handlers — auditar que sean estrictamente async-signal-safe (sigusr2_handler
    llama `SystemTime::now()` — riesgoso en teoría aunque OK en práctica con x86 vDSO).
13. `OnceLock<SHUTDOWN_ROOTFS_PATH>` global per-process — bloquea futuro supervisor de múltiples VMs
    en un solo proceso (no es bug hoy, footgun futuro).
14. Cobertura de tests por archivo crítico.

---

## 11. Contactos y artefactos

| Recurso                        | Ubicación                                                  |
|--------------------------------|------------------------------------------------------------|
| Repo                           | `/root/dev/nano-kernel-runtime`                            |
| Doc de arquitectura API        | `NKR_API.md`                                               |
| Doc de comparativo con Docker  | `NKR_vs_Docker.md`                                         |
| Whitepapers (EN/ES)            | `NKR_WhitePaper_EN.md`, `NKR_WhitePaper_ES.md`             |
| Spec del módulo nkr_sso        | `nkr_sso.md`                                               |
| Bug history (inotify dev_mode) | `BUG_inotify_dev_mode.md`                                  |
| Audits previos                 | `AUDIT_GH.md`, `AUDIT_PMEM_RO.md`                          |
| Esta guía                      | `MAINTENANCE.md` (el doc que estás leyendo)                |
| CLAUDE.md (project leys)       | `CLAUDE.md` — "leyes físicas" del proyecto                 |
| Cells data                     | `/mnt/nkr/cells/<cell>/`                                   |
| Cell registry                  | `/mnt/nkr/cell-registry.json`                              |
| Logs (compose, audit)          | `/mnt/nkr/cells/<cell>/logs/`, `/var/log/nkr-*.log`        |
| State VMs                      | `/tmp/nkr-vms/c<cell_id>-v<vm_id>.json`                    |
| Systemd units                  | `/etc/systemd/system/nkr.service`, `…/nkr-api-server.service` |
| API token (secret)             | `/etc/nkr/api.env` (root-only, 0600)                       |

---

_Última actualización: 2026-05-15 (post-audit + 9 fixes críticos aplicados — commits 
dc830f8 y 5052de1)._
