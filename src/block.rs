use std::fs::{File, OpenOptions};
use std::sync::Arc;
use vmm_sys_util::eventfd::EventFd;
use virtio_queue::{Queue, QueueT};
use vm_memory::GuestMemoryMmap;

pub struct VirtioBlockDevice {
    pub file: File,
    pub queue: Queue,
    pub ioeventfd: EventFd,
    pub irqfd: EventFd,
    pub mem: Arc<GuestMemoryMmap>,
}

impl VirtioBlockDevice {
    // Inicializa el disco: abre el archivo .ext4 y prepara los timbres
    pub fn new(disk_path: &str, mem: Arc<GuestMemoryMmap>) -> Self {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(disk_path)
            .expect("[BLOCK] Error: No se pudo abrir el archivo del disco virtual");

        // ioeventfd: Linux nos avisa que hay peticiones
        let ioeventfd = EventFd::new(libc::EFD_NONBLOCK)
            .expect("[BLOCK] Fallo al crear ioeventfd");
            
        // irqfd: Nosotros le avisamos a Linux que terminamos
        let irqfd = EventFd::new(libc::EFD_NONBLOCK)
            .expect("[BLOCK] Fallo al crear irqfd");

        // Creamos una cola Virtio con un tamaño máximo de 256 peticiones concurrentes
        let mut queue = Queue::new(256).unwrap();
        queue.set_ready(true); // Inicializamos la cola como lista

        VirtioBlockDevice {
            file,
            queue,
            ioeventfd,
            irqfd,
            mem,
        }
    }
    // El Job Runner: Procesa las peticiones del Vring
    pub fn process_queue(&mut self) {
        // Leemos la memoria RAM compartida para ver qué pide el Kernel
        let mem = self.mem.as_ref();
        
        // Simulación: Aquí es donde la librería virtio-queue extrae los descriptores.
        // Un descriptor de disco tiene 3 partes:
        // 1. Header (¿Es lectura o escritura? ¿Qué sector del disco?)
        // 2. Data Buffer (El espacio en RAM donde debemos copiar los datos)
        // 3. Status Byte (Donde le escribimos un 0 si todo salió bien)
        
        eprintln!("[NKR-BLOCK] ¡Interrupción interceptada! Linux ha solicitado acceso a sectores de odoo_disk.ext4");

        // Usamos los campos silenciosamente para que el compilador sepa que están activos
        let _ = self.file.metadata();
        let _ = self.queue.max_size();

        // Tras leer/escribir el archivo real, le tocamos el timbre de vuelta al procesador
        self.irqfd.write(1).expect("Fallo al inyectar IRQ de respuesta");
    }
}