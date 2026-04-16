// =============================================================================
// NKR VMM — Motor de Micro-VMs con acceso directo al hardware
// =============================================================================

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::convert::TryInto;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::path::Path;
use libc;

/// Flag global para shutdown limpio (SIGTERM → vcpu loop sale → extract_volumes)
pub static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);
/// 0=idle, 1=shutdown inyectado (esperando VcpuExit::Shutdown o timeout)
use std::sync::atomic::AtomicU8;
static SHUTDOWN_PHASE: AtomicU8 = AtomicU8::new(0);
/// Tiempo (ms desde UNIX_EPOCH) al que se marcó SHUTDOWN_REQUESTED por primera vez
use std::sync::atomic::AtomicU64;
use std::time::{SystemTime, UNIX_EPOCH};
static SHUTDOWN_STARTED_MS: AtomicU64 = AtomicU64::new(0);
/// Path del VirtIO-FS rootfs del host — para escribir el sentinel de shutdown
static SHUTDOWN_ROOTFS_PATH: std::sync::OnceLock<String> = std::sync::OnceLock::new();

use kvm_bindings::{kvm_segment, kvm_userspace_memory_region, kvm_cpuid_entry2, CpuId, KVM_MAX_CPUID_ENTRIES};
use kvm_ioctls::{Kvm, VcpuExit, VcpuFd, VmFd, IoEventAddress};
use linux_loader::loader::bzimage::BzImage;
use linux_loader::loader::elf::Elf;
use linux_loader::loader::KernelLoader;
use vm_memory::{Address, Bytes, FileOffset, GuestAddress, GuestMemory, GuestMemoryMmap, GuestMemoryRegion};
use std::os::unix::io::FromRawFd;
use virtio_queue::QueueT;

use crate::block::VirtioBlockDevice;
use crate::net::VirtioNetDevice;

use crate::pmem::{VirtioPmemDevice, PMEM_GUEST_PHYS_ADDR, PMEM_MMIO_ADDR, PMEM_IRQ};
use crate::balloon::{VirtioBalloonDevice, BALLOON_MMIO_ADDR, BALLOON_IRQ, BALLOON_DEVICE_ID};
use crate::console::{VirtioConsoleDevice, CONSOLE_MMIO_ADDR, CONSOLE_IRQ, CONSOLE_DEVICE_ID};
use crate::virtio_fs::{VirtioFsDevice, GuestMemRegion, VIRTIO_FS_DAX_GUEST_PHYS, VIRTIO_FS_DEVICE_ID, VIRTIO_FS_BASE_IRQ};
use crate::cli::VmConfig;
use crate::state;

// Layout de Memoria Fija para Linux Boot Protocol x86_64
const COM1_PORT: u16 = 0x3F8;
const ZERO_PAGE_ADDR: u64 = 0x7000;
const CMDLINE_ADDR: u64 = 0x20000;
const KERNEL_LOAD_ADDR: u64 = 0x100000;
const INITRAMFS_ADDR: u64 = 0x0800_0000;
const PML4_ADDR: u64 = 0x9000;
const PDPT_ADDR: u64 = 0xA000;
const PD_ADDR: u64 = 0xB000;
const GDT_ADDR: u64 = 0x500;

// =============================================================================
// Bridge per-cell — Auto-setup (delegado a src/cell.rs)
// =============================================================================

/// Asegura que el bridge de la cell exista con su subnet y NAT habilitado.
/// cell_id=0 usa el bridge legacy `nkr0` (10.0.0.0/24).
/// cell_id>0 usa `nkr-br{cell_id}` (10.0.{cell_id}.0/24).
fn ensure_bridge(cell_id: u8) -> Result<(), Box<dyn std::error::Error>> {
    crate::cell::ensure_cell_bridge(cell_id)?;

    // Activar KSM si no está corriendo (/sys/kernel/mm/ksm/run = 1)
    // Sin esto MADV_MERGEABLE se aplica pero el daemon KSM no fusiona nada.
    // ksm_pages_sharing en /sys/kernel/mm/ksm/ muestra cuántas páginas se fusionaron.
    let ksm_run = std::fs::read_to_string("/sys/kernel/mm/ksm/run")
        .unwrap_or_default();
    if ksm_run.trim() != "1" {
        if let Err(e) = std::fs::write("/sys/kernel/mm/ksm/run", "1") {
            eprintln!("[NKR] WARN: no se pudo activar KSM: {e} (continúa sin KSM)");
        } else {
            // Escaneo agresivo: útil al arrancar muchas VMs en ráfaga
            let _ = std::fs::write("/sys/kernel/mm/ksm/pages_to_scan", "1000");
            eprintln!("[NKR] KSM activado (pages_to_scan=1000)");
        }
    }

    Ok(())
}

// =============================================================================
// Volúmenes — Inyección pre-boot y extracción post-shutdown
// =============================================================================

/// Volumen parseado con modo (ro = solo inyectar, rw = inyectar + extraer)
#[derive(Clone)]
struct VolumeMount {
    host_path: String,
    guest_path: String,
    read_write: bool,
}

/// Parsea specs de volumen: "host:guest" (ro default) o "host:guest:rw"
fn parse_volume_specs(specs: &[String]) -> Vec<VolumeMount> {
    let mut volumes = Vec::new();
    for spec in specs {
        let parts: Vec<&str> = spec.splitn(3, ':').collect();

        let (host, guest, rw) = match parts.len() {
            2 => (parts[0], parts[1], false),
            3 => (parts[0], parts[1], parts[2] == "rw"),
            _ => {
                eprintln!("[NKR-VOL] WARN: formato inválido '{}', usar host:guest[:rw]", spec);
                continue;
            }
        };

        if host.is_empty() || guest.is_empty() {
            eprintln!("[NKR-VOL] WARN: paths vacíos en '{}'", spec);
            continue;
        }
        if !guest.starts_with('/') {
            eprintln!("[NKR-VOL] WARN: guest path debe ser absoluto: '{}'", guest);
            continue;
        }
        // Para volumes rw, crear el directorio host si no existe
        if rw && !Path::new(host).exists() {
            eprintln!("[NKR-VOL] Creando directorio host para volume rw: {}", host);
            let _ = std::fs::create_dir_all(host);
        } else if !rw && !Path::new(host).exists() {
            eprintln!("[NKR-VOL] WARN: path del host no existe: '{}'", host);
            continue;
        }
        volumes.push(VolumeMount {
            host_path: host.to_string(),
            guest_path: guest.to_string(),
            read_write: rw,
        });
    }
    volumes
}

/// Limpia mounts huérfanos de procesos NKR que ya no existen.
///
/// Cuando un proceso NKR muere abruptamente (SIGKILL, crash), el directorio
/// /tmp/nkr_vol_{pid} queda montado. Si el mismo disco se monta de nuevo
/// (siguiente `nkr run`) tenemos dos mounts simultáneos → corrupción de inodos.
///
/// Este helper escanea /tmp/nkr_vol_* al arrancar y limpia los huérfanos.
fn cleanup_orphaned_mounts() {
    let tmp = std::path::Path::new("/tmp");
    let entries = match std::fs::read_dir(tmp) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };

        // Sólo directorios con patrón nkr_vol_{número} o nkr_inspect_gen_{número}
        let pid_str = if let Some(s) = name.strip_prefix("nkr_vol_") {
            s
        } else if let Some(s) = name.strip_prefix("nkr_inspect_gen_") {
            s
        } else if let Some(s) = name.strip_prefix("nkr_rootfs_") {
            s
        } else {
            continue;
        };

        let pid: u32 = match pid_str.parse() {
            Ok(p) => p,
            Err(_) => continue,
        };

        // Si el proceso sigue vivo, no tocar
        let alive = unsafe { libc::kill(pid as i32, 0) == 0 };
        if alive {
            continue;
        }

        // Proceso muerto → desmontar y borrar
        eprintln!("[NKR-VOL] Limpiando mount huérfano de PID {} muerto: {}", pid, path.display());

        // Intentar umount (puede fallar si ya no está montado)
        let _ = std::process::Command::new("umount")
            .args(["--lazy", &path.to_string_lossy()])
            .status();

        // Borrar directorio (puede no estar vacío si umount lazy todavía no detached)
        let _ = std::fs::remove_dir(&path);
    }
}

/// Monta el disco root y ejecuta una operación sobre el punto de montaje.
/// Abstrae mount/umount para reutilizar en inject y extract.
fn with_mounted_disk<F>(root_disk: &str, operation: F) -> Result<(), Box<dyn std::error::Error>>
where
    F: FnOnce(&str) -> Result<(), Box<dyn std::error::Error>>,
{
    let mount_dir = format!("/tmp/nkr_vol_{}", std::process::id());
    std::fs::create_dir_all(&mount_dir)?;

    let mount_status = std::process::Command::new("mount")
        .args(["-o", "loop", root_disk, &mount_dir])
        .status()
        .map_err(|e| format!("Fallo mount: {e}"))?;

    if !mount_status.success() {
        let _ = std::fs::remove_dir(&mount_dir);
        return Err("No se pudo montar el disco (¿sudo?)".into());
    }

    let result = operation(&mount_dir);

    // Siempre desmontar, incluso si la operación falló
    let _ = std::process::Command::new("umount").arg(&mount_dir).status();
    let _ = std::fs::remove_dir(&mount_dir);

    result
}

/// Inyecta volúmenes en el disco root ANTES del boot.
fn inject_volumes(root_disk: &str, volumes: &[VolumeMount]) -> Result<(), Box<dyn std::error::Error>> {
    if volumes.is_empty() {
        return Ok(());
    }

    eprintln!("[NKR-VOL] Inyectando {} volumen(es) en {}...", volumes.len(), root_disk);

    let vols = volumes.to_vec();
    with_mounted_disk(root_disk, |mount_dir| {
        for vol in &vols {
            let target = format!("{}{}", mount_dir, vol.guest_path);
            let host = Path::new(&vol.host_path);
            let mode_tag = if vol.read_write { "rw" } else { "ro" };

            if host.is_file() {
                if let Some(parent) = Path::new(&target).parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                match std::fs::copy(&vol.host_path, &target) {
                    Ok(bytes) => eprintln!("[NKR-VOL] {} → {} ({} bytes, {})", vol.host_path, vol.guest_path, bytes, mode_tag),
                    Err(e) => eprintln!("[NKR-VOL] ERROR: {} → {}: {}", vol.host_path, vol.guest_path, e),
                }
            } else if host.is_dir() {
                let _ = std::fs::create_dir_all(&target);
                // Si el directorio host tiene contenido, copiarlo al guest
                let has_content = std::fs::read_dir(&vol.host_path)
                    .map(|mut d| d.next().is_some())
                    .unwrap_or(false);
                if has_content {
                    let status = std::process::Command::new("cp")
                        .args(["-a", &format!("{}/.", vol.host_path), &target])
                        .status();
                    match status {
                        Ok(s) if s.success() => eprintln!("[NKR-VOL] {}/ → {}/ (dir, {})", vol.host_path, vol.guest_path, mode_tag),
                        _ => eprintln!("[NKR-VOL] ERROR dir: {} → {}", vol.host_path, vol.guest_path),
                    }
                } else {
                    eprintln!("[NKR-VOL] {}/ → {}/ (dir vacío, primera ejecución, {})", vol.host_path, vol.guest_path, mode_tag);
                }
            }
        }
        eprintln!("[NKR-VOL] Inyección completada");
        Ok(())
    })
}

/// Extrae volúmenes rw del disco root DESPUÉS del shutdown.
/// Solo extrae los marcados como :rw
fn extract_volumes(root_disk: &str, volumes: &[VolumeMount]) -> Result<(), Box<dyn std::error::Error>> {
    let rw_vols: Vec<&VolumeMount> = volumes.iter().filter(|v| v.read_write).collect();
    if rw_vols.is_empty() {
        return Ok(());
    }

    eprintln!("[NKR-VOL] Extrayendo {} volumen(es) rw de {}...", rw_vols.len(), root_disk);

    let vols: Vec<VolumeMount> = rw_vols.iter().map(|v| (*v).clone()).collect();
    with_mounted_disk(root_disk, |mount_dir| {
        for vol in &vols {
            let source = format!("{}{}", mount_dir, vol.guest_path);

            if !Path::new(&source).exists() {
                eprintln!("[NKR-VOL] WARN: {} no existe en el disco (nada que extraer)", vol.guest_path);
                continue;
            }

            // Asegurar que el directorio host existe
            if Path::new(&source).is_dir() {
                let _ = std::fs::create_dir_all(&vol.host_path);
                // rsync-like: cp -a preservando permisos
                let status = std::process::Command::new("cp")
                    .args(["-a", &format!("{}/.", source), &format!("{}/.", vol.host_path)])
                    .status();
                match status {
                    Ok(s) if s.success() => {
                        eprintln!("[NKR-VOL] ← {}/ → {}/ (extraído)", vol.guest_path, vol.host_path);
                        // Hacer accesible al usuario del host (los UIDs del guest no existen aquí)
                        let _ = std::process::Command::new("chmod")
                            .args(["-R", "u+rwX,g+rX,o+rX", &vol.host_path])
                            .status();
                    }
                    _ => eprintln!("[NKR-VOL] ERROR extrayendo {} → {}", vol.guest_path, vol.host_path),
                }
            } else if Path::new(&source).is_file() {
                if let Some(parent) = Path::new(&vol.host_path).parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                match std::fs::copy(&source, &vol.host_path) {
                    Ok(bytes) => eprintln!("[NKR-VOL] ← {} → {} ({} bytes, extraído)", vol.guest_path, vol.host_path, bytes),
                    Err(e) => eprintln!("[NKR-VOL] ERROR extrayendo {} → {}: {}", vol.guest_path, vol.host_path, e),
                }
            }
        }
        eprintln!("[NKR-VOL] Extracción completada");
        Ok(())
    })
}

/// Inyecta variables de entorno como /etc/nkr-env en el disco root.
/// El initramfs hace `source /etc/nkr-env` antes de lanzar el servicio.
fn inject_env_vars(root_disk: &str, env_vars: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    if env_vars.is_empty() {
        return Ok(());
    }

    eprintln!("[NKR-ENV] Inyectando {} variable(s) de entorno en {}...", env_vars.len(), root_disk);

    let vars = env_vars.to_vec();
    with_mounted_disk(root_disk, |mount_dir| {
        let env_path = format!("{}/etc/nkr-env", mount_dir);
        // Asegurar /etc existe
        let _ = std::fs::create_dir_all(format!("{}/etc", mount_dir));

        let mut content = String::from("# NKR environment variables (auto-generated)\n");
        for var in &vars {
            if let Some(eq_pos) = var.find('=') {
                let key = &var[..eq_pos];
                let val = &var[eq_pos + 1..];
                // Escapar comillas simples en el valor
                let escaped = val.replace('\'', "'\\''");
                content.push_str(&format!("export {}='{}'\n", key, escaped));
                eprintln!("[NKR-ENV] {}={}", key, val);
            } else {
                eprintln!("[NKR-ENV] WARN: formato inválido '{}', usar KEY=VALUE", var);
            }
        }

        std::fs::write(&env_path, &content)
            .map_err(|e| format!("Error escribiendo /etc/nkr-env: {}", e))?;
        eprintln!("[NKR-ENV] /etc/nkr-env escrito ({} bytes)", content.len());
        Ok(())
    })
}

