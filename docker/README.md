# Remedia in Docker

A Dockerfile and Docker Compose example for the **standalone `remedia-server`**. The image installs a
SHA-256-verified release and FFmpeg 9.0.2 shared libraries; it does not compile Remedia and downloads
nothing at startup. It supports `linux/amd64` and `linux/arm64`.

Full guide (TLS proxy, plugin hosts, resources, updates, troubleshooting):
**<https://remedia.twlite.dev/docs/server/docker>**

## Quick start

```sh
cd docker
cp .env.example .env && chmod 600 .env          # then set REMEDIA_RPC_PASSWORD (openssl rand -hex 32)
cp config/remedia.config.example.json config/remedia.config.json
docker compose up -d --build
docker compose logs -f remedia
```

On Linux, make `./config` writable by the container user: set `REMEDIA_UID`/`REMEDIA_GID` in `.env` to
your own IDs, or run `sudo chown 10001:10001 config`. `remedia plugins` rewrites the configuration
atomically, so the **directory** is mounted, not the file.

## Files

| File                                 | Purpose                                                                 |
| ------------------------------------ | ----------------------------------------------------------------------- |
| `Dockerfile`                         | Multistage image; reuses `scripts/install.sh` for release and FFmpeg    |
| `compose.yaml`                       | Service, hardening, loopback-only port, config mount                    |
| `compose.media.yaml`                 | Optional override mounting `REMEDIA_MEDIA_DIR` read-only at `/media`    |
| `.env.example`                       | Template for secrets and build settings (copy to `.env`; not committed) |
| `config/remedia.config.example.json` | Starting configuration (copy to `config/remedia.config.json`)           |
| `../.dockerignore`                   | Keeps the build context to `scripts/install.sh`                         |

## Common commands

```sh
docker compose run --rm remedia doctor
docker compose run --rm remedia plugins list
docker compose exec remedia remedia --list --json
docker compose build && docker compose up -d      # upgrade after changing REMEDIA_VERSION
```

Clients on the same Compose network connect to `remedia://remedia:8787`. The published port is
`127.0.0.1:8787` for a TLS reverse proxy on the host; remote clients use `remedias://your.domain`.
Never publish the plain WebSocket port on a public interface.
