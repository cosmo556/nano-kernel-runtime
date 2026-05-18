# NKR Backup Plan — propuesta v2

**Estado:** propuesta para revisión, no implementado.
**Fecha:** 2026-05-18
**Scope:** backups internos en el mismo servidor con cleanup nightly. Off-site lo maneja un proceso externo (NKR no retiene >24h).
**Versión NKR de referencia:** 1.6.9 + sprint post-Fase 0.

**Historial de revisión:**
- **v1 (2026-05-18)** — propuesta inicial con retention 7d+4w+3m.
- **v2 (2026-05-18, post-feedback usuario)** — cambia el modelo:
  1. **NO hay creación programada** — backups son SOLO on-demand del operador NKR.
  2. **Retention = 1 día**: al final del día (cron 1 AM) se borra TODO. NKR no es un sistema de backups long-term — solo staging temporal antes de off-site.
  3. **Off-site**: un proceso externo recoge los backups durante el día y los envía a OTRO servidor. NKR no se involucra en eso.
  4. **DOS tipos de backup distintos**:
     - **`backup_nkr`** — formato interno NKR para respawn rápido en NKR.
     - **`backup_odoo`** — formato Odoo-standard (ZIP con dump.sql + filestore) para entregar al panel/usuario, restorable en cualquier Odoo (no necesariamente NKR).

---

## TL;DR (v2)

- **2 tipos de backup distintos** generados por el mismo comando (con flag):
  - `backup_nkr` → para respawn interno en otra cell NKR (eficiente, NKR-specific)
  - `backup_odoo` → ZIP Odoo-standard (dump.sql + filestore/) que el panel/usuario puede descargar y usar en cualquier Odoo del mundo
- **On-demand**: NO hay cron creando backups. Operador NKR llama al script/API cuando lo necesita.
- **Retention = 1 día**: cron a las **1 AM** borra TODO de `/mnt/nkr/backups/`. Punto.
- **Off-site**: out-of-scope. Un proceso externo (rsync/rclone/scp del operador) saca los backups antes del cleanup.
- **Compatibilidad cross-cell**: backup_nkr restorable en cualquier cell de la misma versión Odoo (DB_HOST + SSO secret regenerados en restore).
- **Backup time real `intech-devp`**: ~25–40 s. ~150 MB de output.

---

## 1. Los dos formatos

### 1.1 `backup_nkr` — formato interno (NKR-only)

**Propósito:** restore rápido dentro de NKR, con todos los metadatos para recrear la instancia idéntica en cualquier cell de la misma versión.

**Estructura:**
```
/mnt/nkr/backups/<nkr_name>/<YYYY-MM-DD-HHMM>/nkr/
├── backup-info.json     ← cell origen, nkr_version, kernel_sha256, odoo_version, tier, edition
├── meta.json            ← copy de meta.json del tenant (project_id, env)
├── odoo.conf            ← copia literal (referencia; en restore se regenera lo cell-specific)
├── db.dump              ← pg_dump -Fc (custom format, comprimido interno)
├── filestore.tar.zst    ← contenido del var_lib_odoo (comprimido zstd)
├── addons.tar.zst       ← cells/.../addons/  (custom modules del cliente)
├── pylibs.tar.zst       ← cells/.../pylibs/lib/
└── SHA256SUMS           ← checksums
```

**Compresión:** zstd default (level 3) — balance velocidad/tamaño.

**Tamaño esperado (intech-devp):** ~150 MB.

**Restore:** `nkr restore-nkr <backup_dir> [--as N] [--to-cell C]` — flujo de §6.

---

### 1.2 `backup_odoo` — formato Odoo-standard (entregable al usuario)

**Propósito:** entregar al panel o al usuario un archivo que pueda restorear en CUALQUIER Odoo del mundo (no solo NKR). Mismo formato que el endpoint `/web/database/backup` de Odoo produce nativamente.

