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

/// Script init genérico para cualquier imagen OCI
const GENERIC_INIT_SCRIPT: &str = r#"#!/bin/sh
export PATH=/bin:/sbin
mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devtmpfs devtmpfs /dev
exec > /dev/ttyS0 2>&1

echo "[NKR] init started"
echo /sbin/mdev > /proc/sys/kernel/hotplug
mdev -s

M="/lib/modules/6.6.117-0-virt"
for mod in crc32c_generic.ko crc32c-intel.ko libcrc32c.ko crc16.ko mbcache.ko jbd2.ko ext4.ko virtio_blk.ko failover.ko net_failover.ko virtio_net.ko; do
    [ -f "$M/$mod" ] && insmod "$M/$mod" 2>/dev/null
done
echo "[NKR] modules loaded"

sleep 2
mdev -s

GUEST_IP=""
for param in $(cat /proc/cmdline); do
    case "$param" in nkr.ip=*) GUEST_IP="${param#nkr.ip=}" ;; esac
done
[ -z "$GUEST_IP" ] && GUEST_IP="10.0.0.2"

if [ -d /sys/class/net/eth0 ]; then
    ip link set eth0 up
    ip addr add ${GUEST_IP}/24 dev eth0
    ip route add default via 10.0.0.1
    echo "[NKR] eth0: ${GUEST_IP}/24"
fi

mkdir -p /newroot
mount /dev/vda /newroot
echo "[NKR] rootfs mounted"

mount -o bind /proc /newroot/proc
mount -o bind /sys /newroot/sys
mount -o bind /dev /newroot/dev
chmod 666 /newroot/dev/null /newroot/dev/random /newroot/dev/urandom 2>/dev/null
ln -sf /proc/self/fd /newroot/dev/fd
ln -sf fd/0 /newroot/dev/stdin
ln -sf fd/1 /newroot/dev/stdout
ln -sf fd/2 /newroot/dev/stderr

echo "[NKR] Detecting entrypoint..."
chroot /newroot /bin/sh -c '
[ -f /etc/nkr-env ] && . /etc/nkr-env
export PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin

# Auto-detectar entrypoint en orden de prioridad
ENTRYPOINT=""
for ep in /entrypoint.sh /docker-entrypoint.sh /usr/local/bin/docker-entrypoint.sh /run.sh /start.sh; do
    if [ -x "$ep" ]; then
        ENTRYPOINT="$ep"
        break
    fi
done

if [ -n "$ENTRYPOINT" ]; then
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

/// Asegura que el directorio base del initramfs exista con busybox + módulos.
/// Si ya existe, no hace nada. Si no, lo crea desde un initramfs existente
/// o desde los archivos del sistema.
fn ensure_base_initramfs() -> Result<std::path::PathBuf, Box<dyn Error>> {
    let base = base_initramfs_path();
    let busybox_path = base.join("bin/busybox");

    if busybox_path.exists() {
        return Ok(base);
    }

    eprintln!("[NKR-INITRAMFS] Creando base initramfs en {}...", base.display());
    let initramfs_dir = nkr_data_dir().join("initramfs");

    // Buscar un .cpio.gz existente para extraer busybox + módulos
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
        // Extraer de un cpio existente
        eprintln!(
            "[NKR-INITRAMFS] Extrayendo base desde {}...",
            cpio.display()
        );
        fs::create_dir_all(&base)?;
        let status = Command::new("sh")
            .args([
                "-c",
                &format!(
                    "cd '{}' && zcat '{}' | cpio -idm 2>/dev/null",
                    base.display(),
                    cpio.display()
                ),
            ])
            .status()?;
        if !status.success() {
            return Err("Fallo extrayendo base initramfs desde cpio existente".into());
        }
        // Borrar el init específico, se reemplazará
        let _ = fs::remove_file(base.join("init"));
    } else {
        return Err(
            "No se encontró un initramfs base (.cpio.gz) en /mnt/nkr/initramfs/. \
             Copia un initramfs existente (pg.cpio.gz, odoo.cpio.gz) ahí primero."
                .into(),
        );
    }

    Ok(base)
}

