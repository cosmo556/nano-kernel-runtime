// =============================================================================
// NKR Initramfs Generator — Generates a generic initramfs for any OCI image
// =============================================================================
//
// Creates a self-contained .cpio.gz with:
//   - static busybox + symlinks
//   - kernel modules for ext4 + virtio
//   - generic init script that auto-detects the entrypoint
//
// The resulting initramfs mounts the ext4 disk, detects the Docker image's
// entrypoint (/entrypoint.sh, /docker-entrypoint.sh, etc.) and runs it,
// passing the compose's environment variables.
// =============================================================================

use std::error::Error;
use std::fs;
use std::path::Path;
use std::process::Command;

/// NKR base data directory (configurable via NKR_DATA_DIR)
fn nkr_data_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(
        std::env::var("NKR_DATA_DIR").unwrap_or_else(|_| "/mnt/nkr".to_string()),
    )
}

/// Path to the base initramfs (busybox + modules) used as a template
fn base_initramfs_path() -> std::path::PathBuf {
    nkr_data_dir().join("initramfs").join("base")
}

/// Generic init script for any OCI image — DAX-capable
const GENERIC_INIT_SCRIPT: &str = r#"#!/bin/sh
export PATH=/bin:/sbin:/usr/bin:/usr/sbin:/usr/local/bin
mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devtmpfs devtmpfs /dev

mdev -s
if [ -e /dev/ttyS0 ]; then exec > /dev/ttyS0 2>&1; else exec > /dev/kmsg 2>&1; fi

echo "[NKR] init started"
[ -f /proc/sys/kernel/hotplug ] && echo /sbin/mdev > /proc/sys/kernel/hotplug

# Resolve cmdline parameters
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
    ip link set lo up
    ip link set eth0 up
    ip addr add ${GUEST_IP}/24 dev eth0
    # Default route: derivar del propio GUEST_IP. NKR convención
    # (registry.rs:215): IP = 10.0.{cell_id}.{vm_id+1}. El gateway
    # del bridge de cada cell es 10.0.{cell_id}.1. Antes estaba
    # hardcoded 10.0.0.1 (bridge legacy nkr0) → cells > 0 no podían
    # salir a internet (DNS / IAP / partner-autocomplete failed).
    GW=$(echo "$GUEST_IP" | awk -F. '{print $1"."$2"."$3".1"}')
    ip route add default via "$GW"
    echo "[NKR] eth0: ${GUEST_IP}/24 default-gw=${GW}"
fi

mkdir -p /newroot

# =========================================================================
# PMEM WAIT LOOP (increased to 3 seconds max)
# =========================================================================
WAIT=0
while [ ! -b /dev/pmem0 ] && [ $WAIT -lt 30 ]; do 
    sleep 0.1
    WAIT=$((WAIT+1))
done

# =========================================================================
# SMART MOUNT LOGIC (PMEM/DAX vs VDA)
# =========================================================================
if [ -b /dev/pmem0 ]; then
    echo "[NKR] rootfs: /dev/pmem0 detectado (Intentando DAX)"
    mount -t ext4 -o dax,rw /dev/pmem0 /newroot || \
    mount -t ext4 -o rw /dev/pmem0 /newroot
elif [ -n "$NKR_ROOTFS" ]; then
    echo "[NKR] rootfs: VirtIO-FS compartido (tag=$NKR_ROOTFS, RO)"
    mount -t virtiofs -o ro "$NKR_ROOTFS" /newroot
    echo "[NKR-DEBUG] rootfs mount opts: $(grep ' /newroot ' /proc/mounts 2>/dev/null | head -1 || echo '?')"
    # Breathing zones — tmpfs overlays on RO rootfs
    mount -t tmpfs tmpfs /newroot/tmp
    mount -t tmpfs tmpfs /newroot/run
    mkdir -p /newroot/tmp/nkr
    busybox cp /bin/busybox /newroot/tmp/nkr/busybox
    mkdir -p /newroot/var/run/postgresql /newroot/var/log
    mount -t tmpfs tmpfs /newroot/var/run/postgresql
    mount -t tmpfs tmpfs /newroot/var/log
    # /mnt/ → tmpfs: acá se crean los mountpoints de los virtio-fs shares
    # (/mnt/extra-addons, /mnt/systemouts-addons, /mnt/extra-pylibs, ...).
    # Tapa los dirs baked del rootfs (con .keep) pero los shares se montan
    # encima, así que el contenido baked es irrelevante. Hace que un share
    # /mnt/* NUEVO "just work" sin rebuild del rootfs maestro (que es RO → el
    # initramfs no podría hacer mkdir del mountpoint). Mismo patrón que
    # /var/log y /tmp arriba. Si el rootfs no tiene /mnt (raro), se skipea.
    [ -d /newroot/mnt ] && mount -t tmpfs tmpfs /newroot/mnt \
        && echo "[NKR] /mnt → tmpfs (mountpoints de virtio-fs shares se crean acá)"
else
    echo "[NKR] rootfs: /dev/vda detectado (Modo bloque estándar)"
    mount -t ext4 -o rw /dev/vda /newroot
fi

echo "[NKR] rootfs mounted"

mount -o bind /proc /newroot/proc
mount -o bind /sys /newroot/sys
mount -t devtmpfs devtmpfs /newroot/dev 2>/dev/null \
    || mount -o bind /dev /newroot/dev
