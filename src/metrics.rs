// =============================================================================
// NKR Metrics — Real-time per-micro-VM resource measurement
// =============================================================================
//
// Reads kernel metrics via procfs for each active VMM process.
// CPU% is computed with a 200ms window shared across all VMs to minimize
// total latency when displaying multiple VMs.
//
// Data sources:
//   /proc/{pid}/stat   → CPU times (utime + stime in ticks)
//   /proc/{pid}/status → VmRSS: real physical RAM on the host
//   /proc/{pid}/io     → bytes read/written to disk
//   /proc/net/dev      → RX/TX bytes per TAP interface
// =============================================================================

use std::collections::HashMap;
use std::fs;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::state::VmState;

// ── Disk-usage (`du -sb`) cache ────────────────────────────────────────────
// `du` over a tenant filestore (lots of small files) is not free; Prometheus
// scrapes every ~15s. Cache per-path for DU_TTL so a scrape costs at most one
// `du` per path per minute. The daemon is long-lived so a static is fine.
static DU_CACHE: Mutex<Option<HashMap<String, (Instant, u64)>>> = Mutex::new(None);
const DU_TTL: Duration = Duration::from_secs(300);

// ── Per-VM metrics-JSON cache (the per-instance API endpoint) ──────────────
// The panel polls `GET .../instances/{name}/metrics` while the Metrics tab is
// open. We compute a VM's full snapshot at most once per VM_METRICS_TTL and
// serve the cached JSON in between. A recompute is ~1ms (a handful of procfs +
// cgroup file reads); the only heavy bit, the disk `du`, is independently
// cached for DU_TTL (5 min) so it's untouched by a fast poll. TTL kept just
// below a ~2s panel poll so each poll gets a fresh sample — and it still
// coalesces burst/duplicate requests. (`guest_mem` only refreshes every
// BALLOON_STATS_INTERVAL_SECS — that one number lags the rest.)
static VM_METRICS_CACHE: Mutex<Option<HashMap<String, (Instant, serde_json::Value)>>> = Mutex::new(None);
const VM_METRICS_TTL: Duration = Duration::from_secs(2);

// =============================================================================
// Data structures
// =============================================================================

/// Real-time metrics for a micro-VM
#[derive(Debug, Clone, Default)]
pub struct VmMetrics {
    /// CPU usage percentage (200ms window)
    pub cpu_pct: f32,
    /// Real physical RAM used on the host (RSS of VMM process), in MB
    pub rss_mb: u32,
    /// Configured RAM (limit assigned at VM launch), in MB
    pub ram_allocated_mb: u32,
    /// MB saved via VirtIO-Balloon (returned to host by the guest)
    pub balloon_mb: u32,
    /// Estimated MB saved via DAX/PMEM (page cache not duplicated)
    /// Calculation: max(0, ram_allocated - rss - balloon - overhead_vmm)
    pub dax_savings_mb: u32,
    /// Bytes received on the TAP interface (external→VM traffic)
    pub net_rx_bytes: u64,
    /// Bytes transmitted on the TAP interface (VM→external traffic)
    pub net_tx_bytes: u64,
    /// Bytes read from disk (cumulative since process start)
    pub io_read_bytes: u64,
    /// Bytes written to disk (cumulative since process start)
    pub io_write_bytes: u64,
    /// cgroup CPU accounting: total CPU consumed by the VM's cgroup in µs
    /// (`cpu.stat usage_usec` — includes vCPU + virtiofsd/vhost helpers).
    /// Cumulative since the cgroup was created. Only populated for the
    /// Prometheus exporter; the `nkr stats` CLI path leaves it 0.
    pub cgroup_cpu_usec: u64,
    /// cgroup CPU throttled time in µs (`cpu.stat throttled_usec`) — how long
    /// the VM was held off the CPU because it hit its `cpu.max` quota (`chrs`).
    pub cgroup_cpu_throttled_usec: u64,
    /// cgroup current memory in bytes (`memory.current`) — physical host RAM
    /// charged to the VM's cgroup (VMM + helper processes). More complete than
    /// `rss_mb` (which is the VMM process only).
    pub cgroup_mem_bytes: u64,
}

// =============================================================================
// procfs reading
// =============================================================================

struct CpuSnap {
    utime: u64,
    stime: u64,
}

fn read_cpu_snap(pid: u32) -> Option<CpuSnap> {
    let content = fs::read_to_string(format!("/proc/{}/stat", pid)).ok()?;
    let fields: Vec<&str> = content.split_whitespace().collect();
    // /proc/pid/stat: field 14=utime (index 13), field 15=stime (index 14)
    let utime = fields.get(13)?.parse::<u64>().ok()?;
    let stime = fields.get(14)?.parse::<u64>().ok()?;
    Some(CpuSnap { utime, stime })
}

fn read_rss_mb(pid: u32) -> u32 {
    let content = fs::read_to_string(format!("/proc/{}/status", pid)).unwrap_or_default();
    for line in content.lines() {
        if line.starts_with("VmRSS:") {
            // Format: "VmRSS:   524288 kB"
            return line.split_whitespace()
                .nth(1)
                .and_then(|s| s.parse::<u32>().ok())
                .unwrap_or(0)
                / 1024;
        }
    }
    0
}

