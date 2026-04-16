// =============================================================================
// NKR Metrics — Medición de recursos en tiempo real por micro-VM
// =============================================================================
//
// Lee métricas del kernel via procfs para cada proceso VMM activo.
// CPU% se calcula con una ventana de 200ms compartida entre todas las VMs
// para minimizar la latencia total al mostrar múltiples VMs.
//
// Fuentes de datos:
//   /proc/{pid}/stat   → tiempos de CPU (utime + stime en ticks)
//   /proc/{pid}/status → VmRSS: RAM física real en el host
//   /proc/{pid}/io     → bytes leídos/escritos a disco
//   /proc/net/dev      → bytes RX/TX por interfaz TAP
//   /sys/kernel/mm/ksm → estado del kernel same-page merging
// =============================================================================

use std::fs;
use std::time::Duration;

use crate::state::VmState;

// =============================================================================
// Estructuras de datos
// =============================================================================

/// Métricas en tiempo real de una micro-VM
#[derive(Debug, Clone, Default)]
pub struct VmMetrics {
    /// Uso de CPU en porcentaje (ventana de 200ms)
    pub cpu_pct: f32,
    /// RAM física real usada en el host (RSS del proceso VMM), en MB
    pub rss_mb: u32,
    /// RAM configurada (límite asignado al arrancar la VM), en MB
    pub ram_allocated_mb: u32,
    /// MB ahorrados vía VirtIO-Balloon (devueltos al host por el guest)
    pub balloon_mb: u32,
    /// MB estimados ahorrados vía DAX/PMEM (page cache no duplicada)
    /// Cálculo: max(0, ram_allocated - rss - balloon - overhead_vmm)
    pub dax_savings_mb: u32,
    /// Bytes recibidos por la interfaz TAP (tráfico externo→VM)
    pub net_rx_bytes: u64,
    /// Bytes transmitidos por la interfaz TAP (tráfico VM→externo)
    pub net_tx_bytes: u64,
    /// Bytes leídos de disco (acumulado desde inicio del proceso)
    pub io_read_bytes: u64,
    /// Bytes escritos a disco (acumulado desde inicio del proceso)
    pub io_write_bytes: u64,
}

/// Estado del subsistema KSM del kernel
#[derive(Debug, Default)]
pub struct KsmStatus {
    /// KSM activo (1) o detenido (0)
    pub running: bool,
    /// Páginas únicas compartidas actualmente (cada una sustituye a N copias)
    pub pages_shared: u64,
    /// Total de páginas que apuntan a una compartida (ahorro real = pages_sharing - pages_shared)
    pub pages_sharing: u64,
    /// Páginas evaluadas pero no compartibles (únicas por contenido)
    pub pages_unshared: u64,
    /// ms entre escaneos de páginas
    pub sleep_ms: u64,
    /// Páginas escaneadas por ciclo
    pub pages_to_scan: u64,
}

// =============================================================================
// Lectura de procfs
// =============================================================================

struct CpuSnap {
    utime: u64,
    stime: u64,
}

fn read_cpu_snap(pid: u32) -> Option<CpuSnap> {
    let content = fs::read_to_string(format!("/proc/{}/stat", pid)).ok()?;
    let fields: Vec<&str> = content.split_whitespace().collect();
    // /proc/pid/stat: campo 14=utime (índice 13), campo 15=stime (índice 14)
    let utime = fields.get(13)?.parse::<u64>().ok()?;
    let stime = fields.get(14)?.parse::<u64>().ok()?;
    Some(CpuSnap { utime, stime })
}

