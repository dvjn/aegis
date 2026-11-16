# Quick Start

## Requirements

- Docker
- Credentials for a configured provider
- Claude Code, Codex or pi

## 1. Create the first account

```sh
docker volume create aegis-data
docker run --rm -it -v aegis-data:/data ghcr.io/dvjn/aegis:latest \
  bootstrap-user --email you@example.com
```

Enter the password when prompted. Keep the `aegis-data` volume when you restart or upgrade Aegis.

## 2. Start the server

```sh
docker run --rm --name aegis -p 127.0.0.1:8765:8765 -v aegis-data:/data \
  ghcr.io/dvjn/aegis:latest
```

Open <http://127.0.0.1:8765> and sign in. The first account is a superuser.

Add a provider. Provider IDs become part of the gateway URL. See [Providers, guardrails and API keys](providers.md).

## 3. Run a CLI through Aegis

Install the client, `ags` ([other ways](client-setup.md#install)), and pair this machine:

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

- [Server Deployment](server-deployment.md)
- [Server Configuration](server-configuration.md)
- [Client Setup](client-setup.md)