fn read_tap_stats(tap_name: &str) -> (u64, u64) {
    let content = fs::read_to_string("/proc/net/dev").unwrap_or_default();
    let tap_prefix = format!("{}:", tap_name);
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with(&tap_prefix) {
            let parts: Vec<&str> = trimmed.split_whitespace().collect();
            // Indexes: 0=iface:, 1=rx_bytes, 2..8=other rx, 9=tx_bytes
            let rx = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
            let tx = parts.get(9).and_then(|s| s.parse().ok()).unwrap_or(0);
            return (rx, tx);
        }
    }

    (0, 0)
}

/// Reads the VM's cgroup accounting from `/sys/fs/cgroup/nkr/<vm_name>/`:
/// `cpu.stat` (`usage_usec`, `throttled_usec`) and `memory.current`. This is
/// the CPU/RAM the *whole VM* costs the host — it includes the VMM's vCPU
/// thread(s) plus the virtiofsd/vhost helper processes attributed to that VM,
/// so it's more complete than the per-`/proc/<pid>` figures. Returns
/// `(cpu_usec, throttled_usec, memory_current_bytes)`; any field that can't be
/// read comes back 0 (cgroup missing during teardown, controller not enabled).
fn read_cgroup_stats(vm_name: &str) -> (u64, u64, u64) {
    let base = format!("/sys/fs/cgroup/nkr/{}", vm_name);
    let mut cpu_usec = 0u64;
    let mut throttled_usec = 0u64;
    if let Ok(s) = fs::read_to_string(format!("{}/cpu.stat", base)) {
        for line in s.lines() {
            let mut it = line.split_whitespace();
            match it.next() {
                Some("usage_usec") => cpu_usec = it.next().and_then(|v| v.parse().ok()).unwrap_or(0),
                Some("throttled_usec") => throttled_usec = it.next().and_then(|v| v.parse().ok()).unwrap_or(0),
                _ => {}
            }
        }
    }
    let mem_bytes = fs::read_to_string(format!("{}/memory.current", base))
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0);
    (cpu_usec, throttled_usec, mem_bytes)
}

/// `du -sb <path>` with a 60s per-path cache. Returns apparent-size bytes
/// (logical data size — what the tenant "thinks" they're storing). 0 on error
/// or if the path doesn't exist.
fn du_bytes_cached(path: &str) -> u64 {
    {
        let mut g = DU_CACHE.lock().unwrap();
        if let Some((t, v)) = g.get_or_insert_with(HashMap::new).get(path) {
            if t.elapsed() < DU_TTL { return *v; }
        }
    }
    let v = std::process::Command::new("du")
        .args(["-sb", path])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| {
            String::from_utf8_lossy(&o.stdout)
                .split_whitespace()
                .next()
                .and_then(|s| s.parse::<u64>().ok())
        })
        .unwrap_or(0);
    DU_CACHE.lock().unwrap().get_or_insert_with(HashMap::new)
        .insert(path.to_string(), (Instant::now(), v));
    v
}

/// For an `.ext4` (or any) file: `(allocated_bytes, nominal_size_bytes)` via
/// `stat` — `st_blocks * 512` (real on-disk usage, respects sparse/reflink) and
/// `st_size` (nominal). 0/0 on error.
fn stat_block_used_total(path: &str) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    match fs::metadata(path) {
        Ok(m) => (m.blocks().saturating_mul(512), m.size()),
        Err(_) => (0, 0),
    }
}

/// Locates a tenant's instance directory by scanning `/mnt/nkr/cells/*/instances/<vm_name>`.
/// `VmState` carries `cell_id` (numeric) not the cell name, so we glob. Returns
/// `None` for pg/pgbouncer VMs (no instance dir) or if not found.
fn find_instance_dir(vm_name: &str) -> Option<String> {
    let cells = fs::read_dir("/mnt/nkr/cells").ok()?;
    for ent in cells.flatten() {
        let cand = ent.path().join("instances").join(vm_name);
        if cand.is_dir() {
            return Some(cand.to_string_lossy().into_owned());
        }
    }
    None
}

/// Disk usage breakdown for a VM: `(mount_label, used_bytes, total_bytes)`.
/// total=0 means "no fixed cap" (a virtio-fs share dir grows on the host fs).
/// Covers: the tenant's instance-dir subdirs that exist (`addons`, `filestore`,
/// `logs`, `pylibs`) via `du`, plus its block `.ext4` disks via `stat`.
fn disk_usage_for_vm(vm: &VmState) -> Vec<(String, u64, u64)> {
    let mut out = Vec::new();
    if let Some(dir) = find_instance_dir(&vm.name) {
        for sub in ["addons", "filestore", "logs", "pylibs"] {
            let p = format!("{}/{}", dir, sub);
            if std::path::Path::new(&p).is_dir() {
                out.push((sub.to_string(), du_bytes_cached(&p), 0));
            }
        }
    }
    for disk in &vm.disks {
        // Skip the shared master rootfs ext4 (RO, used==full, uninteresting).
        if disk.contains("/images/") { continue; }
        let label = std::path::Path::new(disk)
            .file_stem()
            .map(|s| format!("disk:{}", s.to_string_lossy()))
            .unwrap_or_else(|| "disk".to_string());
        let (used, total) = stat_block_used_total(disk);
        if total > 0 { out.push((label, used, total)); }
    }
    out
}