mkdir -p /newroot/dev/pts /newroot/dev/shm 2>/dev/null
mount -t devpts devpts /newroot/dev/pts 2>/dev/null || true
mount -t tmpfs tmpfs /newroot/dev/shm 2>/dev/null || true
chmod 666 /newroot/dev/null /newroot/dev/random /newroot/dev/urandom /newroot/dev/zero /newroot/dev/tty 2>/dev/null
ln -sf /proc/self/fd /newroot/dev/fd
ln -sf fd/0 /newroot/dev/stdin
ln -sf fd/1 /newroot/dev/stdout
ln -sf fd/2 /newroot/dev/stderr

# Mount VirtIO-FS shares (virtiofsd) — skip rootfs (already mounted)
for idx in 0 1 2 3 4 5 6 7; do
    eval TAG="\$NKR_FS${idx}_TAG"
    eval MNT="\$NKR_FS${idx}_MNT"
    eval MODE="\$NKR_FS${idx}_RW"
    [ -z "$TAG" ] && continue
    [ -z "$MNT" ] && continue
    # If this tag is the rootfs, it's already mounted at /
    [ "$TAG" = "$NKR_ROOTFS" ] && continue
    mkdir -p "/newroot${MNT}"
    MOUNT_OPT=""
    [ "$MODE" = "ro" ] && MOUNT_OPT="-o ro"
    mount -t virtiofs $MOUNT_OPT "$TAG" "/newroot${MNT}" \
        && echo "[NKR] VirtIO-FS: ${TAG} montado en ${MNT} (${MODE:-rw})" \
        || echo "[NKR] WARN: VirtIO-FS: no se pudo montar ${TAG}"
done

# Mount VirtIO-BLK volumes (ext4 — no virtiofsd, no DAX)
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

# Per-file overrides: bind-mount from /tmp/nkr-overrides onto rootfs
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

# Source env vars: first /etc/nkr-env (origin disk), /tmp/nkr/nkr-env (injected)
# Look for the file in mapped shares (if they came from the host)
for MNT in "$NKR_FS0_MNT" "$NKR_FS1_MNT" "$NKR_FS2_MNT" "$NKR_FS3_MNT" "$NKR_FS4_MNT" "$NKR_FS5_MNT" "$NKR_FS6_MNT" "$NKR_FS7_MNT"; do
    if [ -n "$MNT" ] && [ -f "/newroot${MNT}/nkr-env" ]; then
        mkdir -p /newroot/tmp/nkr
        if [ "${MNT}/nkr-env" != "/tmp/nkr/nkr-env" ]; then
            cp "/newroot${MNT}/nkr-env" /newroot/tmp/nkr/nkr-env
        fi
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

pub fn generate_initramfs(name: &str, disk_path: &str, docker_cmd: Option<&[String]>, env_vars: Option<&std::collections::HashMap<String, String>>) -> Result<String, Box<dyn Error>> {
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

    // Embed env vars in /etc/nkr-env inside the initramfs (if provided).
    // This ensures immediate availability without depending on file delivery via shares.
    if let Some(env_map) = env_vars {
        if !env_map.is_empty() {
            let mut content = String::from("# NKR environment variables (embedded in initramfs)\n");
            for (key, val) in env_map {
                let escaped = val.replace('\'', "'\\''");
                content.push_str(&format!("export {}='{}'\n", key, escaped));
            }
            let env_path = work.join("etc/nkr-env");
            fs::write(&env_path, &content)?;
            eprintln!("[NKR-INITRAMFS] Env vars embebidas en /etc/nkr-env ({} bytes)", content.len());
        }
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

/// Generates a custom init script with a full command — DAX-capable
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
        nkr.fs4=*)     NKR_FS4_TAG="${{param#nkr.fs4=}}" ;;
        nkr.fsm4=*)    NKR_FS4_MNT="${{param#nkr.fsm4=}}" ;;
        nkr.fs5=*)     NKR_FS5_TAG="${{param#nkr.fs5=}}" ;;
        nkr.fsm5=*)    NKR_FS5_MNT="${{param#nkr.fsm5=}}" ;;
        nkr.fs6=*)     NKR_FS6_TAG="${{param#nkr.fs6=}}" ;;
        nkr.fsm6=*)    NKR_FS6_MNT="${{param#nkr.fsm6=}}" ;;
        nkr.fs7=*)     NKR_FS7_TAG="${{param#nkr.fs7=}}" ;;
        nkr.fsm7=*)    NKR_FS7_MNT="${{param#nkr.fsm7=}}" ;;
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
    ip link set lo up
    ip link set eth0 up
    ip addr add ${{GUEST_IP}}/24 dev eth0
    GW=$(echo "$GUEST_IP" | awk -F. '{{print $1"."$2"."$3".1"}}')
    ip route add default via "$GW"
    echo "[NKR-{label}] eth0: ${{GUEST_IP}}/24 default-gw=${{GW}}"
fi

mkdir -p /newroot