fn read_rss_mb(pid: u32) -> u32 {
    let content = fs::read_to_string(format!("/proc/{}/status", pid)).unwrap_or_default();
    for line in content.lines() {
        if line.starts_with("VmRSS:") {
            // Formato: "VmRSS:   524288 kB"
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
            // Índices: 0=iface:, 1=rx_bytes, 2..8=otros rx, 9=tx_bytes
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
// API pública de medición
// =============================================================================

/// Mide recursos de todas las VMs con una sola ventana de 200ms para CPU.
/// Así el coste total es siempre ~200ms independientemente del número de VMs.
pub fn measure_all(vms: &[VmState]) -> Vec<VmMetrics> {
    if vms.is_empty() {
        return Vec::new();
    }

    // Snapshot 1 de CPU para todas las VMs
    let snaps1: Vec<Option<CpuSnap>> = vms.iter().map(|v| read_cpu_snap(v.pid)).collect();

    // Ventana de medición
    std::thread::sleep(Duration::from_millis(200));

    // Snapshot 2 + resto de métricas (instantáneas)
    let clk_tck = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as f64;
    let elapsed_ticks = 0.2 * clk_tck; // 200ms en jiffies

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

            // Cálculo de ahorros:
            //   balloon_mb: páginas donadas al host (desde state)
            //   dax_savings_mb: si PMEM activo, estimamos el ahorro de page cache
            //     = max(0, ram_allocated - rss - balloon_mb - 50 MB overhead VMM)
            let balloon_mb = vm.balloon_mb;
            let overhead_vmm_mb = 50u32; // overhead del proceso NKR (código, stack, etc.)
            let dax_savings_mb = if vm.use_pmem && rss_mb < vm.ram_mb {
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

/// Lee el estado actual de KSM
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

/// Activa KSM con parámetros optimizados para múltiples VMs Odoo
pub fn ksm_enable() -> Result<(), Box<dyn std::error::Error>> {
    // Escanear más páginas por ciclo y con menos pausa → detección más rápida
    ksm_write("pages_to_scan", "1000")?;
    ksm_write("sleep_millisecs", "50")?;
    ksm_write("run", "1")?;
    Ok(())
}

/// Desactiva KSM (las páginas ya compartidas se descomparten gradualmente)
pub fn ksm_disable() -> Result<(), Box<dyn std::error::Error>> {
    ksm_write("run", "0")?;
    Ok(())
}

/// Imprime el estado de KSM en consola
pub fn print_ksm_status() {
    if !std::path::Path::new(KSM_BASE).exists() {
        eprintln!("[KSM] No disponible en este kernel");
        return;
    }
    let s = ksm_status();
    let page_kb = 4u64; // páginas de 4 KB en x86_64
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
// Impresión de tabla de stats
// =============================================================================

/// Formatea bytes a unidades legibles (B/K/M/G)
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
// Exportador Prometheus (Feature 5)
// =============================================================================

/// Genera el cuerpo en formato Prometheus text exposition 0.0.4
fn render_prometheus_metrics() -> String {
    use crate::state;
    let vms = state::list_vms();
    let ksm = ksm_status();

    // Ventana de 50ms para scrapes frecuentes (Prometheus por defecto cada 15s)
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
            let dax_savings_mb = if vm.use_pmem && rss_mb < vm.ram_mb {
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

/// Inicia un servidor HTTP mínimo que expone /metrics en formato Prometheus.
/// Se lanza en un hilo daemon y nunca bloquea el hilo principal.
pub fn start_prometheus_server(port: u16) {
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;

    std::thread::spawn(move || {
        let addr = format!("0.0.0.0:{}", port);
        let listener = match TcpListener::bind(&addr) {
            Ok(l) => {
                eprintln!("[NKR-PROM] Servidor Prometheus en http://{}/metrics", addr);
                l
            }
            Err(e) => {
                eprintln!("[NKR-PROM] Error al bindear {}: {}", addr, e);
                return;
            }
        };

        for stream in listener.incoming() {
            let mut stream = match stream {
                Ok(s) => s,
                Err(_) => continue,
            };

            // Drenar los headers HTTP (leer hasta línea en blanco)
            if let Ok(mut reader) = stream.try_clone().map(BufReader::new) {
                let mut line = String::new();
                loop {
                    line.clear();
                    match reader.read_line(&mut line) {
                        Ok(0) | Err(_) => break,
                        _ => {}
                    }
                    if line == "\r\n" || line == "\n" {
                        break;
                    }
                }
            }

            let body = render_prometheus_metrics();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain; version=0.0.4; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });
}

/// Imprime tabla de métricas en tiempo real para todas las VMs activas
pub fn print_stats_table(vms: &[VmState]) {
    if vms.is_empty() {
        eprintln!("[NKR] No hay micro-VMs activas");
        return;
    }

    eprint!("[NKR] Midiendo recursos");
    let metrics = measure_all(vms);
    eprintln!(" — listo");

    eprintln!("╔═════╦══════════════════╦════════╦══════════╦═════════╦══════════╦══════════╦════════════════╦════════════════╗");
    eprintln!("║ ID  ║      NOMBRE      ║  CPU%  ║ RAM real ║ RAM cfg ║ Balloon  ║ DAX save ║   NET  rx/tx   ║   DISK  r/w    ║");
    eprintln!("╠═════╬══════════════════╬════════╬══════════╬═════════╬══════════╬══════════╬════════════════╬════════════════╣");

    let mut total_rss: u32 = 0;
    let mut total_cfg: u32 = 0;
    let mut total_balloon: u32 = 0;
    let mut total_dax: u32 = 0;

    for (vm, m) in vms.iter().zip(metrics.iter()) {
        let name = if vm.name.is_empty() { "—" } else { &vm.name };

        let cpu_str = format!("{:.1}%", m.cpu_pct);
        let rss_str = format!("{}MB", m.rss_mb);
        let cfg_str = format!("{}MB", vm.ram_mb);
        let balloon_str = if m.balloon_mb > 0 { format!("-{}MB", m.balloon_mb) } else { "—".to_string() };
        let dax_str = if m.dax_savings_mb > 0 { format!("-{}MB", m.dax_savings_mb) } else { "—".to_string() };
        let net_str = format!("{}/{}", fmt_bytes(m.net_rx_bytes), fmt_bytes(m.net_tx_bytes));
        let io_str = format!("{}/{}", fmt_bytes(m.io_read_bytes), fmt_bytes(m.io_write_bytes));

        eprintln!(
            "║ {:<3} ║ {:<16} ║ {:<6} ║ {:<8} ║ {:<7} ║ {:<8} ║ {:<8} ║ {:<14} ║ {:<14} ║",
            vm.vm_id,
            truncate(name, 16),
            cpu_str,
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

    let total_savings = total_balloon.saturating_add(total_dax);
    let savings_pct = if total_cfg > 0 {
        100.0 - (total_rss as f32 / total_cfg as f32 * 100.0)
    } else {
        0.0
    };

    eprintln!("╠═════╩══════════════════╩════════╬══════════╬═════════╬══════════╬══════════╬════════════════╩════════════════╣");
    eprintln!(
        "║ TOTAL                           ║ {:>6}MB ║ {:>5}MB ║ -{:>5}MB ║ -{:>5}MB ║  {:.1}% ahorro total ({} MB)       ║",
        total_rss,
        total_cfg,
        total_balloon,
        total_dax,
        savings_pct,
        total_savings,
    );
    eprintln!("╚═════════════════════════════════╩══════════╩═════════╩══════════╩══════════╩═══════════════════════════════════╝");

    eprintln!();
    print_ksm_status();
}
