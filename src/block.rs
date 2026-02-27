use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write}; // <-- AÑADIR ESTO
use std::sync::Arc;
use vmm_sys_util::eventfd::EventFd;
use virtio_queue::{Descriptor, Queue, QueueOwnedT, QueueT};
use vm_memory::{GuestMemory, GuestMemoryMmap, Bytes};

pub struct VirtioBlockDevice {
    pub file: File,
    pub queue: Queue,
    pub ioeventfd: EventFd,
    pub irqfd: EventFd,
    pub mem: Arc<GuestMemoryMmap>,
    pub status: u32, // <-- NUEVO: Guardaremos el estado aquí
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
            status: 0,
        }
    }
    pub fn process_queue(&mut self) {
        let mem = self.mem.as_ref();

        // Iteramos sobre los descriptores disponibles en el Vring
        while let Some(mut chain) = self.queue.iter(mem).unwrap().next() {
            
            // 1. HEADER: Contiene el tipo de petición (Read/Write) y el sector
            let head: Descriptor = chain.next().expect("Fallo al leer header del disco");
            let mut header_data = [0u8; 16];
            mem.read_slice(&mut header_data, head.addr()).unwrap();
            
            let request_type = u32::from_le_bytes(header_data[0..4].try_into().unwrap());
            let sector = u64::from_le_bytes(header_data[8..16].try_into().unwrap());

            // 2. DATA: El buffer de memoria donde Linux espera los datos (o donde los envía)
            let data_desc: Descriptor = chain.next().expect("Fallo al leer data descriptor");
            
            // 3. STATUS: El byte donde informamos a Linux si la operación fue exitosa (0 = OK)
            let status_desc: Descriptor = chain.next().expect("Fallo al leer status descriptor");

            match request_type {
                0 => { // VIRTIO_BLK_T_IN (Lectura)
                    let offset = sector * 512;
                    let mut buffer = vec![0u8; data_desc.len() as usize];
                    
                    // Buscamos el sector en el archivo .ext4 y leemos
                    self.file.seek(SeekFrom::Start(offset)).unwrap();
                    self.file.read_exact(&mut buffer).unwrap();
                    
                    // Volcamos el contenido del archivo directamente a la RAM del Guest
                    mem.write_slice(&buffer, data_desc.addr()).unwrap();
                    
                    // Escribimos Éxito (0) en el descriptor de status
                    mem.write_obj(0u8, status_desc.addr()).unwrap();
                }
                1 => { // VIRTIO_BLK_T_OUT (Escritura)
                    let offset = sector * 512;
                    let mut buffer = vec![0u8; data_desc.len() as usize];
                    
                    // Leemos de la RAM del Guest lo que Linux quiere guardar
                    mem.read_slice(&mut buffer, data_desc.addr()).unwrap();
                    
                    // Lo escribimos físicamente en nuestro archivo odoo_disk.ext4
                    self.file.seek(SeekFrom::Start(offset)).unwrap();
                    self.file.write_all(&buffer).unwrap();
                    
                    mem.write_obj(0u8, status_desc.addr()).unwrap();
                }
                _ => {
                    // Para peticiones desconocidas (como Flush), respondemos OK por ahora
                    mem.write_obj(0u8, status_desc.addr()).unwrap();
                }
            }

            // Notificamos a la cola que hemos procesado la cadena de descriptores
            // Importante usar head.id() para referenciar el inicio de la cadena
            self.queue.add_used(mem, head.id(), data_desc.len()).unwrap();
        }

        // ¡EL TOQUE FINAL! Inyectamos la interrupción (IRQ 6) 
        // Esto le dice a Linux: "Oye, ya terminé de leer, mira la RAM".
        self.irqfd.write(1).expect("Fallo al inyectar IRQ");
    }
}