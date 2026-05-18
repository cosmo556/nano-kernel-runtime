# NKR Backup Plan — propuesta v1

**Estado:** propuesta para revisión, no implementado.
**Fecha:** 2026-05-18
**Scope:** backups internos en el mismo servidor (no off-site todavía).
**Versión NKR de referencia:** 1.6.9 + sprint post-Fase 0.

---

## TL;DR

- **Qué se respalda**: lo único persistente del cliente → DB + filestore + addons + pylibs + meta.json. El rootfs, kernel, infra y código de NKR NO se respaldan (son recreables desde git + master images).
- **Cómo**: `pg_dump -Fc` para la DB + `tar -I zstd` para los archivos del tenant. Por-tenant, no por-cell. Sin downtime (PG hot dump, filestore copy mientras corre).
- **Cuánto pesa**: ~100–500 MB por tenant pequeño (post-compresión), ~1–5 GB por tenant con datos reales (ERP de 1 año de uso).
- **Cuánto tarda**: 20–60 s por tenant chico (intech-devp baseline), 2–5 min por tenant grande. Backup nightly de 14 tenants futuros: ~10 min secuencial, ~2 min paralelo.
- **Dónde se guarda**: `/mnt/nkr/backups/<tenant>/<YYYY-MM-DD-HHMM>/` (mismo disco, btrfs reflink para minimizar duplicación entre backups consecutivos).
- **Restore (respawn)**: nuevo comando `nkr restore <backup_dir> --as <new_nkr_name> [--cell <cell>]`. Crea instancia limpia vía API normal, después pg_restore + untar el filestore. Cell-agnostic: cualquier cell de la misma versión Odoo sirve.
- **Compatibilidad cross-cell**: SÍ. El backup NO baked-in cell-specific paths/IPs. Lo que cambia per-cell (DB_HOST=10.0.X.3, secret SSO, etc.) lo regenera NKR en el create. Solo restauramos contenido de cliente.

---

## 1. Qué se respalda — alcance exacto

| Componente | Path | Tamaño típico | ¿Backup? | Por qué |
|---|---|---|---|---|
| **PostgreSQL DB** | `db-<cell>-<name>` en PG de la cell | 50 MB – 10 GB | **SÍ** (pg_dump -Fc) | Datos del cliente |
| **Filestore** | `cells/<cell>/.nkr-data/<short>-var_lib_odoo.ext4` | 100 MB – 5 GB | **SÍ** (extraer contenido, no el ext4 entero) | Attachments / Odoo binary fields |
| **Addons custom** | `cells/<cell>/instances/<name>/addons/` | 1–50 MB | **SÍ** | Custom modules del cliente (técnicamente recuperable de git pero defensivo) |
| **Pylibs** | `cells/<cell>/instances/<name>/pylibs/lib/` | 10–500 MB | **SÍ** | Dependencias pip del cliente |
| **odoo.conf** | `cells/<cell>/instances/<name>/config/odoo.conf` | <2 KB | **SÍ** (para refrescar workers/tier en restore) | Settings del tenant |
| **meta.json** | `cells/<cell>/instances/<name>/meta.json` | <500 B | **SÍ** | project_id, env, edition, tier |
| **Logs** | `cells/<cell>/instances/<name>/logs/` | 1–500 MB | **NO** (opcional, off por default) | Útiles para debug, no críticos |
| ~~rootfs (`odoo.ext4`)~~ | `cells/<cell>/instances/<name>/odoo.ext4` | 4 GB (o 0 si symlink) | **NO** | Es el master, se recrea via clone |
| ~~initramfs~~ | `/mnt/nkr/initramfs/<short>.cpio.gz` | 1 MB | **NO** | Se regenera en cada boot |
| ~~compose entry~~ | `cells/<cell>/nkr-compose.yml` | parte de yaml | **NO** | Se regenera en restore vía API |
| ~~SSO secret~~ | sección `[nkr_sso]` en odoo.conf | 64 chars | **NO** (regenera fresh per-restore) | Security — no preservar entre tenants |

