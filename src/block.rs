use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write}; // <-- AÑADIR ESTO
use std::sync::Arc;
use vmm_sys_util::eventfd::EventFd;
use virtio_queue::{DescriptorChain, Queue, QueueT};
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
    // El Job Runner: Procesa las peticiones del Vring
    pub fn process_queue(&mut self) {
        let mem = self.mem.as_ref();

        // Iteramos mientras haya "tickets" (descriptores) en la cola
        while let Some(mut chain) = self.queue.iter(mem).unwrap().next() {
            // 1. El primer descriptor es el HEADER (Tipo de operación y sector)
            let head = chain.next().expect("Fallo al leer header del disco");
            let mut header_data = [0u8; 16];
            mem.read_slice(&mut header_data, head.addr()).unwrap();
            
            let request_type = u32::from_le_bytes(header_data[0..4].try_into().unwrap());
            let sector = u64::from_le_bytes(header_data[8..16].try_into().unwrap());

            // 2. El segundo descriptor es el DATA (donde leemos o escribimos)
            let data_desc = chain.next().expect("Fallo al leer data descriptor");
            
            // 3. El tercer descriptor es el STATUS (donde informamos éxito)
            let status_desc = chain.next().expect("Fallo al leer status descriptor");

            match request_type {
                0 => { // VIRTIO_BLK_T_IN (LECTURA)
                    let offset = sector * 512;
                    let mut buffer = vec![0u8; data_desc.len() as usize];
                    
                    self.file.seek(SeekFrom::Start(offset)).unwrap();
                    self.file.read_exact(&mut buffer).unwrap();
                    
                    // Copiamos del archivo físico a la RAM del Guest
                    mem.write_slice(&buffer, data_desc.addr()).unwrap();
                    
                    // Escribimos éxito (0) en el byte de status
                    mem.write_obj(0u8, status_desc.addr()).unwrap();
                }
                1 => { // VIRTIO_BLK_T_OUT (ESCRITURA)
                    let offset = sector * 512;
                    let mut buffer = vec![0u8; data_desc.len() as usize];
                    
                    // Leemos de la RAM del Guest para guardar en el archivo
                    mem.read_slice(&mut buffer, data_desc.addr()).unwrap();
                    
                    self.file.seek(SeekFrom::Start(offset)).unwrap();
                    self.file.write_all(&buffer).unwrap();
                    
                    mem.write_obj(0u8, status_desc.addr()).unwrap();
                }
                _ => {
                    // Otros tipos como FLUSH o GET_ID los marcamos como éxito por ahora
                    mem.write_obj(0u8, status_desc.addr()).unwrap();
                }
            }

            // Avisamos a la cola que ya usamos este descriptor
            self.queue.add_used(mem, head.addr().0 as u16, data_desc.len()).unwrap();
        }

        // Inyectamos la interrupción física para que Linux sepa que el disco ya no está ocupado
        self.irqfd.write(1).expect("Fallo al inyectar IRQ");
    }
}