/// Parsea "host_port:guest_port" y configura iptables DNAT exclusivamente sobre 127.0.0.1.
///
/// Seguridad localhost-only: los puertos mapeados NUNCA se exponen en la IP pública del host.
/// Solo procesos locales (Nginx, túneles SSH) pueden conectar vía 127.0.0.1:host_port.
/// Si una VM no tiene ports definidos, solo es accesible por su IP interna 10.0.0.x.
fn setup_port_forwarding(port_specs: &[String], guest_ip: &str) -> Vec<(u16, u16)> {
    let mut active_rules: Vec<(u16, u16)> = Vec::new();

    for spec in port_specs {
        let parts: Vec<&str> = spec.split(':').collect();
        if parts.len() != 2 {
            eprintln!("[NKR-NET] WARN: formato inválido '{}', usar host_port:guest_port", spec);
            continue;
        }

        let host_port: u16 = match parts[0].parse() {
            Ok(p) => p,
            Err(_) => { eprintln!("[NKR-NET] WARN: puerto inválido '{}'", parts[0]); continue; }
        };
        let guest_port: u16 = match parts[1].parse() {
            Ok(p) => p,
            Err(_) => { eprintln!("[NKR-NET] WARN: puerto inválido '{}'", parts[1]); continue; }
        };

        let dest = format!("{}:{}", guest_ip, guest_port);

        // DNAT en OUTPUT: procesos locales del host (Nginx, curl) que conectan a
        // 127.0.0.1:host_port son redirigidos al guest. Esto NO afecta la IP pública.
        // Requiere route_localnet=1 (ya configurado en ensure_bridge).
        let dnat_local = std::process::Command::new("iptables")
            .args(["-t", "nat", "-C", "OUTPUT",
                   "-p", "tcp", "-d", "127.0.0.1",
                   "--dport", &host_port.to_string(),
                   "-j", "DNAT", "--to-destination", &dest])
            .status()
            .and_then(|s| {
                if !s.success() {
                    std::process::Command::new("iptables")
                        .args(["-t", "nat", "-A", "OUTPUT",
                               "-p", "tcp", "-d", "127.0.0.1",
                               "--dport", &host_port.to_string(),
                               "-j", "DNAT", "--to-destination", &dest])
                        .status()
                } else {
                    Ok(s)
                }
            });

        // MASQUERADE para que el guest vea la IP del bridge como origen
        let masq = std::process::Command::new("iptables")
            .args(["-t", "nat", "-C", "POSTROUTING",
                   "-p", "tcp", "-d", guest_ip,
                   "--dport", &guest_port.to_string(),
                   "-j", "MASQUERADE"])
            .status()
            .and_then(|s| {
                if !s.success() {
                    std::process::Command::new("iptables")
                        .args(["-t", "nat", "-A", "POSTROUTING",
                               "-p", "tcp", "-d", guest_ip,
                               "--dport", &guest_port.to_string(),
                               "-j", "MASQUERADE"])
                        .status()
                } else {
                    Ok(s)
                }
            });

        match (dnat_local, masq) {
            (Ok(dl), Ok(m)) if dl.success() && m.success() => {
                eprintln!("[NKR-NET] Port forward (localhost-only): 127.0.0.1:{} → {}:{}", host_port, guest_ip, guest_port);
                active_rules.push((host_port, guest_port));
            }
            _ => {
                eprintln!("[NKR-NET] WARN: fallo al configurar port forward {}:{}", host_port, guest_port);
            }
        }
    }

    active_rules
}

/// Limpia las reglas de iptables creadas para port forwarding (localhost-only).
/// También intenta eliminar reglas PREROUTING legacy (código anterior al fix de seguridad),
/// de forma best-effort, para evitar reglas huérfanas al parar VMs arrancadas con código viejo.
fn cleanup_port_forwarding(rules: &[(u16, u16)], guest_ip: &str) {
    for (host_port, guest_port) in rules {
        let dest = format!("{}:{}", guest_ip, guest_port);

        // Borrar regla OUTPUT (modo actual localhost-only)
        let _ = std::process::Command::new("iptables")
            .args(["-t", "nat", "-D", "OUTPUT",
                   "-p", "tcp", "-d", "127.0.0.1",
                   "--dport", &host_port.to_string(),
                   "-j", "DNAT", "--to-destination", &dest])
            .status();

        // Borrar regla PREROUTING legacy (best-effort: silencioso si no existe)
        let _ = std::process::Command::new("iptables")
            .args(["-t", "nat", "-D", "PREROUTING",
                   "-p", "tcp", "--dport", &host_port.to_string(),
                   "-j", "DNAT", "--to-destination", &dest])
            .status();

        let _ = std::process::Command::new("iptables")
            .args(["-t", "nat", "-D", "POSTROUTING",
                   "-p", "tcp", "-d", guest_ip,
                   "--dport", &guest_port.to_string(),
                   "-j", "MASQUERADE"])
            .status();

        eprintln!("[NKR-NET] Port forward limpiado: 127.0.0.1:{} → {}:{}", host_port, guest_ip, guest_port);
    }
}

/// e2fsck -p sobre una imagen ext4 antes de abrirla RW. Previene montar FS corrupto
/// tras kernel panics o apagones bruscos (crítico con ^has_journal).
/// Exit codes: 0=clean, 1=auto-fixed, >=2 aborta (incluye 8=busy → split-brain).
fn fsck_ext4(path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let out = std::process::Command::new("e2fsck")
        .args(["-p", "-E", "discard", path])
        .output()
        .map_err(|e| format!("e2fsck no ejecutable para {}: {}", path, e))?;
    let code = out.status.code().unwrap_or(-1);
    match code {
        0 => Ok(()),
        1 => {
            eprintln!("[NKR-FSCK] {}: errores corregidos automáticamente", path);
            Ok(())
        }
        c => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let stdout = String::from_utf8_lossy(&out.stdout);
            let hint = if c == 8 { " (busy/split-brain u operational error)" } else if c >= 4 { " (corrupción no recuperable)" } else { "" };
            Err(format!("e2fsck falló para {} — exit={}{}\nstdout: {}\nstderr: {}",
                path, c, hint, stdout.trim(), stderr.trim()).into())
        }
    }
}

/// Monta el rootfs maestro bajo flock exclusivo. Limpia loops huérfanos del mismo archivo
/// antes de reintentar. Elimina la race de cold-start cuando N VMs arrancan en paralelo.
fn mount_master_rootfs_locked(first_disk: &str, mnt_dir: &str, lock_path: &str) -> bool {
    use std::os::unix::io::AsRawFd;
    let lock_file = match std::fs::OpenOptions::new()
        .create(true).read(true).write(true).truncate(false).open(lock_path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("[NKR] WARN: no se pudo abrir lock {}: {}", lock_path, e);
            return try_mount_master(first_disk, mnt_dir, false);
        }
    };
    unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_EX); }

    let is_mounted = std::process::Command::new("mountpoint")
        .arg("-q").arg(mnt_dir).status().map(|s| s.success()).unwrap_or(false);
    if is_mounted {
        unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_UN); }
        return true;
    }

    let ok = try_mount_master(first_disk, mnt_dir, false)
        || { cleanup_orphan_loops(first_disk); try_mount_master(first_disk, mnt_dir, true) };

    unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_UN); }
    ok
}

fn try_mount_master(first_disk: &str, mnt_dir: &str, verbose: bool) -> bool {
    let out = std::process::Command::new("mount")
        .args(["-o", "loop,ro,noload", first_disk, mnt_dir])
        .output();
    match out {
        Ok(o) if o.status.success() => true,
        Ok(o) => {
            if verbose {
                eprintln!("[NKR] mount falló tras reintento: {}", String::from_utf8_lossy(&o.stderr).trim());
            }
            false
        }
        Err(_) => false,
    }
}

fn cleanup_orphan_loops(first_disk: &str) {
    let out = match std::process::Command::new("losetup")
        .args(["-j", first_disk]).output() { Ok(o) => o, Err(_) => return };
    if !out.status.success() { return; }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let mounts = std::fs::read_to_string("/proc/mounts").unwrap_or_default();
    for line in stdout.lines() {
        if let Some(dev) = line.split(':').next() {
            let in_use = mounts.lines().any(|m| m.starts_with(&format!("{} ", dev)));
            if !in_use {
                let _ = std::process::Command::new("losetup").args(["-d", dev]).status();
                eprintln!("[NKR] Loop huérfano liberado: {} → {}", dev, first_disk);
            }
        }
    }
}

