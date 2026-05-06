# NKR systemd units (privilege-separated deploy)

Two services:

- **`nkr.service`** — root daemon. Owns KVM, cgroups, netlink, iptables, loop devices. Listens on UDS `/var/run/nkr.sock`.
- **`nkr-api-server.service`** — unprivileged HTTP proxy. Runs as user `nkr-api`. Translates HTTP ↔ UDS only; touches no NKR business logic.

## Install

```bash
# 1. Build both binaries
cargo build --release
sudo install -m 0755 target/release/nkr            /usr/local/bin/nkr
sudo install -m 0755 target/release/nkr-api-server /usr/local/bin/nkr-api-server

# 2. Create the unprivileged user/group
sudo groupadd -r nkr-api
sudo useradd  -r -g nkr-api -s /usr/sbin/nologin -d /nonexistent nkr-api

# 2b. Filesystem-ops group (needed for the git/pylibs endpoints — skip if
# you don't enable those).
sudo groupadd -r nkr-addons
sudo usermod -aG nkr-addons nkr-api
sudo apt-get install -y acl git python3-pip   # tools the proxy invokes
sudo mkdir -p /mnt/nkr/cells /mnt/nkr/enterprise
sudo setfacl -R -m g:nkr-addons:rwx -m d:g:nkr-addons:rwx /mnt/nkr/cells
sudo setfacl -R -m g:nkr-addons:rwx -m d:g:nkr-addons:rwx /mnt/nkr/enterprise

# 3. API token (required in production)
sudo mkdir -p /etc/nkr
sudo sh -c 'echo NKR_API_TOKEN=$(openssl rand -hex 32) > /etc/nkr/api.env'
sudo chmod 640 /etc/nkr/api.env
sudo chown root:nkr-api /etc/nkr/api.env

# 4. Install units
sudo install -m 0644 deploy/systemd/nkr.service            /etc/systemd/system/
sudo install -m 0644 deploy/systemd/nkr-api-server.service /etc/systemd/system/
sudo systemctl daemon-reload

# 5. Start + enable (order matters — daemon first so the socket exists)
sudo systemctl enable --now nkr.service
sudo systemctl enable --now nkr-api-server.service

# 6. Sanity checks
systemctl status nkr.service nkr-api-server.service
curl -s http://127.0.0.1:9090/api/v1/health          # no auth needed
curl -s -H "Authorization: Bearer $(grep -oP '(?<==).*' /etc/nkr/api.env)" \
     http://127.0.0.1:9090/api/v1/cells
```

## TLS (for external panels on different hosts)

Terminate TLS in nginx/caddy in front of `nkr-api-server`. A minimal nginx block:

```nginx
server {
    listen 443 ssl http2;
    server_name nkr.yourdomain.com;
    ssl_certificate     /etc/letsencrypt/live/yourdomain.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/yourdomain.com/privkey.pem;

    # Reject the world — allow only the panel's IP(s).
    allow 203.0.113.10;
    deny  all;

    location / {
        proxy_pass http://127.0.0.1:9090;
        proxy_http_version 1.1;
        proxy_set_header Host $host;
        proxy_read_timeout 120s;   # clone can take ~30-40s
        proxy_send_timeout 120s;
    }
}
```

## Reload-free config changes

- API token → edit `/etc/nkr/api.env`, `systemctl restart nkr-api-server` (no downtime on the daemon).
- Bind address/port → edit `nkr-api-server.service`, `systemctl daemon-reload`, `restart nkr-api-server`.

## Troubleshooting

| Symptom | Likely cause |
|---|---|
| `nkr-api-server` logs `daemon_unreachable` | `nkr.service` not running, or UDS perms wrong. Check `ls -l /var/run/nkr.sock` — group should be `nkr-api`. |
| HTTP 401 on every mutation | `NKR_API_TOKEN` mismatch between `/etc/nkr/api.env` and the client. |
| `nkr-api-server` 503 on every request | Hit `MAX_INFLIGHT=64`. Only happens under real load or a malicious client keeping sockets open. |
| Daemon won't start, socket exists | Stale socket (previous unclean exit). The daemon removes it if it's a real socket; otherwise inspect manually. |

## Security posture

The unprivileged service enables: `NoNewPrivileges`, `ProtectSystem=strict`, `ProtectHome`, `PrivateTmp`, `PrivateDevices`, `ProtectKernel{Tunables,Modules,Logs}`, `ProtectControlGroups`, `ProtectProc=invisible`, `ProcSubset=pid`, `RestrictNamespaces`, `RestrictRealtime`, `RestrictSUIDSGID`, `LockPersonality`, `MemoryDenyWriteExecute`, `RemoveIPC`, `UMask=0077`, `RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6`, `SystemCallFilter=@system-service ~@privileged @resources @mount …`, empty `CapabilityBoundingSet`, `LimitNPROC=256`, `LimitNOFILE=1024`.

A compromise of `nkr-api-server` gives the attacker:
- Whatever the daemon's IPC handlers permit (create/delete instance, action, logs).
- No filesystem writes outside `/tmp` (and `/tmp` is private).
- No `/proc/<other-pid>`, no kernel tunables, no module loading, no ptrace.
- No capabilities, no setuid binaries, no new privileges.

A compromise of `nkr.service` remains root — the daemon still needs root for KVM/cgroups/netlink. The whole point of the split is shrinking the RCE blast radius at the HTTP boundary.