/// Genera un initramfs .cpio.gz para un nombre de servicio dado.
///
/// - `name`: nombre del servicio (ej. "pgbouncer", "redis")
/// - `docker_cmd`: comando completo Docker (ENTRYPOINT + CMD combinados)
///
/// Retorna la ruta al .cpio.gz generado en /mnt/nkr/initramfs/<name>.cpio.gz
pub fn generate_initramfs(name: &str, disk_path: &str, docker_cmd: Option<&[String]>) -> Result<String, Box<dyn Error>> {
    let data_dir = nkr_data_dir();
    let initramfs_dir = data_dir.join("initramfs");
    fs::create_dir_all(&initramfs_dir)?;

    let output_path = initramfs_dir.join(format!("{}.cpio.gz", name));

    eprintln!(
        "[NKR-INITRAMFS] Generando initramfs para '{}' → {}",
        name,
        output_path.display()
    );

    // 1. Asegurar que la base existe
    let base = ensure_base_initramfs()?;

    // 2. Crear directorio temporal de trabajo
    let work_dir = format!("/tmp/nkr_initramfs_gen_{}", std::process::id());
    let work = Path::new(&work_dir);
    if work.exists() {
        fs::remove_dir_all(work)?;
    }
    fs::create_dir_all(work)?;

    // 3. Copiar base al directorio de trabajo
    let cp_status = Command::new("cp")
        .args(["-a", &format!("{}/.", base.display().to_string()), &work_dir])
        .status()?;
    if !cp_status.success() {
        let _ = fs::remove_dir_all(work);
        return Err("Fallo copiando base initramfs".into());
    }

    // 4. Inspeccionar el disco para detectar entrypoints y shell disponible
    let mount_dir = format!("/tmp/nkr_inspect_gen_{}", std::process::id());
    fs::create_dir_all(&mount_dir)?;

    let mut detected_entrypoint: Option<String> = None;

    let mount_ok = Command::new("mount")
        .args(["-o", "loop,ro", disk_path, &mount_dir])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    if mount_ok {
        // Detectar entrypoints
        for ep in &[
            "entrypoint.sh",
            "docker-entrypoint.sh",
            "usr/local/bin/docker-entrypoint.sh",
            "run.sh",
            "start.sh",
        ] {
            let full = Path::new(&mount_dir).join(ep);
            if full.exists() {
                detected_entrypoint = Some(format!("/{}", ep));
                break;
            }
        }

        let _ = Command::new("umount").arg(&mount_dir).status();
    }
    let _ = fs::remove_dir(&mount_dir);

    // 5. Generar init personalizado
    //    Prioridad: docker_cmd (completo) > entrypoint detectado en disco > genérico
    let init_content = if let Some(cmd) = docker_cmd {
        if !cmd.is_empty() {
            let full_cmd = cmd.iter()
                .map(|s| {
                    if s.contains(' ') || s.contains('"') { format!("'{}'", s) } else { s.clone() }
                })
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

    // chmod +x init
    Command::new("chmod")
        .args(["+x", &init_path.to_string_lossy()])
        .status()?;

    // 6. Crear directorios necesarios
    for dir in &["dev", "etc", "newroot", "proc", "run", "sys"] {
        fs::create_dir_all(work.join(dir))?;
    }

    // 7. Empaquetar como cpio.gz
    eprintln!("[NKR-INITRAMFS] Empaquetando {}...", output_path.display());
    let pack_status = Command::new("sh")
        .args([
            "-c",
            &format!(
                "cd '{}' && find . | cpio -o -H newc 2>/dev/null | gzip > '{}'",
                work_dir,
                output_path.display()
            ),
        ])
        .status()?;

    // Limpiar
    let _ = fs::remove_dir_all(work);

    if !pack_status.success() {
        return Err("Fallo empaquetando initramfs cpio.gz".into());
    }

    let size = fs::metadata(&output_path)
        .map(|m| m.len() / 1024)
        .unwrap_or(0);
    eprintln!(
        "[NKR-INITRAMFS] ✅ {} generado ({} KB)",
        output_path.display(),
        size
    );

    Ok(output_path.to_string_lossy().to_string())
}

/// Genera un init script personalizado con comando completo (entrypoint + cmd)
fn generate_init_script(name: &str, full_command: &str) -> String {
    let label = name.to_uppercase();
    format!(
        r#"#!/bin/sh
export PATH=/bin:/sbin
mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devtmpfs devtmpfs /dev
exec > /dev/ttyS0 2>&1

echo "[NKR-{label}] init started"
echo /sbin/mdev > /proc/sys/kernel/hotplug
mdev -s

M="/lib/modules/6.6.117-0-virt"
for mod in crc32c_generic.ko crc32c-intel.ko libcrc32c.ko crc16.ko mbcache.ko jbd2.ko ext4.ko virtio_blk.ko failover.ko net_failover.ko virtio_net.ko; do
    [ -f "$M/$mod" ] && insmod "$M/$mod" 2>/dev/null
done
echo "[NKR-{label}] modules loaded"

sleep 2
mdev -s

GUEST_IP=""
for param in $(cat /proc/cmdline); do
    case "$param" in nkr.ip=*) GUEST_IP="${{param#nkr.ip=}}" ;; esac
done
[ -z "$GUEST_IP" ] && GUEST_IP="10.0.0.2"

if [ -d /sys/class/net/eth0 ]; then
    ip link set eth0 up
    ip addr add ${{GUEST_IP}}/24 dev eth0
    ip route add default via 10.0.0.1
    echo "[NKR-{label}] eth0: ${{GUEST_IP}}/24"
fi

mkdir -p /newroot
mount /dev/vda /newroot
echo "[NKR-{label}] rootfs mounted"

mount -o bind /proc /newroot/proc
mount -o bind /sys /newroot/sys
mount -o bind /dev /newroot/dev
chmod 666 /newroot/dev/null /newroot/dev/random /newroot/dev/urandom 2>/dev/null
ln -sf /proc/self/fd /newroot/dev/fd
ln -sf fd/0 /newroot/dev/stdin
ln -sf fd/1 /newroot/dev/stdout
ln -sf fd/2 /newroot/dev/stderr

echo "[NKR-{label}] Starting {name}..."

# Escribir wrapper que carga env y ejecuta el comando
cat > /newroot/nkr-start.sh << 'NKREOF'
#!/bin/sh
[ -f /etc/nkr-env ] && . /etc/nkr-env
export PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
# Limpiar configs generados para que entrypoints los regeneren con env vars frescos
if [ -n "$NKR_CLEAN" ]; then
    for f in $NKR_CLEAN; do rm -f "$f"; done
fi
exec {full_command}
NKREOF
chmod +x /newroot/nkr-start.sh

chroot /newroot /nkr-start.sh > /dev/ttyS0 2>&1
echo "[NKR-{label}] Process exited. Freezing."
while true; do sleep 3600; done
"#,
        label = label,
        name = name,
        full_command = full_command,
    )
}