/// Enumerates every tenant NKR knows about (running or not) as
/// `(vm_name, cell, tier)`: scans `cells/<cell>/instances/*/meta.json` for Odoo
/// tenants, plus the per-cell infra VMs (`<cell>-db`, `<cell>-pgb`, tier
/// `"infra"`). Used to emit `nkr_up{vm,cell,tier}` — an info-style metric the
/// panel joins against (`metric * on(vm) group_left(cell,tier) nkr_up`) so the
/// other series don't need to carry cell/tier labels themselves.
fn discover_tenants() -> Vec<(String, String, String)> {
    let mut out = Vec::new();
    let cells_dir = crate::cell::cells_dir();
    for cell in crate::cell::list_cells() {
        out.push((format!("{}-db", cell.name), cell.name.clone(), "infra".to_string()));
        out.push((format!("{}-pgb", cell.name), cell.name.clone(), "infra".to_string()));
        let inst_dir = cells_dir.join(&cell.name).join("instances");
        if let Ok(rd) = fs::read_dir(&inst_dir) {
            for ent in rd.flatten() {
                if !ent.path().is_dir() { continue; }
                let name = ent.file_name().to_string_lossy().into_owned();
                let tier = fs::read_to_string(ent.path().join("meta.json"))
                    .ok()
                    .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
                    .and_then(|v| v.get("tier").and_then(|t| t.as_str()).map(String::from))
                    .unwrap_or_else(|| "unknown".to_string());
                out.push((name, cell.name.clone(), tier));
            }
        }
    }
    out
}

fn read_io_bytes(pid: u32) -> (u64, u64) {
    let content = fs::read_to_string(format!("/proc/{}/io", pid)).unwrap_or_default();
    let mut read_b = 0u64;
    let mut write_b = 0u64;
    for line in content.lines() {
        if line.starts_with("read_bytes:") {
            read_b = line.split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
        } else if line.starts_with("write_bytes:") {
            write_b = line.split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
        }
    }
    (read_b, write_b)
}

// =============================================================================
// Public measurement API
// =============================================================================

/// Measures resources of all VMs with a single 200ms CPU window.
/// This way the total cost is always ~200ms regardless of VM count.
pub fn measure_all(vms: &[VmState]) -> Vec<VmMetrics> {
    if vms.is_empty() {
        return Vec::new();
    }

    // CPU snapshot 1 for all VMs
    let snaps1: Vec<Option<CpuSnap>> = vms.iter().map(|v| read_cpu_snap(v.pid)).collect();

    // Measurement window
    std::thread::sleep(Duration::from_millis(200));

    // Snapshot 2 + remaining metrics (instantaneous)
    let clk_tck = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as f64;
    let elapsed_ticks = 0.2 * clk_tck; // 200ms in jiffies

    vms.iter()
        .zip(snaps1.iter())
        .map(|(vm, snap1)| {
            let snap2 = read_cpu_snap(vm.pid);

            let cpu_pct = match (snap1, &snap2) {
                (Some(s1), Some(s2)) => {
                    let delta =
                        ((s2.utime + s2.stime) as f64) - ((s1.utime + s1.stime) as f64);
                    ((delta / elapsed_ticks) * 100.0).clamp(0.0, 100.0) as f32
                }
                _ => 0.0,
            };

            let rss_mb = read_rss_mb(vm.pid);
            let (net_rx_bytes, net_tx_bytes) = read_tap_stats(&vm.tap_name);
            let (io_read_bytes, io_write_bytes) = read_io_bytes(vm.pid);

            // Savings calculation:
            //   balloon_mb: pages donated to host (from state)
            //   dax_savings_mb: if any DAX path is active (virtio-pmem or virtio-fs master
            //     rootfs with DAX window), estimate the RAM not backed by anonymous guest
            //     pages = max(0, ram_allocated - rss - balloon_mb - 50 MB VMM overhead).
            let balloon_mb = vm.balloon_mb;
            let overhead_vmm_mb = 50u32; // NKR process overhead (code, stack, etc.)
            let dax_savings_mb = if vm.use_dax && rss_mb < vm.ram_mb {
                let effective_rss = rss_mb.saturating_add(balloon_mb).saturating_add(overhead_vmm_mb);
                vm.ram_mb.saturating_sub(effective_rss)
            } else {
                0
            };

            VmMetrics {
                cpu_pct,
                rss_mb,
                ram_allocated_mb: vm.ram_mb,
                balloon_mb,
                dax_savings_mb,
                net_rx_bytes,
                net_tx_bytes,
                io_read_bytes,
                io_write_bytes,
                ..Default::default()
            }
        })
        .collect()
}

// =============================================================================
// Stats table printing
// =============================================================================

/// Formats bytes to readable units (B/K/M/G)
pub fn fmt_bytes(bytes: u64) -> String {
    match bytes {
        0..=1023 => format!("{}B", bytes),
        1024..=1_048_575 => format!("{:.1}K", bytes as f64 / 1024.0),
        1_048_576..=1_073_741_823 => format!("{:.1}M", bytes as f64 / 1_048_576.0),
        _ => format!("{:.2}G", bytes as f64 / 1_073_741_824.0),
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max.saturating_sub(1)])
    }
}

// =============================================================================
// Prometheus exporter (Feature 5)
// =============================================================================