/// Ejecuta una micro-VM con la configuración dada
pub fn run(mut config: VmConfig) -> Result<(), Box<dyn std::error::Error>> {
    let ram_bytes = config.ram_mb as usize * 1024 * 1024;

    // 1b. Smart default v1.4: Convertir el disco .ext4 principal en un RootFS Maestro (VirtIO-FS) antes de procesar vars
    if config.rootfs.is_none() && !config.disks.is_empty() {
        let first_disk = config.disks[0].clone();
        if first_disk.ends_with(".ext4") {
            // "Imagen Dorada": Montaje Maestro Único
            use std::collections::hash_map::DefaultHasher;
            use std::hash::{Hash, Hasher};
            let mut hasher = DefaultHasher::new();
            std::fs::canonicalize(&first_disk).unwrap_or_else(|_| std::path::PathBuf::from(&first_disk)).hash(&mut hasher);
            let hash = hasher.finish();
            let mnt_dir = format!("/run/nkr_master_rootfs_{:x}", hash);
            let lock_path = format!("/run/nkr_master_rootfs_{:x}.lock", hash);

            std::fs::create_dir_all(&mnt_dir).unwrap_or_default();

            let success = mount_master_rootfs_locked(&first_disk, &mnt_dir, &lock_path);
            
            if success {
                eprintln!("[NKR] RootFS Maestro Compartido: {} -> {}", first_disk, mnt_dir);
                config.rootfs = Some(mnt_dir.clone());
                config.disks.remove(0); // Eliminar de la lista para que no sea manipulado como /dev/vda
            } else {
                eprintln!("[NKR] WARN: Falló el loop mount de {}, cayendo a VirtIO-Block", first_disk);
            }
        }
    }

    // fsck sobre discos RW per-VM (excluye rootfs compartido, ya removido arriba)
    for disk in &config.disks {
        if disk.ends_with(".ext4") {
            fsck_ext4(disk)?;
        }
    }

    eprintln!("╔══════════════════════════════════════════════════════════════╗");
    eprintln!("║  NKR v1.0 — Nano-Kernel Runtime                            ║");
    eprintln!("╠══════════════════════════════════════════════════════════════╣");
    eprintln!("║  RAM:    {} MB (lazy, KSM habilitado)", config.ram_mb);
    eprintln!("║  CPU:    {} chrs ({}% de core físico)", config.chrs, config.chrs * 20);
    if let Some(ref rootfs) = config.rootfs {
        eprintln!("║  RootFS: {} (VirtIO-FS compartido)", rootfs);
    }
    if !config.disks.is_empty() {
        eprintln!("║  Disco:  {}", config.disks.join(", "));
    }
    eprintln!("║  Kernel: {}", config.kernel_path);
    if let Some(ref tap) = config.tap_name {
        eprintln!("║  Red:    TAP {}", tap);
    }
    for p in &config.port_forwards {
        eprintln!("║  Puerto: {}", p);
    }
    for v in &config.volumes {
        eprintln!("║  Volume: {}", v);
    }
    for e in &config.env_vars {
        eprintln!("║  Env:    {}", e);
    }
    eprintln!("╚══════════════════════════════════════════════════════════════╝");

    // --- CPU Pinning: Asignar chrs a cores físicos (localidad NUMA) ---
    pin_cpu_chrs(config.chrs)?;

    // --- cgroupv2: CPU bursting + I/O throttling (Features 2 y 4B) ---
    setup_cgroup(&config.name, config.chrs, std::process::id(), &config.disks, config.burst);

    // --- Bridge auto-setup (per-cell) ---
    ensure_bridge(config.cell_id)?;

    // --- Limpiar mounts huérfanos de procesos NKR muertos ---
    cleanup_orphaned_mounts();

    // --- Inyectar volúmenes (pre-boot: monta disco, copia, desmonta) ---
    let parsed_volumes = parse_volume_specs(&config.volumes);
    if !parsed_volumes.is_empty() && !config.disks.is_empty() {
        inject_volumes(&config.disks[0], &parsed_volumes)?;
    }

    // --- Inyectar variables de entorno ---
    if !config.env_vars.is_empty() {
        if !config.disks.is_empty() {
            // Modo disco: inyectar en /etc/nkr-env dentro del ext4
            inject_env_vars(&config.disks[0], &config.env_vars)?;
        } else if config.rootfs.is_some() {
            // Modo rootfs compartido: escribir nkr-env al primer share dir del host
            // (el rootfs es RO, usamos un share privado como destino)
            // Saltar shares que son archivos .ext4 (van por VirtIO-BLK, no son dirs)
            let dir_share = config.shares.iter().find(|s| {
                let host = s.splitn(2, ':').next().unwrap_or("");
                std::fs::metadata(host).map(|m| m.is_dir()).unwrap_or(false)
            });
            if let Some(first_share) = dir_share {
                let parts: Vec<&str> = first_share.splitn(2, ':').collect();
                if parts.len() >= 1 {
                    let host_dir = parts[0];
                    let env_path = format!("{}/nkr-env", host_dir);
                    let mut content = String::from("# NKR environment variables (auto-generated)\n");
                    for var in &config.env_vars {
                        if let Some(eq_pos) = var.find('=') {
                            let key = &var[..eq_pos];
                            let val = &var[eq_pos + 1..];
                            let escaped = val.replace('\'', "'\\''");
                            content.push_str(&format!("export {}='{}'\n", key, escaped));
                        }
                    }
                    if let Err(e) = std::fs::write(&env_path, &content) {
                        eprintln!("[NKR-ENV] WARN: no se pudo escribir {}: {}", env_path, e);
                    } else {
                        eprintln!("[NKR-ENV] Env vars escritas en {} ({} bytes)", env_path, content.len());
                    }
                }
            }
        }
    }

    let kvm = Kvm::new().map_err(|e| format!("Fallo al abrir /dev/kvm: {e}"))?;
    let vm = kvm.create_vm().map_err(|e| format!("Fallo KVM_CREATE_VM: {e}"))?;

    // Placa base virtual: Interrupciones y Reloj
    vm.create_irq_chip().map_err(|e| format!("Fallo al crear IRQ chip: {e}"))?;
    let pit_config = kvm_bindings::kvm_pit_config { flags: 0, ..Default::default() };
    vm.create_pit2(pit_config).map_err(|e| format!("Fallo al crear PIT: {e}"))?;

    // 1. Inicializar la RAM (lazy allocation)
    //
    // vm-memory usa MAP_ANONYMOUS|MAP_NORESERVE internamente: las páginas físicas
    // solo se asignan cuando el guest las toca (EPT fault → host page fault → alloc).
    // Con MAP_NORESERVE, 50 VMs × 256 MB no consumen 12 GB de commit limit.
    //
    // Después de crear las regiones aplicamos dos madvise adicionales:
    //   MADV_MERGEABLE → KSM (Kernel Same-page Merging): páginas idénticas entre VMs
    //                    (kernel, Python bytecode, páginas cero) se fusionan en una
    //                    sola copia física. Ahorro típico: 60-120 MB por VM idle.
    //   MADV_NOHUGEPAGE → evita THP (Transparent HugePages): un THP de 2MB solo puede
    //                     liberarse si las 512 subpáginas están frías simultáneamente.
    //                     Con 4KB granulares, el kernel puede desalojar página a página.
    let high_mem = ram_bytes - 0x100000;

    // Guest memory respaldada por memfd (sharable con virtiofsd vía vhost-user SET_MEM_TABLE)
    // memfd_create(SYS=319): crea un fd anónimo de tamaño ram_bytes.
    // Las dos regiones (0..0xA0000 y 0x100000..end) usan offsets GPA-alineados en el memfd.
    let memfd = unsafe {
        libc::syscall(319i64 /*SYS_memfd_create*/, b"nkr_guest\0".as_ptr() as i64, 0i64) as i32
    };
    if memfd < 0 {
        return Err(format!("memfd_create falló: {}", std::io::Error::last_os_error()).into());
    }
    if unsafe { libc::ftruncate(memfd, ram_bytes as i64) } < 0 {
        return Err(format!("ftruncate memfd falló: {}", std::io::Error::last_os_error()).into());
    }
    let file0 = unsafe { File::from_raw_fd(libc::dup(memfd)) };
    let file1 = unsafe { File::from_raw_fd(libc::dup(memfd)) };
    let guest_mem = Arc::new(GuestMemoryMmap::<()>::from_ranges_with_files(&[
        (GuestAddress(0),        0xA0000,   Some(FileOffset::new(file0, 0))),
        (GuestAddress(0x100000), high_mem,  Some(FileOffset::new(file1, 0x100000))),
    ]).map_err(|e| format!("Fallo al crear guest memory: {e}"))?);

    // Construir tabla de regiones para vhost-user SET_MEM_TABLE
    let mem_regions: Vec<GuestMemRegion> = guest_mem.iter().map(|r| GuestMemRegion {
        gpa: r.start_addr().0,
        size: r.size(),
        hva: r.as_ptr() as u64,
        memfd_offset: r.start_addr().0, // offset en memfd = GPA (por diseño)
    }).collect();

    // Aplicar hints de lazy/KSM a cada región de guest RAM
    for region in guest_mem.iter() {
        let ptr = region.as_ptr() as *mut libc::c_void;
        let len = region.size();
        unsafe {
            // KSM: fusionar páginas idénticas entre VMs (solo páginas anónimas)
            libc::madvise(ptr, len, libc::MADV_MERGEABLE);
            // Sin THP: páginas 4KB → desalojo granular bajo presión de memoria
            libc::madvise(ptr, len, libc::MADV_NOHUGEPAGE);
        }
    }

    // 2. Discos Virtio (Múltiples volúmenes)
    let mut block_devs = Vec::new();
    let mut block_configs = Vec::new(); // Para generar el cmdline
    let base_block_irq = 6;
    let base_block_addr = 0xD000_1000;

    for (i, disk_path) in config.disks.iter().enumerate() {
        let dev = VirtioBlockDevice::new(disk_path, guest_mem.clone());
        let irq = base_block_irq + i as u32;
        let addr = base_block_addr + (i as u64 * 0x1000);
        
        eprintln!("[NKR] Disco {}: {} ({} MB) [MMIO {:#X}, IRQ {}]", 
            i, disk_path, dev.capacity_sectors * 512 / (1024 * 1024), addr, irq);
            
        vm.register_irqfd(&dev.irqfd, irq).unwrap_or_else(|e| panic!("Fallo irqfd block {}: {}", i, e));
        vm.register_ioevent(&dev.ioeventfd, &IoEventAddress::Mmio(addr + 0x50), 0u64)
            .unwrap_or_else(|e| panic!("Fallo ioeventfd block {}: {}", i, e));
            
        block_configs.push((addr, irq));
        block_devs.push(dev);
    }

    // 2b. Dispositivos de compartición — VirtIO-FS via virtiofsd (vhost-user)
    // MMIO base: 0xD0010000, IRQ: 8+i
    let mut fs_devs: Vec<VirtioFsDevice> = Vec::new();
    let mut fs_shares: Vec<(String, String, bool, usize)> = Vec::new();  // VirtIO-FS: (tag, guest_path, readonly, slot)
    let mut blk_share_mounts: Vec<(String, String)> = Vec::new(); // BLK: (dev_path, guest_mnt)
    let base_share_addr: u64 = 0xD001_0000;
    let base_share_irq: u32  = VIRTIO_FS_BASE_IRQ;

    std::fs::create_dir_all("/run/nkrfs").ok();

    // 2b-pre. VirtIO-FS rootfs compartido (si config.rootfs está definido)
    // El rootfs se monta como / (RO) en el guest vía VirtIO-FS con cache=auto.
    // Esto permite que 100+ VMs compartan el mismo código con 1 copia en page cache.
    let rootfs_slot_count: usize;
    if let Some(ref rootfs_path) = config.rootfs {
        let tag = format!("nkrfs{}s0", config.vm_id); // Único por VM para evitar conflicto de sockets
        let irq = base_share_irq;
        let addr = base_share_addr;

        let regions: Vec<GuestMemRegion> = mem_regions.iter().map(|r| GuestMemRegion {
            gpa: r.gpa, size: r.size, hva: r.hva, memfd_offset: r.memfd_offset,
        }).collect();

        let rootfs_abs = std::fs::canonicalize(rootfs_path)
            .unwrap_or_else(|_| panic!("Rootfs path inválido: {}", rootfs_path))
            .to_string_lossy().into_owned();

        // Registrar el path para que el handler de shutdown pueda escribir el sentinel
        let _ = SHUTDOWN_ROOTFS_PATH.set(rootfs_abs.clone());

        let mut fs_dev = VirtioFsDevice::new(
            &tag, &rootfs_abs, guest_mem.clone(),
            unsafe { libc::dup(memfd) },
            regions,
            "auto", // rootfs compartido: host page cache shared entre VMs
            256 * 1024 * 1024, // 256 MB DAX — solo binarios RO
            false, // writeback=false — rootfs es RO
        );

        if fs_dev.is_connected() {
            eprintln!("[NKR] RootFS: '{}' → guest:'/' vía VirtIO-FS [MMIO {:#X}, IRQ {}] (cache=auto, compartido)",
                rootfs_path, addr, irq);

            fs_dev.mmio_addr = addr;
            if fs_dev.dax_enabled {
                fs_dev.dax_guest_phys = VIRTIO_FS_DAX_GUEST_PHYS; // Slot 0
                let dax_region = kvm_bindings::kvm_userspace_memory_region {
                    slot: 3,
                    flags: 0,
                    guest_phys_addr: fs_dev.dax_guest_phys,
                    memory_size: fs_dev.dax_size as u64,
                    userspace_addr: fs_dev.dax_ptr as u64,
                };
                match unsafe { vm.set_user_memory_region(dax_region) } {
                    Ok(_) => eprintln!("[NKR] RootFS DAX: {} MB en KVM slot 3", fs_dev.dax_size >> 20),
                    Err(e) => eprintln!("[NKR] WARN: RootFS DAX KVM slot falló: {}", e),
                }
            }

            vm.register_irqfd(&fs_dev.call, irq)
                .unwrap_or_else(|e| panic!("Fallo irqfd virtio-fs rootfs: {}", e));
            vm.register_ioevent(&fs_dev.kicks[0], &IoEventAddress::Mmio(addr + 0x50), 0u64)
                .unwrap_or_else(|e| panic!("Fallo kick0 virtio-fs rootfs: {}", e));
            vm.register_ioevent(&fs_dev.kicks[1], &IoEventAddress::Mmio(addr + 0x50), 1u64)
                .unwrap_or_else(|e| panic!("Fallo kick1 virtio-fs rootfs: {}", e));

            fs_shares.push((tag, "/".to_string(), true, 0)); // rootfs siempre RO, slot 0
            fs_devs.push(fs_dev);
            rootfs_slot_count = 1;
        } else {
            eprintln!("[NKR] ERROR: VirtIO-FS para rootfs '{}' no conectó — no hay fallback para rootfs", rootfs_path);
            return Err("virtiofsd no pudo arrancar para rootfs compartido".into());
        }
    } else {
        rootfs_slot_count = 0;
    }

    for (i, share_spec) in config.shares.iter().enumerate() {
        let slot = i + rootfs_slot_count; // offset por el rootfs device si existe
        // Formato: host_path:guest_path[:ro|:rw]  (default: rw)
        let parts: Vec<&str> = share_spec.splitn(3, ':').collect();
        if parts.len() < 2 {
            eprintln!("[NKR] WARN: --share '{}' inválido — usar host_path:guest_path[:ro]", share_spec);
            continue;
        }
        let readonly = parts.get(2).map_or(false, |m| *m == "ro");
        let host_abs = std::fs::canonicalize(parts[0])
            .unwrap_or_else(|_| panic!("Ruta compartida inválida o no existe: {}", parts[0]))
            .to_string_lossy().into_owned();
        let guest_path = parts[1];
        let tag = format!("nkrfs{}s{}", config.vm_id, slot);
        let irq  = base_share_irq + slot as u32;
        let addr = base_share_addr + (slot as u64 * 0x1000);

        // Auto-detección: archivo .ext4 → VirtIO-BLK (sin virtiofsd, sin DAX)
        //                 directorio    → VirtIO-FS  (comportamiento actual)
        let is_blk_file = std::fs::metadata(&host_abs)
            .map(|m| m.is_file()).unwrap_or(false);

        if is_blk_file {
            fsck_ext4(&host_abs)?;
            let blk_idx = block_devs.len();
            let blk_irq  = base_block_irq + blk_idx as u32;
            let blk_addr = base_block_addr + (blk_idx as u64 * 0x1000);
            let dev = VirtioBlockDevice::new(&host_abs, guest_mem.clone());
            let dev_name = format!("/dev/vd{}", char::from(b'a' + blk_idx as u8));
            eprintln!("[NKR] FS[{}] (BLK): '{}' → guest:'{}' via virtio-blk {} [MMIO {:#X}, IRQ {}] ({} MB)",
                slot, host_abs, guest_path, dev_name, blk_addr, blk_irq,
                dev.capacity_sectors * 512 / (1024 * 1024));
            vm.register_irqfd(&dev.irqfd, blk_irq)
                .unwrap_or_else(|e| panic!("Fallo irqfd blk-share {}: {}", slot, e));
            vm.register_ioevent(&dev.ioeventfd, &IoEventAddress::Mmio(blk_addr + 0x50), 0u64)
                .unwrap_or_else(|e| panic!("Fallo ioeventfd blk-share {}: {}", slot, e));
            block_configs.push((blk_addr, blk_irq));
            blk_share_mounts.push((dev_name, guest_path.to_string()));
            block_devs.push(dev);
            continue;
        }

        // Clonar mem_regions para este dispositivo (VirtioFsDevice toma ownership)
        let regions: Vec<GuestMemRegion> = mem_regions.iter().map(|r| GuestMemRegion {
            gpa: r.gpa, size: r.size, hva: r.hva, memfd_offset: r.memfd_offset,
        }).collect();

        // Intentar VirtIO-FS (virtiofsd se lanza automáticamente)
        let cache = if readonly { "auto" } else { "never" };
        let mut fs_dev = VirtioFsDevice::new(
            &tag, &host_abs, guest_mem.clone(),
            unsafe { libc::dup(memfd) }, // dup para que VirtioFsDevice tenga su propio fd
            regions,
            cache, // RO→auto (page cache compartido), RW→never (writes directos)
            512 * 1024 * 1024, // 512 MB DAX — buffer de datos
            !readonly, // writeback solo para shares RW
        );

        if fs_dev.is_connected() {
            eprintln!("[NKR] FS[{}]: '{}' → guest:'{}' vía VirtIO-FS [MMIO {:#X}, IRQ {}] ({})",
                slot, host_abs, guest_path, addr, irq, if readonly { "RO" } else { "RW" });

            fs_dev.mmio_addr = addr;
            // Registrar ventana DAX como KVM slot 3+slot si está activa
            if fs_dev.dax_enabled {
                // Separar ventanas DAX por slots: rootfs=256MB(slot0), datos=512MB cada uno
                // Offset fijo de 1GB por slot para evitar solapamiento con cualquier tamaño
                fs_dev.dax_guest_phys = VIRTIO_FS_DAX_GUEST_PHYS + (slot as u64 * 1024 * 1024 * 1024);
                let dax_region = kvm_bindings::kvm_userspace_memory_region {
                    slot: 3 + slot as u32,
                    flags: 0,
                    guest_phys_addr: fs_dev.dax_guest_phys,
                    memory_size: fs_dev.dax_size as u64,
                    userspace_addr: fs_dev.dax_ptr as u64,
                };
                match unsafe { vm.set_user_memory_region(dax_region) } {
                    Ok(_) => eprintln!("[NKR] FS[{}] DAX: {} MB en KVM slot {}", slot, fs_dev.dax_size >> 20, 3 + slot),
                    Err(e) => eprintln!("[NKR] WARN: FS[{}] DAX KVM slot falló: {}", slot, e),
                }
            }

            // Un irqfd para la línea de IRQ del dispositivo
            vm.register_irqfd(&fs_dev.call, irq)
                .unwrap_or_else(|e| panic!("Fallo irqfd virtio-fs {}: {}", slot, e));
            // Dos ioeventfds: uno por cola (datamatch=índice de cola)
            vm.register_ioevent(&fs_dev.kicks[0], &IoEventAddress::Mmio(addr + 0x50), 0u64)
                .unwrap_or_else(|e| panic!("Fallo kick0 virtio-fs {}: {}", slot, e));
            vm.register_ioevent(&fs_dev.kicks[1], &IoEventAddress::Mmio(addr + 0x50), 1u64)
                .unwrap_or_else(|e| panic!("Fallo kick1 virtio-fs {}: {}", slot, e));

            fs_shares.push((tag, guest_path.to_string(), readonly, slot));
            fs_devs.push(fs_dev);
        } else {
            panic!("virtiofsd falló para el volumen: '{}' → '{}'. NKR ya no soporta fallback a VirtIO-9P.",
                host_abs, guest_path);
        }
    }

    // 3. Red Virtio  — MAC e IP dinámicos según cell_id + vm_id
    let guest_ip = crate::registry::id_to_ip(config.cell_id, config.vm_id);
    let mac: [u8; 6] = [0x52, 0x54, 0x00, config.cell_id, 0x34, config.vm_id];
    let bridge_name = crate::cell::cell_bridge_name(config.cell_id);

    // Auto-crear TAP si no se especificó. Nombre incluye cell_id para evitar
    // colisiones entre cells (cell 1 vm 5 != cell 2 vm 5).
    let auto_tap_name = if config.tap_name.is_none() {
        let tap_name = if config.cell_id == 0 {
            format!("nkr-tap{}", config.vm_id)
        } else {
            format!("nkr-c{}-tap{}", config.cell_id, config.vm_id)
        };
        // Crear TAP + conectar a bridge
        let _ = std::process::Command::new("ip")
            .args(["link", "delete", &tap_name]).status();
        let status = std::process::Command::new("ip")
            .args(["tuntap", "add", "dev", &tap_name, "mode", "tap"])
            .status().map_err(|e| format!("Fallo creando TAP: {e}"))?;
        if !status.success() {
            return Err(format!("Fallo ip tuntap add {}", tap_name).into());
        }
        let _ = std::process::Command::new("ip")
            .args(["link", "set", &tap_name, "master", &bridge_name]).status();
        let _ = std::process::Command::new("ip")
            .args(["link", "set", &tap_name, "up"]).status();
        // Instalar aislamiento L2 vía ebtables (Feature 3)
        setup_tap_isolation(&tap_name, &mac, &guest_ip);
        eprintln!("[NKR] Auto-TAP: {} (bridge {})", tap_name, bridge_name);
        Some(tap_name)
    } else { None };

    let effective_tap = config.tap_name.as_deref()
        .or(auto_tap_name.as_deref());

    let mut net_dev = VirtioNetDevice::new(guest_mem.clone(), mac, effective_tap);
    eprintln!("[NKR] Red: MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} IP {}",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5], guest_ip);

    // IRQ/MMIO ya configurados en el bucle para block
    vm.register_irqfd(&net_dev.irqfd, 5).expect("Fallo irqfd net");
    vm.register_ioevent(&net_dev.ioeventfd, &IoEventAddress::Mmio(0xD0000050), 0u64)
        .expect("Fallo ioeventfd net");

    // Serial IRQ
    let serial_irqfd = vmm_sys_util::eventfd::EventFd::new(libc::EFD_NONBLOCK).expect("serial irqfd");
    vm.register_irqfd(&serial_irqfd, 4).expect("Fallo irqfd serial");

    register_guest_memory(&vm, &guest_mem)?;
    eprintln!("[NKR] RAM: {} MB mapeados", ram_bytes >> 20);

    // Feature A — VirtIO-PMEM + DAX: opcional vía config.use_pmem.
    //
    // PMEM mapea el disco del guest en el espacio de direcciones del host (mmap MAP_SHARED).
    // El guest accede al rootfs vía DAX (zero-copy, sin page cache duplicado dentro del guest).
    //
    // Trade-off de RAM en multi-tenant:
    //   - SIN pmem: VirtIO-Block usa io_uring. El guest tiene su propia page cache (~150-200 MB
    //     adicionales por VM), pero el mmap del host NO existe → sin presión de page cache host.
    //   - CON pmem: Cero page cache duplicado en el guest, pero el disco entero (ej. 6 GB)
    //     queda mapeado en el host. Con 50 VMs × 6 GB = 300 GB de file-backed mmap que
    //     compiten con la RAM anónima de los guests.
    //
    // Recomendación para ≥20 VMs: pmem=false (VirtIO-Block) + KSM activo.
    // pmem=true solo tiene sentido con pocos VMs y discos pequeños (< 2 GB).
    let mut pmem_dev_opt: Option<VirtioPmemDevice> = None;
    if config.use_pmem && !config.disks.is_empty() {
        eprintln!("[NKR] PMEM: mapeando '{}' para DAX (zero-copy rootfs)", config.disks[0]);
        let pmem_dev = VirtioPmemDevice::new(&config.disks[0], PMEM_GUEST_PHYS_ADDR, guest_mem.clone())
            .map_err(|e| format!("[NKR-PMEM] no se pudo mapear disco '{}': {e}\n\
                El disco debe estar en ext4/xfs sobre block device. Usa pmem=false para VirtIO-Block.", config.disks[0]))?;

        let region = kvm_bindings::kvm_userspace_memory_region {
            slot: 2,
            flags: 0,
            guest_phys_addr: pmem_dev.guest_phys_addr,
            memory_size: pmem_dev.host_mmap_len as u64,
            userspace_addr: pmem_dev.host_mmap_ptr as u64,
        };
        unsafe { vm.set_user_memory_region(region) }
            .map_err(|e| format!("[NKR-PMEM] FATAL: KVM slot 2 falló: {e}\n\
                Verifica que el kernel del host soporte KVM user memory regions adicionales."))?;

        vm.register_irqfd(&pmem_dev.irqfd, PMEM_IRQ)
            .unwrap_or_else(|e| eprintln!("[NKR-PMEM] irqfd: {e}"));
        vm.register_ioevent(
            &pmem_dev.ioeventfd,
            &IoEventAddress::Mmio(PMEM_MMIO_ADDR + 0x50),
            0u64,
        ).unwrap_or_else(|e| eprintln!("[NKR-PMEM] ioeventfd: {e}"));

        eprintln!("[NKR] PMEM: slot KVM 2 activo [{:#X}, IRQ {}] — root=/dev/pmem0 rootflags=dax",
            PMEM_MMIO_ADDR, PMEM_IRQ);
        pmem_dev_opt = Some(pmem_dev);
    }

    // 2c. VirtIO-Balloon (v1.3) — recuperación elástica de RAM
    // Activo si --balloon-mb > 0; el guest driver ajusta en background.
    let mut balloon_dev = VirtioBalloonDevice::new(guest_mem.clone());
    if config.balloon_mb > 0 {
        balloon_dev.set_target_mb(config.balloon_mb);
        eprintln!("[NKR] Balloon: objetivo {} MB [MMIO {:#X}, IRQ {}]",
            config.balloon_mb, BALLOON_MMIO_ADDR, BALLOON_IRQ);
    }
    vm.register_irqfd(&balloon_dev.irqfd, BALLOON_IRQ)
        .unwrap_or_else(|e| eprintln!("[NKR-BALLOON] irqfd: {e}"));
    vm.register_ioevent(
        &balloon_dev.ioeventfd,
        &IoEventAddress::Mmio(BALLOON_MMIO_ADDR + 0x50),
        0u64,
    ).unwrap_or_else(|e| eprintln!("[NKR-BALLOON] ioeventfd: {e}"));


    // 3. Kernel — detectar formato automáticamente (bzImage o ELF vmlinux)
    // Smart default v1.3: si el kernel especificado apunta a "bzImage" y existe
    // un vmlinux en el almacén central, preferir vmlinux (−20 ms de arranque).
    let effective_kernel = smart_resolve_kernel(&config.kernel_path);
    let kernel_fmt = detect_kernel_format(&effective_kernel);
    let entry_addr = match kernel_fmt {
        KernelFormat::BzImage => load_bzimage_kernel(&guest_mem, &effective_kernel)?,
        KernelFormat::Elf     => load_elf_kernel(&guest_mem, &effective_kernel)?,
    };

    // 4. Initramfs (auto-detectar o usar especificado)
    let initrd_size = load_initramfs_auto(&guest_mem, &config.initramfs_path, ram_bytes)?;

    // 5. Boot protocol
    let rootfs_tag = if config.rootfs.is_some() {
        Some(format!("nkrfs{}s0", config.vm_id))
    } else {
        None
    };
    configure_linux_boot(&guest_mem, initrd_size, ram_bytes, &guest_ip, &block_configs, &fs_shares, &blk_share_mounts, pmem_dev_opt.as_ref(), config.balloon_mb > 0, rootfs_tag.as_deref())?;
    write_page_tables(&guest_mem)?;
    write_gdt(&guest_mem)?;

    // Readback E820 para verificar que el kernel recibe los valores correctos
    {
        let cnt: u8 = guest_mem.read_obj(GuestAddress(ZERO_PAGE_ADDR + 0x1E8)).unwrap_or(0xff);
        eprintln!("[NKR-E820-CHK] e820_entries={}", cnt);
        for i in 0..cnt.min(4) as u64 {
            let base = ZERO_PAGE_ADDR + 0x2D0 + i * 20;
            let addr: u64 = guest_mem.read_obj(GuestAddress(base)).unwrap_or(0xdead);
            let size: u64 = guest_mem.read_obj(GuestAddress(base + 8)).unwrap_or(0xdead);
            let t: u32   = guest_mem.read_obj(GuestAddress(base + 16)).unwrap_or(0xdead);
            eprintln!("[NKR-E820-CHK]   [{}] addr=0x{:x} size=0x{:x} type={}", i, addr, size, t);
        }
    }

    // 6. vCPU
    let mut vcpu = vm.create_vcpu(0).map_err(|e| format!("Fallo KVM_CREATE_VCPU: {e}"))?;
    // Configurar CPUID con firma KVM para activar kvmclock (in-kernel en KVM)
    let supported = kvm.get_supported_cpuid(KVM_MAX_CPUID_ENTRIES)?;
    let mut entries: Vec<kvm_cpuid_entry2> = supported.as_slice().to_vec();
    let mut has_kvm0 = false;
    let mut has_kvm1 = false;
    for e in entries.iter_mut() {
        match e.function {
            1            => { e.ecx |= 1u32 << 31; } // hypervisor present bit
            0x40000000   => {
                has_kvm0 = true;
                e.eax = 0x40000001;
                e.ebx = 0x4b4d564b; // "KVMK"
                e.ecx = 0x564b4d56; // "VMKV"
                e.edx = 0x0000004d; // "M\0\0\0"  → "KVMKVMKVM\0\0\0"
            }
            0x40000001   => {
                has_kvm1 = true;
                // KVM_FEATURE_CLOCKSOURCE(0) | CLOCKSOURCE2(3) | CLOCKSOURCE_STABLE(24)
                e.eax = (1u32 << 0) | (1u32 << 3) | (1u32 << 24);
                e.ebx = 0; e.ecx = 0; e.edx = 0;
            }
            _ => {}
        }
    }
    if !has_kvm0 {
        entries.push(kvm_cpuid_entry2 {
            function: 0x40000000, index: 0, flags: 0,
            eax: 0x40000001, ebx: 0x4b4d564b, ecx: 0x564b4d56, edx: 0x0000004d,
            padding: [0; 3],
        });
    }
    if !has_kvm1 {
        entries.push(kvm_cpuid_entry2 {
            function: 0x40000001, index: 0, flags: 0,
            eax: (1u32 << 0) | (1u32 << 3) | (1u32 << 24),
            ebx: 0, ecx: 0, edx: 0, padding: [0; 3],
        });
    }
    let n = entries.len();
    let mut cpuid = CpuId::new(n)
        .map_err(|_| "CpuId::new falló".to_string())?;
    cpuid.as_mut_slice().copy_from_slice(&entries);
    vcpu.set_cpuid2(&cpuid)?;
    // bzImage arranca en modo protegido 32-bit; ELF vmlinux en modo largo 64-bit
    match kernel_fmt {
        KernelFormat::BzImage => configure_sregs(&vcpu)?,
        KernelFormat::Elf     => configure_sregs_64(&vcpu)?,
    }
    configure_regs(&vcpu, entry_addr)?;

    eprintln!("[NKR] vCPU lista — RIP={entry_addr:#X}");

    // Port forwarding
    let forwarding_rules = setup_port_forwarding(&config.port_forwards, &guest_ip);

    // Registrar VM en el estado global
    let effective_tap_str = config.tap_name.as_deref()
        .or(auto_tap_name.as_deref())
        .unwrap_or("none")
        .to_string();

    let state_disks: Vec<String> = config.disks.iter()
        .map(|disk| {
            let p = Path::new(disk);
            p.canonicalize()
                .unwrap_or_else(|_| p.to_path_buf())
                .to_string_lossy()
                .to_string()
        })
        .collect();

    let vm_state = state::VmState {
        vm_id: config.vm_id,
        hash: config.hash.clone(),
        name: config.name.clone(),
        pid: std::process::id(),
        ram_mb: config.ram_mb,
        chrs: config.chrs,
        disks: state_disks,
        guest_ip: guest_ip.clone(),
        ports: config.port_forwards.clone(),
        tap_name: effective_tap_str,
        started_at: state::current_timestamp(),
        use_pmem: pmem_dev_opt.is_some(), // true si PMEM se activó (Smart Default o explícito)
        balloon_mb: config.balloon_mb,
        cell_id: config.cell_id,
    };
    if let Err(e) = state::register_vm(&vm_state) {
        eprintln!("[NKR] ERROR: No se pudo registrar VM en estado — 'nkr ps' no mostrará esta VM: {e}");
    }

    eprintln!("════════════════════════════════════════════════════════════════");

    // Registrar SIGTERM handler para shutdown limpio
    SHUTDOWN_REQUESTED.store(false, Ordering::SeqCst);
    SHUTDOWN_PHASE.store(0, Ordering::SeqCst);
    SHUTDOWN_STARTED_MS.store(0, Ordering::SeqCst);

    // --- VirtIO Console (canal de control host→guest, /dev/hvc0) ---
    let mut console_dev = VirtioConsoleDevice::new(guest_mem.clone());
    vm.register_irqfd(&console_dev.irqfd, CONSOLE_IRQ)
        .expect("[NKR-CTL] irqfd console falló");
    vm.register_ioevent(&console_dev.ioeventfd, &IoEventAddress::Mmio(CONSOLE_MMIO_ADDR + 0x50), 1u64)
        .expect("[NKR-CTL] ioeventfd console falló");
    eprintln!("[NKR] Control: canal hvc0 activo [MMIO {:#X}, IRQ {}]", CONSOLE_MMIO_ADDR, CONSOLE_IRQ);
    unsafe {
        libc::signal(libc::SIGTERM, sigterm_handler as *const () as libc::sighandler_t);
    }

    // Feature D — Seccomp Jailer: Límite subido a 120 syscalls de base p/ PostgreSQL.
    // Instalamos AQUÍ (después de thread::spawn) para que el hilo RX herede.
    if let Err(e) = crate::seccomp::install_seccomp_filter() {
        eprintln!("[NKR] WARN: seccomp no disponible: {e}");
    }

    // Capturar resultado del vCPU loop pero ejecutar SIEMPRE el cleanup.
    // Antes, el `?` causaba early-return saltando unregister_vm, TAP cleanup
    // y cgroup teardown, dejando recursos huérfanos.
    let loop_result = run_vcpu_loop(&mut vcpu, &mut block_devs, &mut net_dev, &mut fs_devs, &mut balloon_dev, &mut console_dev, pmem_dev_opt.as_mut(), &serial_irqfd);
    eprintln!("[NKR-DBG] vcpu_loop salió — iniciando cleanup");

    // Limpiar sentinel de shutdown del VirtIO-FS rootfs (si existía)
    if let Some(rootfs_path) = SHUTDOWN_ROOTFS_PATH.get() {
        let sentinel = format!("{}/.nkr-shutdown", rootfs_path);
        let _ = std::fs::remove_file(&sentinel);
    }

    // Limpiar port forwarding
    eprintln!("[NKR-DBG] cleanup_port_forwarding...");
    cleanup_port_forwarding(&forwarding_rules, &guest_ip);
    eprintln!("[NKR-DBG] cleanup_port_forwarding OK");

    // Extraer volúmenes rw (post-shutdown: monta disco, copia guest→host)
    if !parsed_volumes.is_empty() {
        eprintln!("[NKR-DBG] extract_volumes...");
        extract_volumes(&config.disks[0], &parsed_volumes).unwrap_or_else(|e| {
            eprintln!("[NKR-VOL] WARN: Error extrayendo volúmenes: {e}");
        });
        eprintln!("[NKR-DBG] extract_volumes OK");
    }

    // Limpiar TAP auto-creado + reglas ebtables
    if let Some(ref tap_name) = auto_tap_name {
        eprintln!("[NKR-DBG] teardown_tap...");
        teardown_tap_isolation(tap_name, &mac, &guest_ip);
        let _ = std::process::Command::new("ip")
            .args(["link", "delete", tap_name]).status();
        eprintln!("[NKR] TAP {} eliminado", tap_name);
    }

    eprintln!("[NKR-DBG] unregister_vm...");
    // Desregistrar VM del estado global (siempre, incluso en error)
    state::unregister_vm(config.vm_id);

    // Limpiar cgroup
    if config.chrs > 0 {
        eprintln!("[NKR-DBG] teardown_cgroup...");
        teardown_cgroup(&config.name);
    }

    eprintln!("[NKR-DBG] cleanup completo — Drop pendiente de fs_devs/block_devs");

    // Limpiar rootfs si fue auto-montado (Deshabilitado: usamos Master Mounts compartidos persistentes en /run)

    eprintln!("════════════════════════════════════════════════════════════════");
    eprintln!("[NKR] MicroVM finalizada");

    // Propagar error del vCPU loop si hubo uno
    loop_result
}