# =========================================================================
# PMEM WAIT LOOP AND TOLERANT MOUNT
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
    echo "[NKR-{label}] [DBG] rootfs mount opts: $(grep ' /newroot ' /proc/mounts 2>/dev/null | head -1 || echo '?')"
    # Breathing zones — tmpfs overlays on RO rootfs
    mount -t tmpfs tmpfs /newroot/tmp && echo "[NKR-{label}] [DBG] /tmp OK"
    mount -t tmpfs tmpfs /newroot/run && echo "[NKR-{label}] [DBG] /run OK"
    mkdir -p /newroot/tmp/nkr
    # /var/run is a symlink → /run on Debian; create postgresql/ inside /run's tmpfs
    mkdir -p /newroot/run/postgresql
    chown -R 999:999 /newroot/run/postgresql 2>/dev/null
    echo "[NKR-{label}] [DBG] /run/postgresql OK ($(stat -c %U:%G /newroot/run/postgresql 2>/dev/null || echo 999:999))"
    mount -t tmpfs tmpfs /newroot/var/log \
        && echo "[NKR-{label}] [DBG] /var/log OK" \
        || echo "[NKR-{label}] [DBG] /var/log FALLO"
    # /mnt/ → tmpfs: acá se crean los mountpoints de los virtio-fs shares
    # (/mnt/extra-addons, /mnt/systemouts-addons, /mnt/extra-pylibs, ...).
    # Tapa los dirs baked del rootfs (con .keep) pero los shares se montan
    # encima → contenido baked irrelevante. Hace que un share /mnt/* NUEVO
    # "just work" sin rebuild del rootfs maestro (RO → no se puede mkdir el
    # mountpoint). Mismo patrón que /var/log y /tmp arriba.
    if [ -d /newroot/mnt ]; then
        mount -t tmpfs tmpfs /newroot/mnt \
            && echo "[NKR-{label}] [DBG] /mnt → tmpfs OK" \
            || echo "[NKR-{label}] [DBG] /mnt → tmpfs FALLO"
    fi
else
    echo "[NKR-{label}] rootfs: /dev/vda detectado (Modo bloque estándar)"
    mount -t ext4 -o rw /dev/vda /newroot
fi

echo "[NKR-{label}] [DBG] bind proc/sys/dev..."
mount -o bind /proc /newroot/proc
mount -o bind /sys /newroot/sys
mount -t devtmpfs devtmpfs /newroot/dev 2>/dev/null \
    || mount -o bind /dev /newroot/dev
mkdir -p /newroot/dev/pts /newroot/dev/shm 2>/dev/null
mount -t devpts devpts /newroot/dev/pts 2>/dev/null || true
mount -t tmpfs tmpfs /newroot/dev/shm 2>/dev/null || true
echo "[NKR-{label}] [DBG] bind OK"

# Grant universal permissions to key devices (Postgres does 'su postgres')
chmod 666 /newroot/dev/null /newroot/dev/zero /newroot/dev/random /newroot/dev/urandom /newroot/dev/tty 2>/dev/null

ln -sf /proc/self/fd /newroot/dev/fd
ln -sf fd/0 /newroot/dev/stdin
ln -sf fd/1 /newroot/dev/stdout
ln -sf fd/2 /newroot/dev/stderr

# Mount VirtIO-FS shares (hasta 8 slots — 0 es rootfs, se salta)
echo "[NKR-{label}] [DBG] montando VirtIO-FS extras..."
for idx in 0 1 2 3 4 5 6 7; do
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

# Per-file overrides: bind-mount from /tmp/nkr-overrides onto rootfs
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

# Mount VirtIO-BLK volumes (ext4 — no virtiofsd, no DAX)
for idx in 0 1 2; do
    eval DEV="\$NKR_BLK${{idx}}_DEV"
    eval MNT="\$NKR_BLK${{idx}}_MNT"
    [ -z "$DEV" ] && continue
    [ -z "$MNT" ] && continue
    # Wait for the device to appear (virtio-blk may take a few cycles)
    BWAIT=0
    while [ ! -b "$DEV" ] && [ $BWAIT -lt 50 ]; do sleep 0.1; BWAIT=$((BWAIT+1)); done
    if [ -b "$DEV" ]; then
        mkdir -p "/newroot${{MNT}}"
        mount -t ext4 -o rw,noatime,nodiratime "$DEV" "/newroot${{MNT}}" \
            && echo "[NKR-{label}] BLK: ${{DEV}} → ${{MNT}}" \
            || echo "[NKR-{label}] WARN: BLK mount falló ${{DEV}}"
        # Adjust owner to service user if the mount matches
        case "$MNT" in
            /var/lib/odoo)
                _OUID=$(awk -F: '/^odoo:/{{print $3":"$4; exit}}' /newroot/etc/passwd 2>/dev/null)
                [ -n "$_OUID" ] && chown -R "$_OUID" "/newroot${{MNT}}" 2>/dev/null
                mkdir -p "/newroot${{MNT}}/sessions" "/newroot${{MNT}}/filestore" "/newroot${{MNT}}/addons"
                [ -n "$_OUID" ] && chown -R "$_OUID" "/newroot${{MNT}}" 2>/dev/null
                echo "[NKR-{label}] BLK: chown $_OUID /var/lib/odoo"
                ;;
            /var/lib/postgresql/data)
                chown -R 999:999 "/newroot${{MNT}}" 2>/dev/null
                ;;
        esac
    else
        echo "[NKR-{label}] WARN: ${{DEV}} no apareció tras 5s"
    fi
done

echo "[NKR-{label}] Starting {name}..."