/// Generates the body in Prometheus text exposition 0.0.4 format.
/// Invoked by the IPC server when the HTTP proxy receives GET /metrics.
pub fn render_prometheus_metrics() -> String {
    use crate::state;
    let vms = state::list_vms();

    // 50ms window for frequent scrapes (Prometheus default every 15s)
    let metrics = {
        let snaps1: Vec<Option<CpuSnap>> = vms.iter().map(|v| read_cpu_snap(v.pid)).collect();
        std::thread::sleep(std::time::Duration::from_millis(50));
        let clk_tck = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as f64;
        let elapsed_ticks = 0.05 * clk_tck;
        vms.iter().zip(snaps1.iter()).map(|(vm, snap1)| {
            let snap2 = read_cpu_snap(vm.pid);
            let cpu_pct = match (snap1, &snap2) {
                (Some(s1), Some(s2)) => {
                    let delta = ((s2.utime + s2.stime) as f64) - ((s1.utime + s1.stime) as f64);
                    ((delta / elapsed_ticks) * 100.0).clamp(0.0, 100.0) as f32
                }
                _ => 0.0,
            };
            let rss_mb = read_rss_mb(vm.pid);
            let (net_rx, net_tx) = read_tap_stats(&vm.tap_name);
            let (io_r, io_w) = read_io_bytes(vm.pid);
            let (cg_cpu, cg_throttle, cg_mem) = read_cgroup_stats(&vm.name);
            let balloon_mb = vm.balloon_mb;
            let dax_savings_mb = if vm.use_dax && rss_mb < vm.ram_mb {
                let eff = rss_mb.saturating_add(balloon_mb).saturating_add(50);
                vm.ram_mb.saturating_sub(eff)
            } else { 0 };
            VmMetrics { cpu_pct, rss_mb,
                ram_allocated_mb: vm.ram_mb,
                balloon_mb,
                dax_savings_mb,
                net_rx_bytes: net_rx, net_tx_bytes: net_tx,
                io_read_bytes: io_r, io_write_bytes: io_w,
                cgroup_cpu_usec: cg_cpu, cgroup_cpu_throttled_usec: cg_throttle,
                cgroup_mem_bytes: cg_mem }
        }).collect::<Vec<_>>()
    };

    let mut out = String::with_capacity(2048);

    // ── cpu_pct ──
    out.push_str("# HELP nkr_cpu_pct CPU usage percentage (0-100)\n");
    out.push_str("# TYPE nkr_cpu_pct gauge\n");
    for (vm, m) in vms.iter().zip(metrics.iter()) {
        out.push_str(&format!("nkr_cpu_pct{{vm=\"{}\"}} {:.2}\n", vm.name, m.cpu_pct));
    }

    // ── rss_mb ──
    out.push_str("# HELP nkr_rss_mb Resident Set Size (physical RAM used on host) in MB\n");
    out.push_str("# TYPE nkr_rss_mb gauge\n");
    for (vm, m) in vms.iter().zip(metrics.iter()) {
        out.push_str(&format!("nkr_rss_mb{{vm=\"{}\"}} {}\n", vm.name, m.rss_mb));
    }

    // ── ram_allocated_mb ──
    out.push_str("# HELP nkr_ram_allocated_mb RAM limit assigned at VM launch in MB\n");
    out.push_str("# TYPE nkr_ram_allocated_mb gauge\n");
    for (vm, m) in vms.iter().zip(metrics.iter()) {
        out.push_str(&format!("nkr_ram_allocated_mb{{vm=\"{}\"}} {}\n", vm.name, m.ram_allocated_mb));
    }

    // ── balloon_mb ──
    out.push_str("# HELP nkr_balloon_mb RAM donated back to host via VirtIO-Balloon in MB\n");
    out.push_str("# TYPE nkr_balloon_mb gauge\n");
    for (vm, m) in vms.iter().zip(metrics.iter()) {
        out.push_str(&format!("nkr_balloon_mb{{vm=\"{}\"}} {}\n", vm.name, m.balloon_mb));
    }

    // ── dax_savings_mb ──
    out.push_str("# HELP nkr_dax_savings_mb Estimated RAM saved via VirtIO-PMEM+DAX (no page-cache duplication) in MB\n");
    out.push_str("# TYPE nkr_dax_savings_mb gauge\n");
    for (vm, m) in vms.iter().zip(metrics.iter()) {
        out.push_str(&format!("nkr_dax_savings_mb{{vm=\"{}\"}} {}\n", vm.name, m.dax_savings_mb));
    }

    // ── total_savings_mb ──
    out.push_str("# HELP nkr_total_savings_mb Total RAM saved per VM (balloon + DAX) in MB\n");
    out.push_str("# TYPE nkr_total_savings_mb gauge\n");
    for (vm, m) in vms.iter().zip(metrics.iter()) {
        let total = m.balloon_mb.saturating_add(m.dax_savings_mb);
        out.push_str(&format!("nkr_total_savings_mb{{vm=\"{}\"}} {}\n", vm.name, total));
    }

    // ── io_read_bytes ──
    out.push_str("# HELP nkr_io_read_bytes Total bytes read from disk (cumulative)\n");
    out.push_str("# TYPE nkr_io_read_bytes counter\n");
    for (vm, m) in vms.iter().zip(metrics.iter()) {
        out.push_str(&format!("nkr_io_read_bytes{{vm=\"{}\"}} {}\n", vm.name, m.io_read_bytes));
    }

    // ── io_write_bytes ──
    out.push_str("# HELP nkr_io_write_bytes Total bytes written to disk (cumulative)\n");
    out.push_str("# TYPE nkr_io_write_bytes counter\n");
    for (vm, m) in vms.iter().zip(metrics.iter()) {
        out.push_str(&format!("nkr_io_write_bytes{{vm=\"{}\"}} {}\n", vm.name, m.io_write_bytes));
    }

    // ── net_rx_bytes ──
    out.push_str("# HELP nkr_net_rx_bytes TAP interface bytes received (cumulative)\n");
    out.push_str("# TYPE nkr_net_rx_bytes counter\n");
    for (vm, m) in vms.iter().zip(metrics.iter()) {
        out.push_str(&format!("nkr_net_rx_bytes{{vm=\"{}\"}} {}\n", vm.name, m.net_rx_bytes));
    }

    // ── net_tx_bytes ──
    out.push_str("# HELP nkr_net_tx_bytes TAP interface bytes transmitted (cumulative)\n");
    out.push_str("# TYPE nkr_net_tx_bytes counter\n");
    for (vm, m) in vms.iter().zip(metrics.iter()) {
        out.push_str(&format!("nkr_net_tx_bytes{{vm=\"{}\"}} {}\n", vm.name, m.net_tx_bytes));
    }

    // ── cpu_seconds_total (cgroup — supersedes the jittery nkr_cpu_pct gauge) ──
    out.push_str("# HELP nkr_cpu_seconds_total CPU seconds consumed by the VM's cgroup (vCPU + virtiofsd/vhost helpers), cumulative. Use rate() in Grafana.\n");
    out.push_str("# TYPE nkr_cpu_seconds_total counter\n");
    for (vm, m) in vms.iter().zip(metrics.iter()) {
        out.push_str(&format!("nkr_cpu_seconds_total{{vm=\"{}\"}} {:.6}\n", vm.name, m.cgroup_cpu_usec as f64 / 1_000_000.0));
    }

    // ── cpu_throttled_seconds_total (cgroup) ──
    out.push_str("# HELP nkr_cpu_throttled_seconds_total Seconds the VM was throttled off the CPU by its cpu.max quota (chrs), cumulative.\n");
    out.push_str("# TYPE nkr_cpu_throttled_seconds_total counter\n");
    for (vm, m) in vms.iter().zip(metrics.iter()) {
        out.push_str(&format!("nkr_cpu_throttled_seconds_total{{vm=\"{}\"}} {:.6}\n", vm.name, m.cgroup_cpu_throttled_usec as f64 / 1_000_000.0));
    }

    // ── cgroup_memory_bytes (memory.current — more complete than nkr_rss_mb) ──
    out.push_str("# HELP nkr_cgroup_memory_bytes Physical host RAM charged to the VM's cgroup (VMM + virtiofsd/vhost helpers), in bytes.\n");
    out.push_str("# TYPE nkr_cgroup_memory_bytes gauge\n");
    for (vm, m) in vms.iter().zip(metrics.iter()) {
        out.push_str(&format!("nkr_cgroup_memory_bytes{{vm=\"{}\"}} {}\n", vm.name, m.cgroup_mem_bytes));
    }

    // NOTE: per-VM disk usage (`du` on the tenant dirs) is NOT emitted here —
    // with 100+ instances a `du` over every filestore on each scrape would be
    // O(seconds). It lives on the per-instance endpoint `GET .../instances/
    // {name}/metrics` instead, which the panel calls one tenant at a time.

    // ── guest-internal memory (from the virtio-balloon stats vq; only VMs that have reported) ──
    let any_gm = vms.iter().any(|v| v.guest_mem_total_bytes > 0);
    if any_gm {
        out.push_str("# HELP nkr_guest_mem_total_bytes Total RAM the guest sees (MemTotal), via the virtio-balloon stats vq.\n# TYPE nkr_guest_mem_total_bytes gauge\n");
        for vm in vms.iter().filter(|v| v.guest_mem_total_bytes > 0) {
            out.push_str(&format!("nkr_guest_mem_total_bytes{{vm=\"{}\"}} {}\n", vm.name, vm.guest_mem_total_bytes));
        }
        out.push_str("# HELP nkr_guest_mem_available_bytes RAM available inside the guest (MemAvailable estimate).\n# TYPE nkr_guest_mem_available_bytes gauge\n");
        for vm in vms.iter().filter(|v| v.guest_mem_total_bytes > 0) {
            out.push_str(&format!("nkr_guest_mem_available_bytes{{vm=\"{}\"}} {}\n", vm.name, vm.guest_mem_available_bytes));
        }
        out.push_str("# HELP nkr_guest_mem_free_bytes Free RAM inside the guest (MemFree).\n# TYPE nkr_guest_mem_free_bytes gauge\n");
        for vm in vms.iter().filter(|v| v.guest_mem_total_bytes > 0) {
            out.push_str(&format!("nkr_guest_mem_free_bytes{{vm=\"{}\"}} {}\n", vm.name, vm.guest_mem_free_bytes));
        }
        out.push_str("# HELP nkr_guest_mem_cached_bytes Disk-cache RAM inside the guest (Cached).\n# TYPE nkr_guest_mem_cached_bytes gauge\n");
        for vm in vms.iter().filter(|v| v.guest_mem_total_bytes > 0) {
            out.push_str(&format!("nkr_guest_mem_cached_bytes{{vm=\"{}\"}} {}\n", vm.name, vm.guest_mem_cached_bytes));
        }
    }

    // ── up{vm,cell,tier} — info-style metric: 1 if running, 0 if known-but-stopped.
    //    Carries cell/tier so the panel can `metric * on(vm) group_left(cell,tier) nkr_up`. ──
    {
        use std::collections::HashSet;
        let running: HashSet<&str> = vms.iter().map(|v| v.name.as_str()).collect();
        let mut seen: HashSet<String> = HashSet::new();
        out.push_str("# HELP nkr_up 1 if the micro-VM is running, 0 if known but stopped. Labels cell/tier for joins.\n");
        out.push_str("# TYPE nkr_up gauge\n");
        for (vm_name, cell, tier) in discover_tenants() {
            if !seen.insert(vm_name.clone()) { continue; }
            let up = if running.contains(vm_name.as_str()) { 1 } else { 0 };
            out.push_str(&format!("nkr_up{{vm=\"{}\",cell=\"{}\",tier=\"{}\"}} {}\n", vm_name, cell, tier, up));
        }
        // Any running VM not enumerated above (defensive — odd names): emit up=1.
        for vm in &vms {
            if seen.insert(vm.name.clone()) {
                out.push_str(&format!("nkr_up{{vm=\"{}\",cell=\"\",tier=\"unknown\"}} 1\n", vm.name));
            }
        }
    }

    // ── build_info ──
    out.push_str("# HELP nkr_build_info NKR daemon build info (always 1; read the labels)\n");
    out.push_str("# TYPE nkr_build_info gauge\n");
    out.push_str(&format!("nkr_build_info{{version=\"{}\"}} 1\n", env!("CARGO_PKG_VERSION")));

    // ── cluster totals ──
    let total_rss: u64 = metrics.iter().map(|m| m.rss_mb as u64).sum();
    let total_balloon: u64 = metrics.iter().map(|m| m.balloon_mb as u64).sum();
    let total_dax: u64 = metrics.iter().map(|m| m.dax_savings_mb as u64).sum();
    out.push_str("# HELP nkr_vm_count Number of running micro-VMs registered\n");
    out.push_str("# TYPE nkr_vm_count gauge\n");
    out.push_str(&format!("nkr_vm_count {}\n", vms.len()));
    out.push_str("# HELP nkr_total_rss_mb Sum of per-VM VMM RSS in MB\n# TYPE nkr_total_rss_mb gauge\n");
    out.push_str(&format!("nkr_total_rss_mb {}\n", total_rss));
    out.push_str("# HELP nkr_total_balloon_mb Sum of per-VM balloon-inflated RAM in MB\n# TYPE nkr_total_balloon_mb gauge\n");
    out.push_str(&format!("nkr_total_balloon_mb {}\n", total_balloon));
    out.push_str("# HELP nkr_total_dax_savings_mb Sum of per-VM DAX savings in MB\n# TYPE nkr_total_dax_savings_mb gauge\n");
    out.push_str(&format!("nkr_total_dax_savings_mb {}\n", total_dax));

    out
}

