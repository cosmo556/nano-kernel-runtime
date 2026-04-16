// =============================================================================
// NKR Initramfs Generator — Genera initramfs genérico para cualquier imagen OCI
// =============================================================================
//
// Crea un .cpio.gz auto-contenido con:
//   - busybox estático + symlinks
//   - módulos kernel para ext4 + virtio
//   - init script genérico que auto-detecta entrypoint
//
// El initramfs resultante monta el disco ext4, detecta el entrypoint de la
// imagen Docker (/entrypoint.sh, /docker-entrypoint.sh, etc.) y lo ejecuta
// pasando las variables de entorno del compose.
// =============================================================================

use std::error::Error;
use std::fs;
use std::path::Path;
use std::process::Command;

/// Directorio base de datos NKR (configurable vía NKR_DATA_DIR)
fn nkr_data_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(
        std::env::var("NKR_DATA_DIR").unwrap_or_else(|_| "/mnt/nkr".to_string()),
    )
}

/// Ruta al initramfs base (busybox + módulos) que se usa como plantilla
fn base_initramfs_path() -> std::path::PathBuf {
    nkr_data_dir().join("initramfs").join("base")
}

/// Script init genérico para cualquier imagen OCI — Soporta DAX
const GENERIC_INIT_SCRIPT: &str = r#"#!/bin/sh
export PATH=/bin:/sbin:/usr/bin:/usr/sbin:/usr/local/bin
mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devtmpfs devtmpfs /dev

mdev -s
if [ -e /dev/ttyS0 ]; then exec > /dev/ttyS0 2>&1; else exec > /dev/kmsg 2>&1; fi

echo "[NKR] init started"
[ -f /proc/sys/kernel/hotplug ] && echo /sbin/mdev > /proc/sys/kernel/hotplug

# Resolución de parámetros de cmdline
GUEST_IP=""
for param in $(cat /proc/cmdline); do
    case "$param" in
        nkr.ip=*)      GUEST_IP="${param#nkr.ip=}" ;;
        nkr.rootfs=*)  NKR_ROOTFS="${param#nkr.rootfs=}" ;;
        nkr.fs0=*)     NKR_FS0_TAG="${param#nkr.fs0=}" ;;
        nkr.fsm0=*)    NKR_FS0_MNT="${param#nkr.fsm0=}" ;;
        nkr.fsr0=*)    NKR_FS0_RW="${param#nkr.fsr0=}" ;;
        nkr.fs1=*)     NKR_FS1_TAG="${param#nkr.fs1=}" ;;
        nkr.fsm1=*)    NKR_FS1_MNT="${param#nkr.fsm1=}" ;;
        nkr.fsr1=*)    NKR_FS1_RW="${param#nkr.fsr1=}" ;;
        nkr.fs2=*)     NKR_FS2_TAG="${param#nkr.fs2=}" ;;
        nkr.fsm2=*)    NKR_FS2_MNT="${param#nkr.fsm2=}" ;;
        nkr.fsr2=*)    NKR_FS2_RW="${param#nkr.fsr2=}" ;;
        nkr.fs3=*)     NKR_FS3_TAG="${param#nkr.fs3=}" ;;
        nkr.fsm3=*)    NKR_FS3_MNT="${param#nkr.fsm3=}" ;;
        nkr.fsr3=*)    NKR_FS3_RW="${param#nkr.fsr3=}" ;;
        nkr.fs4=*)     NKR_FS4_TAG="${param#nkr.fs4=}" ;;
        nkr.fsm4=*)    NKR_FS4_MNT="${param#nkr.fsm4=}" ;;
        nkr.fsr4=*)    NKR_FS4_RW="${param#nkr.fsr4=}" ;;
        nkr.fs5=*)     NKR_FS5_TAG="${param#nkr.fs5=}" ;;
        nkr.fsm5=*)    NKR_FS5_MNT="${param#nkr.fsm5=}" ;;
        nkr.fsr5=*)    NKR_FS5_RW="${param#nkr.fsr5=}" ;;
        nkr.fs6=*)     NKR_FS6_TAG="${param#nkr.fs6=}" ;;
        nkr.fsm6=*)    NKR_FS6_MNT="${param#nkr.fsm6=}" ;;
        nkr.fsr6=*)    NKR_FS6_RW="${param#nkr.fsr6=}" ;;
        nkr.fs7=*)     NKR_FS7_TAG="${param#nkr.fs7=}" ;;
        nkr.fsm7=*)    NKR_FS7_MNT="${param#nkr.fsm7=}" ;;
        nkr.fsr7=*)    NKR_FS7_RW="${param#nkr.fsr7=}" ;;
        nkr.blk0=*)    NKR_BLK0_DEV="${param#nkr.blk0=}" ;;
        nkr.blkm0=*)   NKR_BLK0_MNT="${param#nkr.blkm0=}" ;;
        nkr.blk1=*)    NKR_BLK1_DEV="${param#nkr.blk1=}" ;;
        nkr.blkm1=*)   NKR_BLK1_MNT="${param#nkr.blkm1=}" ;;
        nkr.blk2=*)    NKR_BLK2_DEV="${param#nkr.blk2=}" ;;
        nkr.blkm2=*)   NKR_BLK2_MNT="${param#nkr.blkm2=}" ;;
    esac