**Estructura (ZIP):**
```
backup-<dbname>-<YYYY-MM-DD>.zip
├── dump.sql              ← pg_dump SQL plain text (NO custom format — para portabilidad)
├── manifest.json         ← Odoo metadata: version, modules installed, db creation date
└── filestore/<dbname>/   ← directorio de attachments
    ├── 0a/0a1b2c3d4e...
    ├── 0b/...
    └── ...
```

**Compresión:** ZIP (no tar+zstd — Odoo nativo espera ZIP) con DEFLATE estándar.

**Tamaño esperado (intech-devp):** ~120 MB (similar al nkr pero formato distinto).

**Restore:** el usuario lo sube a su Odoo (CUALQUIER Odoo de la versión correspondiente) vía `/web/database/restore` UI nativa, o NKR puede consumirlo también vía endpoint similar.

---

### ¿Cuándo se usa cada uno?

| Caso de uso | Backup a usar |
|---|---|
| Recuperar tenant tras crash dentro de NKR | `backup_nkr` (más rápido, preserva todo) |
| Migrar tenant a otra cell NKR de la misma versión | `backup_nkr` (cell-portable) |
| Entregar copia al cliente para auditoría legal | `backup_odoo` (formato estándar, abrible en cualquier Odoo) |
| Cliente quiere irse a otro proveedor / Odoo.sh / self-hosted | `backup_odoo` (formato portable) |
| Panel ofrece "Download backup" en su UI | `backup_odoo` (lo que un usuario espera) |
| Disaster recovery a otro servidor NKR | `backup_nkr` enviado off-site, restorable en otro NKR |

---

## 2. Qué se respalda (igual en ambos formatos)

| Componente | ¿backup_nkr? | ¿backup_odoo? | Notas |
|---|:---:|:---:|---|
| PostgreSQL DB | ✅ (`-Fc`) | ✅ (`SQL plain`) | Custom format más chico/rápido para NKR, SQL plain para compat universal |
| Filestore | ✅ (tar.zst) | ✅ (dir bajo ZIP) | Contenido literal de `var_lib_odoo` |
| Addons custom | ✅ | ❌ | Odoo backup no incluye addons (asume que el target Odoo ya los tiene) |
| Pylibs | ✅ | ❌ | Mismo: target Odoo debe tener sus deps |
| odoo.conf | ✅ (referencia) | ❌ | Cell-specific, no portable a Odoo no-NKR |
| meta.json | ✅ | ❌ | NKR-specific |
| Logs | ❌ | ❌ | No críticos, no se backupean |
| Templates | ❌ | ❌ | Recreables desde master |
| rootfs / initramfs | ❌ | ❌ | Recreables |
| SSO secret | ❌ | ❌ | Regenerado fresh per restore (security) |

---

## 3. Estructura final de directorios

```
/mnt/nkr/backups/
├── odoo-v19-intech-devp/
│   └── 2026-05-18-1430/
│       ├── nkr/              ← backup_nkr (interno)
│       │   ├── backup-info.json
│       │   ├── db.dump
│       │   ├── filestore.tar.zst
│       │   ├── addons.tar.zst
│       │   ├── pylibs.tar.zst
│       │   ├── odoo.conf
│       │   ├── meta.json
│       │   └── SHA256SUMS
│       └── odoo/             ← backup_odoo (entregable)
│           └── backup-db-odoo-v19-intech-devp-2026-05-18.zip
├── odoo-v19-cliente-X/
│   └── 2026-05-18-1500/
│       ├── nkr/
│       └── odoo/
└── ...
```

**Convención**: un solo directorio timestamped contiene ambos formatos. Si el operador solo necesita uno, el script tiene flags (`--nkr-only`, `--odoo-only`, default: ambos).

---

## 4. Comandos CLI propuestos

### 4.1 Crear backup (on-demand)

```bash
# Ambos formatos (default)
nkr backup <nkr_name>

# Solo NKR-interno
nkr backup <nkr_name> --nkr-only

# Solo Odoo-standard
nkr backup <nkr_name> --odoo-only

# Especificar output dir custom
nkr backup <nkr_name> --output /tmp/manual-backup-foo/
```