### Decisión clave: backup CONTENIDO del filestore, no el ext4

El share `var_lib_odoo.ext4` es un block device de 2 GB asignado (sparse). Para backup:
- **Mal**: copiar el ext4 entero (2 GB declarados, 108 MB reales) → desperdicio + acoplado a tamaño/UUID fijo
- **Bien**: montar el ext4 RO, hacer `tar -I zstd -cf filestore.tar.zst /mnt/inspect/filestore` → portable, compacto, cell-agnostic

---

## 2. Estructura de archivos de backup

```
/mnt/nkr/backups/
├── odoo-v19-intech-devp/
│   ├── 2026-05-18-0300/           ← timestamp del backup
│   │   ├── meta.json              ← cell origen, tier, edition, version Odoo, kernel hash
│   │   ├── db.dump                ← pg_dump -Fc -Z 5 (custom format, comprimido)
│   │   ├── filestore.tar.zst      ← contenido del var_lib_odoo
│   │   ├── addons.tar.zst         ← cells/.../addons/
│   │   ├── pylibs.tar.zst         ← cells/.../pylibs/lib/
│   │   ├── odoo.conf              ← copia literal (referencia, no se restaura tal cual)
│   │   └── SHA256SUMS             ← checksum de cada archivo
│   ├── 2026-05-17-0300/           ← backup anterior
│   ├── 2026-05-16-0300/
│   └── ...
├── odoo-v17-cliente-X/
│   └── ...
└── _cell-level/                   ← backups cell-level (frequencia menor)
    ├── odoo-v17/
    │   ├── 2026-05-18-0400/
    │   │   ├── systemouts-addons.tar.zst  ← módulos cell-wide (nkr_sso, etc.)
    │   │   ├── nkr-compose.yml
    │   │   ├── pg-postgresql.conf
    │   │   └── pgbouncer.ini
    │   └── ...
    └── odoo-v19/
        └── ...
```

### Tooling para minimizar duplicación entre backups consecutivos

Btrfs soporta **reflinks** (CoW a nivel de archivo). Si el backup de hoy comparte el 95% del filestore con el de ayer, podemos:
- `cp --reflink=always` el último filestore → tarball NUEVO solo escribe blocks que cambiaron
- O mejor: tarball SIEMPRE pero zstd con dictionary entrenado en el backup previo (más complejo)

**Decisión simple v1**: tarball completo cada backup, retain N=7 días + 4 weeks (~32 archivos por tenant). Si el espacio se vuelve issue, optimizar después.

---

## 3. Cómo se ejecuta — script `nkr-backup`

### Per-tenant backup (~20–60 s para intech-devp)

```bash
#!/bin/bash
# /usr/local/bin/nkr-backup-tenant <cell> <nkr_name>
set -e
CELL=$1
NAME=$2
SHORT=${NAME#${CELL}-}
TIMESTAMP=$(date +%Y-%m-%d-%H%M)
BACKUP_DIR=/mnt/nkr/backups/${NAME}/${TIMESTAMP}
INST=/mnt/nkr/cells/${CELL}/instances/${NAME}
PG_IP=10.0.$(grep "^cell_id" /mnt/nkr/cells/${CELL}/cell.yml | awk '{print $2}').2
DB_NAME=db-${NAME}

mkdir -p "$BACKUP_DIR"
cd "$BACKUP_DIR"

# 1. Meta
cp "$INST/meta.json" .
cp "$INST/config/odoo.conf" .
echo "{
  \"cell_origin\": \"$CELL\",
  \"nkr_name\": \"$NAME\",
  \"backup_ts\": \"$TIMESTAMP\",
  \"db_name\": \"$DB_NAME\",
  \"nkr_version\": \"$(/usr/local/bin/nkr --version | awk '{print $2}')\",
  \"kernel_sha256\": \"$(sha256sum /mnt/nkr/kernel/nanolinux | awk '{print $1}')\"
}" > backup-info.json

# 2. PG dump (HOT — sin bloquear el tenant)
PGPASSWORD="$(awk -F= '/^db_password/ {print $2}' "$INST/config/odoo.conf" | tr -d ' ')" \
    pg_dump -h "$PG_IP" -p 5432 -U odoo -d "$DB_NAME" -Fc -Z 5 -f db.dump

# 3. Filestore (montar el ext4 RO + tar contenido)
MNT_FS=$(mktemp -d /tmp/nkr-backup-fs-XXX)
mount -o ro,loop /mnt/nkr/cells/${CELL}/.nkr-data/${SHORT}-var_lib_odoo.ext4 "$MNT_FS"
tar -I 'zstd -3' -cf filestore.tar.zst -C "$MNT_FS" .
umount "$MNT_FS" && rmdir "$MNT_FS"

# 4. Addons (custom modules del tenant)
tar -I 'zstd -3' -cf addons.tar.zst -C "$INST" addons/ 2>/dev/null

# 5. Pylibs (instaladas vía PUT /pylibs)
tar -I 'zstd -3' -cf pylibs.tar.zst -C "$INST/pylibs" lib/ 2>/dev/null

# 6. Checksums
sha256sum * > SHA256SUMS

echo "✅ Backup completado en $BACKUP_DIR ($(du -sh . | cut -f1))"
```

