// =============================================================================
// NKR (Nano-Kernel Runtime) v0.1.0 — Virtual Machine Monitor Fundacional
// =============================================================================
//
// Hipervisor bare-metal que se comunica directamente con /dev/kvm para ejecutar
// unikernels (.img) sin dependencias externas (sin QEMU, sin Firecracker).
//
// Arquitectura: VMM single-process, single-vCPU, consola serial por port I/O.
//
// Flujo de ejecución:
//   /dev/kvm → VM fd → GuestMemory (mmap) → ELF loader → vCPU (sregs+regs)
//   → vcpu.run() loop → IoOut(0x3F8) → stdout del host
//
// =============================================================================

use std::env;
use std::fs::File;
use std::io::{self, Write};
use std::process;

use kvm_bindings::{kvm_segment, kvm_userspace_memory_region};
use kvm_ioctls::{Kvm, VcpuExit, VcpuFd, VmFd};
use linux_loader::loader::KernelLoader;
use vm_memory::{Address, Bytes, GuestAddress, GuestMemory, GuestMemoryMmap, GuestMemoryRegion};

// =============================================================================
// Constantes de Arquitectura x86_64
// =============================================================================

/// RAM del guest por defecto: 512 MiB.
/// Suficiente para la mayoría de unikernels. Configurable vía CLI en futuras versiones.
const GUEST_RAM_SIZE: usize = 512 << 20;

/// Puerto I/O del COM1 estándar (UART 16550).
/// El unikernel escribe bytes aquí (`out 0x3F8, al`) para salida de consola.
/// El VMM intercepta el VMEXIT tipo IoOut y redirige el byte a stdout del host.
const COM1_PORT: u16 = 0x3F8;

/// Direcciones físicas para las tablas de paginación identity-mapped.
/// Se ubican en la zona baja de memoria (<64 KiB) para evitar colisión con el
/// kernel que típicamente se carga en direcciones más altas (>1 MiB).
///
/// Estructura de 4 niveles (obligatoria para modo largo x86_64):
///   PML4 (0x9000) → PDPT (0xA000) → PD (0xB000) → páginas de 2 MiB
const PML4_ADDR: u64 = 0x9000;
const PDPT_ADDR: u64 = 0xA000;
const PD_ADDR: u64 = 0xB000;

/// Dirección donde se escribe la Global Descriptor Table en memoria del guest.
/// La GDT es requerida por el hardware x86_64 aunque la segmentación esté
/// efectivamente flat en modo largo. Ubicada en zona baja para no colisionar.
const GDT_ADDR: u64 = 0x500;

/// Ruta por defecto de la imagen del unikernel.
const DEFAULT_KERNEL_PATH: &str = "test-kernel.img";

/// Máximo de entradas CPUID a solicitar al host KVM.
/// 256 es suficiente para cubrir todas las hojas estándar + extended.
const CPUID_MAX_ENTRIES: usize = 256;

// =============================================================================
// Punto de Entrada
// =============================================================================

fn main() {
    if let Err(e) = run_vmm() {
        eprintln!("[NKR] Error fatal: {e}");
        process::exit(1);
    }
}