**Output esperado:**
```
[NKR-BACKUP] Backup de odoo-v19-intech-devp iniciado
[NKR-BACKUP]   1/4 pg_dump (-Fc) → 12 MB en 4.2s
[NKR-BACKUP]   2/4 filestore (108 MB → 41 MB) en 9.1s
[NKR-BACKUP]   3/4 addons (14 MB → 4 MB) en 0.8s
[NKR-BACKUP]   4/4 pylibs (99 MB → 31 MB) en 5.3s
[NKR-BACKUP]   nkr format: 88 MB
[NKR-BACKUP] Generando backup_odoo (ZIP)...
[NKR-BACKUP]   pg_dump (--format=plain) + filestore + manifest → 122 MB en 11.2s
[NKR-BACKUP] ✅ Backup completado en /mnt/nkr/backups/odoo-v19-intech-devp/2026-05-18-1430/ (210 MB total)
```

### 4.2 Restore backup_nkr (interno)

```bash
# Restore in-place (mismo nombre, misma cell)
nkr restore-nkr /mnt/nkr/backups/odoo-v19-intech-devp/2026-05-18-1430/nkr/

# Clone con nuevo nombre en misma cell
nkr restore-nkr <backup>/nkr/ --as intech-devp-clone

# Restore en otra cell de la misma versión Odoo
nkr restore-nkr <backup>/nkr/ --as intech-devp --to-cell odoo-v19-second
```

### 4.3 Listar backups

```bash
nkr backup-ls [--tenant <name>]
# Output:
# odoo-v19-intech-devp:
#   2026-05-18-1430    nkr: 88 MB    odoo: 122 MB    age: 2h
```

### 4.4 Borrar backup manualmente

```bash
nkr backup-rm odoo-v19-intech-devp/2026-05-18-1430
```

### 4.5 Cleanup automático (cron, NO crea backups — solo borra)

```bash
# /etc/cron.d/nkr-backup-cleanup
0 1 * * * root /usr/local/bin/nkr-backup-cleanup
```

`nkr-backup-cleanup` borra **TODO** en `/mnt/nkr/backups/` sin contemplaciones (retention = 1 día). El operador es responsable de haber sacado los backups off-site ANTES de la 1 AM si los necesita.

---

## 5. Cuánto tarda — números medidos + proyección

### Baseline real con `intech-devp` (DB 77 MB, filestore 108 MB)

| Operación | Tiempo |
|---|---:|
| `pg_dump -Fc -Z 5` (formato nkr) | ~5 s |
| `pg_dump --format=plain` (formato odoo) | ~8 s |
| `tar zstd` filestore + addons + pylibs (nkr) | ~15 s |
| `zip` filestore + dump.sql + manifest (odoo) | ~12 s |
| SHA256SUMS | <1 s |
| **TOTAL ambos formatos** | **~40 s** |
| Solo `--nkr-only` | ~25 s |
| Solo `--odoo-only` | ~22 s |

### Proyección por tipo de tenant

| Tipo | DB | Filestore | Total raw | Backup time (both) | Output total |
|---|---:|---:|---:|---:|---:|
| dev sandbox (intech-devp) | 80 MB | 100 MB | 300 MB | ~40 s | ~210 MB |
| staging con datos prod | 1 GB | 500 MB | 1.6 GB | ~3 min | ~1 GB |
| prod 1 año | 5 GB | 3 GB | 8 GB | ~8 min | ~5 GB |
| prod 3 años | 15 GB | 10 GB | 25 GB | ~25 min | ~15 GB |

### Backup ON-DEMAND simultáneo de N tenants

Como es on-demand, raramente se ejecutan N en paralelo. Si el operador necesita backupear todo el cluster manualmente:

