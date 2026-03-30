// =============================================================================
// NKR VMM — Motor de Micro-VMs con acceso directo al hardware
// =============================================================================

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::convert::TryInto;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::path::Path;

/// Flag global para shutdown limpio (SIGTERM → vcpu loop sale → extract_volumes)
static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);

use kvm_bindings::{kvm_segment, kvm_userspace_memory_region, KVM_MAX_CPUID_ENTRIES};
use kvm_ioctls::{Kvm, VcpuExit, VcpuFd, VmFd, IoEventAddress};
use linux_loader::loader::bzimage::BzImage;
use linux_loader::loader::KernelLoader;
use vm_memory::{Address, Bytes, GuestAddress, GuestMemory, GuestMemoryMmap, GuestMemoryRegion};
use virtio_queue::QueueT;

use crate::block::VirtioBlockDevice;
use crate::net::VirtioNetDevice;
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

const GUEST_SUBNET: &str = "10.0.0";

// =============================================================================
// Bridge nkr0 — Auto-setup
// =============================================================================

/// Asegura que el bridge `nkr0` exista con IP 10.0.0.1/24 y forwarding habilitado
fn ensure_bridge() -> Result<(), Box<dyn std::error::Error>> {
    // Verificar si el bridge ya existe
    let check = std::process::Command::new("ip")
        .args(["link", "show", "nkr0"])
        .output();

    let bridge_exists = check.map_or(false, |o| o.status.success());

    if !bridge_exists {
        eprintln!("[NKR] Creando bridge nkr0...");

        // Crear bridge
        let status = std::process::Command::new("ip")
            .args(["link", "add", "name", "nkr0", "type", "bridge"])
            .status()
            .map_err(|e| format!("Fallo creando bridge nkr0: {e}"))?;
        if !status.success() {
            return Err("Fallo 'ip link add nkr0' (¿ejecutando con sudo?)".into());
        }

        // Asignar IP al bridge
        let _ = std::process::Command::new("ip")
            .args(["addr", "add", "10.0.0.1/24", "dev", "nkr0"])
            .status();

        // Levantar el bridge
        let _ = std::process::Command::new("ip")
            .args(["link", "set", "nkr0", "up"])
            .status();

        // Habilitar IP forwarding
        let _ = std::fs::write("/proc/sys/net/ipv4/ip_forward", "1");

        // route_localnet: necesario para DNAT localhost → guest (port forwarding)
        let _ = std::fs::write("/proc/sys/net/ipv4/conf/all/route_localnet", "1");
        let _ = std::fs::write("/proc/sys/net/ipv4/conf/nkr0/route_localnet", "1");

        // NAT/Masquerade para que los guests tengan salida a internet
        let _ = std::process::Command::new("iptables")
            .args(["-t", "nat", "-C", "POSTROUTING", "-s", "10.0.0.0/24",
                   "-j", "MASQUERADE"])
            .status()
            .and_then(|s| {
                if !s.success() {
                    std::process::Command::new("iptables")
                        .args(["-t", "nat", "-A", "POSTROUTING", "-s", "10.0.0.0/24",
                               "-j", "MASQUERADE"])
                        .status()
                } else {
                    Ok(s)
                }
            });

        // Permitir forwarding desde/hacia el bridge
        let _ = std::process::Command::new("iptables")
            .args(["-C", "FORWARD", "-i", "nkr0", "-j", "ACCEPT"])
            .status()
            .and_then(|s| {
                if !s.success() {
                    std::process::Command::new("iptables")
                        .args(["-A", "FORWARD", "-i", "nkr0", "-j", "ACCEPT"])
                        .status()
                } else {
                    Ok(s)
                }
            });

        let _ = std::process::Command::new("iptables")
            .args(["-C", "FORWARD", "-o", "nkr0", "-j", "ACCEPT"])
            .status()
            .and_then(|s| {
                if !s.success() {
                    std::process::Command::new("iptables")
                        .args(["-A", "FORWARD", "-o", "nkr0", "-j", "ACCEPT"])
                        .status()
                } else {
                    Ok(s)
                }
            });

        eprintln!("[NKR] Bridge nkr0 creado (10.0.0.1/24, NAT habilitado)");
    } else {
        eprintln!("[NKR] Bridge nkr0 ya existe");
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

/// Parsea "host_port:guest_port" y configura iptables DNAT + SNAT
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

        // DNAT: paquetes entrantes al host_port → guest_ip:guest_port
        // Usar -C (check) antes de -A (append) para ser idempotente
        let dnat = std::process::Command::new("iptables")
            .args(["-t", "nat", "-C", "PREROUTING",
                   "-p", "tcp", "--dport", &host_port.to_string(),
                   "-j", "DNAT", "--to-destination", &dest])
            .status()
            .and_then(|s| {
                if !s.success() {
                    std::process::Command::new("iptables")
                        .args(["-t", "nat", "-A", "PREROUTING",
                               "-p", "tcp", "--dport", &host_port.to_string(),
                               "-j", "DNAT", "--to-destination", &dest])
                        .status()
                } else {
                    Ok(s)
                }
            });

        // También para conexiones locales (desde el host mismo)
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

        match (dnat, dnat_local, masq) {
            (Ok(d), Ok(dl), Ok(m)) if d.success() && dl.success() && m.success() => {
                eprintln!("[NKR-NET] Port forward: host:{} → guest:{}", host_port, guest_port);
                active_rules.push((host_port, guest_port));
            }
            _ => {
                eprintln!("[NKR-NET] WARN: fallo al configurar port forward {}:{}", host_port, guest_port);
            }
        }
    }

    active_rules
}