done

[ -z "$GUEST_IP" ] && GUEST_IP="10.0.0.2"

if [ -d /sys/class/net/eth0 ]; then
    ip link set eth0 up
    ip addr add ${GUEST_IP}/24 dev eth0
    ip route add default via 10.0.0.1
    echo "[NKR] eth0: ${GUEST_IP}/24"
fi

mkdir -p /newroot

# =========================================================================
# BUCLE DE ESPERA PMEM (Aumentado a 3 segundos max)
# =========================================================================
WAIT=0
while [ ! -b /dev/pmem0 ] && [ $WAIT -lt 30 ]; do 
    sleep 0.1
    WAIT=$((WAIT+1))
done

# =========================================================================
# LÓGICA DE MONTAJE INTELIGENTE (PMEM/DAX vs VDA)
# =========================================================================
if [ -b /dev/pmem0 ]; then
    echo "[NKR] rootfs: /dev/pmem0 detectado (Intentando DAX)"
    mount -t ext4 -o dax,rw /dev/pmem0 /newroot || \
    mount -t ext4 -o rw /dev/pmem0 /newroot
elif [ -n "$NKR_ROOTFS" ]; then
    echo "[NKR] rootfs: VirtIO-FS compartido (tag=$NKR_ROOTFS, RO)"
    mount -t virtiofs -o ro "$NKR_ROOTFS" /newroot
    # Zonas de respiración — overlays tmpfs sobre rootfs RO
    mount -t tmpfs tmpfs /newroot/tmp
    mount -t tmpfs tmpfs /newroot/run
    mkdir -p /newroot/tmp/nkr
    busybox cp /bin/busybox /newroot/tmp/nkr/busybox
    mkdir -p /newroot/var/run/postgresql /newroot/var/log
    mount -t tmpfs tmpfs /newroot/var/run/postgresql
    mount -t tmpfs tmpfs /newroot/var/log
else
    echo "[NKR] rootfs: /dev/vda detectado (Modo bloque estándar)"
    mount -t ext4 -o rw /dev/vda /newroot
fi

echo "[NKR] rootfs mounted"

mount -o bind /proc /newroot/proc
mount -o bind /sys /newroot/sys
mount -o bind /dev /newroot/dev
chmod 666 /newroot/dev/null /newroot/dev/random /newroot/dev/urandom /newroot/dev/zero /newroot/dev/tty 2>/dev/null
ln -sf /proc/self/fd /newroot/dev/fd
ln -sf fd/0 /newroot/dev/stdin
ln -sf fd/1 /newroot/dev/stdout
ln -sf fd/2 /newroot/dev/stderr

# Montar compartidos VirtIO-FS (virtiofsd) — saltar rootfs (ya montado)
for idx in 0 1 2 3 4 5 6 7; do
    eval TAG="\$NKR_FS${idx}_TAG"
    eval MNT="\$NKR_FS${idx}_MNT"
    eval MODE="\$NKR_FS${idx}_RW"
    [ -z "$TAG" ] && continue
    [ -z "$MNT" ] && continue
    # Si este tag es el rootfs, ya está montado en /
    [ "$TAG" = "$NKR_ROOTFS" ] && continue
    mkdir -p "/newroot${MNT}"
    MOUNT_OPT=""
    [ "$MODE" = "ro" ] && MOUNT_OPT="-o ro"
    mount -t virtiofs $MOUNT_OPT "$TAG" "/newroot${MNT}" \
        && echo "[NKR] VirtIO-FS: ${TAG} montado en ${MNT} (${MODE:-rw})" \
        || echo "[NKR] WARN: VirtIO-FS: no se pudo montar ${TAG}"
done

# Montar volúmenes VirtIO-BLK (ext4 — sin virtiofsd, sin DAX)
for idx in 0 1 2; do
    eval BLK_DEV="\$NKR_BLK${idx}_DEV"
    eval BLK_MNT="\$NKR_BLK${idx}_MNT"
    [ -z "$BLK_DEV" ] && continue
    [ -z "$BLK_MNT" ] && continue
    BWAIT=0
    while [ ! -b "$BLK_DEV" ] && [ $BWAIT -lt 50 ]; do sleep 0.1; BWAIT=$((BWAIT+1)); done
    if [ -b "$BLK_DEV" ]; then
        mkdir -p "/newroot${BLK_MNT}"
        mount -t ext4 -o rw,noatime,nodiratime "$BLK_DEV" "/newroot${BLK_MNT}" \
            && echo "[NKR] BLK: ${BLK_DEV} → ${BLK_MNT}" \
            || echo "[NKR] WARN: BLK mount falló ${BLK_DEV}"
    else
        echo "[NKR] WARN: ${BLK_DEV} no apareció tras 5s"
    fi
done

