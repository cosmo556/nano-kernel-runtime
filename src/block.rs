use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write}; // <-- AÑADIR ESTO
use std::sync::Arc;
use vmm_sys_util::eventfd::EventFd;
use virtio_queue::{Descriptor, Queue, QueueOwnedT, QueueT};
use vm_memory::{GuestMemoryMmap, Bytes};

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

        // 1. CREAMOS EL ITERADOR FUERA DEL BUCLE
        // Esto es vital para avanzar por la cola y no procesar siempre lo mismo
        let mut iter = self.queue.iter(mem).unwrap();

        while let Some(mut chain) = iter.next() {
            // Guardamos el head_index de la CADENA. Este es el ID que Linux espera.
            let head_index = chain.head_index();

            // 1. HEADER
            let head: Descriptor = chain.next().expect("Fallo al leer header");
            let mut header_data = [0u8; 16];
            mem.read_slice(&mut header_data, head.addr()).unwrap();
            
            let request_type = u32::from_le_bytes(header_data[0..4].try_into().unwrap());
            let sector = u64::from_le_bytes(header_data[8..16].try_into().unwrap());

            // 2. DATA
            let data_desc: Descriptor = chain.next().expect("Fallo al leer data");
            
            // 3. STATUS
            let status_desc: Descriptor = chain.next().expect("Fallo al leer status");

            let mut len_written = 0u32;

            match request_type {
                0 => { // LECTURA (IN)
                    let offset = sector * 512;
                    let mut buffer = vec![0u8; data_desc.len() as usize];
                    
                    self.file.seek(SeekFrom::Start(offset)).unwrap();
                    self.file.read_exact(&mut buffer).unwrap();
                    
                    mem.write_slice(&buffer, data_desc.addr()).unwrap();
                    mem.write_obj(0u8, status_desc.addr()).unwrap();
                    
                    // En lecturas, informamos a Linux cuántos bytes escribimos en su RAM
                    len_written = data_desc.len() + 1; 
                }
                1 => { // ESCRITURA (OUT)
                    let offset = sector * 512;
                    let mut buffer = vec![0u8; data_desc.len() as usize];
                    
                    mem.read_slice(&mut buffer, data_desc.addr()).unwrap();
                    
                    self.file.seek(SeekFrom::Start(offset)).unwrap();
                    self.file.write_all(&buffer).unwrap();
                    
                    mem.write_obj(0u8, status_desc.addr()).unwrap();
                    
                    // En escrituras, solo escribimos el byte de status (1 byte)
                    len_written = 1;
                }
                _ => {
                    mem.write_obj(0u8, status_desc.addr()).unwrap();
                    len_written = 1;
                }
            }

            // 2. USAMOS chain.head_index() (o nuestra variable head_index)
            // Esto le dice a Linux: "He terminado con la cadena que empezaba en este índice"
            self.queue.add_used(mem, head_index, len_written).unwrap();
        }

        // Notificación de hardware terminada
        self.irqfd.write(1).expect("Fallo al inyectar IRQ");
    }
}