/// Orquesta el ciclo de vida completo del VMM:
///   1. Apertura de /dev/kvm
///   2. Creación de la VM (espacio de direcciones aislado)
///   3. Asignación de memoria física del guest (mmap)
///   4. Carga del kernel ELF en la memoria del guest
///   5. Escritura de tablas de paginación y GDT
///   6. Creación y configuración de la vCPU 0
///   7. Ejecución del bucle vcpu.run()
fn run_vmm() -> Result<(), Box<dyn std::error::Error>> {
    // ═══ 1. Abrir /dev/kvm ═══════════════════════════════════════════════
    // Kvm::new() ejecuta open("/dev/kvm") y verifica la versión de la API.
    // Si falla, es porque KVM no está disponible en el host.
    let kvm = Kvm::new().map_err(|e| {
        format!(
            "Fallo al abrir /dev/kvm: {e}. Verificar: \
             (1) virtualización habilitada en BIOS/UEFI (VT-x/AMD-V), \
             (2) módulo kvm_intel o kvm_amd cargado (lsmod | grep kvm), \
             (3) permisos de lectura/escritura en /dev/kvm (grupo kvm)"
        )
    })?;

    let api_ver = kvm.get_api_version();
    eprintln!("[NKR] /dev/kvm abierto — KVM API version {api_ver}");

    // ═══ 2. Crear la instancia de VM ═════════════════════════════════════
    // KVM_CREATE_VM devuelve un file descriptor que representa una VM completa
    // con su propio espacio de direcciones físicas (GPA space), IRQ routing,
    // y conjunto de vCPUs. Cada VM es un sandbox de hardware aislado.
    let vm = kvm.create_vm().map_err(|e| {
        format!("Fallo KVM_CREATE_VM: {e}. Posible agotamiento de file descriptors")
    })?;

    // ═══ 3. Asignar y registrar memoria del guest ════════════════════════
    // GuestMemoryMmap::from_ranges() crea un mmap(2) anónimo de GUEST_RAM_SIZE
    // bytes. Este bloque de memoria del host se convertirá en la "RAM física"
    // que el guest percibe. KVM traduce GPA → HVA en hardware via EPT/NPT.
    let guest_mem = GuestMemoryMmap::<()>::from_ranges(&[(GuestAddress(0x0), GUEST_RAM_SIZE)])
        .map_err(|e| {
            format!(
                "Fallo al asignar {mib} MiB de RAM para el guest: {e}",
                mib = GUEST_RAM_SIZE >> 20
            )
        })?;

    // Registrar la región en KVM (KVM_SET_USER_MEMORY_REGION).
    // Esto establece el shadow page table o configura EPT/NPT para que
    // los accesos del guest a GPA [0, GUEST_RAM_SIZE) se traduzcan a
    // la HVA del mmap que acabamos de crear.
    register_guest_memory(&vm, &guest_mem)?;
    eprintln!(
        "[NKR] RAM del guest: {} MiB mapeados (slot 0, GPA 0x0)",
        GUEST_RAM_SIZE >> 20
    );

    // ═══ 4. Cargar el kernel ELF en memoria del guest ════════════════════
    // El primer argumento de CLI es la ruta al .img; si no se da, usa el default.
    let kernel_path = env::args()
        .nth(1)
        .unwrap_or_else(|| DEFAULT_KERNEL_PATH.to_string());

    let entry_addr = load_elf_kernel(&guest_mem, &kernel_path)?;
    eprintln!("[NKR] Kernel '{kernel_path}' cargado — entry point: {entry_addr:#X}");

    // ═══ 5. Preparar entorno de ejecución x86_64 en memoria del guest ════
    // Antes de arrancar la vCPU, necesitamos escribir en la "RAM" del guest
    // las estructuras de datos que el procesador consulta al arrancar:
    //
    //   a) Tablas de paginación identity-mapped: GVA == GPA para el primer GiB.
    //      Sin paginación activa, no se puede entrar en modo largo.
    //
    //   b) GDT: el hardware requiere al menos un descriptor de código (CS)
    //      y uno de datos. En modo largo la segmentación es flat, pero la
    //      GDT no puede estar ausente.
    write_page_tables(&guest_mem)?;
    write_gdt(&guest_mem)?;
    eprintln!("[NKR] Tablas de paginación (identity 1 GiB) y GDT escritas en guest RAM");

    // ═══ 6. Crear y configurar vCPU 0 ════════════════════════════════════
    // KVM_CREATE_VCPU devuelve un fd que representa un hilo de ejecución
    // del procesador virtual. En v0.1.0 usamos una sola vCPU (ID=0).
    let vcpu = vm.create_vcpu(0).map_err(|e| format!("Fallo KVM_CREATE_VCPU(0): {e}"))?;

    // Pasar las hojas CPUID del host al guest. Sin esto, el kernel podría
    // intentar usar instrucciones no soportadas o detectar un CPU inválido.
    let cpuid = kvm
        .get_supported_cpuid(CPUID_MAX_ENTRIES)
        .map_err(|e| format!("Fallo KVM_GET_SUPPORTED_CPUID: {e}"))?;
    vcpu.set_cpuid2(&cpuid)
        .map_err(|e| format!("Fallo KVM_SET_CPUID2: {e}"))?;

    // Configurar los registros del procesador:
    //   - sregs: modo largo, paginación, segmentos, GDT
    //   - regs: RIP al entry point, RFLAGS mínimo
    configure_sregs(&vcpu)?;
    configure_regs(&vcpu, entry_addr)?;
    eprintln!("[NKR] vCPU 0 lista — RIP={entry_addr:#X}, modo largo x86_64 activo");

    // ═══ 7. Bucle principal del VMM ══════════════════════════════════════
    eprintln!("[NKR] Iniciando ejecución del unikernel…");
    eprintln!("════════════════════════════════════════════════════════════════");

    run_vcpu_loop(&vcpu)?;

    eprintln!("════════════════════════════════════════════════════════════════");
    eprintln!("[NKR] VMM finalizado correctamente");
    Ok(())
}