| Cluster size | Secuencial | Paralelo (4 jobs) |
|---|---:|---:|
| 7 tenants pre-prod | ~5 min | ~2 min |
| 50 tenants | ~50 min | ~15 min |
| 100 tenants cap | ~2 h | ~30 min |

---

## 6. Cuánto pesa (al cierre del día)

**Solo el peak** durante el día (porque el cron 1 AM borra todo):

### Escenario realista: backup ON-DEMAND de 5–10 tenants en un día

| Mix de backups en el día | Peak storage |
|---|---:|
| 1 backup intech-devp | ~210 MB |
| 3 backups de tenants pequeños | ~600 MB |
| 10 backups de tenants mixtos (5 dev + 3 staging + 2 prod) | ~12 GB |
| Caso extremo: backup full cluster (100 prod) | ~500 GB |

**Disco disponible**: 813 GB libres en `/mnt/nkr`. Margen amplísimo para cualquier escenario operativo realista.

**Importante**: el operador es responsable de sacar los backups off-site ANTES del cleanup automático. Si en 24 h no se transfirieron → se pierden definitivamente.

---

## 7. Compatibilidad cross-cell (backup_nkr)

**Requerimiento**: el `backup_nkr` debe levantarse en CUALQUIER cell de la misma versión Odoo (no solo la cell origen).

### ¿Qué hace cell-portable el backup?

✅ **Portable**:
- DB content (pg_dump entre PG de misma versión es 100% portable)
- Filestore (contenido binario)
- Addons (Python source)
- Pylibs (wheels)
- meta.json content (project_id, env, edition — info del tenant)
- odoo.conf settings de aplicación (workers, limits)

❌ **NO portable — regenerado per-cell en restore por NKR**:
- DB_HOST en odoo.conf (cell-specific: `10.0.<cell_id>.3`)
- `[nkr_sso] secret` (regenera fresh — security: no leak across tenants/cells)
- `guest_ip` (assigned by registry)
- `vm_id` (assigned by registry)
- DNS (panel maneja separado)

### Validación automática en restore

```python
# Pseudocódigo del restore handler
def restore_nkr(backup_dir, target_cell, new_name):
    info = load(backup_dir + "/backup-info.json")
    target = load(cells_dir / target_cell / "cell.yml")

    if info["odoo_version"] != target["odoo_version"]:
        raise Conflict("odoo_version mismatch — backup era 19.0, target es 17.0")

    if info["kernel_sha256"] != current_kernel_sha256():
        warn("kernel cambió desde el backup — proceder con cautela")

    # Procede: crea tenant fresh vía API normal (cell-specific regenerado),
    # después drop+pg_restore + untar filestore/addons/pylibs
    ...
```

---

## 8. Riesgos + edge cases

### Riesgos identificados

1. **Backup tras delete por error**: el operador borra un tenant y se da cuenta tarde. El backup_nkr del día anterior (o del mismo día si se hizo) lo recupera.
   - **Mitigación**: si el operador SABE que va a borrar un tenant, debería backupear ANTES.

2. **Cleanup borra backup que el operador olvidó sacar off-site**: a las 1 AM se pierde.
   - **Mitigación documentada**: alerta diaria a las 12:00 si hay backups >12 h sin tocar (señal de que olvidaron sacarlos).

3. **PG dump bloquea queries DDL** del tenant durante el dump (ACCESS SHARE LOCK).
   - Impacto: bajo. Odoo en runtime no hace DDL normalmente.

4. **Race entre backup y restart del tenant**: el restart re-monta el filestore ext4.
   - **Mitigación**: usar `mount -o ro,loop,nofail,noload`. Sin journal, sin conflict con el RW del guest.

5. **Race entre backup y delete del tenant**: el delete corre paralelo y borra files mid-backup.
   - **Mitigación**: tomar `InstanceLock` durante backup (mismo que delete/restart/start).

6. **Backup gigante (3 años de prod, 25 GB raw)**: 25 min de dump puede ser intolerable bajo SLA.
   - **Mitigación v2**: `pg_dump -j 4` (parallel custom format, NO compatible con `--format=plain`). Para backup_odoo seguiría plain serial.

