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

fn main() {
    let args = cli::parse();

    match args.command {
        cli::Command::Run { hash, name, ram, chrs, id, disk, kernel, initramfs, port, volume, env, tap } => {
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
    }
}