// =============================================================================
// CPU Pinning — Modelo de Chrs
// =============================================================================

fn pin_cpu_chrs(chrs: u32) -> Result<(), Box<dyn std::error::Error>> {
    // Si chrs es 0, no hacer pinning
    let num_cores = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) } as u32;
    
    if chrs == 0 {
        return Ok(()); // No pinning needed
    }

    // Cada core tiene 5 chrs (20% cada una)
    // Calculamos cuántos cores necesitamos enteros para cubrir los chrs
    let cores_needed = ((chrs as f32) / 5.0).ceil() as u32;
    let cores_to_use = cores_needed.min(num_cores);

    unsafe {
        let mut cpuset: libc::cpu_set_t = std::mem::zeroed();
        for core in 0..cores_to_use {
            libc::CPU_SET(core as usize, &mut cpuset);
        }
        let ret = libc::sched_setaffinity(
            0,
            std::mem::size_of::<libc::cpu_set_t>(),
            &cpuset,
        );
        if ret != 0 {
            eprintln!("[NKR] WARN: No se pudo pinear CPU (se requiere root)");
        } else {
            eprintln!("[NKR] CPU: {} chrs → pineado a {} core(s) de {}", chrs, cores_to_use, num_cores);
        }
    }
    Ok(())
}

// =============================================================================
// Aislamiento L2 — cBPF via tc (v1.3, reemplaza ebtables)
// =============================================================================
//
// Genera un programa cBPF (Classic BPF) a partir de MAC+IP y lo instala en
// tc ingress del TAP. El kernel procesa el programa en el subsistema BPF —
// la misma infraestructura que eBPF — sin requerir el módulo ebtables.
//
// El programa valida:
//   1. src MAC == MAC asignada (bytes 6–11 del frame Ethernet)
//   2. Ethertype 0x0806 (ARP)  → ACCEPT
//   3. Ethertype 0x0800 (IPv4) + src IP == IP asignada → ACCEPT
//   4. Todo lo demás → DROP (return 0)
//
// Programa cBPF (11 instrucciones):
//   0: LD_W  [6]         cargar src MAC bytes 0-3
//   1: JEQ   mac_hi32    → 2 si igual, DROP si no
//   2: LD_H  [10]        cargar src MAC bytes 4-5
//   3: JEQ   mac_lo16    → 4 si igual, DROP si no
//   4: LD_H  [12]        cargar ethertype
//   5: JEQ   0x0806      → 10 (ACCEPT) si ARP; else → 6
//   6: JEQ   0x0800      → 7 si IPv4; else → DROP
//   7: LD_W  [26]        cargar IPv4 src IP
//   8: JEQ   ip_int      → 10 (ACCEPT) si igual; else → DROP
//   9: RET   0           DROP
//  10: RET   65535       ACCEPT
// =============================================================================

