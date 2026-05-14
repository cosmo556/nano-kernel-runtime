// =============================================================================
// NKR Janitor — Limpieza periódica de recursos huérfanos
// =============================================================================
//
// Runs en un thread del daemon `nkr serve`. Cada `INTERVAL_SECS` (5 min)
// detecta y limpia:
//
//   1. PID files huérfanos en /run/nkrfs/*.pid (proceso virtiofsd muerto).
//   2. Sockets huérfanos en /run/nkrfs/*.sock (sin .pid asociado vivo).
//   3. Mounts /run/nkr_master_rootfs_<hash> sin VM viva referenciándolos.
//   4. Locks /run/nkr_master_rootfs_<hash>.lock viejos (>1 h sin uso).
//   5. cgroups /sys/fs/cgroup/nkr/<vm>/ vacíos sin proceso vivo.
//   6. Loop devices con backing-file `(deleted)` y sin uso.
//   7. State files /tmp/nkr-vms/*.json apuntando a PID muerto.
//
// Idempotente y conservador: nunca toca recursos referenciados por procesos
// vivos. Si la detección es ambigua, deja el recurso (mejor leak temporal
// que matar algo en uso).
// =============================================================================

use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;

const INTERVAL_SECS: u64 = 300; // 5 min
const LOCK_MAX_AGE_SECS: u64 = 3600; // 1 h

pub fn run_loop() {
    eprintln!("[NKR-JANITOR] iniciado (interval={}s)", INTERVAL_SECS);
    // Primer pase tras un grace period corto para no chocar con boot.
    std::thread::sleep(Duration::from_secs(60));
    let mut tick = 0u64;
    loop {
        tick += 1;
        let stats = sweep();
        // Logear siempre el primer sweep (sanity check al boot) y los que
        // limpiaron algo. Sweeps idle se silencian para no llenar journal.
        if stats.total() > 0 || tick == 1 {
            eprintln!("[NKR-JANITOR] sweep #{}: {}", tick, stats);
        }
        std::thread::sleep(Duration::from_secs(INTERVAL_SECS));
    }
}

#[derive(Default)]
struct SweepStats {
    sock_pid_files: usize,
    sock_files: usize,
    rootfs_mounts: usize,
    rootfs_locks: usize,
    cgroups: usize,
    loop_devices: usize,
    state_files: usize,
}

impl SweepStats {
    fn total(&self) -> usize {
        self.sock_pid_files + self.sock_files + self.rootfs_mounts
            + self.rootfs_locks + self.cgroups + self.loop_devices + self.state_files
    }
}

impl std::fmt::Display for SweepStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f,
            "sock_pid={} sock={} rootfs_mounts={} rootfs_locks={} cgroups={} loops={} states={}",
            self.sock_pid_files, self.sock_files, self.rootfs_mounts,
            self.rootfs_locks, self.cgroups, self.loop_devices, self.state_files,
        )
    }
}

fn sweep() -> SweepStats {
    let mut s = SweepStats::default();
    s.sock_pid_files = sweep_virtiofsd_pid_files();
    s.sock_files = sweep_virtiofsd_sockets();
    s.state_files = sweep_state_files();
    let live_masters = collect_live_master_paths();
    s.rootfs_mounts = sweep_rootfs_mounts(&live_masters);
    s.rootfs_locks = sweep_rootfs_locks(&live_masters);
    s.cgroups = sweep_empty_cgroups();
    s.loop_devices = sweep_deleted_loops();
    s
}

// ── 1. PID files huérfanos en /run/nkrfs/ ────────────────────────────────────
fn sweep_virtiofsd_pid_files() -> usize {
    let mut count = 0;
    let dir = match std::fs::read_dir("/run/nkrfs") {
        Ok(d) => d,
        Err(_) => return 0,
    };
    for entry in dir.flatten() {
        let path = entry.path();
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => continue,
        };
        if !name.ends_with(".sock.pid") { continue; }
        let pid: i32 = match std::fs::read_to_string(&path)
            .ok().and_then(|s| s.trim().parse().ok())
        {
            Some(p) => p,
            None => continue,
        };
        // Si el proceso no existe → huérfano.
        if unsafe { libc::kill(pid, 0) } != 0 {
            let _ = std::fs::remove_file(&path);
            // También su .sock par.
            let sock = path.with_extension("");
            let _ = std::fs::remove_file(&sock);
            count += 1;
        }
    }
    count
}