/// Limpia las reglas de iptables creadas para port forwarding
fn cleanup_port_forwarding(rules: &[(u16, u16)], guest_ip: &str) {
    for (host_port, guest_port) in rules {
        let _ = std::process::Command::new("iptables")
            .args(["-t", "nat", "-D", "PREROUTING",
                   "-p", "tcp", "--dport", &host_port.to_string(),
                   "-j", "DNAT", "--to-destination",
                   &format!("{}:{}", guest_ip, guest_port)])
            .status();

        let _ = std::process::Command::new("iptables")
            .args(["-t", "nat", "-D", "OUTPUT",
                   "-p", "tcp", "-d", "127.0.0.1",
                   "--dport", &host_port.to_string(),
                   "-j", "DNAT", "--to-destination",
                   &format!("{}:{}", guest_ip, guest_port)])
            .status();

        let _ = std::process::Command::new("iptables")
            .args(["-t", "nat", "-D", "POSTROUTING",
                   "-p", "tcp", "-d", guest_ip,
                   "--dport", &guest_port.to_string(),
                   "-j", "MASQUERADE"])
            .status();

        eprintln!("[NKR-NET] Port forward limpiado: {}:{}", host_port, guest_port);
    }
}

/// Ejecuta una micro-VM con la configuración dada
pub fn run(config: VmConfig) -> Result<(), Box<dyn std::error::Error>> {
    let ram_bytes = config.ram_mb as usize * 1024 * 1024;

    eprintln!("╔══════════════════════════════════════════════════════════════╗");
    eprintln!("║  NKR v1.0 — Nano-Kernel Runtime                            ║");
    eprintln!("╠══════════════════════════════════════════════════════════════╣");
    eprintln!("║  RAM:    {} MB (exclusiva, pinned)", config.ram_mb);
    eprintln!("║  CPU:    {} chrs ({}% de core físico)", config.chrs, config.chrs * 20);
    eprintln!("║  Disco:  {}", config.disks.join(", "));
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

    // --- CPU Pinning: Asignar chrs a cores físicos ---
    pin_cpu_chrs(config.chrs)?;

    // --- Bridge auto-setup ---
    ensure_bridge()?;

    // --- Inyectar volúmenes (pre-boot: monta disco, copia, desmonta) ---
    let parsed_volumes = parse_volume_specs(&config.volumes);
    if !parsed_volumes.is_empty() {
        inject_volumes(&config.disks[0], &parsed_volumes)?;
    }

    // --- Inyectar variables de entorno como /etc/nkr-env ---
    if !config.env_vars.is_empty() {
        inject_env_vars(&config.disks[0], &config.env_vars)?;
    }

    let kvm = Kvm::new().map_err(|e| format!("Fallo al abrir /dev/kvm: {e}"))?;
    let vm = kvm.create_vm().map_err(|e| format!("Fallo KVM_CREATE_VM: {e}"))?;

    // Placa base virtual: Interrupciones y Reloj
    vm.create_irq_chip().map_err(|e| format!("Fallo al crear IRQ chip: {e}"))?;
    let pit_config = kvm_bindings::kvm_pit_config { flags: 0, ..Default::default() };
    vm.create_pit2(pit_config).map_err(|e| format!("Fallo al crear PIT: {e}"))?;

    // 1. Inicializar la RAM (parametrizada)
    let high_mem = ram_bytes - 0x100000;
    let guest_mem = Arc::new(GuestMemoryMmap::<()>::from_ranges(&[
        (GuestAddress(0), 0xA0000),              // 640 KB Base RAM
        (GuestAddress(0x100000), high_mem),       // Resto parametrizado
    ]).map_err(|e| format!("Fallo al crear guest memory: {e}"))?);

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

    // 3. Red Virtio  — MAC e IP dinámicos según vm_id
    let guest_ip = format!("{}.{}", GUEST_SUBNET, config.vm_id + 1);
    let mac: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, config.vm_id];

    // Auto-crear TAP si no se especificó
    let auto_tap_name = if config.tap_name.is_none() {
        let tap_name = format!("nkr-tap{}", config.vm_id);
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
            .args(["link", "set", &tap_name, "master", "nkr0"]).status();
        let _ = std::process::Command::new("ip")
            .args(["link", "set", &tap_name, "up"]).status();
        eprintln!("[NKR] Auto-TAP: {} (bridge nkr0)", tap_name);
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

    // 3. Kernel
    let entry_addr = load_bzimage_kernel(&guest_mem, &config.kernel_path)?;

    // 4. Initramfs (auto-detectar o usar especificado)
    let initrd_size = load_initramfs_auto(&guest_mem, &config.initramfs_path)?;

    // 5. Boot protocol
    configure_linux_boot(&guest_mem, initrd_size, ram_bytes, &guest_ip, &block_configs)?;
    write_page_tables(&guest_mem)?;
    write_gdt(&guest_mem)?;

    // 6. vCPU
    let mut vcpu = vm.create_vcpu(0).map_err(|e| format!("Fallo KVM_CREATE_VCPU: {e}"))?;
    let cpuid = kvm.get_supported_cpuid(KVM_MAX_CPUID_ENTRIES)?;
    vcpu.set_cpuid2(&cpuid)?;
    configure_sregs(&vcpu)?;
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
    };
    state::register_vm(&vm_state).unwrap_or_else(|e| {
        eprintln!("[NKR] WARN: No se pudo registrar VM: {e}");
    });

    eprintln!("════════════════════════════════════════════════════════════════");

    // Registrar SIGTERM handler para shutdown limpio
    SHUTDOWN_REQUESTED.store(false, Ordering::SeqCst);
    unsafe {
        libc::signal(libc::SIGTERM, sigterm_handler as *const () as libc::sighandler_t);
    }

    run_vcpu_loop(&mut vcpu, &mut block_devs, &mut net_dev, &serial_irqfd)?;

    // Limpiar port forwarding
    cleanup_port_forwarding(&forwarding_rules, &guest_ip);

    // Extraer volúmenes rw (post-shutdown: monta disco, copia guest→host)
    if !parsed_volumes.is_empty() {
        extract_volumes(&config.disks[0], &parsed_volumes).unwrap_or_else(|e| {
            eprintln!("[NKR-VOL] WARN: Error extrayendo volúmenes: {e}");
        });
    }

    // Limpiar TAP auto-creado
    if let Some(ref tap_name) = auto_tap_name {
        let _ = std::process::Command::new("ip")
            .args(["link", "delete", tap_name]).status();
        eprintln!("[NKR] TAP {} eliminado", tap_name);
    }

    // Desregistrar VM del estado global
    state::unregister_vm(config.vm_id);

    eprintln!("════════════════════════════════════════════════════════════════");
    eprintln!("[NKR] MicroVM finalizada");
    Ok(())
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