/// Maps a numeric `cell_id` to its cell name via the cell registry.
fn cell_name_for_id(cell_id: u8) -> Option<String> {
    crate::cell::list_cells().into_iter().find(|c| c.cell_id == cell_id).map(|c| c.name)
}

/// `(cell, tier)` for a VM: cell from the registry (by `cell_id`); tier from the
/// tenant's `meta.json` (`tier` field), else `"infra"` for `<cell>-db`/`-pgb`,
/// else `"unknown"`.
fn cell_tier_for(vm_name: &str, cell_id: u8) -> (String, String) {
    let cell = cell_name_for_id(cell_id).unwrap_or_default();
    let tier = if !cell.is_empty() {
        fs::read_to_string(crate::cell::cells_dir().join(&cell).join("instances").join(vm_name).join("meta.json"))
            .ok()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            .and_then(|v| v.get("tier").and_then(|t| t.as_str()).map(String::from))
    } else { None };
    let tier = tier.unwrap_or_else(|| {
        if vm_name.ends_with("-db") || vm_name.ends_with("-pgb") { "infra".to_string() }
        else { "unknown".to_string() }
    });
    (cell, tier)
}

/// Per-VM metrics snapshot as JSON, for the panel's per-instance Metrics tab
/// (`GET /api/v1/cells/{cell}/instances/{name}/metrics`). Cached `VM_METRICS_TTL`
/// per VM (the disk `du` is cached longer, see `DU_TTL`). Returns:
///   - `Some(json)` with `running:true` + the full snapshot if the VM is running
///   - `Some(json)` with `running:false` (+ cell/tier if known) if it's a known
///     tenant that's currently stopped
///   - `None` if `nkr_name` doesn't match any running VM or known tenant → 404
pub fn vm_metrics_json(nkr_name: &str) -> Option<serde_json::Value> {
    // Cache hit?
    {
        let mut g = VM_METRICS_CACHE.lock().unwrap();
        if let Some((t, v)) = g.get_or_insert_with(HashMap::new).get(nkr_name) {
            if t.elapsed() < VM_METRICS_TTL {
                let mut v = v.clone();
                if let Some(obj) = v.as_object_mut() { obj.insert("stale".into(), serde_json::json!(true)); }
                return Some(v);
            }
        }
    }

    let vms = crate::state::list_vms();
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();

    let json = if let Some(vm) = vms.iter().find(|v| v.name == nkr_name) {
        let rss_mb = read_rss_mb(vm.pid);
        let (net_rx, net_tx) = read_tap_stats(&vm.tap_name);
        let (io_r, io_w) = read_io_bytes(vm.pid);
        let (cg_cpu, cg_throttle, cg_mem) = read_cgroup_stats(&vm.name);
        let balloon_mb = vm.balloon_mb;
        let dax_savings_mb = if vm.use_dax && rss_mb < vm.ram_mb {
            vm.ram_mb.saturating_sub(rss_mb.saturating_add(balloon_mb).saturating_add(50))
        } else { 0 };
        let disk: Vec<serde_json::Value> = disk_usage_for_vm(vm).into_iter()
            .map(|(mount, used, total)| serde_json::json!({"mount": mount, "used_bytes": used, "total_bytes": total}))
            .collect();
        let (cell, tier) = cell_tier_for(&vm.name, vm.cell_id);
        // guest_mem: present only if the guest has reported via the balloon
        // stats vq (total>0). RAM as the *guest* sees it, in bytes.
        let guest_mem = if vm.guest_mem_total_bytes > 0 {
            serde_json::json!({
                "total_bytes": vm.guest_mem_total_bytes,
                "free_bytes": vm.guest_mem_free_bytes,
                "available_bytes": vm.guest_mem_available_bytes,
                "cached_bytes": vm.guest_mem_cached_bytes,
            })
        } else { serde_json::Value::Null };
        serde_json::json!({
            "vm": vm.name, "cell": cell, "tier": tier,
            "running": true,
            "uptime_seconds": now.saturating_sub(vm.started_at),
            "chrs": vm.chrs,
            "ram_allocated_mb": vm.ram_mb,
            "balloon_mb": balloon_mb,
            "rss_mb": rss_mb,
            "cgroup_memory_bytes": cg_mem,
            "guest_mem": guest_mem,
            "cpu_seconds_total": cg_cpu as f64 / 1_000_000.0,
            "cpu_throttled_seconds_total": cg_throttle as f64 / 1_000_000.0,
            "dax_savings_mb": dax_savings_mb,
            "net_rx_bytes": net_rx, "net_tx_bytes": net_tx,
            "io_read_bytes": io_r, "io_write_bytes": io_w,
            "disk": disk,
            "as_of": now, "stale": false,
        })
    } else {
        // Not running — is it a known tenant?
        let known = discover_tenants();
        let entry = known.iter().find(|(n, _, _)| n == nkr_name)?;
        serde_json::json!({
            "vm": entry.0, "cell": entry.1, "tier": entry.2,
            "running": false, "as_of": now, "stale": false,
        })
    };

    VM_METRICS_CACHE.lock().unwrap().get_or_insert_with(HashMap::new)
        .insert(nkr_name.to_string(), (Instant::now(), json.clone()));
    Some(json)
}

