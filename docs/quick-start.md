# Quick start

## Requirements

- Podman or Docker
- Credentials for a configured provider

The commands below use Podman. Replace `podman` with `docker` if needed.

## 1. Create the configuration

Create `config.toml` from the complete example in [Configuration](configuration/README.md#example). Add the providers this Aegis instance will route.

Set `public_url` when clients will reach Aegis through a URL other than `http://127.0.0.1:8765`.

```sh
podman volume create aegis-data
```

Provider IDs become part of the gateway URL. Keep the configuration file when you restart or upgrade Aegis.

## 2. Create the first account

```sh
podman run --rm -it \
  -v aegis-data:/data \
  -v "$PWD/config.toml:/etc/aegis.toml:ro" \
  -e AEGIS_CONFIG=/etc/aegis.toml \
  ghcr.io/dvjn/aegis:latest \
  bootstrap-user --email you@example.com
```

Enter the password when prompted.

## 3. Start the server

```sh
podman run --rm --name aegis \
  -p 127.0.0.1:8765:8765 \
  -v aegis-data:/data \
  -v "$PWD/config.toml:/etc/aegis.toml:ro" \
  -e AEGIS_CONFIG=/etc/aegis.toml \
  ghcr.io/dvjn/aegis:latest
```

Open <http://127.0.0.1:8765>, sign in, and create an API key under **Account**. Grant the key access to the provider the client will use.

## 4. Connect a client

Follow [Client configuration](clients.md) for Claude Code, Codex, or pi.

Aegis is bring your own credentials. Provider credentials stay in the client and Aegis does not store them.

## Next steps

- [Configure a client](clients.md).
- [Configure Aegis](configuration/README.md).
- [Operate the container](deployment/container.md).
