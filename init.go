package main

import (
"fmt"
"os"
"os/exec"
"syscall"
"time"
"strings"
)

func main() {
fmt.Println("[NKR-INIT] Golang NKR Init started")

os.MkdirAll("/proc", 0755)
os.MkdirAll("/sys", 0755)
os.MkdirAll("/dev", 0755)
os.MkdirAll("/mnt", 0755)

syscall.Mount("proc", "/proc", "proc", 0, "")
syscall.Mount("sysfs", "/sys", "sysfs", 0, "")
syscall.Mount("devtmpfs", "/dev", "devtmpfs", 0, "")

fmt.Println("[NKR-INIT] Mounting /dev/vda to /mnt...")
err := syscall.Mount("/dev/vda", "/mnt", "ext4", 0, "")
if err != nil {
fmt.Printf("[NKR-INIT] Failed to mount /dev/vda: %v\n", err)
time.Sleep(100 * time.Hour)
}

exec.Command("/mnt/sbin/ip", "link", "set", "lo", "up").Run()
exec.Command("/mnt/sbin/ip", "link", "set", "eth0", "up").Run()

cmdline, _ := os.ReadFile("/proc/cmdline")
for _, part := range strings.Fields(string(cmdline)) {
if strings.HasPrefix(part, "nkr.ip=") {
ip := strings.TrimPrefix(part, "nkr.ip=")
fmt.Printf("[NKR-INIT] Configuring IP %s/24\n", ip)
exec.Command("/mnt/sbin/ip", "addr", "add", ip+"/24", "dev", "eth0").Run()
}
}

os.MkdirAll("/mnt/proc", 0755)
os.MkdirAll("/mnt/sys", 0755)
os.MkdirAll("/mnt/dev", 0755)
syscall.Mount("proc", "/mnt/proc", "proc", 0, "")
syscall.Mount("sysfs", "/mnt/sys", "sysfs", 0, "")
syscall.Mount("/dev", "/mnt/dev", "", syscall.MS_BIND, "")

fmt.Println("[NKR-INIT] Chrooting into /mnt and launching pgbouncer...")
syscall.Chroot("/mnt")
os.Chdir("/")

os.MkdirAll("/var/run/postgresql", 0755)
os.MkdirAll("/var/log/postgresql", 0755)
os.Chown("/var/run/postgresql", 70, 70)
os.Chown("/var/log/postgresql", 70, 70)

cmd := exec.Command("/bin/su", "-", "postgres", "-c", "/usr/bin/pgbouncer /etc/pgbouncer/pgbouncer.ini")
cmd.Stdout = os.Stdout
cmd.Stderr = os.Stderr
cmd.Run()

fmt.Println("[NKR-INIT] PgBouncer exited. Freezing.")
for {
time.Sleep(1 * time.Hour)
}
}