fn load_initramfs_auto(guest_mem: &GuestMemoryMmap<()>, explicit_path: &Option<String>) -> Result<u32, Box<dyn std::error::Error>> {
    // Si se especificó una ruta explícita, usarla
    if let Some(path) = explicit_path {
        return load_initramfs(guest_mem, path);
    }

    // Auto-detectar
    for candidate in &["initramfs.cpio.gz", "initramfs.cpio"] {
        if std::path::Path::new(candidate).exists() {
            match load_initramfs(guest_mem, candidate) {
                Ok(size) => return Ok(size),
                Err(e) => eprintln!("[NKR] WARN: No se pudo cargar {candidate}: {e}"),
            }
        }
    }

    eprintln!("[NKR] Sin initramfs — módulos del kernel no disponibles");
    Ok(0)
}

fn load_initramfs(guest_mem: &GuestMemoryMmap<()>, path: &str) -> Result<u32, Box<dyn std::error::Error>> {
    let mut file = File::open(path)?;
    let mut data = Vec::new();
    file.read_to_end(&mut data)?;
    let size = data.len() as u32;
    guest_mem.write_slice(&data, GuestAddress(INITRAMFS_ADDR))?;
    eprintln!("[NKR] Initramfs: {path} ({} KB)", size / 1024);
    Ok(size)
}