# Look for the nkr-env injected by the host in the shared volumes
# Use cat (not cp) since busybox may not have cp
for MNT in "$NKR_FS0_MNT" "$NKR_FS1_MNT" "$NKR_FS2_MNT" "$NKR_FS3_MNT" "$NKR_FS4_MNT" "$NKR_FS5_MNT" "$NKR_FS6_MNT" "$NKR_FS7_MNT"; do
    if [ -n "$MNT" ] && [ -f "/newroot${{MNT}}/nkr-env" ]; then
        mkdir -p /newroot/tmp/nkr
        if [ "${{MNT}}/nkr-env" != "/tmp/nkr/nkr-env" ]; then
            cat "/newroot${{MNT}}/nkr-env" > /newroot/tmp/nkr/nkr-env
        fi
        break
    fi
done

# Make /etc/hosts writable via bind from tmpfs (rootfs is RO)
cat /newroot/etc/hosts > /newroot/tmp/hosts 2>/dev/null || echo "127.0.0.1 localhost" > /newroot/tmp/hosts
mount -o bind /newroot/tmp/hosts /newroot/etc/hosts

# DNS para que el guest pueda resolver dominios externos (partner-autocomplete,
# IAP, mail outbound vía SMTP por hostname, OAuth, etc.). El rootfs es RO, así
# que escribimos a tmpfs y bind-mount sobre /etc/resolv.conf.
# Usamos resolvers públicos por defecto (Cloudflare + Google) — sobrescribibles
# vía kernel cmdline `nkr.dns=<ip>[,<ip2>]`.
NKR_DNS_LIST="1.1.1.1 8.8.8.8"
for param in $(cat /proc/cmdline); do
    case "$param" in
        nkr.dns=*) NKR_DNS_LIST="$(echo ${{param#nkr.dns=}} | tr ',' ' ')" ;;
    esac