// =============================================================================
// Módulo: Memoria del Guest
// =============================================================================

/// Registra todas las regiones de `GuestMemoryMmap` en el file descriptor de la VM.
///
/// Por cada región, emite el ioctl `KVM_SET_USER_MEMORY_REGION` que instruye a
/// KVM a mapear un rango de Guest Physical Addresses (GPA) a la Host Virtual
/// Address (HVA) del mmap anónimo correspondiente.
///
/// En v0.1.0 solo tenemos una región contigua [0, 512 MiB), pero el código
/// está preparado para múltiples regiones (e.g., separar low-mem de high-mem,
/// o crear huecos para MMIO en futuras versiones).
fn register_guest_memory(
    vm: &VmFd,
    guest_mem: &GuestMemoryMmap<()>,
) -> Result<(), Box<dyn std::error::Error>> {
    for (index, region) in guest_mem.iter().enumerate() {
        let mem_region = kvm_userspace_memory_region {
            slot: index as u32,
            flags: 0, // Sin KVM_MEM_LOG_DIRTY_PAGES ni KVM_MEM_READONLY
            guest_phys_addr: region.start_addr().raw_value(),
            memory_size: region.len(),
            // as_ptr() devuelve la HVA del inicio del mmap de esta región.
            // KVM usa esta dirección para configurar EPT/NPT.
            userspace_addr: region.as_ptr() as u64,
        };

        // SAFETY: La región de memoria está respaldada por un mmap válido que
        // permanece vivo durante toda la ejecución de la VM. El mmap no se
        // comparte con otros procesos ni se libera prematuramente.
        unsafe {
            vm.set_user_memory_region(mem_region).map_err(|e| {
                format!(
                    "Fallo KVM_SET_USER_MEMORY_REGION (slot {index}, GPA {:#X}, size {} MiB): {e}",
                    region.start_addr().raw_value(),
                    region.len() >> 20
                )
            })?;
        }
    }
    Ok(())
}

// =============================================================================
// Módulo: Carga del Kernel
// =============================================================================

