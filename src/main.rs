// =============================================================================
// NKR (Nano-Kernel Runtime) v1.0.0
// Contenedores ultra-rápidos con micro-VMs y acceso directo al hardware
// =============================================================================

use std::process;

fn parse_duration(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() { return None; }
    let (num, unit): (&str, &str) = match s.chars().last()? {
        c if c.is_ascii_digit() => (s, "s"),
        _ => (&s[..s.len()-1], &s[s.len()-1..]),
    };
    let n: u64 = num.parse().ok()?;
    match unit {
        "s" | "S" => Some(n),
        "m" | "M" => Some(n * 60),
        "h" | "H" => Some(n * 3600),
        _ => None,
    }
}

mod cli;
mod vmm;
mod block;
mod net;
mod compose;
mod pull;
mod build;
mod state;
mod initramfs;
mod registry;
mod metrics;
mod seccomp;
mod pmem;
mod balloon;
mod virtio_fs;
mod console;
mod cell;
mod fsutil;
mod netlock;
mod api;

fn main() {
    let args = cli::parse();

    match args.command {
        cli::Command::Run { hash, name, ram, chrs, id, disk, kernel, initramfs, port, volume, env, tap, share, rootfs, pmem, balloon_mb, burst, cell_id } => {
            let vm_hash = hash.unwrap_or_else(|| {
                let nanos = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .subsec_nanos();
                format!("{:08x}{:04x}", nanos, std::process::id() & 0xFFFF)
            });

            let vm_name = if name.is_empty() {
                format!("nkr-{}", id)
            } else {
                name
            };

            // Registry scoped por cell: key = "cell_name/vm_name" si cell_id>0.
            // Si el caller pasó --id explícito (compose lo hace siempre), se respeta
            // vía register_explicit_scoped; sin --id se auto-resuelve.
            let has_explicit_name = !vm_name.starts_with("nkr-") || vm_name != format!("nkr-{}", id);
            let cell_name_opt = cell::lookup_cell_name(cell_id);
            let vm_id = if has_explicit_name {
                let result = if id > 0 {
                    registry::register_explicit_scoped(cell_name_opt.as_deref(), &vm_name, id).map(|_| id)
                } else {
                    registry::resolve_id_scoped(cell_name_opt.as_deref(), &vm_name)
                };
                match result {
                    Ok(resolved) => resolved,
                    Err(e) => {
                        eprintln!("[NKR] Error registry: {e}");
                        id
                    }
                }
            } else {
                id
            };

            let config = cli::VmConfig {
                hash: vm_hash,
                name: vm_name,
                ram_mb: ram,
                chrs,
                vm_id,
                disks: disk,
                kernel_path: kernel,
                initramfs_path: initramfs,
                port_forwards: port,
                volumes: volume,
                env_vars: env,
                tap_name: tap,
                shares: share,
                rootfs,
                use_pmem: pmem,
                balloon_mb,
                burst,
                cell_id,
            };

            if let Err(e) = vmm::run(config) {
                eprintln!("[NKR] Error fatal: {e}");
                process::exit(1);
            }
        }

        cli::Command::Ps => {
            state::print_vm_table();
        }

        cli::Command::Stop { id } => {
            if let Some(vm) = state::find_vm_by_id_str(&id) {
                if let Err(e) = state::stop_vm(vm.vm_id) {
                    eprintln!("[NKR] Error deteniendo VM: {e}");
                    process::exit(1);
                }
            } else {
                eprintln!("[NKR] VM '{}' no encontrada. Usa 'nkr ps' para ver VMs activas.", id);
                process::exit(1);
            }
        }

        cli::Command::Restart { id } => {
            let vm = match state::find_vm_by_id_str(&id) {
                Some(v) => v,
                None => {
                    eprintln!("[NKR] VM '{}' no encontrada. Usa 'nkr ps' para ver VMs activas.", id);
                    process::exit(1);
                }
            };

            // Capturar argv original antes de matar el proceso
            let cmdline_path = format!("/proc/{}/cmdline", vm.pid);
            let cmdline = match std::fs::read(&cmdline_path) {
                Ok(b) => b,
                Err(e) => {
                    eprintln!("[NKR] No se pudo leer {}: {}", cmdline_path, e);
                    process::exit(1);
                }
            };
            let argv: Vec<String> = cmdline
                .split(|&b| b == 0)
                .filter(|s| !s.is_empty())
                .map(|s| String::from_utf8_lossy(s).into_owned())
                .collect();
            if argv.len() < 2 {
                eprintln!("[NKR] argv inválido en {}: {:?}", cmdline_path, argv);
                process::exit(1);
            }
            // argv[0] = path al binario, argv[1..] = args reales de nkr (run --name ...)
            let exe = argv[0].clone();
            let rest: Vec<String> = argv[1..].to_vec();

            eprintln!("[NKR-RESTART] Reiniciando VM {} ({})...", vm.vm_id, vm.name);

            if let Err(e) = state::stop_vm(vm.vm_id) {
                eprintln!("[NKR-RESTART] Error deteniendo VM: {e}");
                process::exit(1);
            }

            // Esperar a que se libere el TAP/bridge antes de relanzar
            std::thread::sleep(std::time::Duration::from_millis(500));

            // Relanzar detached con setsid() — sobrevive al cierre del terminal
            use std::os::unix::process::CommandExt;
            let log_path = format!("/tmp/nkr-restart-{}.log", vm.vm_id);
            let log = match std::fs::File::create(&log_path) {
                Ok(f) => f,
                Err(e) => {
                    eprintln!("[NKR-RESTART] No se pudo crear log '{}': {}", log_path, e);
                    process::exit(1);
                }
            };
            let log_err = match log.try_clone() {
                Ok(f) => f,
                Err(e) => {
                    eprintln!("[NKR-RESTART] try_clone log: {e}");
                    process::exit(1);
                }
            };

            let child = unsafe {
                std::process::Command::new(&exe)
                    .args(&rest)
                    .stdout(log)
                    .stderr(log_err)
                    .stdin(std::process::Stdio::null())
                    .pre_exec(|| {
                        if libc::setsid() < 0 {
                            return Err(std::io::Error::last_os_error());
                        }
                        Ok(())
                    })
                    .spawn()
            };
            match child {
                Ok(c) => {
                    eprintln!("[NKR-RESTART] VM {} relanzada (PID {}) — log: {}", vm.vm_id, c.id(), log_path);
                }
                Err(e) => {
                    eprintln!("[NKR-RESTART] Error relanzando: {e}");
                    process::exit(1);
                }
            }
        }

        cli::Command::Nitro { id, duration } => {
            let vm = match state::find_vm_by_id_str(&id) {
                Some(v) => v,
                None => {
                    eprintln!("[NKR] VM '{}' no encontrada. Usa 'nkr ps' para ver VMs activas.", id);
                    process::exit(1);
                }
            };
            let secs = match parse_duration(&duration) {
                Some(s) => s,
                None => {
                    eprintln!("[NKR-NITRO] duración inválida '{}'. Formato: 30s | 5m | 1h", duration);
                    process::exit(1);
                }
            };
            let cpu_max = format!("/sys/fs/cgroup/nkr/{}/cpu.max", vm.name);
            if !std::path::Path::new(&cpu_max).exists() {
                eprintln!("[NKR-NITRO] cgroup no encontrado: {}", cpu_max);
                process::exit(1);
            }
            let restore = format!("{} 100000", vm.chrs * 20_000);
            if let Err(e) = std::fs::write(&cpu_max, "max 100000") {
                eprintln!("[NKR-NITRO] no se pudo relajar cgroup: {}", e);
                process::exit(1);
            }
            eprintln!("[NKR-NITRO] '{}' — CPU sin límite por {}s (restore → {} µs/100ms)",
                vm.name, secs, vm.chrs * 20_000);

            // Fork detached: el comando termina de inmediato, el hijo sleep+restore
            use std::os::unix::process::CommandExt;
            let shell_cmd = format!("sleep {}; echo '{}' > {}", secs, restore, cpu_max);
            let spawn_result = unsafe {
                std::process::Command::new("sh")
                    .arg("-c")
                    .arg(&shell_cmd)
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .pre_exec(|| {
                        if libc::setsid() < 0 {
                            return Err(std::io::Error::last_os_error());
                        }
                        Ok(())
                    })
                    .spawn()
            };
            match spawn_result {
                Ok(_) => eprintln!("[NKR-NITRO] restore programado en background."),
                Err(e) => {
                    eprintln!("[NKR-NITRO] WARN: no se pudo programar restore ({}). Restaura manual con:", e);
                    eprintln!("    echo '{}' | sudo tee {}", restore, cpu_max);
                }
            }
        }

        cli::Command::Compose { action, file, detach } => {
            let result = match action.as_str() {
                "up" => compose::compose_up(&file, detach),
                "down" => compose::compose_down(&file),
                "ps" => compose::compose_ps(),
                _ => {
                    eprintln!("[NKR] Acción desconocida: '{}'. Usar: up, down, ps", action);
                    Ok(())
                }
            };
            if let Err(e) = result {
                eprintln!("[NKR-COMPOSE] Error: {e}");
                process::exit(1);
            }
        }

        cli::Command::Pull { image, dest, size_mb, no_initramfs } => {
            if dest != "auto" || no_initramfs {
                // Modo legacy: destino explícito o sin initramfs
                if let Err(e) = pull::pull_image(&image, &dest, size_mb) {
                    eprintln!("[NKR-PULL] Error: {e}");
                    process::exit(1);
                }
            } else {
                // Modo auto: deposita en /mnt/nkr/ + genera initramfs
                if let Err(e) = pull::pull_and_generate(&image, size_mb) {
                    eprintln!("[NKR-PULL] Error: {e}");
                    process::exit(1);
                }
            }
        }

        cli::Command::Stats { filter } => {
            let vms = state::list_vms();
            let filtered: Vec<_> = if filter.is_empty() {
                vms
            } else {
                vms.into_iter()
                    .filter(|v| {
                        v.name == filter
                            || v.hash == filter
                            || v.vm_id.to_string() == filter
                    })
                    .collect()
            };
            metrics::print_stats_table(&filtered);
        }

        cli::Command::Ksm { action } => {
            match action.as_str() {
                "on" => match metrics::ksm_enable() {
                    Ok(()) => {
                        eprintln!("[KSM] Activado con parámetros optimizados para Odoo");
                        metrics::print_ksm_status();
                    }
                    Err(e) => {
                        eprintln!("[KSM] Error: {e}");
                        process::exit(1);
                    }
                },
                "off" => match metrics::ksm_disable() {
                    Ok(()) => eprintln!("[KSM] Desactivado"),
                    Err(e) => {
                        eprintln!("[KSM] Error: {e}");
                        process::exit(1);
                    }
                },
                "status" | "" => metrics::print_ksm_status(),
                _ => {
                    eprintln!("[KSM] Acción desconocida: '{}'. Usar: on, off, status", action);
                    process::exit(1);
                }
            }
        }

        cli::Command::Serve { port } => {
            metrics::start_prometheus_server(port);
            eprintln!("[NKR] Servidor de métricas iniciado. Ctrl+C para detener.");
            loop {
                std::thread::sleep(std::time::Duration::from_secs(3600));
            }
        }

        cli::Command::Build { file, output, size_mb, context, name, no_initramfs } => {
            if output != "auto" || no_initramfs {
                // Modo legacy: salida explícita
                if let Err(e) = build::build_disk(&file, &output, size_mb, &context) {
                    eprintln!("[NKR-BUILD] Error: {e}");
                    process::exit(1);
                }
            } else {
                // Modo auto: deposita en /mnt/nkr/ + genera initramfs
                let nkr_name = if name.is_empty() {
                    let p = std::path::Path::new(&file);
                    let fname = p.file_name().unwrap_or_default().to_string_lossy();
                    if let Some(suffix) = fname.strip_prefix("Nkrfile.") {
                        suffix.to_string()
                    } else {
                        fname.replace('.', "_")
                    }
                } else {
                    name.clone()
                };
                if let Err(e) = build::build_and_generate(&file, &nkr_name, size_mb, &context) {
                    eprintln!("[NKR-BUILD] Error: {e}");
                    process::exit(1);
                }
            }
        }

        cli::Command::Cell { action } => {
            match action {
                cli::CellAction::Create { name, odoo_version } => {
                    match cell::create_cell(&name, odoo_version.as_deref()) {
                        Ok(config) => {
                            if let Err(e) = cell::ensure_cell_bridge(config.cell_id) {
                                eprintln!("[NKR-CELL] WARN: bridge no creado ({e}). Ejecuta con sudo antes de 'cell up'.");
                            }
                        }
                        Err(e) => {
                            eprintln!("[NKR-CELL] Error: {e}");
                            process::exit(1);
                        }
                    }
                }

                cli::CellAction::Ls => {
                    cell::print_cell_table();
                }

                cli::CellAction::Up { name, detach } => {
                    let config = match cell::load_cell(&name) {
                        Ok(c) => c,
                        Err(e) => {
                            eprintln!("[NKR-CELL] {e}");
                            process::exit(1);
                        }
                    };
                    if let Err(e) = cell::ensure_cell_bridge(config.cell_id) {
                        eprintln!("[NKR-CELL] Error creando bridge: {e}");
                        process::exit(1);
                    }
                    let compose_path = cell::cell_compose_path(&name);
                    if !compose_path.exists() {
                        eprintln!("[NKR-CELL] No existe {}. Genera el compose antes de 'cell up'.",
                            compose_path.display());
                        process::exit(1);
                    }
                    let compose_str = compose_path.to_string_lossy().to_string();
                    if let Err(e) = compose::compose_up(&compose_str, detach) {
                        eprintln!("[NKR-CELL] Error en compose up: {e}");
                        process::exit(1);
                    }
                }

                cli::CellAction::Down { name } => {
                    let config = match cell::load_cell(&name) {
                        Ok(c) => c,
                        Err(e) => {
                            eprintln!("[NKR-CELL] {e}");
                            process::exit(1);
                        }
                    };
                    let compose_path = cell::cell_compose_path(&name);
                    let compose_str = compose_path.to_string_lossy().to_string();
                    if compose_path.exists() {
                        let _ = compose::compose_down(&compose_str);
                    } else {
                        // Sin compose: detener todas las VMs con ese cell_id
                        for vm in state::list_vms() {
                            if vm.cell_id == config.cell_id {
                                let _ = state::stop_vm(vm.vm_id);
                            }
                        }
                    }
                }

                cli::CellAction::Ps { name } => {
                    let vms = state::list_vms();
                    let filtered: Vec<_> = match name.as_deref() {
                        Some(cell_name) => {
                            let cid = cell::lookup_cell_id(cell_name).unwrap_or(0);
                            vms.into_iter().filter(|v| v.cell_id == cid).collect()
                        }
                        None => vms,
                    };
                    if filtered.is_empty() {
                        eprintln!("[NKR] No hay micro-VMs activas para ese filtro");
                    } else {
                        // Reutilizar tabla de estado
                        state::print_vm_table();
                    }
                }

                cli::CellAction::Destroy { name } => {
                    match cell::destroy_cell(&name) {
                        Ok(true) => {}
                        Ok(false) => {
                            eprintln!("[NKR-CELL] Célula '{}' no existe en el registry", name);
                            process::exit(1);
                        }
                        Err(e) => {
                            eprintln!("[NKR-CELL] Error: {e}");
                            process::exit(1);
                        }
                    }
                }

                cli::CellAction::Clone { src, dst, no_db, no_compose } => {
                    if let Err(e) = cell::clone_instance(&src, &dst, no_db, no_compose) {
                        eprintln!("[NKR-CELL] Error clonando: {e}");
                        process::exit(1);
                    }
                }
            }
        }
    }
}