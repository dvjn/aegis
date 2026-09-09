# Container

## 1. Prepare the configuration

Create `config.toml`. At minimum, set the public URL and one provider. See [Configuration](../configuration/README.md).

Create persistent storage:

```sh
podman volume create aegis-data
```

## 2. Create the first account

Run this once before starting the server:

```sh
podman run --rm -it \
  -v aegis-data:/data \
  -v "$PWD/config.toml:/etc/aegis.toml:ro" \
  -e AEGIS_CONFIG=/etc/aegis.toml \
  ghcr.io/dvjn/aegis:latest \
  bootstrap-user --email you@example.com
```

## 3. Run

```sh
podman run -d --name aegis --restart=unless-stopped \
  -p 127.0.0.1:8765:8765 \
  -v aegis-data:/data \
  -v "$PWD/config.toml:/etc/aegis.toml:ro" \
  -e AEGIS_CONFIG=/etc/aegis.toml \
  ghcr.io/dvjn/aegis:latest
```

Bind to a private address when a reverse proxy runs on another host. Set `public_url` to the external HTTPS URL.

## Operating

```sh
podman logs -f aegis
curl -fsS http://127.0.0.1:8765/healthz
podman restart aegis
```

Back up the `aegis-data` volume while Aegis is stopped. The volume contains the SQLite database and `root.key`. Losing `root.key` invalidates password and OAuth secrets derived from it.