# Overrides de archivos individuales: bind-mount desde /tmp/nkr-overrides sobre rootfs
if [ -d /newroot/tmp/nkr-overrides ]; then
    cd /newroot/tmp/nkr-overrides
    /bin/busybox find . -type f 2>/dev/null | while IFS= read -r f; do
        rel="${f#./}"
        tgt="/newroot/${rel}"
        src="/newroot/tmp/nkr-overrides/${rel}"
        if [ -e "$tgt" ]; then
            mount -o bind "$src" "$tgt" \
                && echo "[NKR] Override: /${rel}" \
                || echo "[NKR] WARN: bind fallo para /${rel}"
        else
            echo "[NKR] WARN: override /${rel} no existe en rootfs (omitido)"
        fi
    done
    cd /
fi

echo "[NKR] Detecting entrypoint..."
chroot /newroot /bin/sh -c '
export PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
for pg_bin in /usr/lib/postgresql/*/bin; do
    if [ -d "$pg_bin" ]; then export PATH=$PATH:$pg_bin; fi
done

# Source env vars: primero /etc/nkr-env (disco origin), /tmp/nkr/nkr-env (inyectado)
# Buscar el archivo en los compartidos mapeados (si vinieron del host)
for MNT in "$NKR_FS0_MNT" "$NKR_FS1_MNT" "$NKR_FS2_MNT" "$NKR_FS3_MNT" "$NKR_FS4_MNT" "$NKR_FS5_MNT" "$NKR_FS6_MNT" "$NKR_FS7_MNT"; do
    if [ -n "$MNT" ] && [ -f "/newroot${MNT}/nkr-env" ]; then
        mkdir -p /newroot/tmp/nkr
        cp "/newroot${MNT}/nkr-env" /newroot/tmp/nkr/nkr-env
        break
    fi
done

for env_file in /etc/nkr-env /etc/odoo/nkr-env /tmp/nkr/nkr-env; do
    [ -f "$env_file" ] && . "$env_file" && echo "[NKR] Cargado nkr-env desde $env_file"
done

ENTRYPOINT=""
for ep in /entrypoint.sh /docker-entrypoint.sh /usr/local/bin/docker-entrypoint.sh /run.sh /start.sh; do
    if [ -x "$ep" ]; then
        ENTRYPOINT="$ep"
        break
    fi
done

if [ -x "/usr/bin/odoo" ]; then
    export ODOO_RC="${ODOO_RC:-/etc/odoo/odoo.conf}"
    export PYTHONUNBUFFERED=1
    _ODOO_CMD="/usr/bin/python3 -u /usr/bin/odoo -c $ODOO_RC"
    if [ -x "/usr/local/bin/gosu" ] || [ -x "/usr/sbin/gosu" ] || [ -x "/usr/bin/gosu" ]; then
        echo "[NKR] Running: gosu odoo $_ODOO_CMD"
        exec env PYTHONUNBUFFERED=1 gosu odoo $_ODOO_CMD
    elif [ -x "/usr/local/sbin/su-exec" ] || [ -x "/sbin/su-exec" ]; then
        echo "[NKR] Running: su-exec odoo $_ODOO_CMD"
        exec env PYTHONUNBUFFERED=1 su-exec odoo $_ODOO_CMD
    else
        echo "[NKR] Running: su odoo -c $_ODOO_CMD"
        exec su -p -s /bin/sh odoo -c "export PYTHONUNBUFFERED=1; exec $_ODOO_CMD"
    fi
elif [ -n "$ENTRYPOINT" ]; then
    echo "[NKR] Running entrypoint: $ENTRYPOINT"
    exec "$ENTRYPOINT"
else
    echo "[NKR] No entrypoint found. Dropping to shell."
    exec /bin/sh
fi
' > /dev/ttyS0 2>&1

echo "[NKR] Process exited. Freezing."
while true; do sleep 3600; done
"#;

fn ensure_base_initramfs() -> Result<std::path::PathBuf, Box<dyn Error>> {
    let base = base_initramfs_path();
    let busybox_path = base.join("bin/busybox");

    if busybox_path.exists() {
        return Ok(base);
    }

    eprintln!("[NKR-INITRAMFS] Creando base initramfs en {}...", base.display());
    let initramfs_dir = nkr_data_dir().join("initramfs");

    let mut source_cpio: Option<std::path::PathBuf> = None;
    if initramfs_dir.exists() {
        if let Ok(entries) = fs::read_dir(&initramfs_dir) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.extension().map(|e| e == "gz").unwrap_or(false) && p.is_file() {
                    source_cpio = Some(p);
                    break;
                }
            }
        }
    }

    if let Some(cpio) = source_cpio {
        eprintln!("[NKR-INITRAMFS] Extrayendo base desde {}...", cpio.display());
        fs::create_dir_all(&base)?;
        let status = Command::new("sh")
            .args([
                "-c",
                &format!("cd '{}' && zcat '{}' | cpio -idm 2>/dev/null", base.display(), cpio.display()),
            ])
            .status()?;
        if !status.success() {
            return Err("Fallo extrayendo base initramfs desde cpio existente".into());
        }
        let _ = fs::remove_file(base.join("init"));
    } else {
        return Err("No se encontró un initramfs base (.cpio.gz). Copia uno ahí primero.".into());
    }

    Ok(base)
}

pub fn generate_initramfs(name: &str, disk_path: &str, docker_cmd: Option<&[String]>) -> Result<String, Box<dyn Error>> {
    let data_dir = nkr_data_dir();
    let initramfs_dir = data_dir.join("initramfs");
    fs::create_dir_all(&initramfs_dir)?;

    let output_path = initramfs_dir.join(format!("{}.cpio.gz", name));

    eprintln!("[NKR-INITRAMFS] Generando initramfs para '{}' → {}", name, output_path.display());

    let base = ensure_base_initramfs()?;

    let work_dir = format!("/tmp/nkr_initramfs_gen_{}", std::process::id());
    let work = Path::new(&work_dir);
    if work.exists() { fs::remove_dir_all(work)?; }
    fs::create_dir_all(work)?;

    let cp_status = Command::new("cp")
        .args(["-a", &format!("{}/.", base.display().to_string()), &work_dir])
        .status()?;
    if !cp_status.success() {
        let _ = fs::remove_dir_all(work);
        return Err("Fallo copiando base initramfs".into());
    }

    let mut detected_entrypoint: Option<String> = None;

    let scan_root = Path::new(disk_path);
    if !disk_path.is_empty() && scan_root.is_dir() {
        // disk_path is a pre-mounted rootfs directory (rootfs mode) — scan directly
        for ep in &["entrypoint.sh", "docker-entrypoint.sh", "usr/local/bin/docker-entrypoint.sh", "run.sh", "start.sh"] {
            let full = scan_root.join(ep);
            if full.exists() {
                detected_entrypoint = Some(format!("/{}", ep));
                break;
            }
        }
        // Also detect odoo binary (no entrypoint script needed)
        if detected_entrypoint.is_none() && scan_root.join("usr/bin/odoo").exists() {
            detected_entrypoint = Some("/usr/bin/odoo".to_string());
        }
    } else if !disk_path.is_empty() {
        let mount_dir = format!("/tmp/nkr_inspect_gen_{}", std::process::id());
        fs::create_dir_all(&mount_dir)?;

        let mount_ok = Command::new("mount")
            .args(["-o", "loop,ro,noload", disk_path, &mount_dir])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);

        if mount_ok {
            for ep in &["entrypoint.sh", "docker-entrypoint.sh", "usr/local/bin/docker-entrypoint.sh", "run.sh", "start.sh"] {
                let full = Path::new(&mount_dir).join(ep);
                if full.exists() {
                    detected_entrypoint = Some(format!("/{}", ep));
                    break;
                }
            }
            let _ = Command::new("umount").arg(&mount_dir).status();
        }
        let _ = fs::remove_dir(&mount_dir);
    }

    let init_content = if let Some(cmd) = docker_cmd {
        if !cmd.is_empty() {
            let full_cmd = cmd.iter()
                .map(|s| if s.contains(' ') || s.contains('"') { format!("'{}'", s) } else { s.clone() })
                .collect::<Vec<_>>()
                .join(" ");
            eprintln!("[NKR-INITRAMFS] Usando Docker CMD: {}", full_cmd);
            generate_init_script(name, &full_cmd)
        } else if let Some(ep) = &detected_entrypoint {
            eprintln!("[NKR-INITRAMFS] Entrypoint detectado en disco: {}", ep);
            generate_init_script(name, ep)
        } else {
            eprintln!("[NKR-INITRAMFS] No se detectó entrypoint, usando genérico");
            GENERIC_INIT_SCRIPT.to_string()
        }
    } else if let Some(ep) = &detected_entrypoint {
        eprintln!("[NKR-INITRAMFS] Entrypoint detectado en disco: {}", ep);
        generate_init_script(name, ep)
    } else {
        eprintln!("[NKR-INITRAMFS] No se detectó entrypoint, usando genérico");
        GENERIC_INIT_SCRIPT.to_string()
    };

    let init_path = work.join("init");
    fs::write(&init_path, &init_content)?;
    Command::new("chmod").args(["+x", &init_path.to_string_lossy()]).status()?;

    for dir in &["dev", "etc", "newroot", "proc", "run", "sys"] {
        fs::create_dir_all(work.join(dir))?;
    }

    eprintln!("[NKR-INITRAMFS] Empaquetando {}...", output_path.display());
    let pack_status = Command::new("sh")
        .args([
            "-c",
            &format!("cd '{}' && find . | cpio -o -H newc 2>/dev/null | gzip > '{}'", work_dir, output_path.display()),
        ])
        .status()?;

    let _ = fs::remove_dir_all(work);

    if !pack_status.success() {
        return Err("Fallo empaquetando initramfs cpio.gz".into());
    }

    let size = fs::metadata(&output_path).map(|m| m.len() / 1024).unwrap_or(0);
    eprintln!("[NKR-INITRAMFS] ✅ {} generado ({} KB)", output_path.display(), size);

    Ok(output_path.to_string_lossy().to_string())
}

/// Genera un init script personalizado con comando completo — Soporta DAX
fn generate_init_script(name: &str, full_command: &str) -> String {
    let label = name.to_uppercase();
    format!(
        r#"#!/bin/sh
export PATH=/bin:/sbin:/usr/bin:/usr/sbin:/usr/local/bin
mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devtmpfs devtmpfs /dev

mdev -s
if [ -e /dev/ttyS0 ]; then exec > /dev/ttyS0 2>&1; else exec > /dev/kmsg 2>&1; fi

echo "[NKR-{label}] init started"
[ -f /proc/sys/kernel/hotplug ] && echo /sbin/mdev > /proc/sys/kernel/hotplug

GUEST_IP=""
for param in $(cat /proc/cmdline); do
    case "$param" in
        nkr.ip=*)      GUEST_IP="${{param#nkr.ip=}}" ;;
        nkr.rootfs=*)  NKR_ROOTFS="${{param#nkr.rootfs=}}" ;;
        nkr.fs0=*)     NKR_FS0_TAG="${{param#nkr.fs0=}}" ;;
        nkr.fsm0=*)    NKR_FS0_MNT="${{param#nkr.fsm0=}}" ;;
        nkr.fs1=*)     NKR_FS1_TAG="${{param#nkr.fs1=}}" ;;
        nkr.fsm1=*)    NKR_FS1_MNT="${{param#nkr.fsm1=}}" ;;
        nkr.fs2=*)     NKR_FS2_TAG="${{param#nkr.fs2=}}" ;;
        nkr.fsm2=*)    NKR_FS2_MNT="${{param#nkr.fsm2=}}" ;;
        nkr.fs3=*)     NKR_FS3_TAG="${{param#nkr.fs3=}}" ;;
        nkr.fsm3=*)    NKR_FS3_MNT="${{param#nkr.fsm3=}}" ;;
        nkr.blk0=*)    NKR_BLK0_DEV="${{param#nkr.blk0=}}" ;;
        nkr.blkm0=*)   NKR_BLK0_MNT="${{param#nkr.blkm0=}}" ;;
        nkr.blk1=*)    NKR_BLK1_DEV="${{param#nkr.blk1=}}" ;;
        nkr.blkm1=*)   NKR_BLK1_MNT="${{param#nkr.blkm1=}}" ;;
        nkr.blk2=*)    NKR_BLK2_DEV="${{param#nkr.blk2=}}" ;;
        nkr.blkm2=*)   NKR_BLK2_MNT="${{param#nkr.blkm2=}}" ;;
    esac
done
[ -z "$GUEST_IP" ] && GUEST_IP="10.0.0.2"

if [ -d /sys/class/net/eth0 ]; then
    ip link set eth0 up
    ip addr add ${{GUEST_IP}}/24 dev eth0
    ip route add default via 10.0.0.1
    echo "[NKR-{label}] eth0: ${{GUEST_IP}}/24"
fi

mkdir -p /newroot

# =========================================================================
# BUCLE DE ESPERA PMEM Y MONTAJE TOLERANTE
# =========================================================================
WAIT=0
while [ ! -b /dev/pmem0 ] && [ $WAIT -lt 30 ]; do 
    sleep 0.1
    WAIT=$((WAIT+1))
done

if [ -b /dev/pmem0 ]; then
    echo "[NKR-{label}] rootfs: /dev/pmem0 detectado (Intentando DAX)"
    mount -t ext4 -o dax,rw /dev/pmem0 /newroot || \
    mount -t ext4 -o rw /dev/pmem0 /newroot
elif [ -n "$NKR_ROOTFS" ]; then
    echo "[NKR-{label}] rootfs: VirtIO-FS compartido (tag=$NKR_ROOTFS, RO)"
    mount -t virtiofs -o ro "$NKR_ROOTFS" /newroot \
        && echo "[NKR-{label}] [DBG] rootfs OK" \
        || echo "[NKR-{label}] [DBG] rootfs FALLO"
    # Zonas de respiración — overlays tmpfs sobre rootfs RO
    mount -t tmpfs tmpfs /newroot/tmp && echo "[NKR-{label}] [DBG] /tmp OK"
    mount -t tmpfs tmpfs /newroot/run && echo "[NKR-{label}] [DBG] /run OK"
    mkdir -p /newroot/tmp/nkr
    # /var/run es symlink → /run en Debian; crear postgresql/ dentro del tmpfs de /run
    mkdir -p /newroot/run/postgresql
    chown -R 999:999 /newroot/run/postgresql 2>/dev/null
    echo "[NKR-{label}] [DBG] /run/postgresql OK ($(stat -c %U:%G /newroot/run/postgresql 2>/dev/null || echo 999:999))"
    mount -t tmpfs tmpfs /newroot/var/log \
        && echo "[NKR-{label}] [DBG] /var/log OK" \
        || echo "[NKR-{label}] [DBG] /var/log FALLO"
else
    echo "[NKR-{label}] rootfs: /dev/vda detectado (Modo bloque estándar)"
    mount -t ext4 -o rw /dev/vda /newroot
fi

echo "[NKR-{label}] [DBG] bind proc/sys/dev..."
mount -o bind /proc /newroot/proc
mount -o bind /sys /newroot/sys
mount -o bind /dev /newroot/dev
echo "[NKR-{label}] [DBG] bind OK"

# Dar permisos universales a dispositivos clave (Postgres hace 'su postgres')
chmod 666 /newroot/dev/null /newroot/dev/zero /newroot/dev/random /newroot/dev/urandom /newroot/dev/tty 2>/dev/null

ln -sf /proc/self/fd /newroot/dev/fd
ln -sf fd/0 /newroot/dev/stdin
ln -sf fd/1 /newroot/dev/stdout
ln -sf fd/2 /newroot/dev/stderr

# Montar compartidos VirtIO-FS
echo "[NKR-{label}] [DBG] montando VirtIO-FS extras..."
for idx in 0 1 2 3; do
    eval TAG="\$NKR_FS${{idx}}_TAG"
    eval MNT="\$NKR_FS${{idx}}_MNT"
    [ -z "$TAG" ] && continue
    [ -z "$MNT" ] && continue
    [ "$TAG" = "$NKR_ROOTFS" ] && continue
    echo "[NKR-{label}] [DBG] montando $TAG → $MNT"
    mkdir -p "/newroot${{MNT}}"
    mount -t virtiofs "$TAG" "/newroot${{MNT}}" \
        && echo "[NKR-{label}] VirtIO-FS: ${{TAG}} montado en ${{MNT}}" \
        || echo "[NKR-{label}] WARN: VirtIO-FS falla en montar ${{TAG}}"
done

# Overrides de archivos individuales: bind-mount desde /tmp/nkr-overrides sobre rootfs
if [ -d /newroot/tmp/nkr-overrides ]; then
    cd /newroot/tmp/nkr-overrides
    /bin/busybox find . -type f 2>/dev/null | while IFS= read -r f; do
        rel="${{f#./}}"
        tgt="/newroot/${{rel}}"
        src="/newroot/tmp/nkr-overrides/${{rel}}"
        if [ -e "$tgt" ]; then
            mount -o bind "$src" "$tgt" \
                && echo "[NKR-{label}] Override: /${{rel}}" \
                || echo "[NKR-{label}] WARN: bind fallo para /${{rel}}"
        else
            echo "[NKR-{label}] WARN: override /${{rel}} no existe en rootfs (omitido)"
        fi
    done
    cd /
fi

# Montar volúmenes VirtIO-BLK (ext4 — sin virtiofsd, sin DAX)
for idx in 0 1 2; do
    eval DEV="\$NKR_BLK${{idx}}_DEV"
    eval MNT="\$NKR_BLK${{idx}}_MNT"
    [ -z "$DEV" ] && continue
    [ -z "$MNT" ] && continue
    # Esperar a que el dispositivo aparezca (virtio-blk puede tardar unos ciclos)
    BWAIT=0
    while [ ! -b "$DEV" ] && [ $BWAIT -lt 50 ]; do sleep 0.1; BWAIT=$((BWAIT+1)); done
    if [ -b "$DEV" ]; then
        mkdir -p "/newroot${{MNT}}"
        mount -t ext4 -o rw,noatime,nodiratime "$DEV" "/newroot${{MNT}}" \
            && echo "[NKR-{label}] BLK: ${{DEV}} → ${{MNT}}" \
            || echo "[NKR-{label}] WARN: BLK mount falló ${{DEV}}"
    else
        echo "[NKR-{label}] WARN: ${{DEV}} no apareció tras 5s"
    fi
done

echo "[NKR-{label}] Starting {name}..."

# Buscar el nkr-env inyectado por el host en los volúmenes compartidos
# Usar cat (no cp) ya que busybox puede no tener cp
for MNT in "$NKR_FS0_MNT" "$NKR_FS1_MNT" "$NKR_FS2_MNT" "$NKR_FS3_MNT" "$NKR_FS4_MNT" "$NKR_FS5_MNT" "$NKR_FS6_MNT" "$NKR_FS7_MNT"; do
    if [ -n "$MNT" ] && [ -f "/newroot${{MNT}}/nkr-env" ]; then
        mkdir -p /newroot/tmp/nkr
        cat "/newroot${{MNT}}/nkr-env" > /newroot/tmp/nkr/nkr-env
        break
    fi
done

# Hacer /etc/hosts escribible via bind desde tmpfs (rootfs es RO)
cat /newroot/etc/hosts > /newroot/tmp/hosts 2>/dev/null || echo "127.0.0.1 localhost" > /newroot/tmp/hosts
mount -o bind /newroot/tmp/hosts /newroot/etc/hosts

# Watcher de apagado limpio — corre en el contexto del init (tiene /bin/busybox)
( while [ ! -e /dev/hvc0 ]; do sleep 0.1; done
  read -r _nkr_cmd < /dev/hvc0 || true
  echo "[NKR-{label}] Comando '$_nkr_cmd' en hvc0 — apagado limpio..."
  # Leer PID desde postmaster.pid (buscar en rutas típicas de datos)
  _PG_PID=""
  for _pgdata in /newroot/var/lib/postgresql/data /newroot/var/lib/postgresql/16/data /newroot/var/lib/postgresql/15/data; do
    if [ -f "$_pgdata/postmaster.pid" ]; then
      _PG_PID=$(/bin/busybox head -n1 "$_pgdata/postmaster.pid" 2>/dev/null)
      echo "[NKR-{label}] postmaster.pid encontrado: PID=$_PG_PID en $_pgdata"
      break
    fi
  done
  if [ -n "$_PG_PID" ] && /bin/busybox kill -0 "$_PG_PID" 2>/dev/null; then
    echo "[NKR-{label}] Enviando SIGTERM a postgres PID=$_PG_PID"
    /bin/busybox kill -SIGTERM "$_PG_PID" 2>/dev/null || true
    _w=0
    while /bin/busybox kill -0 "$_PG_PID" 2>/dev/null && [ $_w -lt 15 ]; do
      sleep 1; _w=$((_w+1))
    done
    /bin/busybox kill -0 "$_PG_PID" 2>/dev/null && /bin/busybox kill -SIGKILL "$_PG_PID" 2>/dev/null || true
    sleep 1
  else
    echo "[NKR-{label}] WARN: postgres PID no encontrado o ya terminado"
  fi
  /bin/busybox sync
  for _blk_mnt in $NKR_BLK0_MNT $NKR_BLK1_MNT $NKR_BLK2_MNT; do
    [ -n "$_blk_mnt" ] && umount "/newroot$_blk_mnt" 2>/dev/null || true
  done
  # Desmontar binds y rootfs para cerrar journal ext4 limpio (evita que host
  # falle el loop,ro del próximo arranque por journal sucio)
  umount /newroot/dev 2>/dev/null || true
  umount /newroot/sys 2>/dev/null || true
  umount /newroot/proc 2>/dev/null || true
  umount -l /newroot 2>/dev/null || true
  /bin/busybox sync
  echo "[NKR-{label}] Filesystems sincronizados, apagando."
  /bin/busybox reboot -f
) &

# Escribir nkr-start.sh en tmpfs (no en rootfs RO)
mkdir -p /newroot/tmp
cat > /newroot/tmp/nkr-start.sh << 'NKREOF'
#!/bin/sh
export PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin

# Autodetectar y agregar binarios de PostgreSQL al PATH
for pg_bin in /usr/lib/postgresql/*/bin; do
    if [ -d "$pg_bin" ]; then
        export PATH=$PATH:$pg_bin
    fi