/// Carga un binario ELF desde `path` en la memoria física del guest.
///
/// Utiliza `linux-loader` que:
///   1. Parsea el ELF header (e_ident, e_entry, e_phoff)
///   2. Itera sobre los Program Headers tipo PT_LOAD
///   3. Copia cada segmento a la GPA especificada en p_paddr
///   4. Retorna el entry point (e_entry) como `kernel_load`
///
/// El entry point es la dirección donde el unikernel espera que la CPU
/// comience a ejecutar instrucciones (primer byte del _start o main).
fn load_elf_kernel(
    guest_mem: &GuestMemoryMmap<()>,
    path: &str,
) -> Result<u64, Box<dyn std::error::Error>> {
    let mut kernel_file = File::open(path).map_err(|e| {
        format!(
            "No se pudo abrir '{path}': {e}. \
             Verifica que el archivo existe y es un ELF x86_64 estático"
        )
    })?;

    // kernel_start = None: sin offset adicional, usar las GPAs del ELF tal cual.
    //   Los unikernels típicamente se linkean a direcciones absolutas que
    //   coinciden con su posición deseada en la memoria física del guest.
    //
    // highmem_start_address = None: sin restricción de zona alta.
    //   Todo el espacio de direcciones del guest es válido para carga.
    let load_result = linux_loader::loader::elf::Elf::load(
        guest_mem,
        None, // kernel_start
        &mut kernel_file,
        None, // highmem_start_address
    )
    .map_err(|e| {
        format!(
            "Fallo al cargar ELF '{path}': {e}. \
             Verificar: (1) es un ELF estático x86_64, \
             (2) los segmentos PT_LOAD caben en {} MiB de RAM del guest, \
             (3) no hay segmentos mapeados en GPA 0x0 (zona de page tables)",
            GUEST_RAM_SIZE >> 20
        )
    })?;

    // kernel_load contiene e_entry del ELF: la dirección virtual donde el
    // procesador debe comenzar la ejecución. Con identity mapping, GVA == GPA.
    let entry = load_result.kernel_load.raw_value();

    eprintln!(
        "[NKR]   └─ Kernel end: {:#X} ({} KiB cargados)",
        load_result.kernel_end.raw_value(),
        (load_result.kernel_end.raw_value() - entry) >> 10
    );

    Ok(entry)
}

// =============================================================================
// Módulo: Tablas de Paginación — Identity Mapping x86_64
// =============================================================================

/// Escribe las tablas de paginación de 4 niveles para identity mapping del
/// primer GiB de memoria física.
///
/// ```text
/// ┌─────────────────────────────────────────────────────────────┐
/// │ CR3 ──► PML4 (0x9000)                                      │
/// │           └─ [0] ──► PDPT (0xA000)                         │
/// │                        └─ [0] ──► PD (0xB000)              │
/// │                                    ├─ [0]   → 0x00000000   │
/// │                                    ├─ [1]   → 0x00200000   │
/// │                                    ├─ ...                   │
/// │                                    └─ [511] → 0x3FE00000   │
/// │                                                             │
/// │  512 entradas × 2 MiB = 1 GiB identity-mapped              │
/// └─────────────────────────────────────────────────────────────┘
/// ```
///
/// Cada entrada PDE usa el bit PS (Page Size) para indicar páginas de 2 MiB,
/// eliminando la necesidad de un cuarto nivel (Page Table). Esto simplifica
/// el setup y es suficiente para el rango de memoria que usamos.
///
/// Identity mapping (GVA == GPA) es necesario porque el kernel recién cargado
/// no tiene su propio MMU setup y espera que las direcciones virtuales
/// coincidan con las físicas al menos durante el bootstrap.
fn write_page_tables(
    guest_mem: &GuestMemoryMmap<()>,
) -> Result<(), Box<dyn std::error::Error>> {
    // PML4[0]: apunta a la PDPT.
    // Bits: Present (bit 0) + Read/Write (bit 1) = 0x3
    let pml4e: u64 = PDPT_ADDR | 0x3;
    guest_mem
        .write_obj(pml4e, GuestAddress(PML4_ADDR))
        .map_err(|e| format!("Fallo al escribir PML4[0] en GPA {PML4_ADDR:#X}: {e}"))?;

    // PDPT[0]: apunta al Page Directory.
    // Mismos bits: Present + Read/Write.
    let pdpte: u64 = PD_ADDR | 0x3;
    guest_mem
        .write_obj(pdpte, GuestAddress(PDPT_ADDR))
        .map_err(|e| format!("Fallo al escribir PDPT[0] en GPA {PDPT_ADDR:#X}: {e}"))?;

    // PD[0..511]: cada entrada mapea 2 MiB de memoria física.
    //
    // Bits por entrada:
    //   bit 0 (P)  = Present
    //   bit 1 (RW) = Read/Write
    //   bit 7 (PS) = Page Size (indica 2 MiB, no hay nivel PT)
    //   0x83 = P | RW | PS
    //
    // La dirección física base de cada página es i * 2 MiB.
    // Con identity mapping, GVA 0x00400000 → GPA 0x00400000.
    for i in 0u64..512 {
        let pde: u64 = (i << 21) | 0x83;
        guest_mem
            .write_obj(pde, GuestAddress(PD_ADDR + i * 8))
            .map_err(|e| format!("Fallo al escribir PDE[{i}]: {e}"))?;
    }

    Ok(())
}