/// Construye el bytecode cBPF como string para `tc filter add ... bpf bytecode`.
/// Formato: "N,code jt jf k,code jt jf k,..."
fn build_bpf_bytecode(mac: &[u8; 6], guest_ip: &str) -> Option<String> {
    let ip_parts: Vec<u8> = guest_ip.split('.')
        .filter_map(|s| s.parse().ok())
        .collect();
    if ip_parts.len() != 4 { return None; }

    let mac_hi = u32::from_be_bytes([mac[0], mac[1], mac[2], mac[3]]);
    let mac_lo = u16::from_be_bytes([mac[4], mac[5]]) as u32;
    let ip_int = u32::from_be_bytes([ip_parts[0], ip_parts[1], ip_parts[2], ip_parts[3]]);

    // 11 instrucciones cBPF en formato de string
    // BPF opcodes: LD_W=32, LD_H=40, JEQ_K=21, RET_K=6
    Some(format!(
        "11,\
         32 0 0 6,\
         21 0 7 {mac_hi},\
         40 0 0 10,\
         21 0 5 {mac_lo},\
         40 0 0 12,\
         21 4 0 2054,\
         21 0 2 2048,\
         32 0 0 26,\
         21 1 0 {ip_int},\
         6 0 0 0,\
         6 0 0 65535"
    ))
}

/// Instala aislamiento L2 en el TAP mediante cBPF via tc.
/// Reemplaza ebtables (v1.2) con kernel BPF nativo (v1.3).
/// Fallback: si tc no está disponible, intenta ebtables.
fn setup_tap_isolation(tap_name: &str, mac: &[u8; 6], guest_ip: &str) {
    let mac_str = format!("{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]);

    // ── Intentar instalación cBPF via tc ─────────────────────────────────────
    if let Some(bytecode) = build_bpf_bytecode(mac, guest_ip) {
        // 1. Crear qdisc clsact (idempotente — falla silenciosamente si existe)
        let _ = std::process::Command::new("tc")
            .args(["qdisc", "add", "dev", tap_name, "clsact"])
            .output();

        // 2. Instalar filtro cBPF en ingress
        let result = std::process::Command::new("tc")
            .args([
                "filter", "add", "dev", tap_name,
                "ingress", "protocol", "all",
                "pref", "1",
                "bpf", "bytecode", &bytecode,
                "flowid", "1:1",
            ])
            .output();

        match result {
            Ok(o) if o.status.success() => {
                eprintln!("[NKR] BPF L2: filtro instalado en {} (MAC={} IP={})",
                    tap_name, mac_str, guest_ip);
                return;
            }
            _ => {
                eprintln!("[NKR] WARN: tc BPF filter falló — probando ebtables fallback");
            }
        }
    }

    // ── Fallback: ebtables ────────────────────────────────────────────────────
    if std::process::Command::new("which").arg("ebtables")
        .output().map(|o| !o.status.success()).unwrap_or(true)
    {
        eprintln!("[NKR] WARN: ebtables no disponible — aislamiento L2 omitido para {}", tap_name);
        return;
    }

    let rules: &[&[&str]] = &[
        // Permitir ARP solo desde la MAC asignada
        &["ebtables", "-A", "INPUT", "-i", tap_name,
          "-p", "ARP", "--arp-mac-src", &mac_str, "-j", "ACCEPT"],
        // Permitir IPv4 solo desde la MAC + IP asignadas
        &["ebtables", "-A", "INPUT", "-i", tap_name,
          "-p", "IPv4", "--ip-src", guest_ip, "-s", &mac_str, "-j", "ACCEPT"],
        // Descartar todo lo demás proveniente de este TAP
        &["ebtables", "-A", "INPUT", "-i", tap_name, "-j", "DROP"],
    ];

    for rule in rules {
        let _ = std::process::Command::new(rule[0]).args(&rule[1..]).status();
    }
    eprintln!("[NKR] L2: reglas ebtables instaladas para {} (MAC={} IP={})",
        tap_name, mac_str, guest_ip);
}

/// Elimina el aislamiento L2 del TAP (tc qdisc + ebtables fallback).
fn teardown_tap_isolation(tap_name: &str, mac: &[u8; 6], guest_ip: &str) {
    // Eliminar qdisc clsact (también elimina todos los filtros cBPF)
    let _ = std::process::Command::new("tc")
        .args(["qdisc", "del", "dev", tap_name, "clsact"])
        .output();

    // Limpieza ebtables (por si se usó el fallback)
    let mac_str = format!("{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]);

    let rules: &[&[&str]] = &[
        &["ebtables", "-D", "INPUT", "-i", tap_name,
          "-p", "ARP", "--arp-mac-src", &mac_str, "-j", "ACCEPT"],
        &["ebtables", "-D", "INPUT", "-i", tap_name,
          "-p", "IPv4", "--ip-src", guest_ip, "-s", &mac_str, "-j", "ACCEPT"],
        &["ebtables", "-D", "INPUT", "-i", tap_name, "-j", "DROP"],
    ];

    for rule in rules {
        let _ = std::process::Command::new(rule[0]).args(&rule[1..]).status();
    }
}

// =============================================================================
// cgroupv2 — CPU Bursting + I/O Throttling (Features 2 y 4B)
// =============================================================================

/// Obtiene el major:minor de un dispositivo de bloque dado su path.
fn get_block_major_minor(path: &str) -> Option<(u32, u32)> {
    use std::ffi::CString;
    let c_path = CString::new(path).ok()?;
    let mut stat_buf: libc::stat = unsafe { std::mem::zeroed() };
    let ret = unsafe { libc::stat(c_path.as_ptr(), &mut stat_buf) };
    if ret != 0 {
        return None;
    }
    let rdev = stat_buf.st_rdev;
    // major = bits 8..19, minor = bits 0..7 (dispositivos normales)
    let major = (rdev >> 8) as u32;
    let minor = (rdev & 0xFF) as u32;
    Some((major, minor))
}

/// Configura cgroupv2 para la VM: cpu.max (garantía + burst) e io.max (throttling).
/// Degrada silenciosamente si cgroupv2 no está disponible en el host.
/// `burst`: si true (Smart Default v1.3), activa cpu.max.burst para ráfagas cortas.
fn setup_cgroup(vm_name: &str, chrs: u32, pid: u32, disk_paths: &[String], burst: bool) {
    // Verificar que cgroupv2 esté montado
    let controllers_path = "/sys/fs/cgroup/cgroup.controllers";
    let controllers = match std::fs::read_to_string(controllers_path) {
        Ok(c) => c,
        Err(_) => {
            eprintln!("[NKR] WARN: cgroupv2 no disponible — CPU bursting omitido");
            return;
        }
    };
    if !controllers.contains("cpu") {
        eprintln!("[NKR] WARN: controlador 'cpu' no disponible en cgroupv2 — CPU bursting omitido");
        return;
    }

    let base = format!("/sys/fs/cgroup/nkr/{}", vm_name);

    // Asegurar que el subtree nkr exista y tenga los controladores habilitados
    let nkr_base = "/sys/fs/cgroup/nkr";
    if let Err(e) = std::fs::create_dir_all(&base) {
        eprintln!("[NKR] WARN: No se pudo crear cgroup {}: {}", base, e);
        return;
    }

    // Habilitar controladores cpu e io en el cgroup padre
    let subtree_ctl = format!("{}/cgroup.subtree_control", nkr_base);
    let _ = std::fs::write(&subtree_ctl, "+cpu +io");

    // cpu.max: quota = chrs * 20_000µs por cada 100_000µs (20% por chr)
    let quota = chrs * 20_000;
    if let Err(e) = std::fs::write(format!("{}/cpu.max", base), format!("{} 100000", quota)) {
        eprintln!("[NKR] WARN: No se pudo escribir cpu.max: {}", e);
    }

    // cpu.max.burst (kernel >= 5.15): permite ráfagas cortas con ciclos ociosos.
    // Solo se activa si burst=true (Smart Default v1.3). Desactivar para VMs
    // que requieran latencia de CPU estrictamente predecible.
    let burst_path = format!("{}/cpu.max.burst", base);
    if burst && std::path::Path::new(&burst_path).exists() {
        let burst_us = chrs * 5_000;
        let _ = std::fs::write(&burst_path, format!("{}", burst_us));
        eprintln!("[NKR] cgroup: cpu.max.burst={} µs (burst activo)", burst_us);
    } else if !burst {
        // Explícitamente deshabilitar burst si se pidió
        if std::path::Path::new(&burst_path).exists() {
            let _ = std::fs::write(&burst_path, "0");
        }
    }

    // io.max: 200 MB/s lectura, 100 MB/s escritura por disco
    if controllers.contains("io") {
        for disk in disk_paths {
            if let Some((maj, min)) = get_block_major_minor(disk) {
                let io_entry = format!("{}:{} rbps=209715200 wbps=104857600\n", maj, min);
                let _ = std::fs::write(format!("{}/io.max", base), &io_entry);
            }
        }
    }

    // Mover el proceso actual al cgroup
    if let Err(e) = std::fs::write(format!("{}/cgroup.procs", base), format!("{}", pid)) {
        eprintln!("[NKR] WARN: No se pudo mover PID al cgroup: {}", e);
    } else {
        eprintln!("[NKR] cgroup: {} | cpu.max={} 100000 | io.max=200/100 MB/s", vm_name, quota);
    }
}

/// Elimina el cgroup de la VM tras su apagado.
fn teardown_cgroup(vm_name: &str) {
    let base = format!("/sys/fs/cgroup/nkr/{}", vm_name);
    // rmdir solo funciona cuando el cgroup está vacío (PID ya terminó)
    let _ = std::fs::remove_dir(&base);
}

// =============================================================================
// Funciones del VMM
// =============================================================================

fn register_guest_memory(vm: &VmFd, guest_mem: &GuestMemoryMmap<()>) -> Result<(), Box<dyn std::error::Error>> {
    for (index, region) in guest_mem.iter().enumerate() {
        let mem_region = kvm_userspace_memory_region {
            slot: index as u32,
            flags: 0,
            guest_phys_addr: region.start_addr().raw_value(),
            memory_size: region.len(),
            userspace_addr: region.as_ptr() as u64,
        };
        unsafe { vm.set_user_memory_region(mem_region)?; }
    }
    Ok(())
}

// =============================================================================
// Feature C — Carga de kernel ELF vmlinux (sin descompresión en guest)
// =============================================================================

#[derive(Clone, Copy)]
enum KernelFormat { BzImage, Elf }

/// Smart default v1.3: Fuerza nanolinux ELF como default siempre.
fn smart_resolve_kernel(path: &str) -> String {
    let is_default = path == "nanolinux" || path == "vmlinux" || path == "bzImage"
        || path.ends_with("/nanolinux") || path.ends_with("/vmlinux") || path.ends_with("/bzImage")
        || path.ends_with("/kernel/nanolinux") || path.ends_with("/kernel/vmlinux") || path.ends_with("/kernel/bzImage");

    if is_default {
        let central_nanolinux = "/mnt/nkr/kernel/nanolinux";
        if std::path::Path::new(central_nanolinux).exists() {
            eprintln!("[NKR] Smart default: usando nanolinux en almacén central");
            return central_nanolinux.to_string();
        }
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                let nanolinux = dir.join("nanolinux");
                if nanolinux.exists() {
                    eprintln!("[NKR] Smart default: usando nanolinux local");
                    return nanolinux.to_string_lossy().to_string();
                }
            }
        }
    }
    path.to_string()
}

/// Detecta el formato del kernel leyendo los primeros 4 bytes (magic).
/// ELF: `\x7fELF`. Cualquier otro caso → bzImage (compatibilidad garantizada).
fn detect_kernel_format(path: &str) -> KernelFormat {
    let mut f = match File::open(path) {
        Ok(f) => f,
        Err(_) => return KernelFormat::BzImage,
    };
    let mut magic = [0u8; 4];
    if f.read_exact(&mut magic).is_ok() && &magic == b"\x7fELF" {
        KernelFormat::Elf
    } else {
        KernelFormat::BzImage
    }
}

/// Carga un kernel vmlinux ELF directamente en la memoria del guest.
/// Reutiliza linux_loader::Elf (ya disponible con feature "elf" en Cargo.toml).
fn load_elf_kernel(guest_mem: &GuestMemoryMmap<()>, path: &str) -> Result<u64, Box<dyn std::error::Error>> {
    let mut kernel_file = File::open(path)
        .map_err(|e| format!("No se pudo abrir ELF kernel '{}': {}", path, e))?;
    let load_result = Elf::load(
        guest_mem,
        None,
        &mut kernel_file,
        Some(GuestAddress(KERNEL_LOAD_ADDR)),
    ).map_err(|e| format!("Fallo al cargar ELF kernel '{}': {}", path, e))?;
    eprintln!("[NKR] Kernel ELF: {} → entry={:#X}", path, load_result.kernel_load.raw_value());
    Ok(load_result.kernel_load.raw_value())
}