done
{{
    for ns in $NKR_DNS_LIST; do echo "nameserver $ns"; done
    echo "options timeout:2 attempts:2"
}} > /newroot/tmp/resolv.conf
# Si /etc/resolv.conf en el rootfs es symlink (común en imágenes Debian/Ubuntu
# que tenían systemd-resolved al builder), `mount -o bind` falla porque el
# kernel resuelve el symlink y trata de montar sobre el target inexistente.
# Fix: borrar el symlink en el espacio del rootfs ANTES del mount no es
# posible (rootfs RO). Lo que sí podemos: si es symlink, bind-mount al
# target real (creándolo en tmpfs primero).
if [ -L /newroot/etc/resolv.conf ]; then
    _link=$(readlink /newroot/etc/resolv.conf)
    case "$_link" in
        /*) _abs="/newroot$_link" ;;
        *)  _abs="/newroot/etc/$_link" ;;
    esac
    /bin/busybox mkdir -p "$(/bin/busybox dirname "$_abs")" 2>/dev/null
    : > "$_abs" 2>/dev/null
    mount -o bind /newroot/tmp/resolv.conf "$_abs" 2>/dev/null
    echo "[NKR-{label}] DNS: /etc/resolv.conf era symlink → $_link, montado en $_abs"
else
    mount -o bind /newroot/tmp/resolv.conf /newroot/etc/resolv.conf 2>/dev/null
    echo "[NKR-{label}] DNS: /etc/resolv.conf bind-mount directo"
fi
# Debug en /var/log/odoo (virtio-fs share, visible desde host) — tolerante si
# el dir no existe todavía.
/bin/busybox mkdir -p /newroot/var/log/odoo 2>/dev/null
{{
    echo "=== NKR DNS DEBUG ==="
    echo "DNS_LIST=$NKR_DNS_LIST"
    echo "ls /etc/resolv.conf:"; ls -la /newroot/etc/resolv.conf 2>&1
    echo "cat /etc/resolv.conf:"; /bin/busybox cat /newroot/etc/resolv.conf 2>&1
    echo "mounts(resolv|hosts):"; /bin/busybox grep -E "resolv|hosts" /proc/mounts 2>&1
}} > /newroot/var/log/odoo/nkr-dns-debug.log 2>&1 || true
echo "[NKR-{label}] DNS configurado: $NKR_DNS_LIST"

# Clean-shutdown watcher — runs in the init context (has /bin/busybox)
( echo "[NKR-{label}-WATCHER] esperando /dev/hvc0..."
  _wct=0
  while [ ! -e /dev/hvc0 ] && [ $_wct -lt 600 ]; do sleep 0.1; _wct=$((_wct+1)); done
  if [ ! -e /dev/hvc0 ]; then
    echo "[NKR-{label}-WATCHER] ERROR: /dev/hvc0 no apareció tras 60s — watcher abortado"
    exit 1
  fi
  echo "[NKR-{label}-WATCHER] /dev/hvc0 listo (tras ${{_wct}}*100ms) — bloqueado leyendo..."
  _nkr_cmd=""
  # Loop hvc0 con dispatch: el watcher era single-shot (un read → SHUTDOWN
  # → poweroff). Ahora soporta múltiples comandos:
  #   SHUTDOWN  → apagado limpio + poweroff (terminal, rompe el loop)
  #   REL_OD    → recarga Odoo con código fresh del disco según el modo:
  #               workers>0 (prefork)  → SIGHUP al master (respawnea workers)
  #               workers=0 (threaded) → SIGTERM al proceso (supervisor loop
  #               de nkr-start.sh lo relanza). NO termina el loop, sigue
  #               escuchando hvc0 para próximos comandos.
  # NKR daemon (host) inyecta REL_OD vía SIGUSR1 al proceso de la VM tras
  # un addons/git exitoso o cuando el panel llama POST /reload.
  while true; do
    _nkr_cmd=""
    read -r _nkr_cmd < /dev/hvc0 || break
    [ -z "$_nkr_cmd" ] && continue
    echo "[NKR-{label}] hvc0 cmd='$_nkr_cmd'"

    # ─── REL_OD: reload de workers Odoo (sin matar la VM) ──────────────
    if [ "$_nkr_cmd" = "REL_OD" ]; then
      # Diferenciación por modo Odoo (workers=0 vs workers>0):
      #   - workers=0 (threaded): UN solo proceso Odoo werkzeug. SIGHUP no
      #     respawnea por sí solo. Solución: SIGTERM al proceso → muere →
      #     el supervisor loop de nkr-start.sh lo relanza inmediatamente
      #     con código fresh del disco.
      #   - workers>0 (prefork): master + N workers. SIGHUP al master →
      #     handler interno kill_workers + respawn con código fresh.
      #     Master sobrevive, supervisor loop no se entera.
      # Detección: leer 'workers = N' del odoo.conf bind-mounted bajo
      # /newroot/etc/odoo/odoo.conf. Si no existe o N==0 → threaded.
      _NKR_W=$(/bin/busybox grep -E '^[[:space:]]*workers[[:space:]]*=' \
               /newroot/etc/odoo/odoo.conf 2>/dev/null \
               | /bin/busybox awk -F= '{{gsub(/[[:space:]]/, "", $2); print $2}}' \
               | /bin/busybox head -n1)
      if [ -z "$_NKR_W" ] || [ "$_NKR_W" = "0" ]; then
        # Threaded: SIGTERM + grace 5s + SIGKILL fallback → supervisor respawn.
        # Bug 2026-05-15 en intech-devp: con usuario logueado (websocket activo)
        # + cron en curso, Odoo entró en "Initiating shutdown" y nunca completó
        # el exit graceful → REL_OD colgado >60s → watchdog mataba la VM entera.
        # Fix: si SIGTERM no surte efecto en 5s, escalamos a SIGKILL. El
        # supervisor loop respawnea con código fresh en ~1s.
        if /bin/busybox pkill -TERM -f '/usr/bin/odoo' 2>/dev/null; then
          echo "[NKR-{label}] REL_OD (workers=0/threaded): SIGTERM → esperando 5s"
          _w=0
          while /bin/busybox pgrep -f '/usr/bin/odoo' >/dev/null 2>&1 && [ $_w -lt 5 ]; do
            sleep 1; _w=$((_w+1))
          done
          if /bin/busybox pgrep -f '/usr/bin/odoo' >/dev/null 2>&1; then
            /bin/busybox pkill -KILL -f '/usr/bin/odoo' 2>/dev/null
            echo "[NKR-{label}] REL_OD: SIGKILL fallback tras 5s (Odoo no respondió a SIGTERM)"
          else
            echo "[NKR-{label}] REL_OD: Odoo terminó graceful en ${{_w}}s"
          fi
        else
          echo "[NKR-{label}] REL_OD: proceso Odoo no encontrado (skip)"
        fi
      else
        # Prefork: SIGHUP master → handler respawnea workers
        if /bin/busybox pkill -HUP -f '/usr/bin/odoo' 2>/dev/null; then
          echo "[NKR-{label}] REL_OD (workers=$_NKR_W/prefork): SIGHUP master → workers respawneando"
        else
          echo "[NKR-{label}] REL_OD: master Odoo no encontrado (skip)"
        fi
      fi
      continue
    fi

    # ─── Cualquier otro cmd o cmd vacío: tratar como SHUTDOWN ──────────
    # (back-compat con SHUTDOWN explícito y comandos legacy)
    echo "[NKR-{label}] Apagado limpio…"
    _HANDLED=0
    # PostgreSQL: prefer coordinated shutdown via postmaster.pid
  for _pgdata in /newroot/var/lib/postgresql/data /newroot/var/lib/postgresql/16/data /newroot/var/lib/postgresql/15/data; do
    if [ -f "$_pgdata/postmaster.pid" ]; then
      _PG_PID=$(/bin/busybox head -n1 "$_pgdata/postmaster.pid" 2>/dev/null)
      if [ -n "$_PG_PID" ] && /bin/busybox kill -0 "$_PG_PID" 2>/dev/null; then
        echo "[NKR-{label}] postgres PID=$_PG_PID → SIGTERM"
        /bin/busybox kill -SIGTERM "$_PG_PID" 2>/dev/null || true
        _w=0
        while /bin/busybox kill -0 "$_PG_PID" 2>/dev/null && [ $_w -lt 15 ]; do
          sleep 1; _w=$((_w+1))
        done
        _HANDLED=1
      fi
      break
    fi
  done
  # Generic path: VMs sin postgres (Odoo tenants, pgbouncer, otros).
  # Cada servicio corre en su PROPIA VM con su propia copia de este initramfs;
  # el path se elige según qué encuentre en /var/lib/postgresql/. Si no hay
  # postgres → caemos acá. Broadcast SIGTERM a todos los procesos excepto PID 1
  # y kernel threads. killall5 respeta init y su propia sesión.
  if [ "$_HANDLED" = "0" ]; then
    echo "[NKR-{label}] Broadcast SIGTERM (killall5) — app sin handler específico"
    /bin/busybox killall5 -15 2>/dev/null || true
    _w=0
    # Wait up to 5s (era 25s pre-v1.6.1) para que los procesos userspace terminen.
    # Justificación: el path postgres (arriba) ya tiene su propio timer de 15s
    # vía postmaster.pid. Acá solo caen apps que responden a SIGTERM en <2s
    # típico (Odoo workers: 1-2s, pgbouncer: <1s). El timeout viejo de 25s era
    # overhead pagado innecesariamente — el restart de tenants Odoo baja de
    # ~70s a ~25s con esto. PG no se afecta (corre por path propio).
    # Heuristic: userspace processes have /proc/N/cmdline NON-empty.
    # Kernel threads (kthreadd, ksoftirqd, etc.) have empty cmdline.
    while [ $_w -lt 5 ]; do
      _USER=0
      for _p in /proc/[0-9]*; do
        _pidn=${{_p#/proc/}}
        [ "$_pidn" = "1" ] && continue
        [ -s "$_p/cmdline" ] && {{ _USER=1; break; }}
      done
      [ "$_USER" = "0" ] && break
      sleep 1; _w=$((_w+1))
    done
    echo "[NKR-{label}] userspace drenado en ${{_w}}s — SIGKILL rezagados"
    /bin/busybox killall5 -9 2>/dev/null || true
    sleep 1
  fi
  /bin/busybox sync
  for _blk_mnt in $NKR_BLK0_MNT $NKR_BLK1_MNT $NKR_BLK2_MNT; do
    [ -n "$_blk_mnt" ] && umount "/newroot$_blk_mnt" 2>/dev/null || true
  done
  # Unmount binds and rootfs to close ext4 journal cleanly (prevents host from
  # failing loop,ro of next boot due to dirty journal)
  umount /newroot/dev 2>/dev/null || true
  umount /newroot/sys 2>/dev/null || true
  umount /newroot/proc 2>/dev/null || true
  umount -l /newroot 2>/dev/null || true
  /bin/busybox sync
  echo "[NKR-{label}] Filesystems sincronizados, apagando."
  /bin/busybox reboot -f
  break  # nunca llega — reboot -f es terminal, pero defensivo
  done  # while true del dispatcher hvc0
) &

# Write nkr-start.sh to tmpfs (not on RO rootfs)
mkdir -p /newroot/tmp
cat > /newroot/tmp/nkr-start.sh << 'NKREOF'
#!/bin/sh
export PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin

# Auto-detect and add PostgreSQL binaries to PATH
for pg_bin in /usr/lib/postgresql/*/bin; do
    if [ -d "$pg_bin" ]; then
        export PATH=$PATH:$pg_bin
    fi