done

for env_file in /etc/nkr-env /etc/odoo/nkr-env /tmp/nkr/nkr-env; do
    [ -f "$env_file" ] && . "$env_file" && echo "[NKR-{label}] Cargado nkr-env desde $env_file"
done

# NKR_CLEAN: hacer que los directorios padre sean escribibles (RO rootfs)
# Para cada archivo, montar tmpfs sobre su directorio preservando el contenido
if [ -n "$NKR_CLEAN" ]; then
    _NKR_OVERLAY_DONE=""
    for f in $NKR_CLEAN; do
        _dir=$(dirname "$f")
        # Evitar overlay duplicado del mismo directorio
        case "$_NKR_OVERLAY_DONE" in *"$_dir"*) ;; *)
            if [ -d "$_dir" ]; then
                cp -a "$_dir" "/tmp/nkr_overlay$$" 2>/dev/null
                mount -t tmpfs tmpfs "$_dir"
                cp -a "/tmp/nkr_overlay$$/." "$_dir/" 2>/dev/null
                rm -rf "/tmp/nkr_overlay$$"
                echo "[NKR-{label}] overlay tmpfs: $_dir (RW)"
            fi
            _NKR_OVERLAY_DONE="$_NKR_OVERLAY_DONE $_dir"
        ;; esac
    done
    for f in $NKR_CLEAN; do rm -f "$f"; done
