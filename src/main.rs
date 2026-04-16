// =============================================================================
// NKR (Nano-Kernel Runtime) v1.0.0
// Contenedores ultra-rápidos con micro-VMs y acceso directo al hardware
// =============================================================================

use std::process;

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

            // Auto-registrar en registry si se usa nombre
            let vm_id = if !vm_name.starts_with("nkr-") || vm_name != format!("nkr-{}", id) {
                // Tiene nombre explícito: registrar en registry
                match registry::resolve_id(&vm_name) {
                    Ok(resolved) => resolved,
                    Err(e) => {
                        eprintln!("[NKR] Error registry: {e}");
                        id // fallback al ID manual
                    }
                }
            } else {
                id // Sin nombre significativo, usar ID manual
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
                let nvm_name = if name.is_empty() {
                    // Derivar nombre del Nkrfile: "Nkrfile.nginx" → "nginx"
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
                if let Err(e) = build::build_and_generate(&file, &nvm_name, size_mb, &context) {
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
            }
        }
    }
}