done

# Cleanup trap — if the script dies before the final exec, clean up temp dirs.
_NKR_TMP_OVERLAY=""
trap '[ -n "$_NKR_TMP_OVERLAY" ] && rm -rf "$_NKR_TMP_OVERLAY" 2>/dev/null; true' EXIT INT TERM

for env_file in /etc/nkr-env /etc/odoo/nkr-env /tmp/nkr/nkr-env /tmp/nkr-overrides/nkr-env; do
    [ -f "$env_file" ] && . "$env_file" && echo "[NKR-{label}] Cargado nkr-env desde $env_file"
done

# ── Filestore rename (post-clone) ──────────────────────────────────────────
# When the API clones an instance, it injects NKR_RENAME_FILESTORE_FROM/TO
# so that filestore/db-<src>/ gets renamed to filestore/db-<dst>/ on the
# FIRST boot of the guest. The `.nkr-filestore-renamed` marker ensures idempotency.
# Moving this to the guest avoids mount -o loop on the host (bottleneck under
# concurrent clones + corruption risk if it crashes mid-mv).
if [ -n "$NKR_RENAME_FILESTORE_FROM" ] && [ -n "$NKR_RENAME_FILESTORE_TO" ]; then
    _FS_BASE=/var/lib/odoo/filestore
    _FS_MARKER="$_FS_BASE/.nkr-filestore-renamed"
    _FS_FROM="$_FS_BASE/$NKR_RENAME_FILESTORE_FROM"
    _FS_TO="$_FS_BASE/$NKR_RENAME_FILESTORE_TO"
    if [ ! -f "$_FS_MARKER" ]; then
        if [ -d "$_FS_FROM" ] && [ ! -d "$_FS_TO" ]; then
            mv "$_FS_FROM" "$_FS_TO" \
                && echo "[NKR-{label}] filestore: $NKR_RENAME_FILESTORE_FROM → $NKR_RENAME_FILESTORE_TO" \
                || echo "[NKR-{label}] WARN: mv filestore falló"
            touch "$_FS_MARKER" 2>/dev/null || true
        elif [ -d "$_FS_TO" ]; then
            echo "[NKR-{label}] filestore ya es $NKR_RENAME_FILESTORE_TO (sellando marker)"
            touch "$_FS_MARKER" 2>/dev/null || true
        else
            echo "[NKR-{label}] WARN: $_FS_FROM no existe, skip rename"
        fi
    fi
fi