### Orquestación nightly (cron)

```bash
# /etc/cron.d/nkr-backup
# Daily backups at 03:00 — escalonado por tenant
0 3 * * * root /usr/local/bin/nkr-backup-all-tenants

# Weekly cell-level at 04:00 Sunday
0 4 * * 0 root /usr/local/bin/nkr-backup-cell-level

# GC: borrar backups >32 días (retain ~daily 7 + weekly 4)
0 5 * * * root /usr/local/bin/nkr-backup-gc
```

`nkr-backup-all-tenants` itera los tenants vía `GET /api/v1/cells/*/capacity` y llama a `nkr-backup-tenant` para cada uno. Secuencial (no concurrente — evita saturar PG en cells con muchos tenants).

---

## 4. Cuánto tarda — números medidos + proyección

### Baseline real con `intech-devp` (2026-05-18, datos vivos)

| Operación | Tamaño origen | Tiempo estimado | Output esperado |
|---|---:|---:|---:|
| `pg_dump -Fc -Z 5` (DB 77 MB) | 77 MB | ~5–10 s | ~30–50 MB |
| `tar zstd -3` filestore (108 MB) | 108 MB | ~10–15 s | ~40–60 MB (depende qué hay) |
| `tar zstd -3` addons (14 MB) | 14 MB | <2 s | ~3–5 MB |
| `tar zstd -3` pylibs (99 MB) | 99 MB | ~5–10 s | ~25–40 MB (numpy ya comprimido) |
| SHA256SUMS | n/a | <1 s | <1 KB |
| **TOTAL `intech-devp`** | **~300 MB** | **~25–40 s** | **~100–160 MB compressed** |

### Proyección para tenants reales con datos

| Tipo tenant | DB | Filestore | Addons | Pylibs | Total raw | Backup time | Backup size |
|---|---:|---:|---:|---:|---:|---:|---:|
| dev sandbox (intech-devp-like) | 80 MB | 100 MB | 15 MB | 100 MB | 300 MB | ~30 s | ~150 MB |
| staging con datos prod | 1 GB | 500 MB | 20 MB | 100 MB | 1.6 GB | ~90 s | ~500 MB |
| **prod 1 año de uso** | **5 GB** | **3 GB** | **30 MB** | **100 MB** | **8 GB** | **~5 min** | **~2.5 GB** |
| prod 3+ años pesado | 15 GB | 10 GB | 50 MB | 200 MB | 25 GB | ~15 min | ~8 GB |

### Backup completo del cluster (proyección)

| Escenario | Tenants | Tiempo secuencial | Tiempo paralelo (4 jobs) |
|---|---:|---:|---:|
| Hoy QA | 1 (intech-devp) | 30 s | 30 s |
| Pre-producción | 7 (1 cliente × 3 envs + 4 dev) | ~5 min | ~1.5 min |
| Producción ~50% | 50 (mix tiers) | ~50 min | ~15 min |
| Producción cap (14 tenants × 2 cells × 64 GB host) | 100 | ~2 h | ~30 min |

