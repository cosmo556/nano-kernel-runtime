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
        
        // Almacenamos los resultados (id_descriptor, bytes_escritos) temporalmente
        let mut used_results = Vec::new();

        {
            // Creamos un scope limitado para el iterador
            let mut iter = self.queue.iter(mem).unwrap();

            while let Some(mut chain) = iter.next() {
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

                // Determinamos cuántos bytes se escribieron realmente
                let len_written = match request_type {
                    0 => { // LECTURA (IN)
                        let offset = sector * 512;
                        let mut buffer = vec![0u8; data_desc.len() as usize];
                        
                        self.file.seek(SeekFrom::Start(offset)).unwrap();
                        self.file.read_exact(&mut buffer).unwrap();
                        
                        mem.write_slice(&buffer, data_desc.addr()).unwrap();
                        mem.write_obj(0u8, status_desc.addr()).unwrap();
                        
                        data_desc.len() + 1 // Datos + 1 byte de status
                    }
                    1 => { // ESCRITURA (OUT)
                        let offset = sector * 512;
                        let mut buffer = vec![0u8; data_desc.len() as usize];
                        
                        mem.read_slice(&mut buffer, data_desc.addr()).unwrap();
                        
                        self.file.seek(SeekFrom::Start(offset)).unwrap();
                        self.file.write_all(&buffer).unwrap();
                        
                        mem.write_obj(0u8, status_desc.addr()).unwrap();
                        1 // Solo el byte de status
                    }
                    _ => {
                        mem.write_obj(0u8, status_desc.addr()).unwrap();
                        1
                    }
                };

                // Guardamos para procesar después
                used_results.push((head_index, len_written));
            }
            // Aquí termina el scope del iterador y se libera self.queue
        }

        // 2. AHORA ACTUALIZAMOS LA COLA
        for (idx, len) in used_results {
            self.queue.add_used(mem, idx, len).unwrap();
        }

        // Notificamos al Guest que hay datos listos en el "Used Ring"
        self.irqfd.write(1).expect("Fallo al inyectar IRQ");
    }
}