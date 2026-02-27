// =============================================================================
// NKR (Nano-Kernel Runtime) v0.2.0 — Arquitectura MicroVM
// =============================================================================

use std::env;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::process;
use std::convert::TryInto;
use std::os::unix::io::AsRawFd;

use kvm_bindings::{kvm_segment, kvm_userspace_memory_region, KVM_MAX_CPUID_ENTRIES};
use kvm_ioctls::{Kvm, VcpuExit, VcpuFd, VmFd};
use linux_loader::loader::bzimage::BzImage;
use linux_loader::loader::KernelLoader;
use vm_memory::{Address, Bytes, GuestAddress, GuestMemory, GuestMemoryMmap, GuestMemoryRegion};

const GUEST_RAM_SIZE: usize = 512 << 20; // 512 MiB
const COM1_PORT: u16 = 0x3F8;

// Layout de Memoria Fija para Linux Boot Protocol x86_64
const ZERO_PAGE_ADDR: u64 = 0x7000;
const CMDLINE_ADDR: u64 = 0x20000;
const KERNEL_LOAD_ADDR: u64 = 0x100000;
const INITRAMFS_ADDR: u64 = 0x0800_0000;

const PML4_ADDR: u64 = 0x9000;
const PDPT_ADDR: u64 = 0xA000;
const PD_ADDR: u64 = 0xB000;
const GDT_ADDR: u64 = 0x500;

fn main() {
    if env::args().len() < 3 {
        eprintln!("Uso: sudo ./target/release/nkr <bzImage> <initramfs.cpio.gz>");
        process::exit(1);
    }

    if let Err(e) = run_vmm() {
        eprintln!("[NKR] Error fatal: {e}");
        process::exit(1);
    }
}

fn run_vmm() -> Result<(), Box<dyn std::error::Error>> {
    let kvm = Kvm::new().map_err(|e| format!("Fallo al abrir /dev/kvm: {e}"))?;
    let vm = kvm.create_vm().map_err(|e| format!("Fallo KVM_CREATE_VM: {e}"))?;

    // --- PLACA BASE VIRTUAL: Interrupciones y Reloj (PIT) ---
    // KVM auto-configura el ruteo interno aquí. No necesitamos estructuras C inseguras.
    vm.create_irq_chip().map_err(|e| format!("Fallo al crear IRQ chip: {e}"))?;
    
    let pit_config = kvm_bindings::kvm_pit_config {
        flags: 0,
        ..Default::default()
    };
    vm.create_pit2(pit_config).map_err(|e| format!("Fallo al crear PIT: {e}"))?;
    // --------------------------------------------------------
    
    let guest_mem = GuestMemoryMmap::<()>::from_ranges(&[(GuestAddress(0x0), GUEST_RAM_SIZE)])
        .map_err(|e| format!("Fallo mmap RAM: {e}"))?;

    register_guest_memory(&vm, &guest_mem)?;
    eprintln!("[NKR] RAM del guest: {} MiB mapeados", GUEST_RAM_SIZE >> 20);

    let kernel_path = env::args().nth(1).unwrap();
    let entry_addr = load_bzimage_kernel(&guest_mem, &kernel_path)?;

    let initrd_path = env::args().nth(2).unwrap();
    let initrd_size = load_initramfs(&guest_mem, &initrd_path)?;

    configure_linux_boot(&guest_mem, initrd_size)?;

    write_page_tables(&guest_mem)?;
    write_gdt(&guest_mem)?;

    let mut vcpu = vm.create_vcpu(0).map_err(|e| format!("Fallo KVM_CREATE_VCPU: {e}"))?;
    let cpuid = kvm.get_supported_cpuid(KVM_MAX_CPUID_ENTRIES)?;
    vcpu.set_cpuid2(&cpuid)?;

    configure_sregs(&vcpu)?;
    configure_regs(&vcpu, entry_addr)?; 
    eprintln!("[NKR] vCPU 0 lista — RIP={entry_addr:#X}, Boot Protocol inyectado");

    eprintln!("[NKR] Encendiendo MicroVM...");
    eprintln!("════════════════════════════════════════════════════════════════");

    run_vcpu_loop(&mut vcpu)?;

    eprintln!("════════════════════════════════════════════════════════════════");
    eprintln!("[NKR] VMM finalizado correctamente");
    Ok(())
}

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
    
    // ¡EL PARCHE CRÍTICO! Extraer el ADN original del bzImage (los primeros 4 KB)
    // y colocarlo físicamente en la Zero Page antes de que arranque.
    let mut header = vec![0u8; 4096];
    kernel_file.read_exact(&mut header)?;
    guest_mem.write_slice(&header, GuestAddress(ZERO_PAGE_ADDR))?;
    
    // Rebobinar el archivo a la posición 0 para que el parser lo lea completo
    kernel_file.seek(SeekFrom::Start(0))?;

    let load_result = BzImage::load(guest_mem, Some(GuestAddress(KERNEL_LOAD_ADDR)), &mut kernel_file, None)
        .map_err(|e| format!("Fallo al cargar bzImage: {e}"))?;
    
    eprintln!("[NKR] Linux bzImage cargado. Entry point: {:#X}", load_result.kernel_load.raw_value());
    Ok(load_result.kernel_load.raw_value())
}