fi

# Limpiar postmaster.pid huérfano (shutdown abrupto anterior)
_PGDATA="${{PGDATA:-/var/lib/postgresql/data}}"
if [ -f "$_PGDATA/postmaster.pid" ]; then
    _PG_PID=$(head -n1 "$_PGDATA/postmaster.pid" 2>/dev/null)
    if [ -z "$_PG_PID" ] || ! kill -0 "$_PG_PID" 2>/dev/null; then
        echo "[NKR-{label}] Limpiando postmaster.pid huérfano (pid='$_PG_PID')"
        rm -f "$_PGDATA/postmaster.pid"
    fi
fi

if [ -n "$DB_HOST" ]; then
    echo "$DB_HOST db" >> /etc/hosts
fi
echo "127.0.0.1 localhost" >> /etc/hosts

COMMAND="{full_command}"

if [ "$COMMAND" = "/entrypoint.sh" ] || [ "$COMMAND" = "/docker-entrypoint.sh" ] || [ "$COMMAND" = "/usr/local/bin/docker-entrypoint.sh" ] || [ -z "$COMMAND" ]; then
    if [ -x "/usr/bin/odoo" ]; then
        # Forzar usuario odoo — preservar ODOO_RC + env (DB_*) que ya están exportadas
        export ODOO_RC="${{ODOO_RC:-/etc/odoo/odoo.conf}}"
        export PYTHONUNBUFFERED=1
        _ODOO_CMD="/usr/bin/python3 -u /usr/bin/odoo -c $ODOO_RC"
        if [ -x "/usr/local/bin/gosu" ] || [ -x "/usr/sbin/gosu" ] || [ -x "/usr/bin/gosu" ]; then
            COMMAND="env PYTHONUNBUFFERED=1 gosu odoo $_ODOO_CMD"
        elif [ -x "/usr/local/sbin/su-exec" ] || [ -x "/sbin/su-exec" ]; then
            COMMAND="env PYTHONUNBUFFERED=1 su-exec odoo $_ODOO_CMD"
        else
            # su -p preserva env; exportar explícitamente por si la PAM lo filtra
            COMMAND="su -p -s /bin/sh odoo -c 'export PYTHONUNBUFFERED=1 ODOO_RC=$ODOO_RC DB_HOST=\"$DB_HOST\" DB_PORT=\"$DB_PORT\" DB_USER=\"$DB_USER\" DB_PASSWORD=\"$DB_PASSWORD\"; exec $_ODOO_CMD'"
        fi
    elif [ -d "/var/lib/postgresql" ] || [ -x "/usr/local/bin/postgres" ]; then
        # Inicializar PGDATA si está vacío (primer arranque)
        _PGDATA="${{PGDATA:-/var/lib/postgresql/data}}"
        if [ ! -f "$_PGDATA/postgresql.conf" ]; then
            echo "[NKR-{label}] PGDATA vacío — ejecutando initdb..."
            mkdir -p "$_PGDATA"
            chown -R 999:999 "$_PGDATA" 2>/dev/null || true
            if [ -x "/usr/local/bin/gosu" ] || [ -x "/usr/bin/gosu" ] || [ -x "/usr/sbin/gosu" ]; then
                _GOSU_BIN=$(command -v gosu)
                $_GOSU_BIN postgres initdb -U "${{POSTGRES_USER:-postgres}}" --pgdata="$_PGDATA" \
                    && echo "[NKR-{label}] initdb completado" \
                    || echo "[NKR-{label}] WARN: initdb falló"
            elif [ -x "/usr/local/sbin/su-exec" ] || [ -x "/sbin/su-exec" ]; then
                _SUEXEC_BIN=$(command -v su-exec)
                $_SUEXEC_BIN postgres initdb -U "${{POSTGRES_USER:-postgres}}" --pgdata="$_PGDATA" \
                    && echo "[NKR-{label}] initdb completado" \
                    || echo "[NKR-{label}] WARN: initdb falló"
            fi
        fi
        _PGCMD="postgres -D $_PGDATA -k /var/run/postgresql"
        if [ -x "/usr/local/sbin/su-exec" ]; then
            COMMAND="su-exec postgres $_PGCMD"
        elif [ -x "/sbin/su-exec" ]; then
            COMMAND="su-exec postgres $_PGCMD"
        elif [ -x "/usr/local/bin/gosu" ]; then
            COMMAND="gosu postgres $_PGCMD"
        else
            COMMAND="$_PGCMD"
        fi
    elif [ -x "/usr/bin/pgbouncer" ]; then
        COMMAND="$COMMAND pgbouncer /etc/pgbouncer/pgbouncer.ini"
    fi