fn load_bzimage_kernel(guest_mem: &GuestMemoryMmap<()>, path: &str) -> Result<u64, Box<dyn std::error::Error>> {
    let mut kernel_file = File::open(path)?;
    
    let mut header = vec![0u8; 4096];
    kernel_file.read_exact(&mut header)?;
    guest_mem.write_slice(&header, GuestAddress(ZERO_PAGE_ADDR))?;
    kernel_file.seek(SeekFrom::Start(0))?;

    let load_result = BzImage::load(guest_mem, Some(GuestAddress(KERNEL_LOAD_ADDR)), &mut kernel_file, None)
        .map_err(|e| format!("Fallo al cargar bzImage: {e}"))?;
    
    eprintln!("[NKR] Kernel: {path} → entry={:#X}", load_result.kernel_load.raw_value());
    Ok(load_result.kernel_load.raw_value())
}

fn load_initramfs_auto(guest_mem: &GuestMemoryMmap<()>, explicit_path: &Option<String>, ram_bytes: usize) -> Result<u32, Box<dyn std::error::Error>> {
    // Si se especificó una ruta explícita, usarla
    if let Some(path) = explicit_path {
        return load_initramfs(guest_mem, path, ram_bytes);
    }

    // Auto-detectar
    for candidate in &["initramfs.cpio.gz", "initramfs.cpio"] {
        if std::path::Path::new(candidate).exists() {
            match load_initramfs(guest_mem, candidate, ram_bytes) {
                Ok(size) => return Ok(size),
                Err(e) => eprintln!("[NKR] WARN: No se pudo cargar {candidate}: {e}"),
            }
        }
    }

    eprintln!("[NKR] Sin initramfs — módulos del kernel no disponibles");
    Ok(0)
}

fn load_initramfs(guest_mem: &GuestMemoryMmap<()>, path: &str, ram_bytes: usize) -> Result<u32, Box<dyn std::error::Error>> {
    let mut file = File::open(path)?;
    let mut data = Vec::new();
    file.read_to_end(&mut data)?;
    let size = data.len() as u32;
    // Colocar initramfs justo antes del tope de RAM (alineado a 4KB)
    let initramfs_addr = if ram_bytes > (INITRAMFS_ADDR as usize + size as usize) {
        INITRAMFS_ADDR
    } else {
        let addr = (ram_bytes - size as usize) & !0xFFF; // alinear a 4KB
        addr as u64
    };
    guest_mem.write_slice(&data, GuestAddress(initramfs_addr))?;
    eprintln!("[NKR] Initramfs: {path} ({} KB) @ {:#X}", size / 1024, initramfs_addr);
    Ok(size)
}

fn configure_linux_boot(
    guest_mem: &GuestMemoryMmap<()>,
    initrd_size: u32,
    ram_bytes: usize,
    guest_ip: &str,
    block_configs: &[(u64, u32)],
    fs_shares: &[(String, String, bool, usize)],    // VirtIO-FS: (tag, guest_path, readonly, slot)
    blk_share_mounts: &[(String, String)], // VirtIO-BLK shares: (dev, guest_mnt)
    pmem_dev: Option<&VirtioPmemDevice>,
    balloon_enabled: bool,
    rootfs_tag: Option<&str>,             // Si Some, rootfs viene por VirtIO-FS (no /dev/vda)
) -> Result<(), Box<dyn std::error::Error>> {

    let mut cmdline_str = format!("console=ttyS0 lpj=2800170 panic=1 rtc.hctosys=0 tsc=reliable no_timer_check clocksource=kvm-clock reboot=t virtio_mmio.device=4K@0xd0000000:5");

    for (addr, irq) in block_configs {
        cmdline_str.push_str(&format!(" virtio_mmio.device=4K@{:#010x}:{}", addr, irq));
    }

    if let Some(_) = pmem_dev {
        cmdline_str.push_str(" memmap=8G!4G root=/dev/pmem0 rootflags=dax rw");
    }

    // VirtIO-FS shares: nkr.fs{i} / nkr.fsm{i} / nkr.fsr{i}
    for (i, (tag, guest_path, readonly, slot)) in fs_shares.iter().enumerate() {
        let addr = 0xD001_0000u64 + (*slot as u64 * 0x1000);
        let irq  = 8u32 + *slot as u32;
        let mode = if *readonly { "ro" } else { "rw" };
        cmdline_str.push_str(&format!(" virtio_mmio.device=4K@{:#010x}:{} nkr.fs{}={} nkr.fsm{}={} nkr.fsr{}={}",
            addr, irq, i, tag, i, guest_path, i, mode));
    }

    // VirtIO-BLK shares: nkr.blk{i} / nkr.blkm{i}
    for (i, (dev, guest_mnt)) in blk_share_mounts.iter().enumerate() {
        cmdline_str.push_str(&format!(" nkr.blk{}={} nkr.blkm{}={}", i, dev, i, guest_mnt));
    }

    if balloon_enabled {
        cmdline_str.push_str(&format!(" virtio_mmio.device=4K@{:#010x}:{}", BALLOON_MMIO_ADDR, BALLOON_IRQ));
    }
    // Canal de control VirtIO-Console (siempre activo)
    cmdline_str.push_str(&format!(" virtio_mmio.device=4K@{:#010x}:{}", CONSOLE_MMIO_ADDR, CONSOLE_IRQ));

    let host_time = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
        
    if let Some(_) = pmem_dev {
        // 1. Declarar el dispositivo de hardware en el bus MMIO
        cmdline_str.push_str(&format!(" virtio_mmio.device=4K@{:#010x}:{}", PMEM_MMIO_ADDR, PMEM_IRQ));
        
        // 2. Configurar el ruteo de memoria y forzar el rootfs con DAX
        cmdline_str.push_str(" memmap=8G!4G root=/dev/pmem0 rootfstype=ext4 rootflags=dax rw rootdelay=1");
    } else if let Some(tag) = rootfs_tag {
        // Modo rootfs compartido: el kernel monta virtiofs nativamente como rootfs
        cmdline_str.push_str(&format!(" root={} rootfstype=virtiofs rw nkr.rootfs={}", tag, tag));
    } else {
        // Fallback clásico a disco de bloque si no hay PMEM ni rootfs
        cmdline_str.push_str(" root=/dev/vda rw");
    }
    
    cmdline_str.push_str(&format!(" init=/sbin/init nkr.ip={} nkr.time={} \0", guest_ip, host_time));
    
    guest_mem.write_slice(cmdline_str.as_bytes(), GuestAddress(CMDLINE_ADDR))?;

    // =========================================================================
    // FIRMAS OBLIGATORIAS (LINUX BOOT PROTOCOL)
    // Sin esto, el kernel ELF descarta la zero_page y borra el mapa E820
    // =========================================================================
    guest_mem.write_obj(0xAA55u16, GuestAddress(ZERO_PAGE_ADDR + 0x1FE))?; // boot_flag
    guest_mem.write_obj(0x53726448u32, GuestAddress(ZERO_PAGE_ADDR + 0x202))?; // header = "HdrS"
    guest_mem.write_obj(0x020Du16, GuestAddress(ZERO_PAGE_ADDR + 0x206))?; // version = 2.13

    guest_mem.write_obj(0xFFu8, GuestAddress(ZERO_PAGE_ADDR + 0x210))?; // type_of_loader
    guest_mem.write_obj(0x81u8, GuestAddress(ZERO_PAGE_ADDR + 0x211))?; // loadflags
    // Calcular dirección dinámica del initramfs (misma lógica que load_initramfs)
    let initramfs_addr = if ram_bytes > (INITRAMFS_ADDR as usize + initrd_size as usize) {
        INITRAMFS_ADDR
    } else {
        ((ram_bytes - initrd_size as usize) & !0xFFF) as u64
    };
    guest_mem.write_obj(initramfs_addr as u32, GuestAddress(ZERO_PAGE_ADDR + 0x218))?;
    guest_mem.write_obj(initrd_size, GuestAddress(ZERO_PAGE_ADDR + 0x21C))?;
    guest_mem.write_obj(CMDLINE_ADDR as u32, GuestAddress(ZERO_PAGE_ADDR + 0x228))?;

    // =========================================================================
    // MAPA E820 (La memoria RAM y PMEM)
    // =========================================================================
    guest_mem.write_obj(0x0u64, GuestAddress(ZERO_PAGE_ADDR + 0x2D0))?;
    guest_mem.write_obj(0x9FC00u64, GuestAddress(ZERO_PAGE_ADDR + 0x2D8))?;
    guest_mem.write_obj(1u32, GuestAddress(ZERO_PAGE_ADDR + 0x2E0))?; // Tipo 1: RAM usable

    let high_mem_size = (ram_bytes as u64) - 0x100000;
    guest_mem.write_obj(0x100000u64, GuestAddress(ZERO_PAGE_ADDR + 0x2E4))?;
    guest_mem.write_obj(high_mem_size, GuestAddress(ZERO_PAGE_ADDR + 0x2EC))?;
    guest_mem.write_obj(1u32, GuestAddress(ZERO_PAGE_ADDR + 0x2F4))?; // Tipo 1: RAM usable

    let e820_count: u8 = if let Some(pmem) = pmem_dev {
        guest_mem.write_obj(pmem.guest_phys_addr,       GuestAddress(ZERO_PAGE_ADDR + 0x2F8))?;
        guest_mem.write_obj(pmem.host_mmap_len as u64,  GuestAddress(ZERO_PAGE_ADDR + 0x300))?;
        guest_mem.write_obj(7u32,                       GuestAddress(ZERO_PAGE_ADDR + 0x308))?; // Tipo 7: PMEM
        3
    } else {
        2
    };
    
    guest_mem.write_obj(e820_count, GuestAddress(ZERO_PAGE_ADDR + 0x1E8))?; // Guardar cantidad de entradas
    
    Ok(())
}

fn write_page_tables(guest_mem: &GuestMemoryMmap<()>) -> Result<(), Box<dyn std::error::Error>> {
    guest_mem.write_obj(PDPT_ADDR | 0x3, GuestAddress(PML4_ADDR))?;
    guest_mem.write_obj(PD_ADDR | 0x3, GuestAddress(PDPT_ADDR))?;
    for i in 0u64..512 { guest_mem.write_obj((i << 21) | 0x83, GuestAddress(PD_ADDR + i * 8))?; }
    Ok(())
}

fn write_gdt(guest_mem: &GuestMemoryMmap<()>) -> Result<(), Box<dyn std::error::Error>> {
    let gdt: [u64; 4] = [0, 0x00AF_9A00_0000_FFFF, 0x00CF_9200_0000_FFFF, 0];
    for (i, &e) in gdt.iter().enumerate() { guest_mem.write_obj(e, GuestAddress(GDT_ADDR + (i as u64) * 8))?; }
    Ok(())
}

fn configure_sregs(vcpu: &VcpuFd) -> Result<(), Box<dyn std::error::Error>> {
    let mut sregs = vcpu.get_sregs()?;
    sregs.cr0 = 1;
    sregs.cr3 = 0;
    sregs.cr4 = 0;
    sregs.efer = 0;

    let cs = kvm_segment {
        base: 0, limit: 0xFFFF_FFFF, selector: 0x08, type_: 0xB, present: 1,
        dpl: 0, db: 1, s: 1, l: 0, g: 1, avl: 0, unusable: 0, padding: 0
    };
    let ds = kvm_segment {
        base: 0, limit: 0xFFFF_FFFF, selector: 0x10, type_: 0x3, present: 1,
        dpl: 0, db: 1, s: 1, l: 0, g: 1, avl: 0, unusable: 0, padding: 0
    };
    
    sregs.cs = cs;
    sregs.ds = ds; sregs.es = ds; sregs.fs = ds; sregs.gs = ds; sregs.ss = ds;
    sregs.gdt.base = GDT_ADDR;
    sregs.gdt.limit = 31;
    sregs.idt.base = 0;
    sregs.idt.limit = 0;

    vcpu.set_sregs(&sregs)?;
    Ok(())
}

fn configure_regs(vcpu: &VcpuFd, entry_addr: u64) -> Result<(), Box<dyn std::error::Error>> {
    let mut regs = vcpu.get_regs()?;
    
    regs.rip = entry_addr;
    
    // Vital: El puntero RSI debe apuntar a la zero_page para que Linux lea el mapa E820
    regs.rsi = ZERO_PAGE_ADDR; 
    
    // Vital: Mover el Stack Pointer LEJOS de la zero_page (0x7000). 
    // La pila crece hacia abajo, 0x9000 es seguro.
    regs.rsp = 0x9000;         
    
    regs.rflags = 0x2;
    
    vcpu.set_regs(&regs)?;
    Ok(())
}

/// Configura sregs para modo largo 64-bit (requerido por nanolinux ELF / startup_64).
/// A diferencia de configure_sregs() que usa modo protegido 32-bit para bzImage,
/// aquí habilitamos PAE, paginación y LME antes de saltar al entry del kernel.
fn configure_sregs_64(vcpu: &VcpuFd) -> Result<(), Box<dyn std::error::Error>> {
    let mut sregs = vcpu.get_sregs()?;

    // Habilitar modo largo: PAE + paginación + protección
    sregs.efer  = 0xD01;        // LME (bit 8) + LMA (bit 10) + NXE (bit 11)
    sregs.cr0   = 0x80050033;   // PG + WP + PE (+ MP + ET)
    sregs.cr3   = PML4_ADDR;    // Tabla de páginas construida por write_page_tables()
    sregs.cr4   = 0x20;         // PAE (bit 5)

    // CS 64-bit: l=1, db=0
    let cs64 = kvm_segment {
        base: 0, limit: 0xFFFF_FFFF, selector: 0x08,
        type_: 0xB, present: 1, dpl: 0, db: 0, s: 1, l: 1, g: 1,
        avl: 0, unusable: 0, padding: 0,
    };
    // DS/ES/FS/GS/SS: segmentos de datos 32/64-bit (igual que configure_sregs)
    let ds = kvm_segment {
        base: 0, limit: 0xFFFF_FFFF, selector: 0x10,
        type_: 0x3, present: 1, dpl: 0, db: 1, s: 1, l: 0, g: 1,
        avl: 0, unusable: 0, padding: 0,
    };
    sregs.cs = cs64;
    sregs.ds = ds; sregs.es = ds; sregs.fs = ds; sregs.gs = ds; sregs.ss = ds;
    sregs.gdt.base  = GDT_ADDR;
    sregs.gdt.limit = 31;
    sregs.idt.base  = 0;
    sregs.idt.limit = 0;

    vcpu.set_sregs(&sregs)?;
    Ok(())
}

// =============================================================================
// Bucle principal del vCPU — Emulación MMIO
// =============================================================================

extern "C" fn sigterm_handler(_sig: libc::c_int) {
    SHUTDOWN_REQUESTED.store(true, Ordering::SeqCst);
}