**Limitación**: PG admite N=~10 pg_dumps concurrentes sin saturar I/O. Cell-paralelizable mejor (1 dump por cell a la vez).

---

## 5. Cuánto pesa — proyección de almacenamiento

### Retention sugerida

- **7 backups diarios** (última semana)
- **4 backups semanales** (último mes)
- **3 backups mensuales** (último trimestre)
- = 14 backups por tenant retenidos en cualquier momento

### Costo de storage por tenant

| Tipo tenant | Backup size | × 14 backups | Anual |
|---|---:|---:|---:|
| dev | 150 MB | 2 GB | 2 GB (rotación constante) |
| staging | 500 MB | 7 GB | 7 GB |
| prod 1 año | 2.5 GB | 35 GB | 35 GB |
| prod 3 años | 8 GB | 112 GB | 112 GB |

### Costo total del cluster

| Escenario | # tenants | Mix | Total backups |
|---|---:|---|---:|
| Hoy QA | 1 | 1 dev | 2 GB |
| Pre-prod | 7 | 3 dev + 2 staging + 2 prod chico | ~25 GB |
| Producción 50% | 50 | mix realista | ~250 GB |
| **Producción cap** | **100** | **mix realista** | **~500 GB** |

**Disco actual disponible**: `/mnt/nkr` tiene **813 GB libres** (`btrfs filesystem df` reporta 832 GB raid1/0 totales — verificar). 500 GB para backups deja ~300 GB de overhead — **OK para los próximos años de crecimiento**.

Si el storage se vuelve issue:
- Bajar retention (7 diarios + 2 semanales = 9 backups → -40%)
- Subir compresión (zstd -19 en vez de -3 → -20% más, pero +5× tiempo)
- Off-site weekly + local solo último día (-80% local, agrega complejidad)

---

## 6. Restore (respawn) — cómo se levanta un tenant del backup

### Caso A — Restore en la MISMA cell con MISMO nombre (recovery)

```bash
nkr restore /mnt/nkr/backups/odoo-v19-intech-devp/2026-05-18-0300/
```

Flujo interno:
1. Validar checksums del backup (SHA256SUMS)
2. `DELETE` del tenant existente vía API (drop_db=true) — si existe
3. `POST /instances` con los mismos meta (cell, tier, edition, version) → crea instancia fresh + DB vacía
4. `nkr stop <name>` (para no race con PG)
5. `dropdb db-<name>` + `createdb db-<name>` (DB limpia)
6. `pg_restore -d db-<name> backup.dump`
7. Montar el nuevo `var_lib_odoo.ext4` y untar `filestore.tar.zst` adentro
8. Untar `addons.tar.zst` → `<inst>/addons/`
9. Untar `pylibs.tar.zst` → `<inst>/pylibs/lib/`
10. `nkr cell up <cell> -d` para arrancar de nuevo
11. Verify HTTP `:8069` responde

### Caso B — Restore con NUEVO nombre (cloning / migración)

```bash
nkr restore /mnt/nkr/backups/odoo-v19-intech-devp/2026-05-18-0300/ \
    --as intech-devp-clone
```

Como A pero el create usa `--as intech-devp-clone` como nkr_name. El renaming de DB en pg_restore se maneja vía `--dbname db-odoo-v19-intech-devp-clone` y se reescribe filestore path interno (`filestore/db-OLD/` → `filestore/db-NEW/`).

### Caso C — Restore en OTRA CELL de la misma versión

```bash
nkr restore /mnt/nkr/backups/odoo-v19-intech-devp/2026-05-18-0300/ \
    --as intech-devp \
    --to-cell odoo-v19-second   # nueva cell v19 que se acaba de crear
```

NKR valida `backup_info.json::odoo_version` matchea `cells/<new_cell>/cell.yml::odoo_version`. Si match → procede. Si no → 400 `version_mismatch`.

### Tiempos esperados de restore