# NKR_CLEAN: make parent directories writable (RO rootfs)
# For each file, mount tmpfs on its directory while preserving the contents
if [ -n "$NKR_CLEAN" ]; then
    _NKR_OVERLAY_DONE=""
    for f in $NKR_CLEAN; do
        _dir=$(dirname "$f")
        # Avoid duplicate overlay of the same directory
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

# Clean up orphaned postmaster.pid (previous abrupt shutdown)
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
        # Force odoo user — preserve ODOO_RC + env (DB_*) that are already exported
        export ODOO_RC="${{ODOO_RC:-/etc/odoo/odoo.conf}}"
        export PYTHONUNBUFFERED=1
        # Panel-delivered Python libs: pip install --target=<...>/pylibs/lib on the
        # host, mounted as /mnt/extra-pylibs via virtio-fs. Prepending to PYTHONPATH
        # so Odoo imports these modules without rebuilding the master ext4.
        if [ -d /mnt/extra-pylibs ]; then
            export PYTHONPATH="/mnt/extra-pylibs${{PYTHONPATH:+:$PYTHONPATH}}"
            echo "[NKR-{label}] PYTHONPATH prepended: /mnt/extra-pylibs"
        fi
        # NKR sitecustomize.py: parchea werkzeug log para mostrar IP real del
        # cliente (X-Real-IP / X-Forwarded-For) en vez del REMOTE_ADDR del proxy.
        # Python lo carga automáticamente si su dir está en sys.path.
        if [ -f /tmp/nkr-overrides/sitecustomize.py ]; then
            export PYTHONPATH="/tmp/nkr-overrides${{PYTHONPATH:+:$PYTHONPATH}}"
            echo "[NKR-{label}] PYTHONPATH prepended: /tmp/nkr-overrides (sitecustomize)"
        fi
        _ODOO_CMD="/usr/bin/python3 -u /usr/bin/odoo -c $ODOO_RC"
        # Asegurar que /var/log/odoo (virtio-fs share rw) sea writable por user odoo.
        # Sin esto, odoo.conf con `logfile = /var/log/odoo/odoo.log` no logra
        # crear el archivo (la share viene chowned root:root del host) y el panel
        # nunca ve el log via GET /logs. El chown propaga al host vía virtio-fs.
        if [ -d /var/log/odoo ]; then
            chown odoo:odoo /var/log/odoo 2>/dev/null || true
            chmod 0775 /var/log/odoo 2>/dev/null || true
        fi
        # Fontconfig writable cache para wkhtmltopdf. Sin esto los reports PDF
        # (facturas, presupuestos, etc.) salen sin fonts/styling porque
        # wkhtmltopdf no puede cachear glyphs ni cargar fonts del sistema.
        # Síntoma: WARNING odoo.addons.base.models.ir_actions_report:
        # "wkhtmltopdf: Fontconfig error: No writable cache directories".
        mkdir -p /tmp/.fontconfig 2>/dev/null
        chown odoo:odoo /tmp/.fontconfig 2>/dev/null || true
        export XDG_CACHE_HOME=/tmp
        export FONTCONFIG_PATH=/etc/fonts
        # Explicit export of env for the 3 privilege-drop mechanisms.
        # DB_* already come exported from the nkr-env loaded above; here we just
        # ensure the child process sees them.
        export DB_HOST DB_PORT DB_USER DB_PASSWORD ODOO_RC PYTHONUNBUFFERED PYTHONPATH XDG_CACHE_HOME FONTCONFIG_PATH
        if [ -x "/usr/local/bin/gosu" ] || [ -x "/usr/sbin/gosu" ] || [ -x "/usr/bin/gosu" ]; then
            COMMAND="gosu odoo $_ODOO_CMD"
        elif [ -x "/usr/local/sbin/su-exec" ] || [ -x "/sbin/su-exec" ]; then
            COMMAND="su-exec odoo $_ODOO_CMD"
        else
            # su -p (--preserve-environment) inherits DB_*, ODOO_RC. We do NOT
            # interpolate values into the command string — that would open
            # injection if DB_PASSWORD contained ';' or '$(...)'. The inner
            # shell sees the vars via inherited env (su -p).
            COMMAND="su -p -s /bin/sh odoo -c 'exec $_ODOO_CMD'"
        fi
    elif [ -d "/var/lib/postgresql" ] || [ -x "/usr/local/bin/postgres" ]; then
        # Initialize PGDATA if empty (first boot)
        _PGDATA="${{PGDATA:-/var/lib/postgresql/data}}"
        echo "[NKR-{label}] DBG PGDATA=$PGDATA _PGDATA=$_PGDATA nkr-env-content=$(cat /tmp/nkr/nkr-env 2>&1 | head -3)"
        if [ ! -f "$_PGDATA/postgresql.conf" ]; then
            echo "[NKR-{label}] PGDATA vacío — ejecutando initdb..."
            mkdir -p "$_PGDATA"
            chmod 700 "$_PGDATA" 2>&1 || echo "[NKR-{label}] WARN: chmod 700 $_PGDATA falló"
            chown -R 999:999 "$_PGDATA" 2>&1 || echo "[NKR-{label}] WARN: chown 999:999 $_PGDATA falló"
            echo "[NKR-{label}] DBG perms: $(stat -c '%U:%G %a' "$_PGDATA" 2>/dev/null || ls -ld "$_PGDATA")"
            _INITDB_ARGS="-U ${{POSTGRES_USER:-postgres}} --pgdata=$_PGDATA --encoding=UTF8 --locale=C.UTF-8"
            if [ -x "/usr/local/bin/gosu" ] || [ -x "/usr/bin/gosu" ] || [ -x "/usr/sbin/gosu" ]; then
                _GOSU_BIN=$(command -v gosu)
                $_GOSU_BIN postgres initdb $_INITDB_ARGS \
                    && echo "[NKR-{label}] initdb completado" \
                    || echo "[NKR-{label}] WARN: initdb falló"
            elif [ -x "/usr/local/sbin/su-exec" ] || [ -x "/sbin/su-exec" ]; then
                _SUEXEC_BIN=$(command -v su-exec)
                $_SUEXEC_BIN postgres initdb $_INITDB_ARGS \
                    && echo "[NKR-{label}] initdb completado" \
                    || echo "[NKR-{label}] WARN: initdb falló"
            fi
        fi
        # Allow connections from the cell subnet (pgbouncer + odoos)
        if [ -f "$_PGDATA/postgresql.conf" ]; then
            if ! grep -q "^listen_addresses" "$_PGDATA/postgresql.conf"; then
                echo "listen_addresses = '*'" >> "$_PGDATA/postgresql.conf"
                echo "[NKR-{label}] postgresql.conf: listen_addresses='*'"
            fi
            if [ -f "$_PGDATA/pg_hba.conf" ] && ! grep -q "nkr-cell-net" "$_PGDATA/pg_hba.conf"; then
                cat >> "$_PGDATA/pg_hba.conf" << 'PGHBAEOF'