// =============================================================================
// Módulo: Global Descriptor Table (GDT)
// =============================================================================

/// Escribe una GDT mínima en la memoria del guest.
///
/// En modo largo x86_64, la segmentación está efectivamente deshabilitada
/// (todo es flat, base=0, limit=max), pero el hardware REQUIERE una GDT
/// válida con al menos un descriptor de código para CS.
///
/// Layout de la GDT:
/// ```text
/// ┌────────────┬──────────────────────────────────────────────────┐
/// │ Offset     │ Descriptor                                       │
/// ├────────────┼──────────────────────────────────────────────────┤
/// │ 0x00       │ Null (requerido por arquitectura Intel/AMD)      │
/// │ 0x08       │ Code64: L=1, D=0, P=1, DPL=0, Execute/Read     │
/// │ 0x10       │ Data:   G=1, DB=1, P=1, DPL=0, Read/Write      │
/// │ 0x18       │ Reservado (padding para alineación)              │
/// └────────────┴──────────────────────────────────────────────────┘
/// ```
///
/// El selector de CS (0x08) referencia el descriptor Code64 con L=1,
/// que le dice al procesador que ejecute en modo 64-bit completo.
fn write_gdt(
    guest_mem: &GuestMemoryMmap<()>,
) -> Result<(), Box<dyn std::error::Error>> {
    // Codificación de descriptores de segmento x86_64 (8 bytes cada uno).
    // Referencia: Intel SDM Vol. 3A, Sección 3.4.5 (Segment Descriptors).
    let gdt_table: [u64; 4] = [
        // [0x00] Null descriptor — obligatorio, el procesador lo ignora.
        0x0000_0000_0000_0000,
        // [0x08] Code segment, 64-bit:
        //   Base=0, Limit=0xFFFFF (con G=1 → 4 GiB efectivos)
        //   P=1 (present), DPL=0 (ring 0), S=1 (code/data), Type=0xA (exec/read)
        //   L=1 (long mode), D=0 (debe ser 0 cuando L=1)
        //   Encoding: 0x00AF_9A00_0000_FFFF
        0x00AF_9A00_0000_FFFF,
        // [0x10] Data segment:
        //   Base=0, Limit=0xFFFFF (con G=1 → 4 GiB)
        //   P=1, DPL=0, S=1, Type=0x2 (read/write)
        //   DB=1 (operaciones de 32-bit), G=1 (granularidad 4 KiB)
        //   Encoding: 0x00CF_9200_0000_FFFF
        0x00CF_9200_0000_FFFF,
        // [0x18] Reservado — padding para alineación a 32 bytes.
        0x0000_0000_0000_0000,
    ];

    for (i, &entry) in gdt_table.iter().enumerate() {
        guest_mem
            .write_obj(entry, GuestAddress(GDT_ADDR + (i as u64) * 8))
            .map_err(|e| format!("Fallo al escribir GDT[{i}] en GPA {:#X}: {e}", GDT_ADDR + (i as u64) * 8))?;
    }

    Ok(())
}

// =============================================================================
// Módulo: Configuración de Registros de la vCPU
// =============================================================================