/// Verifies critical binaries that NKR assumes available. Emits WARN for each
/// missing one — doesn't abort, but the operator sees the list before a VM
/// fails due to a missing dependency.
pub fn verify_dependencies() {
    let required: &[(&str, &str)] = &[
        ("ip",          "iproute2 — TAP/bridge setup"),
        ("iptables",    "netfilter — NAT + cell isolation"),
        ("mount",       "util-linux — mount rootfs/pmem"),
        ("umount",      "util-linux — mount teardown"),
        ("mkfs.ext4",   "e2fsprogs — create ext4 disks"),
        ("e2fsck",      "e2fsprogs — pre-boot disk check"),
        ("losetup",     "util-linux — loop devices"),
        ("chattr",      "e2fsprogs — +C on btrfs"),
        ("psql",        "postgresql-client — clone/drop DB"),
        ("pg_isready",  "postgresql-client — readiness probe"),
        ("virtiofsd",   "rust-vmm virtiofsd — shares"),
    ];
    let mut missing: Vec<&str> = Vec::new();
    for (bin, _) in required {
        let found = std::process::Command::new("which")
            .arg(bin)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !found { missing.push(bin); }
    }
    if missing.is_empty() {
        eprintln!("[NKR-DEPS] ✅ dependencias OK ({} binarios)", required.len());
    } else {
        eprintln!("[NKR-DEPS] ⚠ faltan binarios: {:?}", missing);
        eprintln!("[NKR-DEPS] algunas operaciones fallarán silenciosamente. Detalle:");
        for (bin, desc) in required {
            if missing.contains(bin) {
                eprintln!("  - {}: {}", bin, desc);
            }
        }
    }
}