# nkr-cell-net: access from cell subnet (pgbouncer + odoos)
host all all 10.0.0.0/8 trust
PGHBAEOF
                echo "[NKR-{label}] pg_hba.conf: host all all 10.0.0.0/8 trust"
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
        _PGB_INI="${{PGBOUNCER_INI:-/etc/pgbouncer/pgbouncer.ini}}"
        echo "[NKR-{label}] PGB_INI=$PGBOUNCER_INI exists=$([ -f \"$PGBOUNCER_INI\" ] && echo Y || echo N) dir=$(ls -la /etc/pgbouncer/ 2>&1 | head -3)"
        if [ -n "$PGBOUNCER_INI" ] && [ -f "$PGBOUNCER_INI" ]; then
            # External override: copy ini+userlist to /etc/pgbouncer (tmpfs) and launch directly
            mkdir -p /etc/pgbouncer
            cp "$PGBOUNCER_INI" /etc/pgbouncer/pgbouncer.ini || echo "[NKR-{label}] WARN: cp pgbouncer.ini falló: $?"
            _PGB_DIR=$(dirname "$PGBOUNCER_INI")
            [ -f "$_PGB_DIR/userlist.txt" ] && cp "$_PGB_DIR/userlist.txt" /etc/pgbouncer/userlist.txt
            echo "[NKR-{label}] pgbouncer.ini copiado: $(ls -la /etc/pgbouncer/pgbouncer.ini 2>&1)"
            COMMAND="pgbouncer /etc/pgbouncer/pgbouncer.ini"
        else
            COMMAND="$COMMAND pgbouncer $_PGB_INI"
        fi
    fi
fi

# Inject performance overrides into postgresql.auto.conf
# NKR_PG_NOFSYNC=1 → disables fsync/full_page_writes (GCP handles durability)
if [ -n "$NKR_PG_NOFSYNC" ] && [ -d "${{PGDATA:-/var/lib/postgresql/data}}" ]; then
    _AUTOCNF="${{PGDATA:-/var/lib/postgresql/data}}/postgresql.auto.conf"
    # Avoid duplication if already injected
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
# `eval` is safe here because COMMAND does NOT contain interpolated user
# values (DB_HOST, DB_PASSWORD, etc. are inherited via `su -p` from the
# already-exported env, not embedded in the command string). COMMAND is a
# fixed template with binary paths + symbolic references.
#
# Supervisor loop (CLAUDE.md v2.2 — solo Odoo):
#   workers=0 (threaded): REL_OD = SIGTERM al proceso → muere → loop
#     respawnea con código fresh del disco. ~2s end-to-end.
#   workers>0 (prefork):  REL_OD = SIGHUP al master → master respawnea
#     workers internamente. El proceso master NO muere → loop nunca
#     itera (good — el master mantiene estado en memoria).
#   SHUTDOWN: el watcher hvc0 hace killall5 → mata el supervisor loop
#     también → poweroff → loop nunca respawnea (kernel halt).
#
# Resiliencia bonus: si Odoo crashea por OOM o panic Python, el loop
# lo respawnea automáticamente (igual que un systemd restart=always).
#
# Sólo aplicamos el loop a Odoo (cuando _ODOO_CMD está set). PG/pgbouncer
# tienen su propia mecánica (single exec, WAL replay en boot).
if [ -n "$_ODOO_CMD" ]; then
    while true; do
        echo "[NKR-{label}] Lanzando Odoo (supervisor): $COMMAND"
        eval "$COMMAND"
        _RC=$?
        echo "[NKR-{label}] Odoo salió rc=$_RC — respawn en 1s"
        sleep 1
    done
else
    eval "exec $COMMAND"
fi
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