/// Configura los Special Registers (sregs) para arrancar en modo largo x86_64.
///
/// El procesador x86_64 tiene múltiples "puertas" que deben abrirse en orden
/// para transicionar de Real Mode → Protected Mode → Long Mode:
///
///   1. CR0.PE = 1       → Activar Protected Mode
///   2. CR4.PAE = 1      → Habilitar Physical Address Extension (>4 GiB)
///   3. CR3 = &PML4      → Apuntar a las tablas de paginación
///   4. EFER.LME = 1     → Habilitar Long Mode Enable
///   5. CR0.PG = 1       → Activar paginación (esto activa LMA automáticamente)
///   6. CS.L = 1, CS.D=0 → Ejecutar en modo 64-bit
///
/// KVM permite saltar directamente al estado final sin pasar por la
/// transición secuencial, ya que configuramos todos los registros antes
/// del primer vcpu.run().
fn configure_sregs(vcpu: &VcpuFd) -> Result<(), Box<dyn std::error::Error>> {
    let mut sregs = vcpu
        .get_sregs()
        .map_err(|e| format!("Fallo KVM_GET_SREGS: {e}"))?;

    // ── Control Registers ────────────────────────────────────────────────

    // CR0: los dos bits esenciales para modo largo.
    //   bit 0  (PE) = Protected Mode Enable
    //   bit 31 (PG) = Paging Enable
    // Sin estos bits, el procesador está en Real Mode (16-bit).
    sregs.cr0 = (1 << 0) | (1 << 31);

    // CR3: dirección física de la tabla PML4 (raíz del árbol de paginación).
    // El hardware la consulta en cada acceso a memoria para traducir GVA → GPA.
    sregs.cr3 = PML4_ADDR;

    // CR4: bits de extensión del procesador.
    //   bit 5 (PAE) = Physical Address Extension — obligatorio para modo largo.
    //   Sin PAE, la paginación opera en modo legacy 32-bit (2 niveles).
    sregs.cr4 = 1 << 5;

    // EFER (Extended Feature Enable Register, MSR 0xC0000080):
    //   bit 8  (LME) = Long Mode Enable — solicita transición a modo largo
    //   bit 10 (LMA) = Long Mode Active — confirmación de que estamos en 64-bit
    //   LMA se activa automáticamente por el hardware cuando PG=1 y LME=1,
    //   pero KVM requiere que lo configuremos explícitamente.
    sregs.efer = (1 << 8) | (1 << 10);

    // ── Code Segment (selector 0x08) ─────────────────────────────────────
    // CS define el modo de ejecución. En modo largo:
    //   L=1, D=0 → modo 64-bit completo (operandos de 64-bit, RIP de 64-bit)
    //   L=1, D=1 → comportamiento indefinido (PROHIBIDO)
    //   L=0, D=1 → modo compatibilidad 32-bit dentro de long mode
    sregs.cs = kvm_segment {
        base: 0,
        limit: 0xFFFF_FFFF,
        selector: 0x08,  // Segundo descriptor en la GDT (offset = 1 * 8)
        type_: 0xB,      // Execute/Read, Accessed (bits: 1011b)
        present: 1,      // P = segmento presente en memoria
        dpl: 0,          // Descriptor Privilege Level = Ring 0 (kernel)
        db: 0,           // D flag = 0 (obligatorio cuando L=1)
        s: 1,            // S = descriptor de código/datos (no sistema)
        l: 1,            // L = Long mode (64-bit)
        g: 1,            // G = Granularidad de 4 KiB (limit × 4K)
        avl: 0,
        unusable: 0,
        padding: 0,
    };

    // ── Data Segments (selector 0x10) ────────────────────────────────────
    // En modo largo, DS/ES/FS/GS/SS son efectivamente ignorados por el hardware
    // (base siempre tratada como 0, excepto FS y GS que pueden tener base != 0
    // vía MSRs). Pero KVM valida que tengan descriptores presentes y válidos.
    let data_seg = kvm_segment {
        base: 0,
        limit: 0xFFFF_FFFF,
        selector: 0x10,  // Tercer descriptor en la GDT (offset = 2 * 8)
        type_: 0x3,      // Read/Write, Accessed (bits: 0011b)
        present: 1,
        dpl: 0,
        db: 1,           // D = operaciones de 32-bit (estándar para data seg)
        s: 1,
        l: 0,            // L no aplica para segmentos de datos
        g: 1,
        avl: 0,
        unusable: 0,
        padding: 0,
    };

    sregs.ds = data_seg;
    sregs.es = data_seg;
    sregs.fs = data_seg;
    sregs.gs = data_seg;
    sregs.ss = data_seg;

    // ── GDT Register ─────────────────────────────────────────────────────
    // GDTR apunta al inicio de la GDT en la RAM del guest.
    // limit = (4 entradas × 8 bytes) - 1 = 31
    sregs.gdt.base = GDT_ADDR;
    sregs.gdt.limit = (4 * 8) - 1;

    // ── IDT Register ─────────────────────────────────────────────────────
    // En v0.1.0 no configuramos una IDT. Si el unikernel genera una excepción
    // sin IDT válida, se producirá un triple fault → VcpuExit::Shutdown.
    // Futuras versiones deben configurar al menos vectores de excepción básicos.
    sregs.idt.base = 0;
    sregs.idt.limit = 0;

    vcpu.set_sregs(&sregs)
        .map_err(|e| format!("Fallo KVM_SET_SREGS: {e}"))?;

    Ok(())
}

