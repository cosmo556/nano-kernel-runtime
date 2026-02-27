# Makefile para compilar el test-kernel del NKR.
#
# Uso:
#   make              → genera test-kernel.img (ELF64 estático x86_64)
#   make clean        → elimina artefactos
#   make install      → copia test-kernel.img a la raíz del proyecto
#
# Requisitos: gcc (o gas) con soporte x86_64.

AS      = as
LD      = ld
OBJCOPY = objcopy

ASFLAGS = --64
LDFLAGS = -T linker.ld -m elf_x86_64 --nostdlib --static

TARGET  = test-kernel.img
OBJ     = start.o

all: $(TARGET)

$(OBJ): start.S
	$(AS) $(ASFLAGS) -o $@ $<

$(TARGET): $(OBJ) linker.ld
	$(LD) $(LDFLAGS) -o $@ $(OBJ)

install: $(TARGET)
	cp $(TARGET) ../../test-kernel.img
	@echo "Copiado $(TARGET) → ../../test-kernel.img"

clean:
	rm -f $(OBJ) $(TARGET)

verify: $(TARGET)
	@echo "=== file ==="
	file $(TARGET)
	@echo "=== readelf -h ==="
	readelf -h $(TARGET)
	@echo "=== readelf -l ==="
	readelf -l $(TARGET)

.PHONY: all clean install verify