fn configure_linux_boot(guest_mem: &GuestMemoryMmap<()>, initrd_size: u32, ram_bytes: usize, guest_ip: &str, block_configs: &[(u64, u32)]) -> Result<(), Box<dyn std::error::Error>> {
    let mut cmdline_str = format!("console=ttyS0 panic=1 pci=off noapic nolapic clocksource=jiffies tsc=nowatchdog virtio_mmio.device=4K@0xd0000000:5");
    
    // Agregar dinámicamente cada disco VirtIO
    for (addr, irq) in block_configs {
        cmdline_str.push_str(&format!(" virtio_mmio.device=4K@{:#010x}:{}", addr, irq));
    }
    
    cmdline_str.push_str(&format!(" root=/dev/vda rw init=/sbin/init nkr.ip={} \0", guest_ip));
    
    let cmdline = cmdline_str.as_bytes();
    guest_mem.write_slice(cmdline, GuestAddress(CMDLINE_ADDR))?;

    guest_mem.write_obj(0xFFu8, GuestAddress(ZERO_PAGE_ADDR + 0x210))?;
    guest_mem.write_obj(0x81u8, GuestAddress(ZERO_PAGE_ADDR + 0x211))?;

    guest_mem.write_obj(INITRAMFS_ADDR as u32, GuestAddress(ZERO_PAGE_ADDR + 0x218))?;
    guest_mem.write_obj(initrd_size, GuestAddress(ZERO_PAGE_ADDR + 0x21C))?;
    guest_mem.write_obj(CMDLINE_ADDR as u32, GuestAddress(ZERO_PAGE_ADDR + 0x228))?;

    // E820 Map con RAM parametrizada
    guest_mem.write_obj(0x0u64, GuestAddress(ZERO_PAGE_ADDR + 0x2D0))?;
    guest_mem.write_obj(0x9FC00u64, GuestAddress(ZERO_PAGE_ADDR + 0x2D8))?;
    guest_mem.write_obj(1u32, GuestAddress(ZERO_PAGE_ADDR + 0x2E0))?;

    let high_mem_size = (ram_bytes as u64) - 0x100000;
    guest_mem.write_obj(0x100000u64, GuestAddress(ZERO_PAGE_ADDR + 0x2E4))?;
    guest_mem.write_obj(high_mem_size, GuestAddress(ZERO_PAGE_ADDR + 0x2EC))?;
    guest_mem.write_obj(1u32, GuestAddress(ZERO_PAGE_ADDR + 0x2F4))?;

    guest_mem.write_obj(2u8, GuestAddress(ZERO_PAGE_ADDR + 0x1E8))?;
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
    regs.rsi = ZERO_PAGE_ADDR;
    regs.rsp = ZERO_PAGE_ADDR;
    regs.rflags = 0x2;
    vcpu.set_regs(&regs)?;
    Ok(())
}

// =============================================================================
// Bucle principal del vCPU — Emulación MMIO
// =============================================================================

extern "C" fn sigterm_handler(_sig: libc::c_int) {
    SHUTDOWN_REQUESTED.store(true, Ordering::SeqCst);
}

fn run_vcpu_loop(vcpu: &mut VcpuFd, block_devs: &mut Vec<VirtioBlockDevice>, net_dev: &mut VirtioNetDevice, serial_irqfd: &vmm_sys_util::eventfd::EventFd) -> Result<(), Box<dyn std::error::Error>> {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    let mut serial_ier: u8 = 0;

    loop {
        if SHUTDOWN_REQUESTED.load(Ordering::SeqCst) {
            eprintln!("\n[NKR] SIGTERM recibido — shutdown limpio...");
            break;
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
                        // IIR: if THRE enabled, report THRE interrupt (0x02)
                        // else no interrupt pending (0x01)
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
                }
            }

            Ok(VcpuExit::Hlt) => break,
            Ok(VcpuExit::Shutdown) => {
                eprintln!("\n[NKR] vCPU shutdown");
                break;
            }
            Ok(_) => {},
            Err(e) => {
                // EINTR = señal recibida (SIGTERM) → salir limpiamente
                if SHUTDOWN_REQUESTED.load(Ordering::SeqCst) {
                    eprintln!("\n[NKR] SIGTERM recibido — shutdown limpio...");
                    break;
                }
                // EINTR sin shutdown = señal benigna, reintentar
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