/// Configura los registros generales de la vCPU.
///
/// RIP → entry point del kernel ELF (donde comienza la ejecución)
/// RFLAGS → bit 1 activo (reservado por Intel, SIEMPRE debe ser 1)
/// RSP → no configurado (el unikernel es responsable de su propio stack)
fn configure_regs(vcpu: &VcpuFd, entry_addr: u64) -> Result<(), Box<dyn std::error::Error>> {
    let mut regs = vcpu
        .get_regs()
        .map_err(|e| format!("Fallo KVM_GET_REGS: {e}"))?;

    // RIP (Instruction Pointer): primer byte que ejecutará la vCPU.
    // Debe coincidir con e_entry del ELF + cualquier offset aplicado.
    regs.rip = entry_addr;

    // RFLAGS: bit 1 es reservado y SIEMPRE debe estar en 1 (Intel SDM Vol. 1).
    // Todos los demás flags en 0:
    //   IF=0 → interrupciones deshabilitadas (no tenemos IDT ni PIC/APIC)
    //   DF=0 → dirección de strings hacia adelante
    //   TF=0 → sin single-step tracing
    regs.rflags = 0x2;

    // RSP: deliberadamente no se configura aquí.
    // Razones:
    //   1. El unikernel puede tener su propio linker script que define _stack_top
    //   2. Asignar un stack arbitrario podría colisionar con el kernel cargado
    //   3. Muchos unikernels configuran RSP como primera instrucción en _start
    //
    // Si necesitas un stack pre-configurado (e.g., para kernels Linux):
    //   regs.rsp = GUEST_RAM_SIZE as u64 - 0x1000;  // Tope de RAM - guard page

    vcpu.set_regs(&regs)
        .map_err(|e| format!("Fallo KVM_SET_REGS: {e}"))?;

    Ok(())
}

// =============================================================================
// Módulo: Bucle Principal de Ejecución
// =============================================================================

