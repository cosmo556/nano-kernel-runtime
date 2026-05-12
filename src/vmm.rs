// =============================================================================
// NKR VMM — Micro-VM engine with direct hardware access
// =============================================================================

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::convert::TryInto;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::path::Path;
use libc;

/// Global flag for clean shutdown (SIGTERM → vcpu loop exits → extract_volumes)
pub static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Global flag for live reload (SIGUSR1 → inject "REL_OD" via hvc0 → guest
/// triggers SIGHUP al master Odoo → workers respawnean con código fresh).
/// El daemon NKR setea SIGUSR1 al PID de la VM tras un addons/git exitoso o
/// cuando el panel llama POST /reload. Ver src/console.rs y nkr_api_server.rs.
pub static RELOAD_REQUESTED: AtomicBool = AtomicBool::new(false);

// ─── Ballooning ACTIVE/IDLE state machine (CLAUDE.md v2.2) ─────────────────
// La VM nace ACTIVE: balloon_mb es el target de boot (DEV=0, STAG=256, PROD=0).
// Esto evita que el OOM killer del guest masacre Odoo durante bootstrap, lo
// cual ocurriría si arrancara en IDLE (squeeze a 256 MB).
//
// SIGUSR2 (POST /balloon) → renueva BALLOON_ACTIVE_TS = now(). El vcpu loop:
//   - Si TS != 0 y now-TS < decay_secs → ACTIVE → target=BALLOON_ACTIVE_MB
//   - Si TS != 0 y now-TS >= decay_secs → IDLE auto → target=BALLOON_IDLE_MB
//
// Boot: TS se setea a now() — la VM cuenta como recién renovada y se queda
// ACTIVE durante el bootstrap completo (~60s) + grace period (10 min default
// hasta que `nkr compose` confirme NKR-READY y el panel mande su primer
// SIGUSR2). Tras 600s sin renovación, transición a IDLE.
//
// Si BALLOON_IDLE_MB == BALLOON_ACTIVE_MB (PROD por tier), el state machine
// no se activa — la VM se queda estática en balloon_mb del boot (=0).
// `BALLOON_LAST_APPLIED_STATE` evita raise_config_change redundante (0=idle, 1=active).
use std::sync::atomic::AtomicU32;
pub static BALLOON_ACTIVE_REQUESTED_TS: AtomicU64 = AtomicU64::new(0);
pub static BALLOON_LAST_APPLIED_STATE: AtomicU8 = AtomicU8::new(1); // boot = ACTIVE
pub static BALLOON_IDLE_MB: AtomicU32 = AtomicU32::new(0);
pub static BALLOON_ACTIVE_MB: AtomicU32 = AtomicU32::new(0);
pub static BALLOON_DECAY_SECS: AtomicU32 = AtomicU32::new(600);
/// Unix ts of the last statsq drain — we consume the balloon stats virtqueue
/// at most every BALLOON_STATS_INTERVAL_SECS (consuming on every guest kick
/// would ping-pong with the guest's refill).
static BALLOON_STATS_LAST_TS: AtomicU64 = AtomicU64::new(0);
const BALLOON_STATS_INTERVAL_SECS: u64 = 15;
/// 0=idle, 1=shutdown injected (waiting for VcpuExit::Shutdown or timeout)
use std::sync::atomic::AtomicU8;
static SHUTDOWN_PHASE: AtomicU8 = AtomicU8::new(0);
/// Time (ms since UNIX_EPOCH) when SHUTDOWN_REQUESTED was first marked
use std::sync::atomic::AtomicU64;
use std::time::{SystemTime, UNIX_EPOCH};
static SHUTDOWN_STARTED_MS: AtomicU64 = AtomicU64::new(0);
/// Last moment (ms) at which we re-injected "SHUTDOWN\n" after SIGTERM.
/// Used to retry every 2s if the guest watcher did not respond.
static SHUTDOWN_LAST_REINJECT_MS: AtomicU64 = AtomicU64::new(0);
/// Host VirtIO-FS rootfs path — used to write the shutdown sentinel
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

// Fixed memory layout for Linux Boot Protocol x86_64
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

/// Ensures the cell's bridge exists with its subnet and NAT enabled.
/// cell_id=0 uses the legacy `nkr0` bridge (10.0.0.0/24).
/// cell_id>0 uses `nkr-br{cell_id}` (10.0.{cell_id}.0/24).
fn ensure_bridge(cell_id: u8) -> Result<(), Box<dyn std::error::Error>> {
    crate::cell::ensure_cell_bridge(cell_id)?;

    // KSM intentionally NOT enabled. Reason: NKR maps guest RAM via
    // memfd_create + MAP_SHARED (required by vhost-user SET_MEM_TABLE so
    // virtiofsd can access virtio buffers). The kernel rejects
    // MADV_MERGEABLE (silent EINVAL) on VMAs with VM_SHARED, so KSM
    // never receives pages to scan and only burns kmmod CPU.
    //
    // The real density comes from virtio-fs + DAX (dedupes Python binaries,
    // .pyc, shared libs) and, in the future, pre-compile of QWeb assets at
    // build-time. See CLAUDE.md §1 for details.
    //
    // If you move to hybrid memory (anon-private + memfd-shared) in the future,
    // re-enable here with metrics::ksm_enable().

    Ok(())
}

// =============================================================================
// Volumes — Pre-boot injection and post-shutdown extraction
// =============================================================================

/// Parsed volume with mode (ro = inject only, rw = inject + extract)
#[derive(Clone)]
struct VolumeMount {
    host_path: String,
    guest_path: String,
    read_write: bool,
}

/// Parses volume specs: "host:guest" (ro default) or "host:guest:rw"
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
        // For rw volumes, create the host directory if it doesn't exist
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

/// Cleans up orphaned mounts from NKR processes that no longer exist.
///
/// When an NKR process dies abruptly (SIGKILL, crash), the directory
/// /tmp/nkr_vol_{pid} stays mounted. If the same disk is mounted again
/// (next `nkr run`) we get two simultaneous mounts → inode corruption.
///
/// This helper scans /tmp/nkr_vol_* on startup and cleans up the orphans.
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

        // Only directories matching nkr_vol_{number} or nkr_inspect_gen_{number}
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

        // If the process is still alive, don't touch
        let alive = unsafe { libc::kill(pid as i32, 0) == 0 };
        if alive {
            continue;
        }

        // Dead process → unmount and delete
        eprintln!("[NKR-VOL] Limpiando mount huérfano de PID {} muerto: {}", pid, path.display());

        // Try umount (may fail if already unmounted)
        let _ = std::process::Command::new("umount")
            .args(["--lazy", &path.to_string_lossy()])
            .status();

        // Remove directory (may not be empty if lazy umount hasn't detached yet)
        let _ = std::fs::remove_dir(&path);
    }
}

/// Mounts the root disk and executes an operation over the mount point.
/// Abstracts mount/umount for reuse in inject and extract.
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

    // Always unmount, even if the operation failed
    let _ = std::process::Command::new("umount").arg(&mount_dir).status();
    let _ = std::fs::remove_dir(&mount_dir);

    result
}

/// Injects volumes into the root disk BEFORE boot.
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
                // If the host directory has content, copy it to the guest
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

