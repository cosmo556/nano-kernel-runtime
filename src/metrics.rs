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
//   /sys/kernel/mm/ksm → kernel same-page merging state
// =============================================================================

use std::fs;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::state::VmState;

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
}

/// State of the kernel KSM subsystem
#[derive(Debug, Default)]
pub struct KsmStatus {
    /// KSM active (1) or stopped (0)
    pub running: bool,
    /// Unique pages currently shared (each one substitutes N copies)
    pub pages_shared: u64,
    /// Total pages pointing to a shared one (real savings = pages_sharing - pages_shared)
    pub pages_sharing: u64,
    /// Pages evaluated but not shareable (unique by content)
    pub pages_unshared: u64,
    /// ms between page scans
    pub sleep_ms: u64,
    /// Pages scanned per cycle
    pub pages_to_scan: u64,
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
            }
        })
        .collect()
}

// =============================================================================
// KSM — Kernel Same-page Merging
// =============================================================================

const KSM_BASE: &str = "/sys/kernel/mm/ksm";

fn ksm_read(file: &str) -> u64 {
    fs::read_to_string(format!("{}/{}", KSM_BASE, file))
        .unwrap_or_default()
        .trim()
        .parse()
        .unwrap_or(0)
}

fn ksm_write(file: &str, value: &str) -> Result<(), Box<dyn std::error::Error>> {
    if !std::path::Path::new(KSM_BASE).exists() {
        return Err("KSM no disponible en este kernel (CONFIG_KSM no compilado)".into());
    }
    fs::write(format!("{}/{}", KSM_BASE, file), value)?;
    Ok(())
}

/// Reads the current KSM state
pub fn ksm_status() -> KsmStatus {
    if !std::path::Path::new(KSM_BASE).exists() {
        return KsmStatus::default();
    }
    KsmStatus {
        running: ksm_read("run") == 1,
        pages_shared: ksm_read("pages_shared"),
        pages_sharing: ksm_read("pages_sharing"),
        pages_unshared: ksm_read("pages_unshared"),
        sleep_ms: ksm_read("sleep_millisecs"),
        pages_to_scan: ksm_read("pages_to_scan"),
    }
}

/// Enables KSM with parameters optimized for multiple Odoo VMs.
/// With 20 identical Odoos per cell, aggressive values reduce the convergence
/// time from ~10 min to ~1 min. ksmd consumes 5-10% of a core while scanning,
/// acceptable because it's bounded (already-merged pages stay merged).
pub fn ksm_enable() -> Result<(), Box<dyn std::error::Error>> {
    ksm_write("pages_to_scan", "5000")?;
    ksm_write("sleep_millisecs", "10")?;
    ksm_write("run", "1")?;
    Ok(())
}

/// Disables KSM (already-shared pages unshare gradually)
pub fn ksm_disable() -> Result<(), Box<dyn std::error::Error>> {
    ksm_write("run", "0")?;
    Ok(())
}

/// Prints KSM state to the console
pub fn print_ksm_status() {
    if !std::path::Path::new(KSM_BASE).exists() {
        eprintln!("[KSM] No disponible en este kernel");
        return;
    }
    let s = ksm_status();
    let page_kb = 4u64; // 4 KB pages on x86_64
    let saved_kb = s.pages_sharing.saturating_sub(s.pages_shared) * page_kb;
    let saved_mb = saved_kb / 1024;

    eprintln!(
        "[KSM] estado={} | compartidas={} | ahorro≈{}MB | sin_compartir={} | escaneo={}p/{}ms",
        if s.running { "ACTIVO" } else { "detenido" },
        s.pages_sharing,
        saved_mb,
        s.pages_unshared,
        s.pages_to_scan,
        s.sleep_ms,
    );
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
    let ksm = ksm_status();

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
                io_read_bytes: io_r, io_write_bytes: io_w }
        }).collect::<Vec<_>>()
    };

    let page_kb = 4u64;
    let ksm_savings_mb = ksm.pages_sharing.saturating_sub(ksm.pages_shared) * page_kb / 1024;

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

    // ── ksm_savings_mb ──
    out.push_str("# HELP nkr_ksm_savings_mb Estimated RAM saved by KSM in MB\n");
    out.push_str("# TYPE nkr_ksm_savings_mb gauge\n");
    out.push_str(&format!("nkr_ksm_savings_mb {}\n", ksm_savings_mb));

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

    out
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

    eprintln!();
    print_ksm_status();
}