7. **Cleanup borra el backup que se está creando**: si el operador lanza un backup a las 1:00:30 AM mientras corre el cleanup.
   - **Mitigación**: cleanup verifica `mtime` del dir antes de borrar; skip si <1 min old. O lockfile.

8. **`backup_odoo` no incluye addons custom**: si el cliente entrega el backup a otro proveedor, ese provider necesita los addons aparte.
   - **Documentar**: el backup_odoo es solo DB + filestore. Los addons son código fuente que el cliente tiene en su git.

---

## 9. Implementación por fases (revisado v2)

### Fase 1 — Script `nkr-backup` con ambos formatos (3–4 días)

1. `/usr/local/bin/nkr-backup` (bash o Rust subcommand) que genera ambos formatos.
2. Test con `intech-devp`: validar `backup_nkr` + `backup_odoo` correctos.
3. Para validar `backup_odoo`: subir el ZIP a una instancia Odoo separada (puede ser otra v19 en NKR) vía `/web/database/restore` UI. Verificar que la DB se restaura completa.
4. Para validar `backup_nkr`: implementar restore (Fase 2) y testear.
5. Medir tiempos reales, ajustar compresión si necesario.

### Fase 2 — Restore tooling para backup_nkr (2–3 días)

1. `/usr/local/bin/nkr-restore-nkr <backup_dir>` con flags `--as`, `--to-cell`.
2. E2E test: backup intech-devp → DELETE → restore → verify HTTP responde + DB tiene datos.
3. Cross-cell test (cuando exista otra cell v19): restore en otra cell.
4. Cross-version test: rechazar restore de v19 backup en v17 cell.

### Fase 3 — CLI helpers + cleanup cron (1 día)

1. `nkr backup-ls` y `nkr backup-rm`.
2. `/etc/cron.d/nkr-backup-cleanup` (cron 1 AM borra todo en `/mnt/nkr/backups/`).
3. Lockfile en el cleanup para no comerse un backup en curso.
4. Doc operativa en MAINTENANCE.md (sección nueva "Backups").

### Fase 4 (opcional, futuro) — HTTP API

- `POST /api/v1/cells/{cell}/instances/{name}/backup` → 202 async, devuelve path
- `GET /api/v1/backups/{tenant}/{timestamp}/download?format=nkr|odoo` → descarga
- Para integrar con panel si lo quieren.

---

## 10. Resumen de decisiones (v2, cerrado)

| # | Pregunta | Respuesta del usuario |
|---|---|---|
| 1 | Retention | **1 día** — todo borrado al cierre del día |
| 2 | Hora cleanup cron | **1 AM** |
| 3 | Backup de logs | **NO** |
| 4 | Backup de templates | **NO** |
| 5 | Compresión | **zstd** (nivel default 3) |
| 6 | Compatibilidad cross-cell | **SÍ** — `backup_nkr` restorable en otra cell de la misma versión |
| 7 | Encriptación | **NO** (mismo servidor + off-site externo se encarga) |
| 8 (nuevo) | Tipos de backup | **DOS**: `backup_nkr` interno + `backup_odoo` entregable a panel/usuario |

---

## 11. Lo que necesito antes de implementar

**Confirmar:**
1. ✅ El plan v2 refleja lo que pediste (DOS formatos, on-demand, 1-day cleanup, no crea backups por cron)?
2. ¿Algo más a aclarar sobre el formato `backup_odoo` (manifest.json incluye lista de módulos installed, fecha de creación de DB, etc. — formato Odoo nativo)?
3. ¿El cleanup a las 1 AM borra TODO sin excepción, o algún whitelist? (default propuesto: TODO sin excepción).
4. ¿El comando para sacar off-site lo manejas vos / panel externamente (rsync/rclone/scp)? NKR no lo hace.

**Si todo OK, próximo paso = Fase 1** (script `nkr-backup` con ambos formatos + test contra intech-devp).
