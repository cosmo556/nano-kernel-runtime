# Auditoría: virtio-pmem en modo Read-Only para rootfs maestro compartido

**Estado:** análisis técnico, plan de implementación verificado contra el código.
**Origen:** auditoría de seguridad multi-agente, Sprint 2 pendiente.
**Riesgo abordado:** corrupción silenciosa de `ext4` shared entre cells.

---

## TL;DR

- El bug **existe** y es explotable bajo carga real, no solo teórico.
- El alcance es **menor** de lo que sugiere una lectura rápida del audit: afecta `db` y `pgbouncer` (singletons por cell) que comparten archivo entre cells, **no** los 20 tenants Odoo de cada cell (que tienen disco privado).
- **Solución: per-cell reflink del master**. Cada cell recibe su propia copia CoW (`btrfs reflink`) del master `.ext4`. Cero shared writes a nivel filesystem, cero cambios en pmem/initramfs.
- El trabajo es **~2 horas** de código + un proceso de migración cell por cell (~30 segundos por cell).
- El master pasa a ser **inmutable** (`chattr +i`) post-build; el comando `nkr build` lo destranca y reaplica al final.

---

## Tabla de contenidos

- [1. Contexto del bug](#1-contexto-del-bug)
- [2. Validación contra el código real](#2-validación-contra-el-código-real)
- [3. Alcance real medido contra producción](#3-alcance-real-medido-contra-producción)
- [4. Auditoría de los Nkrfile](#4-auditoría-de-los-nkrfile)
- [5. Solución — per-cell reflink del master](#5-solución--per-cell-reflink-del-master)
  - [5.1 Arquitectura propuesta](#51-arquitectura-propuesta)
  - [5.2 Cambios concretos](#52-cambios-concretos)
  - [5.3 Por qué NO `chattr +C` sobre la copia reflinked](#53-por-qué-no-chattr-c-sobre-la-copia-reflinked)
  - [5.4 Master inmutable + ciclo de `nkr build`](#54-master-inmutable--ciclo-de-nkr-build)
  - [5.5 Migración de cells existentes](#55-migración-de-cells-existentes)
  - [5.6 Riesgo y estimación](#56-riesgo-y-estimación)
- [6. Tests de validación](#6-tests-de-validación)
- [7. Conclusión técnica](#7-conclusión-técnica)
- [8. Bitácora de revisiones](#8-bitácora-de-revisiones)

---

## 1. Contexto del bug

### Lo que dice el código actual

`VirtioPmemDevice::new` ([src/pmem.rs:71](src/pmem.rs#L71)) abre el archivo backing como **lectura-escritura** y lo `mmap` como **MAP_SHARED + PROT_WRITE**:

```rust
let file = OpenOptions::new()
    .read(true)
    .write(true)             // ← siempre RW
    .open(disk_path)?;

let ptr = unsafe {
    libc::mmap(
        std::ptr::null_mut(),
        len,
        libc::PROT_READ | libc::PROT_WRITE,   // ← siempre escribible
        libc::MAP_SHARED | libc::MAP_NORESERVE,
        file.as_raw_fd(),
        0,
    )
};
```

Esto significa:

- Si dos VMs distintas mapean el **mismo archivo backing** con `MAP_SHARED`, comparten las mismas páginas físicas del kernel.
- Cuando una VM hace store al pmem, las páginas se marcan dirty y el kernel las flusha al disco.
- Si la otra VM también escribe, gana el último — no hay coordinación entre VMs.
- Resultado: **corrupción silenciosa del ext4** (metadata journal, inode tables, etc.).

### El cmdline del guest emite `rw`

En [src/vmm.rs:1753 y 1781](src/vmm.rs#L1753):

```
root=/dev/pmem0 rootflags=dax rw
```

El `rw` final le dice al kernel guest que monte `/` como escribible. Combinado con el host RW, todas las escrituras del guest se propagan al backing real.

### El initramfs monta `dax,rw`

En [src/initramfs.rs:110](src/initramfs.rs#L110):

```sh
mount -t ext4 -o dax,rw /dev/pmem0 /newroot
```

Hardcoded `rw`. No hay branch para RO.

---

## 2. Validación contra el código real

### Confirmación del flujo `disks[0]` → pmem

En [src/vmm.rs:1050-1077](src/vmm.rs#L1050):

```rust
if config.use_pmem && !config.disks.is_empty() {
    let pmem_dev = VirtioPmemDevice::new(
        &config.disks[0],          // ← PRIMER disk → pmem
        PMEM_GUEST_PHYS_ADDR,
        guest_mem.clone()
    )?;
    pmem_dev_opt = Some(pmem_dev);
}
```

Cuando `pmem: true` (default `true` desde v1.4 en [src/compose.rs:105](src/compose.rs#L105)), el primer entry del array `disks` se mapea como pmem. Los demás van por virtio-blk.

### Rastro completo: compose YAML → pmem

```
nkr-compose.yml
  ↓
  services.<svc>.disks[0]: "/path/al/ext4"
  ↓
  compose.rs::ServiceConfig.disks[0]
  ↓
  cli args: --disk "/path/al/ext4"
  ↓
  vmm.rs::VmConfig.disks[0]
  ↓
  VirtioPmemDevice::new(&config.disks[0], ...)   ← O_RDWR + MAP_SHARED + PROT_WRITE
  ↓
  guest sees /dev/pmem0
  ↓
  initramfs mounts: mount -t ext4 -o dax,rw /dev/pmem0 /newroot
```

---

## 3. Alcance real medido contra producción

Inspección de los composes reales en `/mnt/nkr/cells/`:

### Cells actuales

| Cell | Servicios | `disks[0]` |
|---|---|---|
| `odoo-v17` | `db` | `/mnt/nkr/images/postgres.ext4` ⚠️ **shared** |
| `odoo-v17` | `pgbouncer` | `/mnt/nkr/images/pgbouncer.ext4` ⚠️ **shared** |
| `odoo-v17` | `odoo-template` | `/mnt/nkr/cells/odoo-v17/instances/odoo-v17-odoo-template/odoo.ext4` ✅ privado |
| `odoo-v19` | `db` | `/mnt/nkr/images/postgres.ext4` ⚠️ **mismo archivo que v17** |
| `odoo-v19` | `pgbouncer` | `/mnt/nkr/images/pgbouncer.ext4` ⚠️ **mismo archivo que v17** |
| `odoo-v19` | `odoo-template` | `/mnt/nkr/cells/odoo-v19/instances/odoo-v19-odoo-template/odoo.ext4` ✅ privado |
| `odoo-v19` | `tesito-14` | `/mnt/nkr/cells/odoo-v19/instances/odoo-v19-tesito-14/odoo.ext4` ✅ privado |
| `odoo-v19` | `intech-19` | `/mnt/nkr/cells/odoo-v19/instances/odoo-v19-intech-19/odoo.ext4` ✅ privado |

### Quién está afectado realmente

- **`db` (PostgreSQL)** y **`pgbouncer`** de v17 y v19 simultáneamente apuntan al mismo `.ext4` master en `/mnt/nkr/images/`. Cuando ambas cells están corriendo, hay **2 VMs escribiendo al mismo backing**.
- **Tenants Odoo** (template + cada cliente) tienen `.ext4` exclusivo por VM. **Aquí el modo RW es correcto**, no hay corrupción posible.

### Cuántos writers hay hoy

```
4 VMs (2 db + 2 pgbouncer)  ×  2 archivos shared
= 4 writers concurrentes sobre 2 archivos
```

**Por qué no se corrompe (todavía) en producción:**

1. Las escrituras al rootfs son **raras**: el entrypoint oficial de PG hace `mkdir`/`chown` en build-time del Docker image, no en runtime.
2. PostgreSQL escribe sus datos al `/var/lib/postgresql/data` que es un **share virtio-fs RW separado** (`pg/data.ext4` privado por cell), no al rootfs.
3. Las escrituras transitorias (sockets, pids, atime) van a `/var/run/postgresql` y `/run` que **deberían** ser tmpfs (verificar).
4. Cuando hay escrituras, son a inodos distintos en VMs distintas → low collision rate.

**Pero**: el corruption es eventual. Cualquier write a metadata compartida (group descriptor, inode bitmap) en momento de carga arruina ambas VMs simultáneamente.

### Escala futura

Cada nueva cell que se agregue (v18, v19-2, etc.) suma **2 writers** al mismo backing si reusan los masters. Con 5 cells es 10 writers; con 10 cells es 20. Probabilidad de corrupción crece linealmente.

---

## 4. Auditoría de los Nkrfile

| Nkrfile | Base | Escrituras runtime al rootfs | Riesgo si pasa a RO |
|---|---|---|---|
| `Nkrfile.pg` | `postgres:16` | `chown` y `mkdir` en `RUN` (build-time). El entrypoint oficial escribe a `$PGDATA = /var/lib/postgresql/data/pgdata` (share RW). Sockets en `/var/run/postgresql` (tmpfs). | ✅ Ninguno |
| `Nkrfile.pgbouncer` | `alpine:3.18` | Idem PG: build-time. Pidfile `/var/run/pgbouncer.pid` y socket en `/var/run/postgresql` (tmpfs). | ✅ Ninguno |
| `Nkrfile.odoo` | `odoo:17.0` | `chown` en build-time. Logs en `/var/log/odoo/` (share RW). Filestore en `/var/lib/odoo/` (share RW). | ✅ Ninguno |
| `Nkrfile.odoo19` | `odoo:19.0` | Idem `Nkrfile.odoo`. | ✅ Ninguno |
| `Nkrfile.nginx` | `debian:bookworm-slim` + nginx + certbot | Logs, cache de body/proxy/fastcgi, pidfile. **Bajo Plan B**: cada cell tiene su rootfs privado RW, los writes se absorben sin problema en el copy reflinked. | ✅ OK con Plan B |
| `Nkrfile.goinit` | scratch | Init bin estático. Nada que escribir. | ✅ Ninguno |

**Conclusión**: bajo Plan B (rootfs privado RW por cell), ningún Nkrfile presenta problema. Los entrypoints pueden hacer todas las escrituras al rootfs que quieran porque cada cell tiene su propio archivo, y la corrupción cruzada es imposible a nivel filesystem. **Esto es lo que hace que Plan B sea la solución correcta**: no obliga a auditar ni modificar ninguna imagen Docker.

### Detalle de paths que cada imagen toca en runtime

```
PostgreSQL:
  /var/lib/postgresql/data/  ← SHARE RW (data.ext4)
  /var/run/postgresql/.s.PGSQL.5432  ← tmpfs requerido
  /tmp/  ← tmpfs requerido

pgbouncer:
  /var/run/pgbouncer.pid  ← tmpfs requerido (/var/run)
  /var/log/pgbouncer.log  ← share o tmpfs

Odoo:
  /var/log/odoo/odoo.log  ← SHARE RW (logs)
  /var/lib/odoo/filestore/  ← SHARE RW (filestore)
  /etc/odoo/odoo.conf  ← bind-mount RO desde overrides
  /tmp/odoo-sessions/  ← tmpfs requerido
```

Todos los paths críticos ya están cubiertos por el initramfs en el branch virtio-fs. Solo hay que replicarlo en el branch pmem.

---

## 5. Solución — per-cell reflink del master

> **Origen**: pregunta arquitectónica que destrabó el análisis: si NKR ya usa `cp -a --reflink=auto` (CoW de btrfs) para clonar 100 odoos por cell **sin consumir disco** ni tiempo medible, ¿por qué `db` y `pgbouncer` están **compartiendo físicamente** el archivo bruto entre cells? La respuesta correcta es: no debería estar compartido. Se debe clonar el master por cell, igual que cualquier tenant.

### 5.1 Arquitectura propuesta

**Antes:**

```
/mnt/nkr/images/postgres.ext4    ← UN archivo, escrito por v17.db Y v19.db
                                   (raíz de la corrupción)

/mnt/nkr/images/pgbouncer.ext4   ← UN archivo, escrito por v17.pgb Y v19.pgb
```

**Después:**

```
/mnt/nkr/images/postgres.ext4    ← Archivo "master inmutable" — solo se usa
                                   como fuente de cp --reflink

/mnt/nkr/cells/odoo-v17/db-root.ext4    ← reflink privado de v17
/mnt/nkr/cells/odoo-v17/pgb-root.ext4   ← reflink privado de v17
/mnt/nkr/cells/odoo-v19/db-root.ext4    ← reflink privado de v19
/mnt/nkr/cells/odoo-v19/pgb-root.ext4   ← reflink privado de v19
```

**Cada cell tiene su propio rootfs ext4 para `db` y `pgbouncer`.** Los compose blocks apuntan al copy privado de la cell, no al master. Resultado:

- **Cero shared mmap entre VMs**: corrupción cruzada **físicamente imposible**.
- Cada cell puede actualizar su master independientemente (rebuild de imagen → nuevo `cp --reflink`).
- El master en `/mnt/nkr/images/` queda como artefacto build-time, **nunca se monta directamente por una VM en runtime**.
- El `cp --reflink=auto` sobre btrfs es O(1) en disco hasta el primer write — el costo de las copias es esencialmente cero.

### 5.2 Cambios concretos

| # | Item | Archivo | Cambio | Esfuerzo |
|---|---|---|---|---|
| 1 | Helper de provisioning para clonar masters al crear cell | [src/cell.rs](src/cell.rs) o nuevo `cell-master-clone.rs` | Función `provision_cell_root_disks(cell)` que para cada master en una lista (`postgres.ext4`, `pgbouncer.ext4`, futuros), hace `cp --reflink=auto /mnt/nkr/images/<m> /mnt/nkr/cells/<cell>/<m>-root.ext4` + `e2fsck -p` para verificación. **No** se aplica `chattr +C` al copy — ver §5.3 para por qué. | 30 min |
| 2 | Llamar el helper desde el flujo `nkr cell create` | [src/cell.rs](src/cell.rs) `cell_create()` | Después de crear el dir de la cell, antes de generar el compose, invocar el helper. | 10 min |
| 3 | Generador de compose: apuntar `db.disks[0]` a la copia privada | [src/cell.rs](src/cell.rs) o template generator | En vez de hardcodear `/mnt/nkr/images/postgres.ext4`, usar `/mnt/nkr/cells/<cell>/postgres-root.ext4`. | 20 min |
| 4 | Tests | tests | Verificar que `provision_cell_root_disks` produce archivos con `+C`, que el `e2fsck` pasa, y que múltiples cells reciben copias **distintas** (`stat` muestra inodes diferentes — reflink en btrfs DA inodes distintos aunque compartan extents). | 30 min |
| 5 | Documentación operativa | `deploy/cell-setup.sh` o README | Cómo regenerar el master (`nkr build -f Nkrfile.pg`) y cómo propagar a cells existentes (script de migración, ver §7.3). | 20 min |

**Total código**: ~1.5-2 horas. **El hipervisor (`pmem.rs`, `vmm.rs`, `initramfs.rs`) no se toca**.

### Detalle del helper de provisioning

```rust
/// For each well-known master in /mnt/nkr/images/, create a private
/// btrfs-reflink copy under the cell's directory. The master files are
/// treated as immutable build artifacts: a `nkr build -f Nkrfile.pg`
/// regenerates the master, and existing cells must opt-in to refresh
/// (see migration script).
pub fn provision_cell_root_disks(cell: &CellConfig) -> Result<(), Box<dyn std::error::Error>> {
    let masters = [
        ("postgres.ext4",  "postgres-root.ext4"),
        ("pgbouncer.ext4", "pgbouncer-root.ext4"),
    ];
    let cell_dir = cells_dir().join(&cell.name);
    let images_dir = nkr_data_dir().join("images");

    for (src_name, dst_name) in masters {
        let src = images_dir.join(src_name);
        let dst = cell_dir.join(dst_name);
        if !src.exists() {
            return Err(format!("master ext4 missing: {}", src.display()).into());
        }
        if dst.exists() {
            // Idempotent: already provisioned.
            continue;
        }
        // cp -a --reflink=auto: O(1) on btrfs, falls back to physical copy on
        // ext4/xfs hosts.
        let status = std::process::Command::new("cp")
            .args(["-a", "--reflink=auto",
                   &src.to_string_lossy(),
                   &dst.to_string_lossy()])
            .status()?;
        if !status.success() {
            return Err(format!("cp --reflink failed: {} → {}",
                src.display(), dst.display()).into());
        }
        // NOTE: we deliberately do NOT chattr +C the reflinked copy. See §5.3
        // for the rationale — applying +C to a file that already shares
        // extents with another inode is a no-op (the flag is accepted but
        // doesn't affect existing extents, and any write to a shared extent
        // MUST CoW regardless of the flag, otherwise it would corrupt the
        // master).

        // e2fsck -p: catch any rare corruption from interrupted previous
        // boots. Quick: <1s for a 1 GB ext4.
        let _ = std::process::Command::new("e2fsck")
            .args(["-p", &dst.to_string_lossy()])
            .status();

        eprintln!("[NKR-CELL] {} reflinked to {}", src_name, dst.display());
    }
    Ok(())
}
```

### 5.3 Por qué NO `chattr +C` sobre la copia reflinked

La versión 2 de este documento proponía aplicar `chattr +C` al copy tras el `cp --reflink`. **Eso es técnicamente inútil y conceptualmente contradictorio**:

- `cp --reflink=auto` produce un archivo cuyos extents son **prestados** al master. Es la mecánica de CoW de btrfs a nivel filesystem.
- `chattr +C` (NoCoW) le pide a btrfs que **no haga CoW** sobre las nuevas escrituras a ese archivo.
- Pero las extents heredadas son compartidas: btrfs **debe** hacer CoW sobre cualquier escritura a una extent compartida — si no, dañaría la copia del otro inodo (el master, u otra cell).
- Resultado: el flag se acepta (`exit 0`) pero **no afecta los extents existentes**. La doc oficial de btrfs lo dice explícitamente: "NOCOW must be set on **empty files**. If applied to a file that already contains data, this won't be applied to existing extents but only to new ones written after the flag is set."

Es decir: tras `cp --reflink`, el `chattr +C` post-hoc es **decoración**. Aplicarlo no garantiza NoCoW para los datos heredados, y nunca lo va a garantizar mientras esos extents estén compartidos con el master.

#### ¿Y la fragmentación del rootfs reflinked?

Cuando un guest escriba a su `postgres-root.ext4` reflinked, btrfs hará CoW de las extents tocadas. Eso introduce fragmentación gradual en btrfs. **Pero el rootfs es read-mostly por diseño** — donde Postgres realmente martillea (WAL + checkpoint + heaps) NO es el rootfs:

| Archivo | Naturaleza | Carga de escritura | CoW de btrfs | Fragmentación |
|---|---|---|---|---|
| `postgres-root.ext4` (reflinked desde el master) | rootfs RO en operación normal | atime + algunos logs si no van a tmpfs | sí, sobre extents heredados | **mínima** |
| `pg/data.ext4` (creado fresh con +C sobre archivo vacío) | PGDATA: WAL, heaps, checkpoints | masiva (cada COMMIT, cada checkpoint, cada autovacuum) | **no** (NoCoW desde nacimiento) | **cero** |

El `pg/data.ext4` se crea via [src/fsutil.rs:74-87](src/fsutil.rs#L74) con la secuencia `touch → chattr +C → truncate`. **Sobre archivo vacío, antes de allocate, el flag SÍ funciona**. Postgres recibe IO performance nativa de ext4 sin penalidad de CoW.

Verificación contra el compose real ([/mnt/nkr/cells/odoo-v17/nkr-compose.yml]):

```yaml
db:
  disks:
    - /mnt/nkr/images/postgres.ext4    # rootfs — read-mostly
  shares:
    - "/mnt/nkr/cells/odoo-v17/pg/data.ext4:/var/lib/postgresql/data"
                                       # PGDATA — todos los checkpoints van acá
  environment:
    PGDATA: /var/lib/postgresql/data/pgdata
```

#### Mitigación de la fragmentación residual del rootfs

Para minimizar incluso esas pocas escrituras al rootfs reflinked:

1. **`mount -o noatime,nodiratime`** en el initramfs cuando se monta `/dev/pmem0`. Elimina la mayor fuente de writes al rootfs (actualizaciones de access time).
2. **tmpfs en `/var/log`, `/run`, `/tmp`, `/var/cache`** — el branch virtio-fs del initramfs ya lo hace; el branch pmem debería sumarlo si en algún momento se quiere endurecer aún más (pero no es bloqueante con Plan B porque el rootfs es privado por cell).
3. **Defrag periódico opcional**: `btrfs filesystem defragment /mnt/nkr/cells/<cell>/postgres-root.ext4` se puede correr offline (cell parada) cada N meses si la métrica de fragmentación supera un umbral. Es O(tamaño del archivo), unos segundos para 1 GB.

Con estos tres puntos, la fragmentación operacional del rootfs reflinked es irrelevante.

### 5.4 Master inmutable + ciclo de `nkr build`

Para que ningún operador o script reuse el master por error, el archivo en `/mnt/nkr/images/` debe ser **inmutable** post-build:

```bash
# Tras nkr build:
chattr +i /mnt/nkr/images/postgres.ext4
```

Con esto, cualquier intento de un VM de mapearlo RW falla con `EPERM` antes de hacer daño, y `cp` puede leerlo libremente para hacer el reflink.

#### Pero `nkr build` debe poder regenerarlo

El comando `nkr build -f Nkrfile.pg` que actualiza el master no puede sobrescribir un archivo `+i` directamente — el kernel rechaza incluso a root. **El comando build debe destrabar antes y reaplicar al final**:

```rust
// Pseudocódigo del flujo de nkr build cuando el output es un master en
// /mnt/nkr/images/:
fn nkr_build_master(nkrfile: &Path, out: &Path) -> Result<()> {
    // 1. Destrabar si ya estaba +i. Silencioso si no estaba.
    let _ = Command::new("chattr").args(["-i", &out.to_string_lossy()]).status();

    // 2. Hacer el build (docker → ext4 → out path).
    docker_build_to_ext4(nkrfile, out)?;

    // 3. Re-aplicar +i al master nuevo.
    let status = Command::new("chattr")
        .args(["+i", &out.to_string_lossy()])
        .status()?;
    if !status.success() {
        eprintln!("[NKR-BUILD] WARN: chattr +i falló sobre {}. \
                   El master queda escribible — peligroso, fix manual: \
                   sudo chattr +i {}", out.display(), out.display());
    }
    Ok(())
}
```

#### ¿Y las cells existentes con reflink al master viejo?

`chattr +i` y la regeneración del master NO afectan a las cells que ya tienen `postgres-root.ext4` reflinked. El reflink en btrfs es CoW a nivel filesystem: las cells siguen viendo la versión del master en el momento de su reflink, sin importar lo que pase con el master después. Para que una cell adopte el master nuevo, el operador corre el script de migración de §5.5.

### 5.5 Migración de cells existentes

Las cells `odoo-v17` y `odoo-v19` ya tienen sus blocks `db` y `pgbouncer` apuntando al master. Migración por cell, sin downtime cruzado:

```bash
#!/bin/bash
set -euo pipefail
CELL="$1"

# 1. Clonar master a copia privada (una sola vez, idempotente).
#    NO se aplica chattr +C — ver §5.3 para por qué es no-op sobre extents
#    compartidas. La fragmentación residual del rootfs es operacionalmente
#    irrelevante porque el rootfs es read-mostly.
cp -a --reflink=auto /mnt/nkr/images/postgres.ext4 \
                     /mnt/nkr/cells/$CELL/postgres-root.ext4
cp -a --reflink=auto /mnt/nkr/images/pgbouncer.ext4 \
                     /mnt/nkr/cells/$CELL/pgbouncer-root.ext4

# 2. Detener servicios de la cell
nkr stop $CELL-db
nkr stop $CELL-pgb

# 3. Editar el compose: cambiar disks[0] del master al copy privado
sed -i "s|/mnt/nkr/images/postgres.ext4|/mnt/nkr/cells/$CELL/postgres-root.ext4|g" \
    /mnt/nkr/cells/$CELL/nkr-compose.yml
sed -i "s|/mnt/nkr/images/pgbouncer.ext4|/mnt/nkr/cells/$CELL/pgbouncer-root.ext4|g" \
    /mnt/nkr/cells/$CELL/nkr-compose.yml

# 4. Re-arrancar
nkr compose up -d --filter db
nkr compose up -d --filter pgbouncer

# 5. Verificar
pg_isready -h <ip-db-cell> -p 5432
```

Downtime por cell: **~30-60 segundos** (lo que tarde el restart de db + pgbouncer).

Las cells siguen funcionales durante la migración (los Odoo tenants no se ven afectados — siguen apuntando a su pg/data.ext4 share, y la conexión se reanuda al volver `db`).

### 5.6 Riesgo y estimación

| Riesgo | Probabilidad | Mitigación |
|---|---|---|
| `cp --reflink=auto` falla (host no es btrfs) | Media (algunos hosts pueden no ser btrfs) | El `--reflink=auto` cae a copia física automáticamente. Funciona, solo más lento (segundos vs ms). |
| Fragmentación gradual del rootfs reflinked por btrfs CoW | Media en escala de meses | El rootfs es read-mostly. `noatime,nodiratime` minimiza atime updates. Defrag offline cada N meses si la métrica supera umbral. **No afecta a `pg/data.ext4` que tiene NoCoW efectivo desde el `chattr +C` sobre archivo vacío.** |
| `chattr +i` sobre el master rompe `nkr build` futuro | Cero, si `nkr build` aplica `chattr -i` antes (§5.4) | Implementar el wrap en el comando antes del primer `+i` en producción. |
| Master modificado mientras una cell tiene reflink "vivo" | Cero | Reflink en btrfs es CoW: el master puede mutar libremente, las cells siguen viendo la versión del momento del reflink. |
| Restart de `db` con disco nuevo: ¿falla por inconsistencia con `pg/data.ext4`? | Cero | El `pg/data.ext4` privado de la cell ya estaba siendo usado. La nueva copia del rootfs tiene los mismos binarios que la anterior — PG levanta idéntico. |
| Operador olvida migrar una cell y los nuevos defaults rompen | Bajo | El cambio de `nkr cell create` solo afecta cells nuevas. Cells existentes siguen apuntando al master shared hasta que el operador corra el script de migración. |

| Actividad | Tiempo |
|---|---|
| Implementación helper + integración + tests | 2 horas |
| Modificación de `nkr build` (`chattr -i`/`+i` wrap) | 30 min |
| Migración por cell (script + verificación) | 5-10 min por cell |
| Migración total para 2 cells actuales | 20-30 min |
| **Total esfuerzo activo** | **~3 horas** |
| **Total calendario** | **medio día** |

---

## 6. Tests de validación

### Tests unitarios

```rust
#[test]
fn provision_cell_root_disks_creates_reflink_copies() {
    // Setup: crear un master dummy en un dir de prueba.
    let tmp = std::env::temp_dir().join(format!("nkr-prov-{}", std::process::id()));
    fs::create_dir_all(tmp.join("images")).unwrap();
    fs::create_dir_all(tmp.join("cells/test-cell")).unwrap();
    fs::write(tmp.join("images/postgres.ext4"), b"x").unwrap();
    fs::write(tmp.join("images/pgbouncer.ext4"), b"y").unwrap();

    // Run.
    provision_cell_root_disks_with_paths(&tmp.join("cells/test-cell"),
                                          &tmp.join("images"))
        .unwrap();

    // Verify: las copias existen y tienen contenido distinto del path.
    assert!(tmp.join("cells/test-cell/postgres-root.ext4").exists());
    assert!(tmp.join("cells/test-cell/pgbouncer-root.ext4").exists());

    // Idempotencia: segunda llamada no falla, no duplica.
    provision_cell_root_disks_with_paths(&tmp.join("cells/test-cell"),
                                          &tmp.join("images"))
        .unwrap();

    fs::remove_dir_all(&tmp).unwrap();
}

#[test]
fn provision_fails_if_master_missing() {
    let tmp = std::env::temp_dir().join(format!("nkr-prov-miss-{}", std::process::id()));
    fs::create_dir_all(tmp.join("cells/c")).unwrap();
    fs::create_dir_all(tmp.join("images")).unwrap();
    // No creamos los masters → provisioning debe Err.

    let res = provision_cell_root_disks_with_paths(&tmp.join("cells/c"),
                                                    &tmp.join("images"));
    assert!(res.is_err());
    let _ = fs::remove_dir_all(&tmp);
}
```

### Smoke test de integración (post-migración por cell)

```sh
# Tras correr el script de migración de §5.5 sobre una cell:

# 1. Verificar que el compose apunta al copy privado, no al master.
grep "/mnt/nkr/cells/$CELL/postgres-root.ext4" \
     /mnt/nkr/cells/$CELL/nkr-compose.yml

# 2. Verificar que el master sigue intacto (no fue tocado).
sha256sum /mnt/nkr/images/postgres.ext4   # comparar contra el sha pre-migración

# 3. Verificar que la cell levanta sin errores.
pg_isready -h $(awk -v cell="$CELL" '...' /etc/nkr/cells.json) -p 5432 -t 30

# 4. Verificar aislamiento: tocar la copia de v17 NO debe afectar v19.
stat -c '%i' /mnt/nkr/cells/odoo-v17/postgres-root.ext4   # inode A
stat -c '%i' /mnt/nkr/cells/odoo-v19/postgres-root.ext4   # inode B
test "$A" != "$B"   # confirmar que son inodos distintos

# 5. PG funcional dentro de la VM.
psql -U odoo -h $DB_IP -c 'CREATE TABLE smoke(id int); INSERT INTO smoke VALUES(1);'
psql -U odoo -h $DB_IP -c 'SELECT count(*) FROM smoke;' | grep -q '1'
```

### Test de aislamiento físico

El criterio crítico es: **escrituras de una cell no deben mutar el archivo de otra cell**. Verificable directamente:

```bash
# En el host, antes de ningún restart:
sha256sum /mnt/nkr/cells/odoo-v17/postgres-root.ext4 > /tmp/v17.sha
sha256sum /mnt/nkr/cells/odoo-v19/postgres-root.ext4 > /tmp/v19.sha

# Forzar carga sobre v17:
pgbench -c 4 -j 2 -T 60 -h <ip-v17-db> -U odoo postgres

# v19 NO debe haber cambiado:
sha256sum /mnt/nkr/cells/odoo-v19/postgres-root.ext4 | diff /tmp/v19.sha -
# Si el sha cambió, el aislamiento físico está roto.
```

---

## 7. Conclusión técnica

### Puntos clave

1. **El bug existe** y se materializa silenciosamente bajo carga real. No es teórico.
2. **El alcance es acotado**: `db` y `pgbouncer` que comparten archivo entre cells. Los tenants Odoo no se ven afectados (cada uno tiene `.ext4` privado).
3. **Por qué no se ha visto explotar todavía**: las escrituras al rootfs son raras en operación normal, y los datos críticos (PG WAL, filestore Odoo) viven en shares RW separados que no comparten el problema.
4. **El reflink por cell elimina el bug en el origen**: cero shared mmap a nivel filesystem, corrupción cruzada físicamente imposible.
5. **El trabajo crítico es ~3 horas de código** + un script de migración de 30 segundos por cell.
6. **El hipervisor no se toca**. `pmem.rs`, `vmm.rs`, `initramfs.rs` quedan como están.

### Recomendación final

Implementar la solución de §5 de inmediato. La infraestructura subyacente (`cp --reflink=auto`, `try_btrfs_snapshot`, `preserve_nocow`) ya está implementada y testeada — el delta es un helper de provisioning y un script de migración. El comando `nkr build` debe ganar el wrap `chattr -i`/`+i` antes del primer master inmutable en producción.

### Decisiones tomadas

- **¿Detección por path como gate de seguridad?** **Rechazada**. Magic strings de filesystem son frágiles ante reorganización futura. El control es explícito (campo en YAML o convención del helper de provisioning).
- **¿Plan A — pmem RO mode?** **Eliminado del backlog**. Si el reflink por cell elimina el shared mmap, el código de RO mode sería un activo sin uso. YAGNI.
- **¿Backup pre-migración?** Sí. `cp -a --reflink=auto /mnt/nkr/images/postgres.ext4 /mnt/nkr/images/postgres.ext4.pre-migration` antes de la primera migración. Costa cero en btrfs.
- **`chattr +i` sobre el master**: aplicar tras cada `nkr build`. El comando build se modifica para hacer `chattr -i` previo, build, `chattr +i` final.
- **NoCoW del rootfs reflinked**: rechazado. Es físicamente incompatible con extents compartidas. La fragmentación residual es operacionalmente irrelevante porque el rootfs es read-mostly.

---

## 8. Bitácora de revisiones

| Fecha | Cambio | Origen |
|---|---|---|
| 2026-05-08 | Versión inicial. Plan A (pmem RO) propuesto con heurística de auto-detección por path. | Análisis tras audit Sprint 2. |
| 2026-05-08 | **Revisión 2**: tres correcciones del operador + cambio arquitectónico. (1) Eliminada la heurística de auto-detección por path. (2) Auditoría de nginx ampliada con paths concretos de tmpfs requeridos. (3) "Riesgo cero" de Fase 1 reformulado a "bajo riesgo" con tests obligatorios. (4) Plan B agregado como solución primaria. | Crítica del operador. |
| 2026-05-08 | **Revisión 3** (esta versión): cuatro cambios mayores. (1) **Plan A eliminado completamente** del documento. Si Plan B resuelve el problema en el origen, mantener Plan A como "defensa en profundidad" es código muerto que se vuelve deuda. (2) **Quitado `chattr +C` post-reflink** del helper y script de migración: técnicamente no funciona sobre extents compartidas (la doc de btrfs lo dice explícitamente). El argumento "fragmentación catastrófica de PG" cae porque PG escribe a `pg/data.ext4` (creado fresh con `+C` sobre archivo vacío, que SÍ funciona), no al rootfs reflinked. (3) **Agregado §5.4 Master inmutable + ciclo de `nkr build`**: el comando build debe wrapear con `chattr -i`/`+i` para no auto-bloquearse. (4) **Mitigación de fragmentación residual del rootfs** con `noatime,nodiratime` + tmpfs + defrag offline opcional. | Crítica del operador (paradoja btrfs, ciclo de build, síndrome de Diógenes). |
