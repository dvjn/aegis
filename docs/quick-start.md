# Quick start

## Requirements

- Podman or Docker
- Credentials for a configured provider
- Claude Code, Codex or pi

The commands below use Podman. Replace `podman` with `docker` if needed.

## 1. Create the configuration

Create `config.toml` from the complete example in [Configuration](configuration/README.md#config-file).

Set `public_url` when clients will reach Aegis through a URL other than `http://127.0.0.1:8765`.

```sh
podman volume create aegis-data
```

Keep the configuration file and the `aegis-data` volume when you restart or upgrade Aegis.

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

Open <http://127.0.0.1:8765> and sign in. The first account is a superuser.

Add a provider. Provider IDs become part of the gateway URL. See [Providers, guardrails and API keys](providers.md).

## 4. Run a CLI through Aegis

Install the client, `ags` ([other ways](client.md#install)), and pair this machine:

```sh
mise use -g "github:dvjn/aegis[matching=ags-]"
ags login http://127.0.0.1:8765
```

Approve the machine in the browser and choose its providers. ags then signs in to each provider in the terminal.

To run plain `claude`, `codex` and `pi` through ags, add the line for your shell to its startup file, then open a new shell:

```sh
eval "$(ags init bash)"    # ~/.bashrc
eval "$(ags init zsh)"     # ~/.zshrc
ags init fish | source     # ~/.config/fish/config.fish
```

Then run the CLI as usual:

```sh
claude
```

Aegis is bring your own credentials. Provider credentials stay on the machine, and Aegis does not store them.

## Next steps

- [Use the client](client.md).
- [Configure Aegis](configuration/README.md).
- [Operate the container](deployment/container.md).
