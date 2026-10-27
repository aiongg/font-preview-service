# Deploying to a Flatcar host

`deploy.sh` runs from any Linux machine with `ssh`, `tar`, `curl` and `docker`. The host needs Flatcar Container Linux, a user with passwordless sudo, and a DNS name proxied by Cloudflare.

```sh
deploy/deploy.sh digest docker.io/dylanmtaylorartcraft/font-preview-service:latest   # → image@sha256:…
deploy/deploy.sh setup    <ssh-dest> <domain> <image@sha256:…>   # first install; re-run to apply changed files
deploy/deploy.sh deploy   <ssh-dest> <image@sha256:…>            # new image; rolls back if unhealthy
deploy/deploy.sh rollback <ssh-dest>
deploy/deploy.sh status   <ssh-dest>
deploy/deploy.sh ignition <domain> <image@sha256:…> host.ign     # Ignition config for a new host
```

## What runs on the host

| Unit | Does |
|---|---|
| `fps.service` | The service container on the internal `fps` network. Nonroot, read-only root, no capabilities, 1.2 GB memory, 1 CPU, 256 pids. |
| `fps-proxy.service` | Caddy on host ports 80/443 (8080/8443 inside, nonroot). Certificates from Let's Encrypt, kept in `/var/lib/font-preview/caddy`. |
| `iptables-restore`, `ip6tables-restore` | Incoming: SSH, ICMP, and 80/443 from Cloudflare's address ranges only. |

- Images are pinned by digest in `/etc/font-preview/release.env`; the previous one is kept in `release.env.prev`. Caddy's and Butane's digests are in `versions.env`.
- Container logs go to journald: `journalctl -u fps -u fps-proxy`, capped at 500 MB.
- Flatcar updates itself on the LTS channel and reboots only between 04:00 and 05:00 UTC. Everything is enabled, so it comes back after a reboot (about 35 s on a 1-vCPU host).
- Caddy serves port 80 without redirecting to HTTPS, so every Cloudflare SSL mode works.

## Host-specific values

None are stored here. The SSH destination and domain are arguments; Cloudflare's ranges are fetched at each `setup`. Re-run `setup` when they change.