/// Extracts rw volumes from the root disk AFTER shutdown.
/// Only extracts those marked as :rw
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

            // Ensure the host directory exists
            if Path::new(&source).is_dir() {
                let _ = std::fs::create_dir_all(&vol.host_path);
                // rsync-like: cp -a preserving permissions
                let status = std::process::Command::new("cp")
                    .args(["-a", &format!("{}/.", source), &format!("{}/.", vol.host_path)])
                    .status();
                match status {
                    Ok(s) if s.success() => {
                        eprintln!("[NKR-VOL] ← {}/ → {}/ (extraído)", vol.guest_path, vol.host_path);
                        // Make it accessible to the host user (guest UIDs don't exist here)
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

/// Injects environment variables as /etc/nkr-env into the root disk.
/// The initramfs runs `source /etc/nkr-env` before launching the service.
fn inject_env_vars(root_disk: &str, env_vars: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    if env_vars.is_empty() {
        return Ok(());
    }

    eprintln!("[NKR-ENV] Inyectando {} variable(s) de entorno en {}...", env_vars.len(), root_disk);

    let vars = env_vars.to_vec();
    with_mounted_disk(root_disk, |mount_dir| {
        let env_path = format!("{}/etc/nkr-env", mount_dir);
        // Ensure /etc exists
        let _ = std::fs::create_dir_all(format!("{}/etc", mount_dir));

        let mut content = String::from("# NKR environment variables (auto-generated)\n");
        for var in &vars {
            if let Some(eq_pos) = var.find('=') {
                let key = &var[..eq_pos];
                let val = &var[eq_pos + 1..];
                // Escape single quotes in the value
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

/// Parses "host_port:guest_port" and sets up iptables DNAT exclusively on 127.0.0.1.
///
/// Localhost-only security: mapped ports are NEVER exposed on the host's public IP.
/// Only local processes (Nginx, SSH tunnels) can connect via 127.0.0.1:host_port.
/// If a VM has no ports defined, it is only accessible by its internal IP 10.0.0.x.
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

        // Serializes the DNAT + MASQUERADE pair to prevent two concurrent
        // processes with the same failed -C from both ending up doing -A
        // (duplicate rules).
        let _netlock = crate::netlock::NetLock::acquire("port-fwd-setup");

        // DNAT in OUTPUT: local host processes (Nginx, curl) that connect to
        // 127.0.0.1:host_port get redirected to the guest. This does NOT affect the public IP.
        // Requires route_localnet=1 (already set in ensure_bridge).
        let dnat_local = crate::netlock::iptables()
            .args(["-t", "nat", "-C", "OUTPUT",
                   "-p", "tcp", "-d", "127.0.0.1",
                   "--dport", &host_port.to_string(),
                   "-j", "DNAT", "--to-destination", &dest])
            .status()
            .and_then(|s| {
                if !s.success() {
                    crate::netlock::iptables()
                        .args(["-t", "nat", "-A", "OUTPUT",
                               "-p", "tcp", "-d", "127.0.0.1",
                               "--dport", &host_port.to_string(),
                               "-j", "DNAT", "--to-destination", &dest])
                        .status()
                } else {
                    Ok(s)
                }
            });

        // MASQUERADE so the guest sees the bridge IP as source
        let masq = crate::netlock::iptables()
            .args(["-t", "nat", "-C", "POSTROUTING",
                   "-p", "tcp", "-d", guest_ip,
                   "--dport", &guest_port.to_string(),
                   "-j", "MASQUERADE"])
            .status()
            .and_then(|s| {
                if !s.success() {
                    crate::netlock::iptables()
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

/// Cleans up iptables rules created for port forwarding (localhost-only).
/// Also attempts to delete legacy PREROUTING rules (code prior to the security fix),
/// best-effort, to avoid orphaned rules when stopping VMs started with old code.
fn cleanup_port_forwarding(rules: &[(u16, u16)], guest_ip: &str) {
    for (host_port, guest_port) in rules {
        let dest = format!("{}:{}", guest_ip, guest_port);

        let _netlock = crate::netlock::NetLock::acquire("port-fwd-cleanup");

        // Delete OUTPUT rule (current localhost-only mode)
        let _ = crate::netlock::iptables()
            .args(["-t", "nat", "-D", "OUTPUT",
                   "-p", "tcp", "-d", "127.0.0.1",
                   "--dport", &host_port.to_string(),
                   "-j", "DNAT", "--to-destination", &dest])
            .status();

        // Delete legacy PREROUTING rule (best-effort: silent if it doesn't exist)
        let _ = crate::netlock::iptables()
            .args(["-t", "nat", "-D", "PREROUTING",
                   "-p", "tcp", "--dport", &host_port.to_string(),
                   "-j", "DNAT", "--to-destination", &dest])
            .status();

        let _ = crate::netlock::iptables()
            .args(["-t", "nat", "-D", "POSTROUTING",
                   "-p", "tcp", "-d", guest_ip,
                   "--dport", &guest_port.to_string(),
                   "-j", "MASQUERADE"])
            .status();

        eprintln!("[NKR-NET] Port forward limpiado: 127.0.0.1:{} → {}:{}", host_port, guest_ip, guest_port);
    }
}

/// e2fsck -p on an ext4 image before opening it RW. Prevents mounting a corrupted FS
/// after kernel panics or hard power-offs (critical with ^has_journal).
/// Exit codes: 0=clean, 1=auto-fixed, >=2 aborts (includes 8=busy → split-brain).
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

/// Mounts the master rootfs under an exclusive flock. Cleans up orphaned loops of the same file
/// before retrying. Eliminates the cold-start race when N VMs start in parallel.
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

/// Runs a micro-VM with the given configuration
pub fn run(mut config: VmConfig) -> Result<(), Box<dyn std::error::Error>> {
    let ram_bytes = config.ram_mb as usize * 1024 * 1024;

    // 1b. Smart default v1.4: Convert the main .ext4 disk into a Master RootFS (VirtIO-FS) before processing vars
    if config.rootfs.is_none() && !config.disks.is_empty() {
        let first_disk = config.disks[0].clone();
        if first_disk.ends_with(".ext4") {
            // "Golden Image": Single Master Mount
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
                config.disks.remove(0); // Remove from list so it's not handled as /dev/vda
            } else {
                eprintln!("[NKR] WARN: Falló el loop mount de {}, cayendo a VirtIO-Block", first_disk);
            }
        }
    }

    // fsck on per-VM RW disks (excludes shared rootfs, already removed above)
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

    // --- CPU Pinning: Assign chrs to physical cores (NUMA locality) ---
    pin_cpu_chrs(config.chrs)?;

    // --- cgroupv2: CPU bursting + I/O throttling + memory.max (Features 2 and 4B) ---
    setup_cgroup(&config.name, config.chrs, config.ram_mb, std::process::id(), &config.disks, config.burst);

    // --- Bridge auto-setup (per-cell) ---
    ensure_bridge(config.cell_id)?;

    // --- Clean up orphaned mounts of dead NKR processes ---
    cleanup_orphaned_mounts();

    // --- Inject volumes (pre-boot: mount disk, copy, unmount) ---
    let parsed_volumes = parse_volume_specs(&config.volumes);
    if !parsed_volumes.is_empty() && !config.disks.is_empty() {
        inject_volumes(&config.disks[0], &parsed_volumes)?;
    }

    // --- Inject environment variables ---
    if !config.env_vars.is_empty() {
        if !config.disks.is_empty() {
            // Disk mode: inject into /etc/nkr-env inside the ext4
            inject_env_vars(&config.disks[0], &config.env_vars)?;
        } else if config.rootfs.is_some() {
            // Shared rootfs mode: write nkr-env to ALL host share dirs.
            // The init scans each mount looking for nkr-env; writing to all of them ensures
            // it gets found regardless of slot order (FS0, FS1, ...).
            let mut content = String::from("# NKR environment variables (auto-generated)\n");
            for var in &config.env_vars {
                if let Some(eq_pos) = var.find('=') {
                    let key = &var[..eq_pos];
                    let val = &var[eq_pos + 1..];
                    let escaped = val.replace('\'', "'\\''");
                    content.push_str(&format!("export {}='{}'\n", key, escaped));
                }
            }
            let mut written = 0;
            for share in &config.shares {
                let host = share.splitn(2, ':').next().unwrap_or("");
                if host.is_empty() { continue; }
                if !std::fs::metadata(host).map(|m| m.is_dir()).unwrap_or(false) { continue; }
                let env_path = format!("{}/nkr-env", host);
                match std::fs::write(&env_path, &content) {
                    Ok(_) => {
                        written += 1;
                        eprintln!("[NKR-ENV] Escrito: {} ({} bytes)", env_path, content.len());
                        // Verify the file was actually written
                        match std::fs::read(&env_path) {
                            Ok(data) => {
                                if data.len() != content.len() {
                                    eprintln!("[NKR-ENV] BUG: archivo {} tiene {} bytes, esperaba {}",
                                        env_path, data.len(), content.len());
                                }
                            }
                            Err(e) => eprintln!("[NKR-ENV] BUG: no se puede releer {}: {}", env_path, e),
                        }
                    }
                    Err(e) => eprintln!("[NKR-ENV] WARN: no se pudo escribir {}: {}", env_path, e),
                }
            }
            if written > 0 {
                eprintln!("[NKR-ENV] Env vars escritas en {} share(s) ({} bytes)", written, content.len());
            } else {
                eprintln!("[NKR-ENV] WARN: no se encontró ningún share dir para escribir nkr-env");
            }
        }
    }

    let kvm = Kvm::new().map_err(|e| format!("Fallo al abrir /dev/kvm: {e}"))?;
    let vm = kvm.create_vm().map_err(|e| format!("Fallo KVM_CREATE_VM: {e}"))?;

    // Virtual motherboard: Interrupts and Clock
    vm.create_irq_chip().map_err(|e| format!("Fallo al crear IRQ chip: {e}"))?;
    let pit_config = kvm_bindings::kvm_pit_config { flags: 0, ..Default::default() };
    vm.create_pit2(pit_config).map_err(|e| format!("Fallo al crear PIT: {e}"))?;

    // 1. Initialize RAM (lazy allocation)
    //
    // vm-memory uses MAP_ANONYMOUS|MAP_NORESERVE internally: physical pages
    // are only allocated when the guest touches them (EPT fault → host page fault → alloc).
    // With MAP_NORESERVE, 50 VMs × 256 MB do not consume 12 GB of commit limit.
    //
    // After creating the regions we apply two additional madvise calls:
    //   MADV_MERGEABLE → KSM (Kernel Same-page Merging). NOTE: the call is
    //                    KEPT for documentation/future use, but it returns
    //                    EINVAL silently here because guest RAM is backed by
    //                    memfd+MAP_SHARED (required by vhost-user
    //                    SET_MEM_TABLE for virtiofsd). The kernel rejects
    //                    MERGEABLE on VMAs with VM_SHARED. CLAUDE.md §1
    //                    documents this — KSM would require a hybrid layout
    //                    (anon-private + memfd-shared regions).
    //   MADV_NOHUGEPAGE → avoids THP (Transparent HugePages): a 2MB THP can only
    //                     be freed if all 512 subpages are cold simultaneously.
    //                     With granular 4KB pages, the kernel can evict page by page.
    let high_mem = ram_bytes - 0x100000;

    // Guest memory backed by memfd (sharable with virtiofsd via vhost-user SET_MEM_TABLE)
    // memfd_create(SYS=319): creates an anonymous fd of size ram_bytes.
    // The two regions (0..0xA0000 and 0x100000..end) use GPA-aligned offsets in the memfd.
    //
    // MFD_CLOEXEC (=1): close-on-exec. Without it, any future fork+exec from
    // this process (e.g. spawning a helper) would inherit the memfd, leaking
    // the entire guest RAM into the child. The duplicated FDs handed to
    // virtiofsd via SET_MEM_TABLE travel over a Unix socket, not via fork —
    // so this flag is purely defense-in-depth against future code paths.
    const MFD_CLOEXEC: i64 = 1;
    let memfd = unsafe {
        libc::syscall(319i64 /*SYS_memfd_create*/,
            b"nkr_guest\0".as_ptr() as i64,
            MFD_CLOEXEC) as i32
    };
    if memfd < 0 {
        return Err(format!("memfd_create falló: {}", std::io::Error::last_os_error()).into());
    }
    if unsafe { libc::ftruncate(memfd, ram_bytes as i64) } < 0 {
        return Err(format!("ftruncate memfd falló: {}", std::io::Error::last_os_error()).into());
    }
    // Duplicate with F_DUPFD_CLOEXEC so the duplicates inherit close-on-exec
    // from the parent memfd. Plain dup() resets CLOEXEC, which would leak the
    // FDs across any future fork+exec.
    let dup0 = unsafe { libc::fcntl(memfd, libc::F_DUPFD_CLOEXEC, 0) };
    let dup1 = unsafe { libc::fcntl(memfd, libc::F_DUPFD_CLOEXEC, 0) };
    if dup0 < 0 || dup1 < 0 {
        return Err(format!("F_DUPFD_CLOEXEC falló: {}", std::io::Error::last_os_error()).into());
    }
    let file0 = unsafe { File::from_raw_fd(dup0) };
    let file1 = unsafe { File::from_raw_fd(dup1) };
    let guest_mem = Arc::new(GuestMemoryMmap::<()>::from_ranges_with_files(&[
        (GuestAddress(0),        0xA0000,   Some(FileOffset::new(file0, 0))),
        (GuestAddress(0x100000), high_mem,  Some(FileOffset::new(file1, 0x100000))),
    ]).map_err(|e| format!("Fallo al crear guest memory: {e}"))?);

    // Build region table for vhost-user SET_MEM_TABLE
    let mem_regions: Vec<GuestMemRegion> = guest_mem.iter().map(|r| GuestMemRegion {
        gpa: r.start_addr().0,
        size: r.size(),
        hva: r.as_ptr() as u64,
        memfd_offset: r.start_addr().0, // memfd offset = GPA (by design)
    }).collect();

    // Apply lazy/KSM hints to each guest RAM region
    for region in guest_mem.iter() {
        let ptr = region.as_ptr() as *mut libc::c_void;
        let len = region.size();
        unsafe {
            // KSM (no-op on MAP_SHARED — see comment above; kept for future
            // hybrid memory layout that re-enables it).
            libc::madvise(ptr, len, libc::MADV_MERGEABLE);
            // No THP: 4KB pages → granular eviction under memory pressure
            libc::madvise(ptr, len, libc::MADV_NOHUGEPAGE);
        }
    }

    // 2. Virtio disks (multiple volumes)
    let mut block_devs = Vec::new();
    let mut block_configs = Vec::new(); // To generate the cmdline
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

    // 2b. Sharing devices — VirtIO-FS via virtiofsd (vhost-user)
    // MMIO base: 0xD0010000, IRQ: 8+i
    let mut fs_devs: Vec<VirtioFsDevice> = Vec::new();
    let mut fs_shares: Vec<(String, String, bool, usize)> = Vec::new();  // VirtIO-FS: (tag, guest_path, readonly, slot)
    let mut blk_share_mounts: Vec<(String, String)> = Vec::new(); // BLK: (dev_path, guest_mnt)
    let base_share_addr: u64 = 0xD001_0000;
    let base_share_irq: u32  = VIRTIO_FS_BASE_IRQ;

    std::fs::create_dir_all("/run/nkrfs").ok();

    // 2b-pre. Shared VirtIO-FS rootfs (if config.rootfs is defined)
    // The rootfs is mounted as / (RO) in the guest via VirtIO-FS with cache=auto.
    // This lets 100+ VMs share the same code with 1 copy in page cache.
    let rootfs_slot_count: usize;
    if let Some(ref rootfs_path) = config.rootfs {
        let tag = format!("nkrfsc{}v{}s0", config.cell_id, config.vm_id); // cell_id+vm_id avoids cross-cell socket collision
        let irq = base_share_irq;
        let addr = base_share_addr;

        let regions: Vec<GuestMemRegion> = mem_regions.iter().map(|r| GuestMemRegion {
            gpa: r.gpa, size: r.size, hva: r.hva, memfd_offset: r.memfd_offset,
        }).collect();

        let rootfs_abs = std::fs::canonicalize(rootfs_path)
            .unwrap_or_else(|_| panic!("Rootfs path inválido: {}", rootfs_path))
            .to_string_lossy().into_owned();

        // Register the path so the shutdown handler can write the sentinel
        let _ = SHUTDOWN_ROOTFS_PATH.set(rootfs_abs.clone());

        let mut fs_dev = VirtioFsDevice::new(
            &tag, &rootfs_abs, guest_mem.clone(),
            // F_DUPFD_CLOEXEC: keep close-on-exec on the duplicate so the
            // memfd is not silently inherited by any future fork+exec from
            // this process. The intended path to virtiofsd is SCM_RIGHTS.
            unsafe { libc::fcntl(memfd, libc::F_DUPFD_CLOEXEC, 0) },
            regions,
            "auto", // shared rootfs: host page cache shared between VMs
            256 * 1024 * 1024, // 256 MB DAX — RO binaries only
            false, // writeback=false — rootfs is RO
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

            fs_shares.push((tag, "/".to_string(), true, 0)); // rootfs always RO, slot 0
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
        let slot = i + rootfs_slot_count; // offset by the rootfs device if present
        // Format: host_path:guest_path[:ro|:rw]  (default: rw)
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
        let tag = format!("nkrfsc{}v{}s{}", config.cell_id, config.vm_id, slot);
        let irq  = base_share_irq + slot as u32;
        let addr = base_share_addr + (slot as u64 * 0x1000);

        // Auto-detection: .ext4 file → VirtIO-BLK (no virtiofsd, no DAX)
        //                 directory    → VirtIO-FS  (current behavior)
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

        // Clone mem_regions for this device (VirtioFsDevice takes ownership)
        let regions: Vec<GuestMemRegion> = mem_regions.iter().map(|r| GuestMemRegion {
            gpa: r.gpa, size: r.size, hva: r.hva, memfd_offset: r.memfd_offset,
        }).collect();

        // Try VirtIO-FS (virtiofsd is launched automatically)
        let cache = if readonly { "auto" } else { "never" };
        let mut fs_dev = VirtioFsDevice::new(
            &tag, &host_abs, guest_mem.clone(),
            // F_DUPFD_CLOEXEC: same rationale as the rootfs path above —
            // duplicate carries close-on-exec so it doesn't leak to children.
            unsafe { libc::fcntl(memfd, libc::F_DUPFD_CLOEXEC, 0) },
            regions,
            cache, // RO→auto (shared page cache), RW→never (direct writes)
            512 * 1024 * 1024, // 512 MB DAX — data buffer
            !readonly, // writeback only for RW shares
        );

        if fs_dev.is_connected() {
            eprintln!("[NKR] FS[{}]: '{}' → guest:'{}' vía VirtIO-FS [MMIO {:#X}, IRQ {}] ({})",
                slot, host_abs, guest_path, addr, irq, if readonly { "RO" } else { "RW" });

            fs_dev.mmio_addr = addr;
            // Register DAX window as KVM slot 3+slot if active
            if fs_dev.dax_enabled {
                // Separate DAX windows per slot: rootfs=256MB(slot0), data=512MB each
                // Fixed 1GB offset per slot to avoid overlap with any size
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

            // One irqfd for the device's IRQ line
            vm.register_irqfd(&fs_dev.call, irq)
                .unwrap_or_else(|e| panic!("Fallo irqfd virtio-fs {}: {}", slot, e));
            // Two ioeventfds: one per queue (datamatch=queue index)
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

    // 3. Virtio network — dynamic MAC and IP from cell_id + vm_id
    let guest_ip = crate::registry::id_to_ip(config.cell_id, config.vm_id);
    let mac: [u8; 6] = [0x52, 0x54, 0x00, config.cell_id, 0x34, config.vm_id];
    let bridge_name = crate::cell::cell_bridge_name(config.cell_id);

    // Auto-create TAP if not specified. Name includes cell_id to avoid
    // collisions between cells (cell 1 vm 5 != cell 2 vm 5).
    let auto_tap_name = if config.tap_name.is_none() {
        let tap_name = if config.cell_id == 0 {
            format!("nkr-tap{}", config.vm_id)
        } else {
            format!("nkr-c{}-tap{}", config.cell_id, config.vm_id)
        };
        // Serializes netlink/iptables/tc between concurrent nkr run processes.
        // Covers: delete→add of the tap, bridge join, set up, and L2 isolation.
        let _netlock = crate::netlock::NetLock::acquire("tap-create");
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
        // Install L2 isolation via tc/ebtables (under the same netlock)
        setup_tap_isolation(&tap_name, &mac, &guest_ip);
        eprintln!("[NKR] Auto-TAP: {} (bridge {})", tap_name, bridge_name);
        Some(tap_name)
    } else { None };

    let effective_tap = config.tap_name.as_deref()
        .or(auto_tap_name.as_deref());

    let mut net_dev = VirtioNetDevice::new(guest_mem.clone(), mac, effective_tap);
    eprintln!("[NKR] Red: MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} IP {}",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5], guest_ip);

    // IRQ/MMIO already configured in the loop for block
    vm.register_irqfd(&net_dev.irqfd, 5).expect("Fallo irqfd net");
    vm.register_ioevent(&net_dev.ioeventfd, &IoEventAddress::Mmio(0xD0000050), 0u64)
        .expect("Fallo ioeventfd net");

    // Serial IRQ
    let serial_irqfd = vmm_sys_util::eventfd::EventFd::new(libc::EFD_NONBLOCK).expect("serial irqfd");
    vm.register_irqfd(&serial_irqfd, 4).expect("Fallo irqfd serial");

    register_guest_memory(&vm, &guest_mem)?;
    eprintln!("[NKR] RAM: {} MB mapeados", ram_bytes >> 20);

    // Feature A — VirtIO-PMEM + DAX: optional via config.use_pmem.
    //
    // PMEM maps the guest disk into the host's address space (mmap MAP_SHARED).
    // The guest accesses the rootfs via DAX (zero-copy, no duplicated page cache inside the guest).
    //
    // RAM trade-off in multi-tenant:
    //   - WITHOUT pmem: VirtIO-Block uses io_uring. The guest has its own page cache (~150-200 MB
    //     extra per VM), but the host mmap does NOT exist → no host page cache pressure.
    //   - WITH pmem: Zero duplicated page cache in the guest, but the entire disk (e.g. 6 GB)
    //     stays mapped on the host. With 50 VMs × 6 GB = 300 GB of file-backed mmap that
    //     competes with the anonymous RAM of the guests.
    //
    // Recommendation for ≥20 VMs: pmem=false (VirtIO-Block) + KSM active.
    // pmem=true only makes sense with few VMs and small disks (< 2 GB).
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

    // 2c. VirtIO-Balloon (v1.3) — elastic RAM reclaim
    // Advertised to the guest if --balloon-mb > 0 (estático) O si hay
    // ballooning dinámico configurado (--balloon-idle-mb != --balloon-mb): en
    // ese caso la VM nace ACTIVE con target = balloon_mb (típicamente 0 en
    // tier=dev) pero el state machine necesita el driver del guest attacheado
    // para poder inflar al transicionar a IDLE. Si NO emitimos el MMIO device
    // en el cmdline cuando balloon_mb==0, el guest nunca probea el driver y el
    // `set_target_mb(idle)` posterior es un no-op (bug pre-v1.6.4: tier=dev
    // "tenía" ballooning dinámico que en realidad nunca inflaba).
    // We pass ram_mb so the device caps its HashSet of inflated pages to the
    // guest's total RAM — defense against host DoS from a hostile guest.
    let balloon_dynamic = config.balloon_idle_mb != 0
        && config.balloon_idle_mb != config.balloon_mb;
    let balloon_advertised = config.balloon_mb > 0 || balloon_dynamic;
    let mut balloon_dev = VirtioBalloonDevice::new(guest_mem.clone(), config.ram_mb);
    if config.balloon_mb > 0 {
        balloon_dev.set_target_mb(config.balloon_mb);
    }
    if balloon_advertised {
        eprintln!("[NKR] Balloon: objetivo boot {} MB{} [MMIO {:#X}, IRQ {}]",
            config.balloon_mb,
            if balloon_dynamic { format!(" (dinámico, IDLE={} MB)", config.balloon_idle_mb) } else { String::new() },
            BALLOON_MMIO_ADDR, BALLOON_IRQ);
    }
    vm.register_irqfd(&balloon_dev.irqfd, BALLOON_IRQ)
        .unwrap_or_else(|e| eprintln!("[NKR-BALLOON] irqfd: {e}"));
    vm.register_ioevent(
        &balloon_dev.ioeventfd,
        &IoEventAddress::Mmio(BALLOON_MMIO_ADDR + 0x50),
        0u64,
    ).unwrap_or_else(|e| eprintln!("[NKR-BALLOON] ioeventfd: {e}"));


    // 3. Kernel — auto-detect format (bzImage or ELF vmlinux)
    // Smart default v1.3: if the specified kernel points to "bzImage" and a
    // vmlinux exists in the central store, prefer vmlinux (−20 ms boot).
    let effective_kernel = smart_resolve_kernel(&config.kernel_path);
    let kernel_fmt = detect_kernel_format(&effective_kernel);
    let entry_addr = match kernel_fmt {
        KernelFormat::BzImage => load_bzimage_kernel(&guest_mem, &effective_kernel)?,
        KernelFormat::Elf     => load_elf_kernel(&guest_mem, &effective_kernel)?,
    };

    // 4. Initramfs (auto-detect or use specified)
    let initrd_size = load_initramfs_auto(&guest_mem, &config.initramfs_path, ram_bytes)?;

    // 5. Boot protocol
    let rootfs_tag = if config.rootfs.is_some() {
        Some(format!("nkrfsc{}v{}s0", config.cell_id, config.vm_id))
    } else {
        None
    };
    configure_linux_boot(&guest_mem, initrd_size, ram_bytes, &guest_ip, &block_configs, &fs_shares, &blk_share_mounts, pmem_dev_opt.as_ref(), balloon_advertised, rootfs_tag.as_deref())?;
    write_page_tables(&guest_mem)?;
    write_gdt(&guest_mem)?;

    // E820 readback to verify the kernel receives the correct values
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
    // Configure CPUID with KVM signature to enable kvmclock (in-kernel in KVM)
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
    // bzImage boots in 32-bit protected mode; ELF vmlinux in 64-bit long mode
    match kernel_fmt {
        KernelFormat::BzImage => configure_sregs(&vcpu)?,
        KernelFormat::Elf     => configure_sregs_64(&vcpu)?,
    }
    configure_regs(&vcpu, entry_addr)?;

    eprintln!("[NKR] vCPU lista — RIP={entry_addr:#X}");

    // Port forwarding
    let forwarding_rules = setup_port_forwarding(&config.port_forwards, &guest_ip);

    // Register VM in global state
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
        use_pmem: pmem_dev_opt.is_some(), // true if PMEM was enabled (Smart Default or explicit)
        use_dax: pmem_dev_opt.is_some() || config.rootfs.is_some(), // DAX via pmem OR virtio-fs master rootfs
        balloon_mb: config.balloon_mb,
        cell_id: config.cell_id,
        guest_mem_total_bytes: 0,
        guest_mem_free_bytes: 0,
        guest_mem_available_bytes: 0,
        guest_mem_cached_bytes: 0,
    };
    if let Err(e) = state::register_vm(&vm_state) {
        eprintln!("[NKR] ERROR: No se pudo registrar VM en estado — 'nkr ps' no mostrará esta VM: {e}");
    }

    eprintln!("════════════════════════════════════════════════════════════════");

    // Register SIGTERM handler for clean shutdown
    SHUTDOWN_REQUESTED.store(false, Ordering::SeqCst);
    SHUTDOWN_PHASE.store(0, Ordering::SeqCst);
    SHUTDOWN_STARTED_MS.store(0, Ordering::SeqCst);
    SHUTDOWN_LAST_REINJECT_MS.store(0, Ordering::SeqCst);

    // --- VirtIO Console (host→guest control channel, /dev/hvc0) ---
    let mut console_dev = VirtioConsoleDevice::new(guest_mem.clone());
    vm.register_irqfd(&console_dev.irqfd, CONSOLE_IRQ)
        .expect("[NKR-CTL] irqfd console falló");
    vm.register_ioevent(&console_dev.ioeventfd, &IoEventAddress::Mmio(CONSOLE_MMIO_ADDR + 0x50), 1u64)
        .expect("[NKR-CTL] ioeventfd console falló");
    eprintln!("[NKR] Control: canal hvc0 activo [MMIO {:#X}, IRQ {}]", CONSOLE_MMIO_ADDR, CONSOLE_IRQ);
    unsafe {
        libc::signal(libc::SIGTERM, sigterm_handler as *const () as libc::sighandler_t);
        // SIGUSR1 → reload de workers Odoo. El daemon lo manda tras
        // addons/git exitoso o vía POST /reload del panel.
        libc::signal(libc::SIGUSR1, sigusr1_handler as *const () as libc::sighandler_t);
        // SIGUSR2 → marca balloon ACTIVE (renueva timestamp). Se procesa en
        // el vcpu loop si la VM tiene ballooning dinámico configurado.
        libc::signal(libc::SIGUSR2, sigusr2_handler as *const () as libc::sighandler_t);
    }
    // Configura state machine de balloon. Sólo se activa el polling si
    // idle_mb != balloon_mb (= sólo cuando la VM realmente cambia de target).
    if config.balloon_idle_mb != 0 && config.balloon_idle_mb != config.balloon_mb {
        BALLOON_ACTIVE_MB.store(config.balloon_mb, Ordering::SeqCst);
        BALLOON_IDLE_MB.store(config.balloon_idle_mb, Ordering::SeqCst);
        BALLOON_DECAY_SECS.store(config.balloon_decay_secs, Ordering::SeqCst);
        // Estado inicial: ACTIVE — la VM ya arrancó con
        // balloon_dev.set_target_mb(balloon_mb) arriba (= ACTIVE), así que
        // marcamos LAST_APPLIED_STATE=1 (active) para que el state machine
        // no haga un re-apply redundante en el primer iter.
        BALLOON_LAST_APPLIED_STATE.store(1, Ordering::SeqCst);
        // TS=now() arranca el reloj de decay. La VM se queda ACTIVE durante
        // los próximos `decay_secs` (default 600s = 10 min) — suficiente
        // para todo el bootstrap de Odoo (~60s) + grace para que el panel
        // mande su primer POST /balloon. Si pasan 600s sin renovación,
        // transición automática a IDLE.
        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        BALLOON_ACTIVE_REQUESTED_TS.store(now_secs, Ordering::SeqCst);
        // Itimer cada 5s para asegurar que el vcpu loop se despierte y
        // chequee decay incluso si el guest está en HLT (sin tráfico).
        // SIGALRM ya tiene handler (sigalrm_handler — no-op solo para EINTR).
        arm_balloon_itimer();
        eprintln!("[NKR-BALLOON] Dinámico activado: ACTIVE(boot)={} MB, IDLE(post-decay)={} MB, decay={}s. \
                   VM nace ACTIVE — grace period hasta primer decay.",
            config.balloon_mb, config.balloon_idle_mb, config.balloon_decay_secs);
    }

    // Feature D — Seccomp Jailer: baseline raised to 120 syscalls for PostgreSQL.
    // Installed HERE (after thread::spawn) so the RX thread inherits it.
    if let Err(e) = crate::seccomp::install_seccomp_filter() {
        eprintln!("[NKR] WARN: seccomp no disponible: {e}");
    }

    // Capture the vCPU loop result but ALWAYS run cleanup.
    // Previously, `?` caused early-return skipping unregister_vm, TAP cleanup
    // and cgroup teardown, leaving orphaned resources.
    let loop_result = run_vcpu_loop(&mut vcpu, &mut block_devs, &mut net_dev, &mut fs_devs, &mut balloon_dev, &mut console_dev, pmem_dev_opt.as_mut(), &serial_irqfd, config.cell_id, config.vm_id);
    eprintln!("[NKR-DBG] vcpu_loop salió — iniciando cleanup");

    // Clean up shutdown sentinel from VirtIO-FS rootfs (if it existed)
    if let Some(rootfs_path) = SHUTDOWN_ROOTFS_PATH.get() {
        let sentinel = format!("{}/.nkr-shutdown", rootfs_path);
        let _ = std::fs::remove_file(&sentinel);
    }

    // Clean up port forwarding
    eprintln!("[NKR-DBG] cleanup_port_forwarding...");
    cleanup_port_forwarding(&forwarding_rules, &guest_ip);
    eprintln!("[NKR-DBG] cleanup_port_forwarding OK");

    // Extract rw volumes (post-shutdown: mount disk, copy guest→host)
    // Only if there's a disk — with shared rootfs (promoted ext4), config.disks is empty.
    if !parsed_volumes.is_empty() && !config.disks.is_empty() {
        eprintln!("[NKR-DBG] extract_volumes...");
        extract_volumes(&config.disks[0], &parsed_volumes).unwrap_or_else(|e| {
            eprintln!("[NKR-VOL] WARN: Error extrayendo volúmenes: {e}");
        });
        eprintln!("[NKR-DBG] extract_volumes OK");
    }

    // Clean up auto-created TAP + ebtables rules (under netlock)
    if let Some(ref tap_name) = auto_tap_name {
        eprintln!("[NKR-DBG] teardown_tap...");
        let _netlock = crate::netlock::NetLock::acquire("tap-teardown");
        teardown_tap_isolation(tap_name, &mac, &guest_ip);
        let _ = std::process::Command::new("timeout")
            .args(["5", "ip", "link", "delete", tap_name])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        eprintln!("[NKR] TAP {} eliminado", tap_name);
    }

    eprintln!("[NKR-DBG] unregister_vm...");
    // Unregister VM from global state (always, even on error)
    state::unregister_vm(config.vm_id);

    // Clean up cgroup
    if config.chrs > 0 {
        eprintln!("[NKR-DBG] teardown_cgroup...");
        teardown_cgroup(&config.name);
    }

    eprintln!("[NKR-DBG] cleanup completo — Drop pendiente de fs_devs/block_devs");

    // Clean up rootfs if auto-mounted (Disabled: we use persistent shared Master Mounts in /run)

    eprintln!("════════════════════════════════════════════════════════════════");
    eprintln!("[NKR] MicroVM finalizada");

    // Propagate vCPU loop error if any
    loop_result
}

// =============================================================================
// CPU Pinning — Chrs Model
// =============================================================================

fn pin_cpu_chrs(chrs: u32) -> Result<(), Box<dyn std::error::Error>> {
    // If chrs is 0, no pinning
    let num_cores = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) } as u32;

    if chrs == 0 {
        return Ok(()); // No pinning needed
    }

    // Each core has 5 chrs (20% each)
    // Compute how many whole cores are needed to cover the chrs
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
// L2 Isolation — cBPF via tc (v1.3, replaces ebtables)
// =============================================================================
//
// Generates a cBPF (Classic BPF) program from MAC+IP and installs it on
// the TAP's tc ingress. The kernel processes the program in the BPF subsystem —
// the same infrastructure as eBPF — without requiring the ebtables module.
//
// The program validates:
//   1. src MAC == assigned MAC (bytes 6–11 of the Ethernet frame)
//   2. Ethertype 0x0806 (ARP)  → ACCEPT
//   3. Ethertype 0x0800 (IPv4) + src IP == assigned IP → ACCEPT
//   4. Everything else → DROP (return 0)
//
// cBPF program (11 instructions):
//   0: LD_W  [6]         load src MAC bytes 0-3
//   1: JEQ   mac_hi32    → 2 if equal, DROP otherwise
//   2: LD_H  [10]        load src MAC bytes 4-5
//   3: JEQ   mac_lo16    → 4 if equal, DROP otherwise
//   4: LD_H  [12]        load ethertype
//   5: JEQ   0x0806      → 10 (ACCEPT) if ARP; else → 6
//   6: JEQ   0x0800      → 7 if IPv4; else → DROP
//   7: LD_W  [26]        load IPv4 src IP
//   8: JEQ   ip_int      → 10 (ACCEPT) if equal; else → DROP
//   9: RET   0           DROP
//  10: RET   65535       ACCEPT
// =============================================================================

/// Builds the cBPF bytecode as a string for `tc filter add ... bpf bytecode`.
/// Format: "N,code jt jf k,code jt jf k,..."
fn build_bpf_bytecode(mac: &[u8; 6], guest_ip: &str) -> Option<String> {
    let ip_parts: Vec<u8> = guest_ip.split('.')
        .filter_map(|s| s.parse().ok())
        .collect();
    if ip_parts.len() != 4 { return None; }

    let mac_hi = u32::from_be_bytes([mac[0], mac[1], mac[2], mac[3]]);
    let mac_lo = u16::from_be_bytes([mac[4], mac[5]]) as u32;
    let ip_int = u32::from_be_bytes([ip_parts[0], ip_parts[1], ip_parts[2], ip_parts[3]]);

    // 11 cBPF instructions in string format
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

/// Installs L2 isolation on the TAP via cBPF via tc.
/// Replaces ebtables (v1.2) with native kernel BPF (v1.3).
/// Fallback: if tc is not available, tries ebtables.
fn setup_tap_isolation(tap_name: &str, mac: &[u8; 6], guest_ip: &str) {
    let mac_str = format!("{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]);

    // ── Try cBPF install via tc ──────────────────────────────────────────────
    if let Some(bytecode) = build_bpf_bytecode(mac, guest_ip) {
        // 1. Create clsact qdisc (idempotent — fails silently if it exists)
        let _ = std::process::Command::new("tc")
            .args(["qdisc", "add", "dev", tap_name, "clsact"])
            .output();

        // 2. Install cBPF filter on ingress
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
        // Allow ARP only from the assigned MAC
        &["ebtables", "-A", "INPUT", "-i", tap_name,
          "-p", "ARP", "--arp-mac-src", &mac_str, "-j", "ACCEPT"],
        // Allow IPv4 only from the assigned MAC + IP
        &["ebtables", "-A", "INPUT", "-i", tap_name,
          "-p", "IPv4", "--ip-src", guest_ip, "-s", &mac_str, "-j", "ACCEPT"],
        // Drop everything else coming from this TAP
        &["ebtables", "-A", "INPUT", "-i", tap_name, "-j", "DROP"],
    ];

    for rule in rules {
        let _ = std::process::Command::new(rule[0]).args(&rule[1..]).status();
    }
    eprintln!("[NKR] L2: reglas ebtables instaladas para {} (MAC={} IP={})",
        tap_name, mac_str, guest_ip);
}

/// Removes L2 isolation from the TAP (tc qdisc + ebtables fallback).
fn teardown_tap_isolation(tap_name: &str, _mac: &[u8; 6], _guest_ip: &str) {
    // NOTE: In the current flow the TAP is deleted with `ip link delete` right
    // after this fn. Deleting the link also releases its clsact qdisc and
    // any ebtables filter associated with its ifname. Skipping tc/ebtables
    // avoids vmm getting stuck 60s+ if rtnetlink/ebtables are busy with another
    // process (observed: `tc qdisc del` hangs in D-state during cleanup
    // with TAP still referenced by vhost-net in pending drop).
    eprintln!("[NKR-DBG] teardown_tap: skip (ip link delete limpia qdisc/filters del {})", tap_name);
}

// =============================================================================
// cgroupv2 — CPU Bursting + I/O Throttling (Features 2 and 4B)
// =============================================================================

/// Returns the major:minor of a block device given its path.
fn get_block_major_minor(path: &str) -> Option<(u32, u32)> {
    use std::ffi::CString;
    let c_path = CString::new(path).ok()?;
    let mut stat_buf: libc::stat = unsafe { std::mem::zeroed() };
    let ret = unsafe { libc::stat(c_path.as_ptr(), &mut stat_buf) };
    if ret != 0 {
        return None;
    }
    let rdev = stat_buf.st_rdev;
    // major = bits 8..19, minor = bits 0..7 (regular devices)
    let major = (rdev >> 8) as u32;
    let minor = (rdev & 0xFF) as u32;
    Some((major, minor))
}

/// Configures cgroupv2 for the VM: cpu.max, memory.max and io.max.
/// Degrades silently if cgroupv2 is not available on the host.
/// `burst`: if true (Smart Default v1.3), enables cpu.max.burst for short bursts.
/// `ram_mb`: used for memory.max with +15% headroom (stack + guest kernel).
fn setup_cgroup(vm_name: &str, chrs: u32, ram_mb: u32, pid: u32, disk_paths: &[String], burst: bool) {
    // Verify that cgroupv2 is mounted
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

    // Ensure the nkr subtree exists and has the controllers enabled
    let nkr_base = "/sys/fs/cgroup/nkr";
    if let Err(e) = std::fs::create_dir_all(&base) {
        eprintln!("[NKR] WARN: No se pudo crear cgroup {}: {}", base, e);
        return;
    }

    // Enable cpu, memory, io controllers in the parent cgroup
    let subtree_ctl = format!("{}/cgroup.subtree_control", nkr_base);
    let _ = std::fs::write(&subtree_ctl, "+cpu +memory +io");

    // cpu.max: quota = chrs * 20_000µs per 100_000µs (20% per chr)
    let quota = chrs * 20_000;
    if let Err(e) = std::fs::write(format!("{}/cpu.max", base), format!("{} 100000", quota)) {
        eprintln!("[NKR] WARN: No se pudo escribir cpu.max: {}", e);
    }

    // cpu.max.burst (kernel >= 5.15): allows short bursts using idle cycles.
    // Only enabled if burst=true (Smart Default v1.3). Disable for VMs
    // that require strictly predictable CPU latency.
    let burst_path = format!("{}/cpu.max.burst", base);
    if burst && std::path::Path::new(&burst_path).exists() {
        let burst_us = chrs * 5_000;
        let _ = std::fs::write(&burst_path, format!("{}", burst_us));
        eprintln!("[NKR] cgroup: cpu.max.burst={} µs (burst activo)", burst_us);
    } else if !burst {
        // Explicitly disable burst if requested
        if std::path::Path::new(&burst_path).exists() {
            let _ = std::fs::write(&burst_path, "0");
        }
    }

    // memory.max: configured RAM + headroom (guest kernel stack + KVM overhead
    // + virtiofsd + page tables). Formula: max(ram*15%, 128 MB). The 128 MB floor
    // is critical for small VMs (pgbouncer 64 MB): with linear 15% headroom
    // (9.6 MB), the guest kernel alone runs out of memory.
    // If the VM exceeds it, LOCAL cgroup OOM killer — does not drag down the host.
    let headroom_bytes: u64 = std::cmp::max((ram_mb as u64) * 1024 * 1024 * 15 / 100, 128 * 1024 * 1024);
    let memory_max_bytes: u64 = (ram_mb as u64) * 1024 * 1024 + headroom_bytes;
    let memory_controller_ok = controllers.contains("memory");
    if memory_controller_ok {
        if let Err(e) = std::fs::write(format!("{}/memory.max", base), memory_max_bytes.to_string()) {
            eprintln!("[NKR] WARN: No se pudo escribir memory.max: {}", e);
        }
    }

    // io.max: 200 MB/s read, 100 MB/s write per disk
    if controllers.contains("io") {
        for disk in disk_paths {
            if let Some((maj, min)) = get_block_major_minor(disk) {
                let io_entry = format!("{}:{} rbps=209715200 wbps=104857600\n", maj, min);
                let _ = std::fs::write(format!("{}/io.max", base), &io_entry);
            }
        }
    }

    // Move the current process to the cgroup
    if let Err(e) = std::fs::write(format!("{}/cgroup.procs", base), format!("{}", pid)) {
        eprintln!("[NKR] WARN: No se pudo mover PID al cgroup: {}", e);
    } else {
        let mem_mb = memory_max_bytes / (1024 * 1024);
        eprintln!("[NKR] cgroup: {} | cpu.max={} 100000 | memory.max={} MB | io.max=200/100 MB/s",
            vm_name, quota, mem_mb);
    }
}

/// Removes the VM's cgroup after shutdown.
fn teardown_cgroup(vm_name: &str) {
    let base = format!("/sys/fs/cgroup/nkr/{}", vm_name);
    if !std::path::Path::new(&base).exists() {
        return;
    }
    // rmdir only works when la cgroup está vacía. Si quedan procesos pegados
    // (zombies, threads del vmm que no hicieron exit limpio), enviamos SIGKILL
    // y esperamos hasta 500ms para que el kernel los retire. Después rmdir.
    if let Ok(content) = std::fs::read_to_string(format!("{}/cgroup.procs", base)) {
        let pids: Vec<i32> = content.lines()
            .filter_map(|s| s.trim().parse::<i32>().ok())
            .collect();
        if !pids.is_empty() {
            eprintln!("[NKR-CGROUP] cgroup '{}' tiene {} proc(s) pegados, SIGKILL...",
                vm_name, pids.len());
            for pid in &pids {
                unsafe { libc::kill(*pid, libc::SIGKILL); }
            }
            // Esperar hasta 500ms a que el kernel los retire de la cgroup.
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
            loop {
                let still = std::fs::read_to_string(format!("{}/cgroup.procs", base))
                    .map(|s| s.lines().any(|l| !l.trim().is_empty()))
                    .unwrap_or(false);
                if !still { break; }
                if std::time::Instant::now() >= deadline { break; }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
    }
    if let Err(e) = std::fs::remove_dir(&base) {
        eprintln!("[NKR-CGROUP] WARN: rmdir {} falló: {} (probablemente threads aún activos)",
            base, e);
    }
}

// =============================================================================
// VMM functions
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
// Feature C — ELF vmlinux kernel loading (no decompression in guest)
// =============================================================================

#[derive(Clone, Copy)]
enum KernelFormat { BzImage, Elf }

/// Smart default v1.3: always force nanolinux ELF as default.
fn smart_resolve_kernel(path: &str) -> String {
    let is_default = path == "nanolinux" || path == "vmlinux" || path == "bzImage"
        || path.ends_with("/nanolinux") || path.ends_with("/vmlinux") || path.ends_with("/bzImage")
        || path.ends_with("/kernel/nanolinux") || path.ends_with("/kernel/vmlinux") || path.ends_with("/kernel/bzImage");

    if is_default {
        let nkr_data = std::env::var("NKR_DATA_DIR").unwrap_or_else(|_| "/mnt/nkr".to_string());
        let central_nanolinux = format!("{}/kernel/nanolinux", nkr_data);
        if std::path::Path::new(&central_nanolinux).exists() {
            eprintln!("[NKR] Smart default: usando nanolinux en almacén central");
            return central_nanolinux;
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

/// Detects the kernel format by reading the first 4 bytes (magic).
/// ELF: `\x7fELF`. Anything else → bzImage (guaranteed compatibility).
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

/// Loads an ELF vmlinux kernel directly into guest memory.
/// Reuses linux_loader::Elf (already available with the "elf" feature in Cargo.toml).
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
    // If an explicit path was specified, use it
    if let Some(path) = explicit_path {
        return load_initramfs(guest_mem, path, ram_bytes);
    }

    // Auto-detect
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
    // Place initramfs just before the top of RAM (4KB-aligned)
    let initramfs_addr = if ram_bytes > (INITRAMFS_ADDR as usize + size as usize) {
        INITRAMFS_ADDR
    } else {
        let addr = (ram_bytes - size as usize) & !0xFFF; // align to 4KB
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
    rootfs_tag: Option<&str>,             // If Some, rootfs comes via VirtIO-FS (not /dev/vda)
) -> Result<(), Box<dyn std::error::Error>> {

    let mut cmdline_str = format!("console=ttyS0 lpj=2800170 panic=1 rtc.hctosys=0 tsc=reliable no_timer_check clocksource=kvm-clock reboot=t virtio_mmio.device=4K@0xd0000000:5");

    for (addr, irq) in block_configs {
        cmdline_str.push_str(&format!(" virtio_mmio.device=4K@{:#010x}:{}", addr, irq));
    }

    if let Some(_) = pmem_dev {
        cmdline_str.push_str(" memmap=8G!4G root=/dev/pmem0 rootflags=dax rw");
    }

    // VirtIO-FS shares: nkr.fs{i} / nkr.fsm{i} / nkr.fsr{i}.
    // El cmdline del kernel tiene un tope (COMMAND_LINE_SIZE del nano-kernel,
    // ~1024 bytes) — con muchas shares se truncaba el final (se perdían
    // `init=` y `nkr.ip=`, y parcialmente `nkr.rootfs=`). Para dar holgura,
    // omitimos lo redundante (el initramfs ya tolera la ausencia):
    //   - el rootfs (guest_path == "/") solo necesita su `virtio_mmio.device`;
    //     el initramfs lo monta vía `nkr.rootfs=` (más abajo) y su mount loop
    //     ya skipeaba esa entrada → omitimos `nkr.fs0/fsm0/fsr0` (~40 bytes).
    //   - `nkr.fsr{i}=` solo se emite cuando es `ro`; el initramfs trata la
    //     ausencia como `rw` (default) → ~13 bytes por share RW.
    for (i, (tag, guest_path, readonly, slot)) in fs_shares.iter().enumerate() {
        let addr = 0xD001_0000u64 + (*slot as u64 * 0x1000);
        let irq  = 8u32 + *slot as u32;
        cmdline_str.push_str(&format!(" virtio_mmio.device=4K@{:#010x}:{}", addr, irq));
        if guest_path == "/" { continue; } // rootfs → solo el virtio_mmio.device
        cmdline_str.push_str(&format!(" nkr.fs{}={} nkr.fsm{}={}", i, tag, i, guest_path));
        if *readonly {
            cmdline_str.push_str(&format!(" nkr.fsr{}=ro", i));
        }
    }

    // VirtIO-BLK shares: nkr.blk{i} / nkr.blkm{i}
    for (i, (dev, guest_mnt)) in blk_share_mounts.iter().enumerate() {
        cmdline_str.push_str(&format!(" nkr.blk{}={} nkr.blkm{}={}", i, dev, i, guest_mnt));
    }

    if balloon_enabled {
        cmdline_str.push_str(&format!(" virtio_mmio.device=4K@{:#010x}:{}", BALLOON_MMIO_ADDR, BALLOON_IRQ));
    }
    // VirtIO-Console control channel (always active)
    cmdline_str.push_str(&format!(" virtio_mmio.device=4K@{:#010x}:{}", CONSOLE_MMIO_ADDR, CONSOLE_IRQ));
        
    if let Some(_) = pmem_dev {
        // 1. Declare the hardware device on the MMIO bus
        cmdline_str.push_str(&format!(" virtio_mmio.device=4K@{:#010x}:{}", PMEM_MMIO_ADDR, PMEM_IRQ));

        // 2. Configure memory routing and force the rootfs with DAX
        cmdline_str.push_str(" memmap=8G!4G root=/dev/pmem0 rootfstype=ext4 rootflags=dax rw rootdelay=1");
    } else if let Some(tag) = rootfs_tag {
        // Shared rootfs mode: kernel mounts virtiofs natively as rootfs
        cmdline_str.push_str(&format!(" root={} rootfstype=virtiofs rw nkr.rootfs={}", tag, tag));
    } else {
        // Classic fallback to block disk if no PMEM nor rootfs
        cmdline_str.push_str(" root=/dev/vda rw");
    }
    
    cmdline_str.push_str(&format!(" init=/sbin/init nkr.ip={} \0", guest_ip));
    
    guest_mem.write_slice(cmdline_str.as_bytes(), GuestAddress(CMDLINE_ADDR))?;

    // =========================================================================
    // MANDATORY SIGNATURES (LINUX BOOT PROTOCOL)
    // Without this, the ELF kernel discards the zero_page and wipes the E820 map
    // =========================================================================
    guest_mem.write_obj(0xAA55u16, GuestAddress(ZERO_PAGE_ADDR + 0x1FE))?; // boot_flag
    guest_mem.write_obj(0x53726448u32, GuestAddress(ZERO_PAGE_ADDR + 0x202))?; // header = "HdrS"
    guest_mem.write_obj(0x020Du16, GuestAddress(ZERO_PAGE_ADDR + 0x206))?; // version = 2.13

    guest_mem.write_obj(0xFFu8, GuestAddress(ZERO_PAGE_ADDR + 0x210))?; // type_of_loader
    guest_mem.write_obj(0x81u8, GuestAddress(ZERO_PAGE_ADDR + 0x211))?; // loadflags
    // Compute dynamic initramfs address (same logic as load_initramfs)
    let initramfs_addr = if ram_bytes > (INITRAMFS_ADDR as usize + initrd_size as usize) {
        INITRAMFS_ADDR
    } else {
        ((ram_bytes - initrd_size as usize) & !0xFFF) as u64
    };
    guest_mem.write_obj(initramfs_addr as u32, GuestAddress(ZERO_PAGE_ADDR + 0x218))?;
    guest_mem.write_obj(initrd_size, GuestAddress(ZERO_PAGE_ADDR + 0x21C))?;
    guest_mem.write_obj(CMDLINE_ADDR as u32, GuestAddress(ZERO_PAGE_ADDR + 0x228))?;

    // =========================================================================
    // E820 MAP (RAM memory and PMEM)
    // =========================================================================
    guest_mem.write_obj(0x0u64, GuestAddress(ZERO_PAGE_ADDR + 0x2D0))?;
    guest_mem.write_obj(0x9FC00u64, GuestAddress(ZERO_PAGE_ADDR + 0x2D8))?;
    guest_mem.write_obj(1u32, GuestAddress(ZERO_PAGE_ADDR + 0x2E0))?; // Type 1: usable RAM

    let high_mem_size = (ram_bytes as u64) - 0x100000;
    guest_mem.write_obj(0x100000u64, GuestAddress(ZERO_PAGE_ADDR + 0x2E4))?;
    guest_mem.write_obj(high_mem_size, GuestAddress(ZERO_PAGE_ADDR + 0x2EC))?;
    guest_mem.write_obj(1u32, GuestAddress(ZERO_PAGE_ADDR + 0x2F4))?; // Type 1: usable RAM

    let e820_count: u8 = if let Some(pmem) = pmem_dev {
        guest_mem.write_obj(pmem.guest_phys_addr,       GuestAddress(ZERO_PAGE_ADDR + 0x2F8))?;
        guest_mem.write_obj(pmem.host_mmap_len as u64,  GuestAddress(ZERO_PAGE_ADDR + 0x300))?;
        guest_mem.write_obj(7u32,                       GuestAddress(ZERO_PAGE_ADDR + 0x308))?; // Type 7: PMEM
        3
    } else {
        2
    };

    guest_mem.write_obj(e820_count, GuestAddress(ZERO_PAGE_ADDR + 0x1E8))?; // Store entry count
    
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
    
    // Vital: RSI must point to the zero_page so Linux reads the E820 map
    regs.rsi = ZERO_PAGE_ADDR;

    // Vital: Move the Stack Pointer AWAY from the zero_page (0x7000).
    // The stack grows downward, 0x9000 is safe.
    regs.rsp = 0x9000;         
    
    regs.rflags = 0x2;
    
    vcpu.set_regs(&regs)?;
    Ok(())
}

/// Configures sregs for 64-bit long mode (required by nanolinux ELF / startup_64).
/// Unlike configure_sregs() which uses 32-bit protected mode for bzImage,
/// here we enable PAE, paging and LME before jumping to the kernel entry.
fn configure_sregs_64(vcpu: &VcpuFd) -> Result<(), Box<dyn std::error::Error>> {
    let mut sregs = vcpu.get_sregs()?;

    // Enable long mode: PAE + paging + protection
    sregs.efer  = 0xD01;        // LME (bit 8) + LMA (bit 10) + NXE (bit 11)
    sregs.cr0   = 0x80050033;   // PG + WP + PE (+ MP + ET)
    sregs.cr3   = PML4_ADDR;    // Page table built by write_page_tables()
    sregs.cr4   = 0x20;         // PAE (bit 5)

    // CS 64-bit: l=1, db=0
    let cs64 = kvm_segment {
        base: 0, limit: 0xFFFF_FFFF, selector: 0x08,
        type_: 0xB, present: 1, dpl: 0, db: 0, s: 1, l: 1, g: 1,
        avl: 0, unusable: 0, padding: 0,
    };
    // DS/ES/FS/GS/SS: 32/64-bit data segments (same as configure_sregs)
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
// Main vCPU loop — MMIO emulation
// =============================================================================

extern "C" fn sigterm_handler(_sig: libc::c_int) {
    SHUTDOWN_REQUESTED.store(true, Ordering::SeqCst);
}

/// SIGUSR1 → marca pedido de reload. El vcpu loop ve la flag y dispara la
/// inyección de "REL_OD\n" por hvc0. Idempotente: múltiples SIGUSR1 seguidos
/// resultan en una sola inyección (la flag se reset al inyectar). Si llega
/// SIGUSR1 mientras hay un SHUTDOWN en vuelo, el shutdown gana (la flag de
/// shutdown se chequea primero en el loop).
extern "C" fn sigusr1_handler(_sig: libc::c_int) {
    RELOAD_REQUESTED.store(true, Ordering::SeqCst);
}

/// SIGUSR2 → renueva el timestamp ACTIVE del balloon. El vcpu loop chequea
/// el TS cada iter (despertado por SIGALRM o por VM exit) y aplica el target
/// ACTIVE si la VM está en IDLE. Idempotente: múltiples SIGUSR2 seguidos sólo
/// renuevan el TS sin re-aplicar config change redundante (LAST_APPLIED_STATE
/// gobierna eso).
///
/// Si el state machine no está habilitado (BALLOON_ACTIVE_MB == 0), el TS se
/// guarda igual pero el loop lo ignora — sin efecto secundario.
extern "C" fn sigusr2_handler(_sig: libc::c_int) {
    // SystemTime::now no es signal-safe estrictamente, pero clock_gettime sí
    // y SystemTime::now lo usa internamente en Linux. Para el caso no-Linux
    // sería problemático, pero NKR es Linux-only.
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    BALLOON_ACTIVE_REQUESTED_TS.store(secs, Ordering::SeqCst);
}

/// Arms ITIMER_REAL para SIGALRM cada 5s. Mantiene el vcpu loop despierto
/// para chequear decay del balloon incluso si el guest está en HLT (sin
/// tráfico). Es independiente de `arm_shutdown_itimer` — si SIGTERM llega,
/// el shutdown reprograma el itimer a 1s (ambos usan ITIMER_REAL, sólo hay
/// uno por proceso, el último gana).
fn arm_balloon_itimer() {
    unsafe {
        libc::signal(libc::SIGALRM, sigalrm_handler as *const () as libc::sighandler_t);
        let it = libc::itimerval {
            it_interval: libc::timeval { tv_sec: 5, tv_usec: 0 },
            it_value:    libc::timeval { tv_sec: 5, tv_usec: 0 },
        };
        libc::setitimer(libc::ITIMER_REAL, &it, std::ptr::null_mut());
    }
}

// SIGALRM handler: no action of its own. Only used to EINTR the KVM_RUN and
// let the loop top check SHUTDOWN_REQUESTED / the 60s timeout.
extern "C" fn sigalrm_handler(_sig: libc::c_int) {}

/// Arms a repeating itimer (SIGALRM every 1s). Call once after injecting
/// SHUTDOWN so vcpu.run() doesn't stay blocked in HLT indefinitely if the
/// guest doesn't respond (no traffic, no more host signals coming in).
fn arm_shutdown_itimer() {
    unsafe {
        libc::signal(libc::SIGALRM, sigalrm_handler as *const () as libc::sighandler_t);
        let it = libc::itimerval {
            it_interval: libc::timeval { tv_sec: 1, tv_usec: 0 },
            it_value:    libc::timeval { tv_sec: 1, tv_usec: 0 },
        };
        libc::setitimer(libc::ITIMER_REAL, &it, std::ptr::null_mut());
    }
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
    cell_id: u8,
    vm_id: u8,
) -> Result<(), Box<dyn std::error::Error>> {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    let mut serial_ier: u8 = 0;

    loop {
        if SHUTDOWN_REQUESTED.load(Ordering::SeqCst) {
            let phase = SHUTDOWN_PHASE.load(Ordering::SeqCst);
            let now_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64;
            if phase == 0 {
                SHUTDOWN_STARTED_MS.store(now_ms, Ordering::SeqCst);
                SHUTDOWN_LAST_REINJECT_MS.store(now_ms, Ordering::SeqCst);
                SHUTDOWN_PHASE.store(1, Ordering::SeqCst);
                eprintln!("\n[NKR] SIGTERM recibido — enviando SHUTDOWN por hvc0...");
                console_dev.try_inject(b"SHUTDOWN\n");
                // Arms SIGALRM every 1s to break vcpu.run() if the guest
                // is in HLT — without this, the loop doesn't check the 60s timeout.
                arm_shutdown_itimer();
            } else {
                let elapsed_secs = now_ms.saturating_sub(SHUTDOWN_STARTED_MS.load(Ordering::SeqCst)) / 1000;
                if elapsed_secs >= 60 {
                    eprintln!("[NKR] Timeout 60s esperando shutdown del huésped — forzando salida");
                    break;
                }
                // Re-inject SHUTDOWN every 2s while the guest doesn't respond.
                // Mitigates races with virtio-console driver initialization:
                // if the first injection was lost (IRQ not hooked, buffers not
                // ready), retries arrive once hvc0 is operational.
                let last_inj = SHUTDOWN_LAST_REINJECT_MS.load(Ordering::SeqCst);
                if now_ms.saturating_sub(last_inj) >= 2000 {
                    SHUTDOWN_LAST_REINJECT_MS.store(now_ms, Ordering::SeqCst);
                    console_dev.try_inject(b"SHUTDOWN\n");
                }
            }
            // Retry injection if the queue wasn't ready yet
            console_dev.poll_pending();
        }

        // Reload trigger: SIGUSR1 → inyecta "REL_OD\n" por hvc0. El watcher
        // del init guest recarga Odoo según el modo (SIGHUP master si prefork,
        // SIGTERM+respawn si threaded) → código fresh del disco. La flag es
        // one-shot (se consume al inyectar). Si la cola del receiveq todavía
        // no está lista (boot
        // muy temprano), poll_pending re-intenta. Independiente de SHUTDOWN.
        if RELOAD_REQUESTED.swap(false, Ordering::SeqCst) {
            eprintln!("[NKR] SIGUSR1 recibido — enviando REL_OD por hvc0...");
            console_dev.try_inject(b"REL_OD\n");
            console_dev.poll_pending();
        }

        // ─── Balloon ACTIVE/IDLE state machine (CLAUDE.md v2.2) ─────────────
        // Sólo activa si la VM tiene balloon dinámico configurado
        // (BALLOON_IDLE_MB != 0 y distinto al ACTIVE). En caso estático
        // (PROD: idle==active==0), el chequeo es free (un load atómico).
        let idle_mb = BALLOON_IDLE_MB.load(Ordering::Relaxed);
        let active_mb = BALLOON_ACTIVE_MB.load(Ordering::Relaxed);
        if idle_mb != active_mb && idle_mb > 0 {
            let active_ts = BALLOON_ACTIVE_REQUESTED_TS.load(Ordering::SeqCst);
            let cur_state = BALLOON_LAST_APPLIED_STATE.load(Ordering::SeqCst);
            if active_ts > 0 {
                let now_secs = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                let decay = BALLOON_DECAY_SECS.load(Ordering::Relaxed) as u64;
                if now_secs.saturating_sub(active_ts) < decay {
                    // Dentro del decay → ACTIVE. Aplicar si veníamos de IDLE
                    // (= LAST_APPLIED_STATE != 1). Caso típico: SIGUSR2 tras
                    // estar en IDLE → renueva TS y re-active el target.
                    if cur_state != 1 {
                        balloon_dev.set_target_mb(active_mb);
                        balloon_dev.raise_config_change();
                        BALLOON_LAST_APPLIED_STATE.store(1, Ordering::SeqCst);
                        // Refleja el target actual en el state file (nkr ps / metrics)
                        state::update_balloon_mb(cell_id, vm_id, active_mb);
                        eprintln!("[NKR-BALLOON] IDLE→ACTIVE (target={} MB)", active_mb);
                    }
                } else {
                    // Decay expirado → transición a IDLE.
                    if cur_state != 0 {
                        balloon_dev.set_target_mb(idle_mb);
                        balloon_dev.raise_config_change();
                        BALLOON_LAST_APPLIED_STATE.store(0, Ordering::SeqCst);
                        BALLOON_ACTIVE_REQUESTED_TS.store(0, Ordering::SeqCst);
                        state::update_balloon_mb(cell_id, vm_id, idle_mb);
                        eprintln!("[NKR-BALLOON] ACTIVE→IDLE por decay (target={} MB)", idle_mb);
                    } else {
                        // cur_state ya era 0 — sólo limpiar TS para no
                        // re-evaluar este branch en cada iter.
                        BALLOON_ACTIVE_REQUESTED_TS.store(0, Ordering::SeqCst);
                    }
                }
            }
        }

        // Drain the virtio-balloon stats virtqueue at most every
        // BALLOON_STATS_INTERVAL_SECS: read the guest's MemTotal/Free/Available/
        // Cached etc. and persist them to the state file (the daemon exposes
        // them via /metrics and the per-instance endpoint). Independent of the
        // ACTIVE↔IDLE machinery — the statsq is present whenever the guest
        // negotiated F_STATS_VQ (we always advertise it).
        {
            let now_secs = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
            let last = BALLOON_STATS_LAST_TS.load(Ordering::Relaxed);
            if now_secs.saturating_sub(last) >= BALLOON_STATS_INTERVAL_SECS {
                BALLOON_STATS_LAST_TS.store(now_secs, Ordering::Relaxed);
                if balloon_dev.process_stats() {
                    let s = balloon_dev.last_stats;
                    state::update_guest_mem(cell_id, vm_id,
                        s.mem_total_bytes, s.mem_free_bytes, s.mem_available_bytes, s.mem_cached_bytes);
                }
            }
        }

        // Feature B — io_uring: drain completions before each KVM_RUN
        for dev in block_devs.iter_mut() {
            dev.poll_completions();
        }

        match vcpu.run() {
            Ok(VcpuExit::IoOut(port, data)) => {
                if port == COM1_PORT {
                    // Best-effort write to host stdout. Ignoring the error
                    // here is intentional: a closed/broken stdout (terminal
                    // gone, nohup misconfigured, journal pressure) would
                    // otherwise panic and abort the VM on every line of
                    // guest serial output. The guest kernel doesn't depend
                    // on the host actually reading COM1.
                    let _ = out.write_all(data);
                    let _ = out.flush();
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
                // 1. VirtIO-Net network (0xD0000000)
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
                            let val = if sel < 2 && { net_dev.state.lock().unwrap_or_else(|p| p.into_inner()).queue_ready[sel] } { 1u32 } else { 0u32 };
                            data.copy_from_slice(&val.to_le_bytes());
                        }
                        0x060 => data.copy_from_slice(&net_dev.state.lock().unwrap_or_else(|p| p.into_inner()).interrupt_status.to_le_bytes()),
                        0x070 => data.copy_from_slice(&net_dev.state.lock().unwrap_or_else(|p| p.into_inner()).status.to_le_bytes()),
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
                    // Look up in block devices
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
                    
                    // 3. VirtIO Console (0xD0002000 - moved to future if used)
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
                                // VIRTIO_MMIO_INT_VRING(0x1): always active when queues are ready.
                                // The PIC irqfd is edge-triggered → no storm.
                                // Without this, vm_interrupt() reads 0, doesn't call vring_interrupt,
                                // and FUSE responses get stuck in the used ring.
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
                                let qn = if qi < 3 { balloon_dev.queue_num[qi] } else { 256 };
                                data.copy_from_slice(&qn.to_le_bytes());
                            }
                            0x044 => {
                                let qi = balloon_dev.queue_sel as usize;
                                let v = if qi < 3 && balloon_dev.queue_ready[qi] { 1u32 } else { 0u32 };
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
                                // VIRTIO_F_VERSION_1 (bit 32) in sel=1
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
                // 1. VirtIO-Net network (0xD0000000)
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
                                let mut st = net_dev.state.lock().unwrap_or_else(|p| p.into_inner());
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
                            let mut st = net_dev.state.lock().unwrap_or_else(|p| p.into_inner());
                            st.interrupt_status &= !val;
                        }
                        0x070 => {
                            if val == 0 { net_dev.reset(); }
                            else {
                                let mut st = net_dev.state.lock().unwrap_or_else(|p| p.into_inner());
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
                    // Look up in block devices
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
                    // 3. Console (0xD0002000)
                    if addr >= 0xD0002000 && addr < 0xD0003000 {
                        // Stub: ignore writes to the console for now
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
                            0x038 => { if qi < 3 { balloon_dev.queue_num[qi] = val; } }
                            0x044 => {
                                if val == 1 { balloon_dev.activate_queue(qi); }
                                else if qi < 3 { balloon_dev.queue_ready[qi] = false; }
                            }
                            0x050 => {
                                // QueueNotify: 0=inflateq, 1=deflateq, 2=statsq.
                                // statsq is NOT consumed here (consuming on every
                                // guest kick would ping-pong) — the balloon timer
                                // in the vcpu loop drains it at most every ~30s.
                                if val == 0 { balloon_dev.process_inflate(); }
                                else if val == 1 { balloon_dev.process_deflate(); }
                            }
                            0x064 => { balloon_dev.interrupt_status &= !val; }
                            0x070 => {
                                if val == 0 {
                                    balloon_dev.status = 0;
                                    balloon_dev.queue_ready = [false, false, false];
                                } else {
                                    balloon_dev.status = val;
                                    if val == 15 { eprintln!("[NKR-BALLOON] ¡DRIVER_OK! Balloon listo."); }
                                }
                            }
                            0x080 => { if qi < 3 { balloon_dev.desc_low[qi] = val; } }
                            0x084 => { if qi < 3 { balloon_dev.desc_high[qi] = val; } }
                            0x090 => { if qi < 3 { balloon_dev.avail_low[qi] = val; } }
                            0x094 => { if qi < 3 { balloon_dev.avail_high[qi] = val; } }
                            0x0A0 => { if qi < 3 { balloon_dev.used_low[qi] = val; } }
                            0x0A4 => { if qi < 3 { balloon_dev.used_high[qi] = val; } }
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
                            0x050 => {} // QueueNotify transmitq: ignore guest data
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
                // EINTR = signal received (SIGTERM or another benign signal)
                // Let the loop top handle SHUTDOWN_REQUESTED (SysRq injection)
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