// HTTP TCP listener removed in v1.5 — privilege split.
// The daemon now listens on a Unix Domain Socket (see src/ipc_server.rs).
// The unprivileged nkr-api-server binary speaks HTTP on TCP and proxies to UDS.

/// Reads /proc/loadavg → (1min, 5min, 15min).
fn read_loadavg() -> (f32, f32, f32) {
    let s = fs::read_to_string("/proc/loadavg").unwrap_or_default();
    let f: Vec<&str> = s.split_whitespace().collect();
    let p = |i: usize| f.get(i).and_then(|x| x.parse().ok()).unwrap_or(0.0);
    (p(0), p(1), p(2))
}

/// Reads MemAvailable + MemTotal from /proc/meminfo → (avail_mb, total_mb).
fn read_host_mem_mb() -> (u32, u32) {
    let s = fs::read_to_string("/proc/meminfo").unwrap_or_default();
    let mut avail = 0u32;
    let mut total = 0u32;
    for line in s.lines() {
        let mut it = line.split_whitespace();
        match it.next() {
            Some("MemTotal:") => total = it.next().and_then(|v| v.parse::<u32>().ok()).unwrap_or(0) / 1024,
            Some("MemAvailable:") => avail = it.next().and_then(|v| v.parse::<u32>().ok()).unwrap_or(0) / 1024,
            _ => {}
        }
    }
    (avail, total)
}

fn now_hms() -> String {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let h = (secs / 3600) % 24;
    let m = (secs / 60) % 60;
    let s = secs % 60;
    format!("{:02}:{:02}:{:02} UTC", h, m, s)
}