fn load_initramfs(guest_mem: &GuestMemoryMmap<()>, path: &str) -> Result<u32, Box<dyn std::error::Error>> {
    let mut initrd_file = File::open(path)?;
    let mut initrd_data = Vec::new();
    initrd_file.read_to_end(&mut initrd_data)?;
    let size = initrd_data.len() as u32;
    guest_mem.write_slice(&initrd_data, GuestAddress(INITRAMFS_ADDR))?;
    eprintln!("[NKR] Initramfs cargado en {INITRAMFS_ADDR:#X} ({} MiB)", size >> 20);
    Ok(size)
}

fn configure_linux_boot(guest_mem: &GuestMemoryMmap<()>, initrd_size: u32) -> Result<(), Box<dyn std::error::Error>> {
    let cmdline = b"console=ttyS0 panic=1 pci=off noapic nolapic clocksource=jiffies tsc=nowatchdog 8250.nr_uarts=1 i8042.noaux i8042.nomux i8042.nopnp i8042.dumbkbd virtio_mmio.device=4K@0xd0000000:5 rdinit=/sbin/init\0";
    guest_mem.write_slice(cmdline, GuestAddress(CMDLINE_ADDR))?;

    // Ya no creamos el header desde cero, solo "parcheamos" el original
    guest_mem.write_obj(0xFFu8, GuestAddress(ZERO_PAGE_ADDR + 0x210))?; // type_of_loader
    
    // loadflags: Forzamos LOADED_HIGH (bit 0) y CAN_USE_HEAP (bit 7)
    guest_mem.write_obj(0x81u8, GuestAddress(ZERO_PAGE_ADDR + 0x211))?;

    // Punteros a nuestros datos dinámicos
    guest_mem.write_obj(INITRAMFS_ADDR as u32, GuestAddress(ZERO_PAGE_ADDR + 0x218))?;
    guest_mem.write_obj(initrd_size, GuestAddress(ZERO_PAGE_ADDR + 0x21C))?;
    guest_mem.write_obj(CMDLINE_ADDR as u32, GuestAddress(ZERO_PAGE_ADDR + 0x228))?;

    // E820 Map: El Kernel ahora sabrá exactamente que tiene 512 MB de RAM
    guest_mem.write_obj(0x0u64, GuestAddress(ZERO_PAGE_ADDR + 0x2D0))?;
    guest_mem.write_obj(0x9FC00u64, GuestAddress(ZERO_PAGE_ADDR + 0x2D8))?;
    guest_mem.write_obj(1u32, GuestAddress(ZERO_PAGE_ADDR + 0x2E0))?; 

    let high_mem_size = (GUEST_RAM_SIZE as u64) - 0x100000;
    guest_mem.write_obj(0x100000u64, GuestAddress(ZERO_PAGE_ADDR + 0x2E4))?;
    guest_mem.write_obj(high_mem_size, GuestAddress(ZERO_PAGE_ADDR + 0x2EC))?;
    guest_mem.write_obj(1u32, GuestAddress(ZERO_PAGE_ADDR + 0x2F4))?; 

    guest_mem.write_obj(2u8, GuestAddress(ZERO_PAGE_ADDR + 0x1E8))?; // e820_entries
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
    
    // ¡EL ARREGLO! Modo Protegido de 32 bits (Sin Paginación, Sin Long Mode)
    sregs.cr0 = 1;      // Solo bit 0 (PE). Paginación (PG) = 0.
    sregs.cr3 = 0;      // No necesitamos tablas de paginación aún
    sregs.cr4 = 0;      // Sin extensiones físicas
    sregs.efer = 0;     // Nada de modo 64-bit

    // Descriptor de Código de 32 bits
    let cs = kvm_segment { 
        base: 0, limit: 0xFFFF_FFFF, selector: 0x08, type_: 0xB, present: 1, 
        dpl: 0, 
        db: 1, // db=1 -> Instrucciones de 32 bits
        s: 1, 
        l: 0,  // l=0 -> NO estamos en 64 bits
        g: 1, avl: 0, unusable: 0, padding: 0 
    };
    
    // Descriptor de Datos de 32 bits
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
    regs.rsi = ZERO_PAGE_ADDR;   // Linux encuentra sus boot_params aquí
    
    // ¡EL STACK! Le damos memoria física libre (crece de 0x7000 hacia abajo)
    regs.rsp = ZERO_PAGE_ADDR;   
    
    regs.rflags = 0x2;
    vcpu.set_regs(&regs)?; 
    Ok(())
}