fi

# Inyectar overrides de rendimiento en postgresql.auto.conf
# NKR_PG_NOFSYNC=1 → desactiva fsync/full_page_writes (GCP maneja durabilidad)
if [ -n "$NKR_PG_NOFSYNC" ] && [ -d "${{PGDATA:-/var/lib/postgresql/data}}" ]; then
    _AUTOCNF="${{PGDATA:-/var/lib/postgresql/data}}/postgresql.auto.conf"
    # Evitar duplicar si ya está inyectado
    if ! grep -q "nkr-nofsync" "$_AUTOCNF" 2>/dev/null; then
        cat >> "$_AUTOCNF" << 'PGEOF'
# nkr-nofsync: NKR performance overrides — GCP storage provides durability
fsync = off
synchronous_commit = off
full_page_writes = off
PGEOF
        echo "[NKR-{label}] WARN: fsync=off inyectado en postgresql.auto.conf"
    fi
fi

echo "[NKR-{label}] Ejecutando comando final: $COMMAND"
eval "exec $COMMAND"
NKREOF

chmod +x /newroot/tmp/nkr-start.sh

chroot /newroot /tmp/nkr-start.sh > /dev/ttyS0 2>&1
echo "[NKR-{label}] Process exited. Freezing."
while true; do sleep 3600; done
"#,
        label = label,
        name = name,
        full_command = full_command,
    )
}