/// Colors CPU% by load: >80 red, >50 yellow, else default.
fn color_cpu(pct: f32) -> String {
    let s = format!("{:.1}%", pct);
    if pct > 80.0 { format!("\x1b[31m{}\x1b[0m", s) }
    else if pct > 50.0 { format!("\x1b[33m{}\x1b[0m", s) }
    else { s }
}

/// Pads a string that may contain ANSI escape codes to `width` visible chars.
fn pad_visible(s: &str, raw_len: usize, width: usize) -> String {
    if raw_len >= width { s.to_string() } else { format!("{}{}", s, " ".repeat(width - raw_len)) }
}

/// Prints a real-time metrics table for all active VMs.
/// Sorted by CPU% desc (htop-style). In watch mode this is called repeatedly
/// after a screen clear; in single-shot mode it runs once.
pub fn print_stats_table(vms: &[VmState]) {
    if vms.is_empty() {
        eprintln!("[NKR] No hay micro-VMs activas");
        return;
    }

    let metrics = measure_all(vms);

    // Host header.
    let (la1, la5, la15) = read_loadavg();
    let (mem_avail, mem_total) = read_host_mem_mb();
    eprintln!(
        "[NKR stats] {}  vms={}  load={:.2} {:.2} {:.2}  host_mem={}/{} MB free",
        now_hms(), vms.len(), la1, la5, la15, mem_avail, mem_total,
    );

    let headers = [
        ("CELL", 14usize),
        ("ID", 3),
        ("NOMBRE", 22),
        ("CPU%", 6),
        ("RAM real", 8),
        ("RAM cfg", 8),
        ("BALLOON", 8),
        ("DAX save", 8),
        ("NET rx/tx", 14),
        ("DISK r/w", 14),
    ];
    crate::state::print_header_row(&headers);

    // Resolve cell_id → name (igual que en print_vm_table).
    let mut cell_names: std::collections::HashMap<u8, String> =
        std::collections::HashMap::new();
    for vm in vms {
        if !cell_names.contains_key(&vm.cell_id) {
            let name = if vm.cell_id == 0 { "—".to_string() }
            else {
                crate::cell::lookup_cell_name(vm.cell_id)
                    .unwrap_or_else(|| format!("cell-{}", vm.cell_id))
            };
            cell_names.insert(vm.cell_id, name);
        }
    }

    // Sort: cell_name asc, then CPU% desc dentro de la cell.
    let mut order: Vec<usize> = (0..vms.len()).collect();
    order.sort_by(|&a, &b| {
        let na = cell_names.get(&vms[a].cell_id).cloned().unwrap_or_default();
        let nb = cell_names.get(&vms[b].cell_id).cloned().unwrap_or_default();
        na.cmp(&nb).then_with(|| {
            metrics[b].cpu_pct.partial_cmp(&metrics[a].cpu_pct)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
    });

    let mut total_rss: u32 = 0;
    let mut total_cfg: u32 = 0;
    let mut total_balloon: u32 = 0;
    let mut total_dax: u32 = 0;

    for &i in &order {
        let vm = &vms[i];
        let m = &metrics[i];
        let name = if vm.name.is_empty() { "—" } else { &vm.name };
        let cell_disp = cell_names.get(&vm.cell_id).map(|s| s.as_str()).unwrap_or("—");

        // CPU% con color: el helper print_data_row hace pad sin saber del ANSI;
        // hacemos el pad manual aquí y pasamos el string ya ancho-correcto.
        let cpu_plain = format!("{:.1}%", m.cpu_pct);
        let cpu_colored = color_cpu(m.cpu_pct);
        let cpu_cell = pad_visible(&cpu_colored, cpu_plain.len(), 6);

        let rss_str = format!("{}MB", m.rss_mb);
        let cfg_str = format!("{}MB", vm.ram_mb);
        let balloon_str = if m.balloon_mb > 0 { format!("-{}MB", m.balloon_mb) } else { "—".to_string() };
        let dax_str = if m.dax_savings_mb > 0 { format!("-{}MB", m.dax_savings_mb) } else { "—".to_string() };
        let net_str = format!("{}/{}", fmt_bytes(m.net_rx_bytes), fmt_bytes(m.net_tx_bytes));
        let io_str = format!("{}/{}", fmt_bytes(m.io_read_bytes), fmt_bytes(m.io_write_bytes));

        // Render manual de la fila para preservar el ANSI del CPU%.
        eprintln!(
            "{:<14}  {:<3}  {:<22}  {}  {:<8}  {:<8}  {:<8}  {:<8}  {:<14}  {:<14}",
            truncate(cell_disp, 14),
            vm.vm_id,
            truncate(name, 22),
            cpu_cell,
            rss_str,
            cfg_str,
            balloon_str,
            dax_str,
            net_str,
            io_str,
        );

        total_rss += m.rss_mb;
        total_cfg += vm.ram_mb;
        total_balloon += m.balloon_mb;
        total_dax += m.dax_savings_mb;
    }

    crate::state::print_footer_separator(&headers);

    let total_savings = total_balloon.saturating_add(total_dax);
    let savings_pct = if total_cfg > 0 {
        100.0 - (total_rss as f32 / total_cfg as f32 * 100.0)
    } else {
        0.0
    };
    eprintln!(
        "TOTAL  RAM real={}MB  cfg={}MB  -balloon={}MB  -dax={}MB  ({:.1}% ahorro, {} MB)",
        total_rss, total_cfg, total_balloon, total_dax, savings_pct, total_savings,
    );
}