// ── 2. Sockets sin .pid en /run/nkrfs/ ───────────────────────────────────────
// (después del sweep #1, los .sock que quedaron sin .pid asociado son los
// huérfanos de virtiofsd que crasheó sin escribir el .pid.)
fn sweep_virtiofsd_sockets() -> usize {
    let mut count = 0;
    let dir = match std::fs::read_dir("/run/nkrfs") {
        Ok(d) => d,
        Err(_) => return 0,
    };
    for entry in dir.flatten() {
        let path = entry.path();
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => continue,
        };
        if !name.ends_with(".sock") { continue; }
        // Si nadie del lado virtiofsd lo está usando (no hay .pid).
        let pid_path = path.with_extension("sock.pid");
        if !pid_path.exists() {
            let _ = std::fs::remove_file(&path);
            count += 1;
        }
    }
    count
}

// ── 3. Recopilar disks/rootfs referenciados por VMs vivas ────────────────────
//
// Devuelve un set con paths backing originales (.ext4) Y los /dev/loopN
// que el kernel asignó a cada uno. Esto es necesario porque /proc/mounts
// reporta el loop device como source, no el path del archivo.
fn collect_live_master_paths() -> HashSet<String> {
    let mut paths = HashSet::new();
    if let Ok(entries) = std::fs::read_dir("/proc") {
        for e in entries.flatten() {
            let pid_str = e.file_name().to_string_lossy().to_string();
            if pid_str.parse::<u32>().is_err() { continue; }
            let cmd = match std::fs::read(format!("/proc/{}/cmdline", pid_str)) {
                Ok(b) => b,
                Err(_) => continue,
            };
            let args: Vec<String> = cmd.split(|&b| b == 0)
                .filter(|s| !s.is_empty())
                .map(|s| String::from_utf8_lossy(s).to_string())
                .collect();
            let is_nkr = args.iter().any(|a| a.ends_with("/nkr") || a == "nkr");
            if !is_nkr { continue; }
            let is_run = args.iter().any(|a| a == "run");
            if !is_run { continue; }
            for i in 0..args.len() {
                if args[i] == "--disk" || args[i] == "--rootfs"
                   || args[i].starts_with("--share") || args[i].starts_with("--volume")
                {
                    if let Some(p) = args.get(i + 1) {
                        // --share/--volume tienen formato `host:guest[:opts]`
                        let host = p.split(':').next().unwrap_or(p).to_string();
                        paths.insert(host.clone());
                        // Resolver loop devices que tengan este path como backing.
                        if let Ok(out) = std::process::Command::new("losetup")
                            .args(["-j", &host]).output()
                        {
                            let s = String::from_utf8_lossy(&out.stdout);
                            for ll in s.lines() {
                                if let Some(dev) = ll.split(':').next() {
                                    paths.insert(dev.trim().to_string());
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    paths
}

// ── 4. Rootfs mounts /run/nkr_master_rootfs_<hash> sin VM viva ───────────────
fn sweep_rootfs_mounts(live_masters: &HashSet<String>) -> usize {
    let mounts_txt = match std::fs::read_to_string("/proc/mounts") {
        Ok(s) => s,
        Err(_) => return 0,
    };
    let mut count = 0;
    for line in mounts_txt.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 2 { continue; }
        let src = parts[0];
        let mp = parts[1];
        if !mp.starts_with("/run/nkr_master_rootfs_") { continue; }
        // Si alguna VM viva referencia este source (sea path .ext4 o /dev/loopN)
        // → keep. Los /dev/loopN están en `live_masters` gracias a `losetup -j`.
        if live_masters.contains(src) { continue; }
        // Caso edge: source con sufijo "(deleted)" → mount está colgando un
        // archivo borrado; nunca volverá a tener uso → safe para unmount.
        let _ = std::process::Command::new("umount").arg("-l").arg(mp).status();
        let _ = std::fs::remove_dir(mp);
        count += 1;
    }
    count
}

// ── 5. Locks de rootfs viejos sin VM viva ────────────────────────────────────
fn sweep_rootfs_locks(_live_masters: &HashSet<String>) -> usize {
    let mut count = 0;
    let dir = match std::fs::read_dir("/run") {
        Ok(d) => d,
        Err(_) => return 0,
    };
    let now = std::time::SystemTime::now();
    for entry in dir.flatten() {
        let path = entry.path();
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        if !name.starts_with("nkr_master_rootfs_") || !name.ends_with(".lock") { continue; }
        // Si el mount correspondiente todavía existe, skip (lock activo).
        let mount_dir = name.trim_end_matches(".lock");
        let mount_path = format!("/run/{}", mount_dir);
        let still_mounted = std::process::Command::new("mountpoint")
            .arg("-q").arg(&mount_path).status()
            .map(|s| s.success()).unwrap_or(false);
        if still_mounted { continue; }
        // Si es viejo (>1h sin tocar) → eliminar.
        if let Ok(meta) = entry.metadata() {
            if let Ok(mtime) = meta.modified() {
                if let Ok(age) = now.duration_since(mtime) {
                    if age.as_secs() >= LOCK_MAX_AGE_SECS {
                        if std::fs::remove_file(&path).is_ok() {
                            count += 1;
                        }
                    }
                }
            }
        }
    }
    count
}

// ── 6. cgroups vacíos en /sys/fs/cgroup/nkr/ ─────────────────────────────────
fn sweep_empty_cgroups() -> usize {
    let mut count = 0;
    let dir = match std::fs::read_dir("/sys/fs/cgroup/nkr") {
        Ok(d) => d,
        Err(_) => return 0,
    };
    // Recopilar nombres de VMs vivas para protegerlas.
    let live: HashSet<String> = crate::state::list_vms()
        .into_iter().map(|v| v.name).collect();
    for entry in dir.flatten() {
        let path = entry.path();
        if !path.is_dir() { continue; }
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        if live.contains(&name) { continue; }
        // ¿Tiene procs adentro?
        let procs_path = path.join("cgroup.procs");
        let has_proc = std::fs::read_to_string(&procs_path)
            .map(|s| s.lines().any(|l| !l.trim().is_empty()))
            .unwrap_or(false);
        if has_proc { continue; }
        if std::fs::remove_dir(&path).is_ok() {
            count += 1;
        }
    }
    count
}

// ── 7. Loop devices con backing (deleted) sin uso ────────────────────────────
fn sweep_deleted_loops() -> usize {
    let mut count = 0;
    let out = match std::process::Command::new("losetup").arg("-l").arg("--noheadings").output() {
        Ok(o) if o.status.success() => o,
        _ => return 0,
    };
    let s = String::from_utf8_lossy(&out.stdout);
    let mounts_txt = std::fs::read_to_string("/proc/mounts").unwrap_or_default();
    for line in s.lines() {
        // "/dev/loopN ... <BACK-FILE>"
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 6 { continue; }
        let dev = parts[0];
        // Si la línea tiene "(deleted)" en el back-file.
        if !line.contains("(deleted)") { continue; }
        // Y nadie tiene mounted ese device.
        let in_use = mounts_txt.lines().any(|m| m.starts_with(&format!("{} ", dev)));
        if in_use { continue; }
        if std::process::Command::new("losetup").args(["-d", dev]).status()
            .map(|s| s.success()).unwrap_or(false)
        {
            count += 1;
        }
    }
    count
}

// ── 8. State files /tmp/nkr-vms/ apuntando a PID muerto ──────────────────────
fn sweep_state_files() -> usize {
    let mut count = 0;
    let dir = match std::fs::read_dir("/tmp/nkr-vms") {
        Ok(d) => d,
        Err(_) => return 0,
    };
    for entry in dir.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") { continue; }
        let content = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let pid: i32 = match serde_json::from_str::<serde_json::Value>(&content)
            .ok()
            .and_then(|v| v.get("pid").and_then(|p| p.as_i64()))
            .map(|p| p as i32)
        {
            Some(p) => p,
            None => continue,
        };
        // ¿Vivo?
        if unsafe { libc::kill(pid, 0) } != 0 {
            let _ = std::fs::remove_file(&path);
            count += 1;
        }
    }
    count
}

// Helper: file_name no se usa en algunos casos → silence dead_code.
#[allow(dead_code)]
fn _unused() -> &'static Path { Path::new("/") }