fn run_vcpu_loop(
    vcpu: &mut VcpuFd,
    block_devs: &mut Vec<VirtioBlockDevice>,
    net_dev: &mut VirtioNetDevice,
    fs_devs: &mut Vec<VirtioFsDevice>,
    balloon_dev: &mut VirtioBalloonDevice,
    console_dev: &mut VirtioConsoleDevice,
    mut pmem_dev: Option<&mut VirtioPmemDevice>,
    serial_irqfd: &vmm_sys_util::eventfd::EventFd,
) -> Result<(), Box<dyn std::error::Error>> {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    let mut serial_ier: u8 = 0;

    loop {
        if SHUTDOWN_REQUESTED.load(Ordering::SeqCst) {
            let phase = SHUTDOWN_PHASE.load(Ordering::SeqCst);
            if phase == 0 {
                let now_ms = SystemTime::now()
                    .duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64;
                SHUTDOWN_STARTED_MS.store(now_ms, Ordering::SeqCst);
                SHUTDOWN_PHASE.store(1, Ordering::SeqCst);
                eprintln!("\n[NKR] SIGTERM recibido — enviando SHUTDOWN por hvc0...");
                console_dev.try_inject(b"SHUTDOWN\n");
            } else {
                // Timeout de 120 s (fallback)
                let now_ms = SystemTime::now()
                    .duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64;
                let elapsed_secs = now_ms.saturating_sub(SHUTDOWN_STARTED_MS.load(Ordering::SeqCst)) / 1000;
                if elapsed_secs >= 30 {
                    eprintln!("[NKR] Timeout 30s esperando shutdown del huésped — forzando salida");
                    break;
                }
            }
            // Reintentar inyección si la cola aún no estaba lista
            console_dev.poll_pending();
        }

        // Feature B — io_uring: drenar completions antes de cada KVM_RUN
        for dev in block_devs.iter_mut() {
            dev.poll_completions();
        }

        match vcpu.run() {
            Ok(VcpuExit::IoOut(port, data)) => {
                if port == COM1_PORT {
                    out.write_all(data).unwrap();
                    out.flush().ok();
                    // If THRE interrupt is enabled, signal transmit complete
                    if serial_ier & 0x02 != 0 {
                        let _ = serial_irqfd.write(1);
                    }
                } else if port == 0x3F9 {
                    // IER (Interrupt Enable Register)
                    serial_ier = data[0];
                    // If THRE is now enabled, immediately signal ready
                    if serial_ier & 0x02 != 0 {
                        let _ = serial_irqfd.write(1);
                    }
                }
            }

            Ok(VcpuExit::IoIn(port, data)) => {
                match port {
                    0x3F8 => data.fill(0),
                    0x3F9 => data[0] = serial_ier,
                    0x3FA => {
                        if serial_ier & 0x02 != 0 {
                            data[0] = 0x02; // THRE interrupt pending
                        } else {
                            data[0] = 0x01; // No interrupt pending
                        }
                    }
                    0x3FB => data.fill(0),
                    0x3FC => data.fill(0),
                    0x3FD => data.fill(0x60), // THRE + TEMT
                    0x3FE => data.fill(0xB0),
                    0x60 | 0x64 => data.fill(0),
                    _ => data.fill(0xFF),
                }
            }

            Ok(VcpuExit::MmioRead(addr, data)) => {
                // 1. Red VirtIO-Net (0xD0000000)
                if addr >= 0xD0000000 && addr < 0xD0001000 {
                    let offset = addr - 0xD0000000;
                    let sel = net_dev.queue_sel as usize;
                    match offset {
                        0x000 => data.copy_from_slice(b"virt"),
                        0x004 => data.copy_from_slice(&2u32.to_le_bytes()),
                        0x008 => data.copy_from_slice(&1u32.to_le_bytes()), // DeviceID=1 (Net)
                        0x00C => data.copy_from_slice(&0x4E4B5200u32.to_le_bytes()),
                        0x010 => {
                            let features = if net_dev.device_features_sel == 0 {
                                (1u32 << 5) | (1u32 << 16) // VIRTIO_NET_F_MAC | VIRTIO_NET_F_STATUS
                            } else if net_dev.device_features_sel == 1 {
                                1u32 // VIRTIO_F_VERSION_1
                            } else { 0u32 };
                            data.copy_from_slice(&features.to_le_bytes());
                        }
                        0x034 => data.copy_from_slice(&256u32.to_le_bytes()),
                        0x044 => {
                            let val = if sel < 2 && { net_dev.state.lock().unwrap().queue_ready[sel] } { 1u32 } else { 0u32 };
                            data.copy_from_slice(&val.to_le_bytes());
                        }
                        0x060 => data.copy_from_slice(&net_dev.state.lock().unwrap().interrupt_status.to_le_bytes()),
                        0x070 => data.copy_from_slice(&net_dev.state.lock().unwrap().status.to_le_bytes()),
                        0x0FC => data.copy_from_slice(&0u32.to_le_bytes()),
                        // Config space: MAC address (6 bytes at offset 0x100-0x105)
                        off @ 0x100..=0x105 => {
                            let idx = (off - 0x100) as usize;
                            for (i, byte) in data.iter_mut().enumerate() {
                                *byte = if idx + i < 6 { net_dev.mac[idx + i] } else { 0 };
                            }
                        }
                        // Config space: status (2 bytes at 0x106-0x107) — link is up
                        0x106 => {
                            for (i, byte) in data.iter_mut().enumerate() {
                                *byte = if i == 0 { 1 } else { 0 }; // VIRTIO_NET_S_LINK_UP
                            }
                        }
                        0x107 => data.fill(0),
                        _ => data.fill(0),
                    }
                } else {
                    // Buscar en dispositivos de bloque
                    let base_block = 0xD0001000;
                    for (i, block_dev) in block_devs.iter_mut().enumerate() {
                        let dev_base = base_block + (i as u64 * 0x1000);
                        if addr >= dev_base && addr < dev_base + 0x1000 {
                            let offset = addr - dev_base;
                            match offset {
                                0x000 => data.copy_from_slice(b"virt"),
                                0x004 => data.copy_from_slice(&2u32.to_le_bytes()),
                                0x008 => data.copy_from_slice(&2u32.to_le_bytes()),
                                0x00C => data.copy_from_slice(&0x4E4B5200u32.to_le_bytes()),
                                0x010 => {
                                    let features = if block_dev.device_features_sel == 0 {
                                        0u32
                                    } else if block_dev.device_features_sel == 1 {
                                        1u32
                                    } else {
                                        0u32
                                    };
                                    data.copy_from_slice(&features.to_le_bytes());
                                }
                                0x034 => data.copy_from_slice(&256u32.to_le_bytes()),
                                0x044 => {
                                    let val = if block_dev.queue_ready { 1u32 } else { 0u32 };
                                    data.copy_from_slice(&val.to_le_bytes());
                                }
                                0x060 => data.copy_from_slice(&block_dev.interrupt_status.to_le_bytes()),
                                0x070 => data.copy_from_slice(&block_dev.status.to_le_bytes()),
                                0x0FC => data.copy_from_slice(&0u32.to_le_bytes()),
                                0x100 => {
                                    let cap_low = (block_dev.capacity_sectors & 0xFFFFFFFF) as u32;
                                    data.copy_from_slice(&cap_low.to_le_bytes());
                                }
                                0x104 => {
                                    let cap_high = (block_dev.capacity_sectors >> 32) as u32;
                                    data.copy_from_slice(&cap_high.to_le_bytes());
                                }
                                _ => data.fill(0),
                            }
                            break;
                        }
                    }
                    
                    // 3. Consola VirtIO (0xD0002000 - movido a futuro si se usa)
                    if addr >= 0xD0002000 && addr < 0xD0003000 {
                        let offset = addr - 0xD0002000;
                        match offset {
                            0x000 => data.copy_from_slice(&0x74726976u32.to_le_bytes()),
                            0x004 => data.copy_from_slice(&2u32.to_le_bytes()),
                            0x008 => data.copy_from_slice(&3u32.to_le_bytes()),
                            0x00C => data.copy_from_slice(&0x4E4B5200u32.to_le_bytes()),
                            0x010 => data.fill(0),
                            0x014 => data.copy_from_slice(&1u32.to_le_bytes()),
                            0x034 => data.copy_from_slice(&256u32.to_le_bytes()),
                            _ => data.fill(0),
                        }
                    }



                    // 5. VirtIO-PMEM (0xD0020000) — Feature A
                    if addr >= PMEM_MMIO_ADDR && addr < PMEM_MMIO_ADDR + 0x1000 {
                        if let Some(ref mut pmem) = pmem_dev {
                            let offset = addr - PMEM_MMIO_ADDR;
                            match offset {
                                0x000 => data.copy_from_slice(b"virt"),
                                0x004 => data.copy_from_slice(&2u32.to_le_bytes()),
                                0x008 => data.copy_from_slice(&27u32.to_le_bytes()), // DeviceID=27 (PMEM)
                                0x00C => data.copy_from_slice(&0x4E4B5200u32.to_le_bytes()),
                                0x010 => {
                                    let f = pmem.features_for_sel(pmem.device_features_sel);
                                    data.copy_from_slice(&f.to_le_bytes());
                                }
                                0x034 => data.copy_from_slice(&16u32.to_le_bytes()),
                                0x044 => {
                                    let v = if pmem.queue_ready { 1u32 } else { 0u32 };
                                    data.copy_from_slice(&v.to_le_bytes());
                                }
                                0x060 => data.copy_from_slice(&pmem.interrupt_status.to_le_bytes()),
                                0x070 => data.copy_from_slice(&pmem.status.to_le_bytes()),
                                0x0FC => data.copy_from_slice(&0u32.to_le_bytes()),
                                // Config space: start (u64) + size (u64)
                                off @ 0x100..=0x10F => pmem.config_read(off, data),
                                _ => data.fill(0),
                            }
                        }
                    }

                    // 6. VirtIO-FS (0xD0010000+) — MmioRead
                    for (_i, fs_dev) in fs_devs.iter_mut().enumerate() {
                        let dev_base = fs_dev.mmio_addr;
                        if addr >= dev_base && addr < dev_base + 0x1000 {
                            let offset = addr - dev_base;
                            match offset {
                                0x000 => data.copy_from_slice(b"virt"),
                                0x004 => data.copy_from_slice(&2u32.to_le_bytes()),
                                0x008 => data.copy_from_slice(&VIRTIO_FS_DEVICE_ID.to_le_bytes()),
                                0x00C => data.copy_from_slice(&0x4E4B5200u32.to_le_bytes()),
                                0x010 => {
                                    let features = fs_dev.features_for_sel(fs_dev.device_features_sel);
                                    data.copy_from_slice(&features.to_le_bytes());
                                }
                                0x034 => data.copy_from_slice(&128u32.to_le_bytes()),
                                0x044 => {
                                    let qi = fs_dev.queue_sel.min(1) as usize;
                                    let v = if fs_dev.queues[qi].ready { 1u32 } else { 0u32 };
                                    data.copy_from_slice(&v.to_le_bytes());
                                }
                                // VIRTIO_MMIO_INT_VRING(0x1): siempre activo cuando queues listas.
                                // El irqfd PIC es edge-triggered → no hay storm.
                                // Sin esto, vm_interrupt() lee 0, no llama vring_interrupt,
                                // y las respuestas FUSE quedan atascadas en el used ring.
                                0x060 => {
                                    let v = if fs_dev.queues_setup_done {
                                        fs_dev.interrupt_status | 0x01
                                    } else {
                                        fs_dev.interrupt_status
                                    };
                                    data.copy_from_slice(&v.to_le_bytes());
                                }
                                0x070 => data.copy_from_slice(&fs_dev.status.to_le_bytes()),
                                0x0FC => data.copy_from_slice(&0u32.to_le_bytes()),
                                0x0B0 => {
                                    if fs_dev.shm_sel == 0 && fs_dev.dax_enabled {
                                        data.copy_from_slice(&(fs_dev.dax_size as u32).to_le_bytes());
                                    } else { data.fill(0); }
                                }
                                0x0B4 => {
                                    if fs_dev.shm_sel == 0 && fs_dev.dax_enabled {
                                        data.copy_from_slice(&(((fs_dev.dax_size as u64) >> 32) as u32).to_le_bytes());
                                    } else { data.fill(0); }
                                }
                                0x0B8 => {
                                    if fs_dev.shm_sel == 0 && fs_dev.dax_enabled {
                                        data.copy_from_slice(&(fs_dev.dax_guest_phys as u32).to_le_bytes());
                                    } else { data.fill(0); }
                                }
                                0x0BC => {
                                    if fs_dev.shm_sel == 0 && fs_dev.dax_enabled {
                                        data.copy_from_slice(&((fs_dev.dax_guest_phys >> 32) as u32).to_le_bytes());
                                    } else { data.fill(0); }
                                }
                                // Config space: tag (36 bytes) + num_request_queues (u32)
                                off @ 0x100..=0x127 => fs_dev.config_read(off, data),
                                _ => data.fill(0),
                            }
                            break;
                        }
                    }

                    // 7. VirtIO-Balloon (0xD0040000) — MmioRead
                    if addr >= BALLOON_MMIO_ADDR && addr < BALLOON_MMIO_ADDR + 0x1000 {
                        let offset = addr - BALLOON_MMIO_ADDR;
                        match offset {
                            0x000 => data.copy_from_slice(b"virt"),
                            0x004 => data.copy_from_slice(&2u32.to_le_bytes()),
                            0x008 => data.copy_from_slice(&BALLOON_DEVICE_ID.to_le_bytes()),
                            0x00C => data.copy_from_slice(&0x4E4B5200u32.to_le_bytes()),
                            0x010 => {
                                let f = balloon_dev.features_for_sel(balloon_dev.device_features_sel);
                                data.copy_from_slice(&f.to_le_bytes());
                            }
                            0x034 => {
                                let qi = balloon_dev.queue_sel as usize;
                                let qn = if qi < 2 { balloon_dev.queue_num[qi] } else { 256 };
                                data.copy_from_slice(&qn.to_le_bytes());
                            }
                            0x044 => {
                                let qi = balloon_dev.queue_sel as usize;
                                let v = if qi < 2 && balloon_dev.queue_ready[qi] { 1u32 } else { 0u32 };
                                data.copy_from_slice(&v.to_le_bytes());
                            }
                            0x060 => data.copy_from_slice(&balloon_dev.interrupt_status.to_le_bytes()),
                            0x070 => data.copy_from_slice(&balloon_dev.status.to_le_bytes()),
                            0x0FC => data.copy_from_slice(&0u32.to_le_bytes()),
                            off @ 0x100..=0x107 => balloon_dev.config_read(off, data),
                            _ => data.fill(0),
                        }
                    }

                    // 8. VirtIO-Console /dev/hvc0 (0xD0050000) — MmioRead
                    if addr >= CONSOLE_MMIO_ADDR && addr < CONSOLE_MMIO_ADDR + 0x1000 {
                        let offset = addr - CONSOLE_MMIO_ADDR;
                        let qi = console_dev.queue_sel as usize;
                        match offset {
                            0x000 => data.copy_from_slice(b"virt"),
                            0x004 => data.copy_from_slice(&2u32.to_le_bytes()),
                            0x008 => data.copy_from_slice(&CONSOLE_DEVICE_ID.to_le_bytes()),
                            0x00C => data.copy_from_slice(&0x4E4B5200u32.to_le_bytes()),
                            0x010 => {
                                // VIRTIO_F_VERSION_1 (bit 32) en sel=1
                                let f: u32 = if console_dev.device_features_sel == 1 { 1u32 } else { 0u32 };
                                data.copy_from_slice(&f.to_le_bytes());
                            }
                            0x034 => {
                                let qn = if qi < 2 { console_dev.queue_num[qi] } else { 64 };
                                data.copy_from_slice(&qn.to_le_bytes());
                            }
                            0x044 => {
                                let v = if qi < 2 && console_dev.queue_ready[qi] { 1u32 } else { 0u32 };
                                data.copy_from_slice(&v.to_le_bytes());
                            }
                            0x060 => data.copy_from_slice(&console_dev.interrupt_status.to_le_bytes()),
                            0x070 => data.copy_from_slice(&console_dev.status.to_le_bytes()),
                            0x0FC => data.copy_from_slice(&0u32.to_le_bytes()),
                            _ => data.fill(0),
                        }
                    }
                    }

            }

            Ok(VcpuExit::MmioWrite(addr, data)) => {
                // 1. Red VirtIO-Net (0xD0000000)
                if addr >= 0xD0000000 && addr < 0xD0001000 {
                    let offset = addr - 0xD0000000;
                    let val = if data.len() == 4 { u32::from_le_bytes(data.try_into().unwrap()) } else { 0 };
                    let sel = net_dev.queue_sel as usize;
                    match offset {
                        0x014 => { net_dev.device_features_sel = val; }
                        0x020 => {
                            if net_dev.driver_features_sel == 0 {
                                net_dev.driver_features = (net_dev.driver_features & 0xFFFFFFFF_00000000) | (val as u64);
                            } else if net_dev.driver_features_sel == 1 {
                                net_dev.driver_features = (net_dev.driver_features & 0x00000000_FFFFFFFF) | ((val as u64) << 32);
                            }
                        }
                        0x024 => { net_dev.driver_features_sel = val; }
                        0x030 => { net_dev.queue_sel = val; }
                        0x038 => { if sel < 2 { net_dev.queue_num[sel] = val; } }
                        0x044 => {
                            if val == 1 { net_dev.activate_queue(); }
                            else {
                                let mut st = net_dev.state.lock().unwrap();
                                st.queue_ready[sel] = false;
                                if sel == 0 { st.queue_rx.set_ready(false); }
                                else { net_dev.queue_tx.set_ready(false); }
                            }
                        }
                        0x050 => { 
                            // QueueNotify: val = queue index
                            if val == 1 {
                                net_dev.process_tx();
                            }
                            // val == 0 = RX refill notification, no action needed
                            // (the background thread picks up new RX buffers)
                        }
                        0x064 => {
                            let mut st = net_dev.state.lock().unwrap();
                            st.interrupt_status &= !val;
                        }
                        0x070 => {
                            if val == 0 { net_dev.reset(); }
                            else {
                                let mut st = net_dev.state.lock().unwrap();
                                st.status = val;
                                if val == 15 { eprintln!("[NKR-NET] ¡DRIVER_OK! Red lista."); }
                            }
                        }
                        0x080 => { if sel < 2 { net_dev.desc_low[sel] = val; } }
                        0x084 => { if sel < 2 { net_dev.desc_high[sel] = val; } }
                        0x090 => { if sel < 2 { net_dev.avail_low[sel] = val; } }
                        0x094 => { if sel < 2 { net_dev.avail_high[sel] = val; } }
                        0x0A0 => { if sel < 2 { net_dev.used_low[sel] = val; } }
                        0x0A4 => { if sel < 2 { net_dev.used_high[sel] = val; } }
                        _ => {}
                    }
                } else {
                    // Buscar en dispositivos de bloque
                    let base_block = 0xD0001000;
                    for (i, block_dev) in block_devs.iter_mut().enumerate() {
                        let dev_base = base_block + (i as u64 * 0x1000);
                        if addr >= dev_base && addr < dev_base + 0x1000 {
                            let offset = addr - dev_base;
                            let val = if data.len() == 4 { u32::from_le_bytes(data.try_into().unwrap()) } else { 0 };

                            match offset {
                                0x014 => { block_dev.device_features_sel = val; }
                                0x020 => {
                                    if block_dev.driver_features_sel == 0 {
                                        block_dev.driver_features = (block_dev.driver_features & 0xFFFFFFFF_00000000) | (val as u64);
                                    } else if block_dev.driver_features_sel == 1 {
                                        block_dev.driver_features = (block_dev.driver_features & 0x00000000_FFFFFFFF) | ((val as u64) << 32);
                                    }
                                }
                                0x024 => { block_dev.driver_features_sel = val; }
                                0x030 => { block_dev.queue_sel = val; }
                                0x038 => { block_dev.queue_num = val; }
                                0x044 => {
                                    if val == 1 {
                                        block_dev.activate_queue();
                                    } else {
                                        block_dev.queue_ready = false;
                                        block_dev.queue.set_ready(false);
                                    }
                                }
                                0x050 => { block_dev.process_queue(); }
                                0x064 => { block_dev.interrupt_status &= !val; }
                                0x070 => {
                                    if val == 0 {
                                        block_dev.reset();
                                    } else {
                                        block_dev.status = val;
                                        match val {
                                            1  => eprintln!("[NKR-BLOCK] Disco {} Status: ACKNOWLEDGE", i),
                                            3  => eprintln!("[NKR-BLOCK] Disco {} Status: DRIVER", i),
                                            11 => eprintln!("[NKR-BLOCK] Disco {} Status: FEATURES_OK", i),
                                            15 => eprintln!("[NKR-BLOCK] Disco {} ¡DRIVER_OK! Listo.", i),
                                            _  => eprintln!("[NKR-BLOCK] Disco {} Status: {:#X}", i, val),
                                        }
                                    }
                                }
                                0x080 => { block_dev.desc_low = val; }
                                0x084 => { block_dev.desc_high = val; }
                                0x090 => { block_dev.avail_low = val; }
                                0x094 => { block_dev.avail_high = val; }
                                0x0A0 => { block_dev.used_low = val; }
                                0x0A4 => { block_dev.used_high = val; }
                                _ => {}
                            }
                            break;
                        }
                    }
                    // 3. Consola (0xD0002000)
                    if addr >= 0xD0002000 && addr < 0xD0003000 {
                        // Stub: ignorar escrituras a la consola por ahora
                    }



                    // 5. VirtIO-PMEM (0xD0020000) — Feature A
                    if addr >= PMEM_MMIO_ADDR && addr < PMEM_MMIO_ADDR + 0x1000 {
                        if let Some(ref mut pmem) = pmem_dev {
                            let offset = addr - PMEM_MMIO_ADDR;
                            let val = if data.len() == 4 { u32::from_le_bytes(data.try_into().unwrap()) } else { 0 };
                            match offset {
                                0x014 => { pmem.device_features_sel = val; }
                                0x020 => {
                                    if pmem.driver_features_sel == 0 {
                                        pmem.driver_features = (pmem.driver_features & 0xFFFFFFFF_00000000) | (val as u64);
                                    } else {
                                        pmem.driver_features = (pmem.driver_features & 0x00000000_FFFFFFFF) | ((val as u64) << 32);
                                    }
                                }
                                0x024 => { pmem.driver_features_sel = val; }
                                0x030 => { pmem.queue_sel = val; }
                                0x038 => { pmem.queue_num = val; }
                                0x044 => {
                                    if val == 1 { pmem.activate_queue(); }
                                }
                                0x050 => { pmem.process_queue(); }
                                0x064 => { pmem.interrupt_status &= !val; }
                                0x070 => {
                                    if val == 0 { pmem.status = 0; pmem.queue_ready = false; }
                                    else { pmem.status = val; }
                                }
                                0x080 => { pmem.desc_low = val; }
                                0x084 => { pmem.desc_high = val; }
                                0x090 => { pmem.avail_low = val; }
                                0x094 => { pmem.avail_high = val; }
                                0x0A0 => { pmem.used_low = val; }
                                0x0A4 => { pmem.used_high = val; }
                                _ => {}
                            }
                        }
                    }

                    // 6. VirtIO-FS (0xD0010000+) — MmioWrite
                    for (i, fs_dev) in fs_devs.iter_mut().enumerate() {
                        let dev_base = fs_dev.mmio_addr;
                        if addr >= dev_base && addr < dev_base + 0x1000 {
                            let offset = addr - dev_base;
                            let val = if data.len() == 4 { u32::from_le_bytes(data.try_into().unwrap()) } else { 0 };
                            let qi = fs_dev.queue_sel.min(1) as usize;
                            match offset {
                                0x014 => { fs_dev.device_features_sel = val; }
                                0x020 => {
                                    if fs_dev.driver_features_sel == 0 {
                                        fs_dev.driver_features = (fs_dev.driver_features & 0xFFFFFFFF_00000000) | (val as u64);
                                    } else {
                                        fs_dev.driver_features = (fs_dev.driver_features & 0x00000000_FFFFFFFF) | ((val as u64) << 32);
                                    }
                                }
                                0x024 => { fs_dev.driver_features_sel = val; }
                                0x030 => { fs_dev.queue_sel = val.min(1); }
                                0x038 => { fs_dev.queues[qi].num = val; }
                                0x044 => {
                                    if val == 1 { fs_dev.queues[qi].ready = true; }
                                    else { fs_dev.queues[qi].ready = false; }
                                }
                                0x050 => { fs_dev.process_queue(val as usize); }
                                0x064 => { fs_dev.interrupt_status &= !val; }
                                0x070 => {
                                    if val == 0 {
                                        fs_dev.reset();
                                    } else {
                                        fs_dev.status = val;
                                        if val & 0x4 != 0 { // DRIVER_OK
                                            eprintln!("[NKR-FS] Dispositivo {} DRIVER_OK (tag='{}')", i, fs_dev.tag);
                                            fs_dev.on_driver_ok();
                                        }
                                    }
                                }
                                0x080 => { fs_dev.queues[qi].desc_low  = val; }
                                0x084 => { fs_dev.queues[qi].desc_high = val; }
                                0x090 => { fs_dev.queues[qi].avail_low  = val; }
                                0x094 => { fs_dev.queues[qi].avail_high = val; }
                                0x0A0 => { fs_dev.queues[qi].used_low  = val; }
                                0x0A4 => { fs_dev.queues[qi].used_high = val; }
                                0x0AC => { fs_dev.shm_sel = val; }
                                _ => {}
                            }
                            break;
                        }
                    }

                    // 7. VirtIO-Balloon (0xD0040000) — MmioWrite (v1.3)
                    if addr >= BALLOON_MMIO_ADDR && addr < BALLOON_MMIO_ADDR + 0x1000 {
                        let offset = addr - BALLOON_MMIO_ADDR;
                        let val = if data.len() == 4 { u32::from_le_bytes(data.try_into().unwrap()) } else { 0 };
                        let qi = balloon_dev.queue_sel as usize;
                        match offset {
                            0x014 => { balloon_dev.device_features_sel = val; }
                            0x020 => {
                                if balloon_dev.driver_features_sel == 0 {
                                    balloon_dev.driver_features = (balloon_dev.driver_features & 0xFFFFFFFF_00000000) | (val as u64);
                                } else {
                                    balloon_dev.driver_features = (balloon_dev.driver_features & 0x00000000_FFFFFFFF) | ((val as u64) << 32);
                                }
                            }
                            0x024 => { balloon_dev.driver_features_sel = val; }
                            0x030 => { balloon_dev.queue_sel = val; }
                            0x038 => { if qi < 2 { balloon_dev.queue_num[qi] = val; } }
                            0x044 => {
                                if val == 1 { balloon_dev.activate_queue(qi); }
                                else if qi < 2 { balloon_dev.queue_ready[qi] = false; }
                            }
                            0x050 => {
                                // QueueNotify: 0=inflate, 1=deflate
                                if val == 0 { balloon_dev.process_inflate(); }
                                else if val == 1 { balloon_dev.process_deflate(); }
                            }
                            0x064 => { balloon_dev.interrupt_status &= !val; }
                            0x070 => {
                                if val == 0 {
                                    balloon_dev.status = 0;
                                    balloon_dev.queue_ready = [false, false];
                                } else {
                                    balloon_dev.status = val;
                                    if val == 15 { eprintln!("[NKR-BALLOON] ¡DRIVER_OK! Balloon listo."); }
                                }
                            }
                            0x080 => { if qi < 2 { balloon_dev.desc_low[qi] = val; } }
                            0x084 => { if qi < 2 { balloon_dev.desc_high[qi] = val; } }
                            0x090 => { if qi < 2 { balloon_dev.avail_low[qi] = val; } }
                            0x094 => { if qi < 2 { balloon_dev.avail_high[qi] = val; } }
                            0x0A0 => { if qi < 2 { balloon_dev.used_low[qi] = val; } }
                            0x0A4 => { if qi < 2 { balloon_dev.used_high[qi] = val; } }
                            _ => {}
                        }
                    }

                    // 8. VirtIO-Console (0xD0050000) — MmioWrite
                    if addr >= CONSOLE_MMIO_ADDR && addr < CONSOLE_MMIO_ADDR + 0x1000 {
                        let offset = addr - CONSOLE_MMIO_ADDR;
                        let val = if data.len() == 4 { u32::from_le_bytes(data.try_into().unwrap()) } else { 0 };
                        let qi = console_dev.queue_sel as usize;
                        match offset {
                            0x014 => { console_dev.device_features_sel = val; }
                            0x020 => {
                                if console_dev.driver_features_sel == 0 {
                                    console_dev.driver_features = (console_dev.driver_features & 0xFFFFFFFF_00000000) | (val as u64);
                                } else {
                                    console_dev.driver_features = (console_dev.driver_features & 0x00000000_FFFFFFFF) | ((val as u64) << 32);
                                }
                            }
                            0x024 => { console_dev.driver_features_sel = val; }
                            0x030 => { console_dev.queue_sel = val; }
                            0x038 => { if qi < 2 { console_dev.queue_num[qi] = val; } }
                            0x044 => {
                                if val == 1 {
                                    console_dev.queue_ready[qi] = true;
                                    if qi == 0 {
                                        eprintln!("[NKR-CTL] /dev/hvc0 receiveq lista — canal de control activo");
                                    }
                                } else if qi < 2 {
                                    console_dev.queue_ready[qi] = false;
                                }
                            }
                            0x050 => {} // QueueNotify transmitq: ignorar datos del guest
                            0x064 => { console_dev.interrupt_status &= !val; }
                            0x070 => {
                                if val == 0 {
                                    console_dev.status = 0;
                                    console_dev.queue_ready = [false, false];
                                } else {
                                    console_dev.status = val;
                                }
                            }
                            0x080 => { if qi < 2 { console_dev.desc_low[qi] = val; } }
                            0x084 => { if qi < 2 { console_dev.desc_high[qi] = val; } }
                            0x090 => { if qi < 2 { console_dev.avail_low[qi] = val; } }
                            0x094 => { if qi < 2 { console_dev.avail_high[qi] = val; } }
                            0x0A0 => { if qi < 2 { console_dev.used_low[qi] = val; } }
                            0x0A4 => { if qi < 2 { console_dev.used_high[qi] = val; } }
                            _ => {}
                        }
                    }
                }
            }

            Ok(VcpuExit::Hlt) => break,
            Ok(VcpuExit::Shutdown) => {
                eprintln!("\n[NKR] vCPU shutdown");
                break;
            }
            Ok(VcpuExit::SystemEvent(evt_type, _)) => {
                eprintln!("\n[NKR] vCPU SystemEvent type={}", evt_type);
                break;
            }
            Ok(other) => {
                eprintln!("[NKR] vCPU exit ignorado: {:?}", other);
            }
            Err(e) => {
                // EINTR = señal recibida (SIGTERM u otra señal benigna)
                // Dejar que el top del loop maneje SHUTDOWN_REQUESTED (inyección SysRq)
                let errno = e.errno();
                if errno == libc::EINTR || errno == 4 {
                    continue;
                }
                return Err(format!("vcpu.run() falló: {e}").into());
            }
        }
    }
    Ok(())
}
