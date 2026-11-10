# Aegis

A personal LLM gateway. Aegis authenticates clients, forwards provider requests, and records usage for its web dashboard.

## Install ags with mise

Install the version served by your Aegis server:

```sh
curl -fsSL https://your-aegis-server.example.com/install-mise.sh | sh
```

Linux x86_64 and ARM64 are supported. See [client setup](docs/clients.md) for wrappers and upgrades.

## Development

```sh
mise run dev
```

The local bootstrap task creates a development account and API key. See the command output for their locations and values.

In another terminal, run a CLI through the source-built relay:

```sh
mise run ags -- login --no-wrappers
mise run ags -- config
mise run relay -- claude
mise run relay -- codex exec "Explain this project"
mise run relay -- pi
mise run relay -- agy
```

Login opens device-code OAuth pairing and asks for default providers. Relay state stays in `data/ags`; `--no-wrappers` leaves global shell configuration unchanged. You can perform these steps manually or run `mise run relay -- claude` directly: missing setup runs the same wizard before launching the CLI. `mise run ags -- run claude --provider work -- <CLI arguments>` selects a route for one launch. Use `mise run ags -- disconnect` before testing a fresh login.

Put machine-specific `HTTP_ADDR`, `PUBLIC_URL`, and optional `AEGIS_RELAY_DIR` values in untracked `mise.local.toml`. The server installer at `/connect` uses release artifacts; `mise run dist` builds them under `dist/relay`. Local relay tasks use the current source binary directly.

## Documentation

- [Quick start](docs/quick-start.md) - run Aegis and connect a client.
- [Server deployment](docs/deployment/README.md) - run the container and operate the service.
- [Server configuration](docs/configuration/README.md) - config file fields and environment variables.
- [Client configuration](docs/clients.md) - configure Claude Code, Codex, pi, and Antigravity.

The server's `/connect` page installs **AGS**, a local relay with OAuth pairing, provider-default selection, and per-instance wrappers for Claude Code, Codex, pi, and Antigravity.

## License

MIT.