fn run_vcpu_loop(vcpu: &mut VcpuFd) -> Result<(), Box<dyn std::error::Error>> {
    let stdout = io::stdout(); 
    let mut out = stdout.lock();
    
    loop {
        match vcpu.run() {
            Ok(VcpuExit::IoOut(port, data)) => { 
                if port == COM1_PORT { 
                    out.write_all(data).unwrap(); 
                    out.flush().ok(); // <-- Quitamos el IF. Forzamos el flush siempre.
                } 
            }
            
            Ok(VcpuExit::IoIn(port, data)) => { 
                match port {
                    // 0x3F8 (COM1 Data Register)
                    0x3F8 => data.fill(0), 
                    
                    // 0x3F9 (Interrupt Enable Register)
                    0x3F9 => data.fill(0),
                    
                    // 0x3FA (Interrupt Identification Register)
                    // Devolver 1 significa "No hay interrupciones pendientes por procesar"
                    0x3FA => data.fill(1),
                    
                    // 0x3FD (Line Status Register) - ¡EL PUERTO CRÍTICO!
                    // 0x60 = La tubería está libre, puedes enviar datos ahora mismo.
                    0x3FD => data.fill(0x60), 
                    
                    // Emular respuestas inofensivas para teclado y PIT
                    0x60 | 0x64 => data.fill(0), 
                    
                    // Cualquier otro puerto desconocido: "Hardware desconectado" (0xFF)
                    _ => data.fill(0xFF),
                }
            }
            
            // --- NUEVO: EMULACIÓN DE LA TARJETA VIRTIO-NET ---
            // Cuando Linux lee la dirección 0xD0000000, nos pregunta qué hardware es.
            Ok(VcpuExit::MmioRead(addr, data)) => {
                // 1. Tarjeta de Red (0xD0000000) - DeviceID: 1
                if addr >= 0xD0000000 && addr < 0xD0001000 {
                    let offset = addr - 0xD0000000;
                    match offset {
                        0x000 => data.copy_from_slice(&0x74726976u32.to_le_bytes()), // "virt"
                        0x004 => data.copy_from_slice(&2u32.to_le_bytes()),          // Version 2
                        0x008 => data.copy_from_slice(&1u32.to_le_bytes()),          // DeviceID = 1 (Net)
                        0x00C => data.copy_from_slice(&0x4E4B5200u32.to_le_bytes()), // Vendor "NKR"
                        _ => data.fill(0),
                    }
                }
                // 2. Disco Duro (0xD0001000) - DeviceID: 2
                else if addr >= 0xD0001000 && addr < 0xD0002000 {
                    let offset = addr - 0xD0001000;
                    match offset {
                        0x000 => data.copy_from_slice(&0x74726976u32.to_le_bytes()), // Magic "virt"
                        0x004 => data.copy_from_slice(&2u32.to_le_bytes()),          // Version 2
                        0x008 => data.copy_from_slice(&2u32.to_le_bytes()),          // DeviceID = 2 (Block)
                        0x00C => data.copy_from_slice(&0x4E4B5200u32.to_le_bytes()), // Vendor "NKR"
                        
                        // Feature Bits: Le decimos a Linux que soportamos Virtio v1 moderno (Bit 32)
                        0x010 => data.fill(0), // Features Low (0-31)
                        0x014 => data.copy_from_slice(&1u32.to_le_bytes()), // Features High (32-63)
                        
                        // Tamaño máximo de tareas en la cola simultáneas (256 peticiones)
                        0x034 => data.copy_from_slice(&256u32.to_le_bytes()), 
                        
                        // --- CONFIG SPACE DE VIRTIO-BLK ---
                        // Offset 0x100: Capacidad del disco (64 bits) en sectores de 512 bytes
                        // 2GB = 4,194,304 sectores
                        0x100..=0x107 => {
                            let capacity: u64 = 4_194_304; 
                            let bytes = capacity.to_le_bytes();
                            let idx = (offset - 0x100) as usize;
                            let len = data.len();
                            data.copy_from_slice(&bytes[idx..idx+len]);
                        }
                        _ => data.fill(0),
                    }
                }
            }
            
            // Cuando Linux escribe para configurar la tarjeta
            Ok(VcpuExit::MmioWrite(addr, data)) => {
                if addr >= 0xD0000000 && addr < 0xD0001000 {
                    let offset = addr - 0xD0000000;
                    if offset == 0x070 {
                        let status = u32::from_le_bytes(data[0..4].try_into().unwrap());
                        if status == 3 || status == 7 { eprintln!("\n[NKR] ¡Linux detectó la Tarjeta de Red!"); }
                    }
                }
                if addr >= 0xD0001000 && addr < 0xD0002000 {
                    let offset = addr - 0xD0001000;
                    let val = if data.len() == 4 { u32::from_le_bytes(data.try_into().unwrap()) } else { 0 };
                    
                    match offset {
                        // Linux confirma en qué fase de inicialización está
                        0x070 => {
                            if val == 3 || val == 7 { 
                                eprintln!("\n[NKR] ¡Linux inicializando el Disco Duro Virtio!"); 
                            }
                            if val == 15 { // VIRTIO_CONFIG_S_DRIVER_OK
                                eprintln!("\n[NKR] ¡Disco Duro de 2GB listo y montado como /dev/vda!");
                            }
                        }
                        // Linux nos dice dónde creó la tabla de descriptores en RAM
                        0x080 => eprintln!("[NKR-Vring] Descriptor Table Address Low: {:#X}", val),
                        0x084 => eprintln!("[NKR-Vring] Descriptor Table Address High: {:#X}", val),
                        // Linux avisa que la cola está lista para usarse
                        0x044 => if val == 1 { eprintln!("[NKR-Vring] ¡Cola de I/O activada!"); },
                        _ => {}
                    }
                }
            }
            // ------------------------------------------------

            Ok(VcpuExit::Hlt) => break,
            Ok(VcpuExit::Shutdown) => { 
                eprintln!("\n[NKR] vCPU shutdown (Kernel Panic o fin de ejecución)"); 
                break; 
            }
            Ok(_) => {},
            Err(e) => return Err(format!("vcpu.run() falló: {e}").into()),
        }
    }
    Ok(())
}