# Server Deployment

Aegis runs as one container backed by SQLite. It stores its database and root key in `/data`. Configure it with [environment variables](server-configuration.md).

## 1. Create the first account

Run this once before starting the server:

```sh
docker volume create aegis-data
docker run --rm -it \
  -v aegis-data:/data \
  ghcr.io/dvjn/aegis:latest \
  bootstrap-user --email you@example.com
```

## 2. Run

```sh
docker run -d --name aegis --restart=unless-stopped \
  -p 127.0.0.1:8765:8765 \
  -v aegis-data:/data \
  ghcr.io/dvjn/aegis:latest

docker logs -f aegis

curl -fsS http://127.0.0.1:8765/healthz
```