/// Bucle de ejecución de la vCPU — corazón del VMM.
///
/// Cada iteración:
///   1. `vcpu.run()` ejecuta VMLAUNCH/VMRESUME en hardware
///   2. La CPU ejecuta instrucciones del guest a velocidad nativa
///   3. Cuando ocurre un evento que requiere intervención del host (VMEXIT),
///      KVM retorna al userspace con un `VcpuExit` que describe el motivo
///   4. El VMM maneja el evento y decide si continuar o terminar
///
/// VMEXITs manejados en v0.1.0:
///
/// | Exit            | Causa                              | Acción                    |
/// |─────────────────|────────────────────────────────────|───────────────────────────|
/// | IoOut(0x3F8)    | `out 0x3F8, al` en el guest        | Byte → stdout del host    |
/// | IoIn(0x3F8)     | `in al, 0x3F8` en el guest         | Retorna 0x0 (sin input)   |
/// | Hlt             | Instrucción `hlt`                  | Terminación limpia        |
/// | Shutdown        | Triple fault o shutdown explícito  | Terminación con warning   |
/// | MmioRead/Write  | Acceso a GPA sin memoria mapeada   | Log warning, continuar    |
fn run_vcpu_loop(vcpu: &VcpuFd) -> Result<(), Box<dyn std::error::Error>> {
    // Adquirimos el lock de stdout UNA VEZ fuera del loop para evitar el overhead
    // de lock/unlock en cada byte de salida serial. Esto es seguro porque:
    //   1. Somos single-threaded (una sola vCPU)
    //   2. Los mensajes diagnósticos usan eprintln! (stderr, lock separado)
    let stdout = io::stdout();
    let mut out = stdout.lock();

    loop {
        match vcpu.run() {
            Ok(exit_reason) => match exit_reason {
                // ── Salida Serial (COM1) ─────────────────────────────────
                // La instrucción `out dx, al` con DX=0x3F8 causa un VMEXIT
                // tipo IoOut. El array `data` contiene el/los byte(s) escritos.
                // Los redirigimos directamente a stdout para que el usuario
                // vea la salida del unikernel en su terminal, sin buffering.
                VcpuExit::IoOut(port, data) => {
                    if port == COM1_PORT {
                        out.write_all(data)
                            .map_err(|e| format!("Fallo write(stdout): {e}"))?;
                        out.flush().ok();
                    }
                    // Otros puertos (e.g., 0x3F9-0x3FF para control de UART,
                    // 0x60/0x64 para teclado PS/2) se ignoran silenciosamente.
                    // En v0.2.0 se puede agregar un bus de I/O con dispatch.
                }

                // ── Lectura Serial ───────────────────────────────────────
                // `in al, dx` con DX=0x3F8. El guest está leyendo el Receive
                // Buffer Register o el Line Status Register del UART.
                // Sin emulación completa de 16550, retornamos 0.
                VcpuExit::IoIn(port, data) => {
                    if port == COM1_PORT {
                        // 0x00 = sin datos disponibles.
                        // Un UART real retornaría el Line Status en puerto +5.
                        data.fill(0);
                    }
                }

                // ── Halt ─────────────────────────────────────────────────
                // El guest ejecutó `hlt`. En un unikernel, esto típicamente
                // significa "trabajo completado, no hay más que hacer".
                VcpuExit::Hlt => {
                    eprintln!("\n[NKR] vCPU ejecutó HLT — unikernel finalizado normalmente");
                    break;
                }

                // ── Shutdown ─────────────────────────────────────────────
                // Triple fault: el guest generó una excepción, no había IDT,
                // la excepción de "no hay handler" generó otra excepción,
                // y la tercera excepción anidada causa shutdown del procesador.
                VcpuExit::Shutdown => {
                    eprintln!("\n[NKR] vCPU shutdown — posible triple fault");
                    eprintln!(
                        "[NKR] Causas comunes: (1) excepción sin IDT configurada, \
                         (2) acceso a memoria no mapeada, (3) instrucción inválida"
                    );
                    break;
                }

                // ── MMIO (no implementado) ───────────────────────────────
                // El guest intentó acceder a una dirección física que no tiene
                // memoria RAM mapeada. En un VMM completo, estas direcciones
                // se usan para dispositivos virtuales (virtio-net, etc.).
                VcpuExit::MmioRead(addr, _data) => {
                    eprintln!("[NKR] WARN: MMIO read no manejado en GPA {addr:#X}");
                }
                VcpuExit::MmioWrite(addr, _data) => {
                    eprintln!("[NKR] WARN: MMIO write no manejado en GPA {addr:#X}");
                }

                // ── Exit no reconocido ───────────────────────────────────
                other => {
                    eprintln!("[NKR] VMEXIT no soportado: {other:?}");
                    return Err(format!(
                        "VMEXIT no manejado: {other:?}. \
                         Reportar como bug o implementar handler en run_vcpu_loop()"
                    )
                    .into());
                }
            },

            // Error en vcpu.run() — condición irrecuperable.
            // Causas típicas:
            //   - Registros configurados de forma inconsistente (e.g., PG=1 sin CR3 válido)
            //   - Memoria del guest no cubre las direcciones que el kernel intenta acceder
            //   - Bug en KVM o el kernel del host
            Err(e) => {
                return Err(format!(
                    "vcpu.run() falló: {e}. \
                     Verificar: (1) sregs configurados correctamente (CR0/CR3/CR4/EFER), \
                     (2) la RAM del guest cubre todas las GPAs del kernel ELF, \
                     (3) el binario es un ELF x86_64 estático válido"
                )
                .into());
            }
        }
    }

    Ok(())
}