| Tenant | Backup size | Restore time | Componentes lentos |
|---|---:|---:|---|
| intech-devp | 150 MB | ~30 s | pg_restore (~5 s) + untar filestore (~10 s) + boot (~15 s) |
| prod 1 año | 2.5 GB | ~5 min | pg_restore (~2 min) + untar (~1 min) + boot |
| prod 3 años | 8 GB | ~20 min | pg_restore (~10 min) + untar (~5 min) + boot |

---

## 7. Compatibilidad cross-cell de misma versión — la pregunta clave

**Requerimiento del usuario**: "tiene que ser compatible para que se levante en cualquier cell de la misma version".

### ¿Qué hace cell-portable el backup?

✅ **Portable (no cambia entre cells)**:
- DB content (pg_dump es 100% portable entre PG de misma versión)
- Filestore (contenido binario)
- Addons (Python source code)
- Pylibs (wheels Python)
- meta.json (project_id, env, edition — info del tenant)
- odoo.conf settings de aplicación (workers, limits)

❌ **NO portable (regenerado per-cell en restore)**:
- DB_HOST en odoo.conf (cell-specific: `10.0.<cell_id>.3`)
- `[nkr_sso] secret` (se regenera fresh per-tenant para no leak across cells)
- IP del guest (`guest_ip` en meta.json — se asigna nuevo)
- vm_id (registry asigna)
- DNS (panel maneja separado vía `/dns`)

### Validación automática en restore

```python
# Pseudocódigo del restore handler
def restore(backup_dir, target_cell, new_name):
    info = load(backup_dir + "/backup-info.json")
    target = load(cells_dir / target_cell / "cell.yml")

    # Validar compat
    if info["odoo_version"] != target["odoo_version"]:
        raise Conflict("odoo_version mismatch")
    if info["kernel_sha256"] != current_kernel_sha256():
        warn("kernel cambió desde el backup — proceder con cautela")

    # Procede con restore (los settings cell-specific se regeneran)
    ...
```

---

## 8. Riesgos + edge cases

### Riesgos identificados

1. **PG dump bloquea queries DDL** del tenant durante el dump. Sin embargo, DML (INSERT/UPDATE) sigue funcionando. Bajo riesgo en producción Odoo (rara vez hace DDL en runtime).
   - Mitigación: ya está mitigado por design (`pg_dump` toma `ACCESS SHARE LOCK`).

2. **Race entre backup y delete del tenant**: si el tenant se borra mientras se backupea, el dump falla.
   - Mitigación: agregar lock en NKR — `nkr backup` toma el mismo InstanceLock que `delete_instance`.

3. **Race entre backup y restart del tenant**: el restart re-monta el filestore ext4 — si nuestro mount RO ya está activo, el guest no puede tomar el RW exclusive.
   - Mitigación: usar `mount -o ro,loop,nofail` con `noload` (skip journal). El guest abrirá su mount sin conflicto.

4. **PG dump de tenant con tabla muy grande**: 100M+ rows pueden tardar >30 min.
   - Mitigación: para esos casos, `pg_dump -j 4` (parallel). v2 feature.

5. **Filestore con archivos abiertos por Odoo**: tar puede ver archivos a medio-escribir.
   - Mitigación: el filestore de Odoo es append-only (nuevos files con hash único). Files en escritura tienen `.tmp` y se rename atómico. Tar puede leer files completos.

6. **Espacio en disco para backups durante el dump**: pg_dump escribe el dump completo a tmp file. 10 GB DB → necesita 10 GB libres temporales.
   - Mitigación: pipe pg_dump | gzip directo, o validar espacio antes.

7. **Backup corrupto silencioso** (PG dump OK pero filestore corrupto): SHA256SUMS detecta diffs en futuras restauraciones, pero solo cuando se intenta.
   - Mitigación v2: `nkr backup --verify` opcional que pg_restore + tar -t para validar.

### Edge cases

