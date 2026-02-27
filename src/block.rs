use std::fs::{File, OpenOptions};
use std::sync::Arc;
use vmm_sys_util::eventfd::EventFd;
use virtio_queue::Queue;
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
}