- **Tenant sin DB** (`db_name=False` en odoo.conf): skip pg_dump, solo respaldar filestore + addons + meta. Útil para cold templates.
- **Tenant nunca booteó**: filestore puede estar vacío, addons vacío. Backup completa pero pequeño.
- **DB con extensiones custom** (pg_trgm, postgis...): pg_dump las incluye automáticamente. Restore funciona si target cell tiene las mismas extensiones disponibles (deben estar en master PG).
- **Tenant durante POST /addons/git en curso**: skip backup (race con el rename atómico de addons). Detectar via lockfile.

---

## 9. Implementación por fases

### Fase 1 — Script manual + smoke test (2–3 días)

1. Implementar `/usr/local/bin/nkr-backup-tenant` (bash inicialmente).
2. Probar con `intech-devp`: backup completo + checksum + restore en nuevo nombre `intech-devp-restored`.
3. Verificar HTTP del restorado responde + DB tiene mismos datos.
4. Medir tiempos reales y comparar con estimaciones.
5. Commit.

### Fase 2 — Restore tooling (2 días)

1. Implementar `/usr/local/bin/nkr-restore <backup_dir> [--as N] [--to-cell C]`.
2. E2E test: backup intech-devp → DELETE → restore → verify.
3. Cross-cell test: backup en odoo-v19 → restore en odoo-v19-otra (cuando exista).
4. Commit.

### Fase 3 — HTTP API (3 días)

1. `POST /api/v1/cells/{cell}/instances/{name}/backup` → 202 async, devuelve poll URL
2. `GET /api/v1/backups/{tenant}` → lista backups disponibles
3. `POST /api/v1/restore` → restore con body JSON (backup_path, target cell, new_name)
4. Panel-integrable. Commit.

### Fase 4 — Cron + retention + observability (2 días)

1. `/etc/cron.d/nkr-backup` con backups nightly por tenant
2. `/usr/local/bin/nkr-backup-gc` que aplica retention (7d + 4w + 3m)
3. Métrica en `/metrics` Prometheus: `nkr_backup_age_seconds{tenant}`, `nkr_backup_size_bytes{tenant}`
4. Alarma: si tenant no se backupeó en >24 h, log WARN al journal del daemon.
5. Commit.

### Fase 5 (futuro, no en scope inicial) — Off-site

- Rclone a S3/Backblaze de los backups del último día
- WAL archiving para PITR
- Verificación periódica via `nkr backup --verify`

---

## 10. Preguntas abiertas antes de implementar

1. **Retention exacta**: ¿7d+4w+3m está bien o querés diferente?
2. **Hora del cron**: 03:00 sirve o preferís otra ventana?
3. **¿Backupear logs?** Default propuesto: NO (no críticos). Lo activamos solo si se necesita para debug forense en algún tenant.
4. **¿Backup también de templates** (`<cell>-odoo-template`, `<cell>-odoo-template-enterprise`)? Default propuesto: NO (son recreables desde el master rootfs + cell setup), pero sí del `db-<cell>-odoo-template` (para reset rápido si se corrompe).
5. **¿Compresión**: zstd -3 (default propuesto, rápido) vs zstd -19 (más chico, mucho más lento)?
6. **¿Permitir backups concurrentes** entre tenants? Default: NO (secuencial dentro de una cell, paralelizable entre cells).
7. **¿Encriptar backups en disco?** Default: NO en v1 (mismo servidor, mismo acceso). v2 si vamos a off-site.

---

## 11. Lo que YO recomiendo como punto de partida

1. **Empezar Fase 1** (script bash + test con intech-devp). Concreto, mide tiempos reales, sin riesgo.
2. Implementar Fase 2 (restore) inmediatamente después — un backup que no podemos restaurar es inútil.
3. **Saltarse la HTTP API por ahora** — los backups son ops, no del panel. CLI + cron es suficiente.
4. Cron + retention (Fase 4) en cuanto haya 3+ tenants reales.
5. Off-site lo dejamos para cuando haya datos reales de producción que justifiquen el costo.

**Antes de implementar, esperar tu aprobación de:**
- Estructura de archivos (§2)
- Plan ejecutivo de Fases (§9)
- Respuestas a las 7 preguntas abiertas (§10)
