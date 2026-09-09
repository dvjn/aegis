# Aegis

A personal LLM gateway. Aegis authenticates clients, forwards provider requests, and records usage for its web dashboard.

## Development

```sh
mise run serve
```

The local bootstrap task creates a development account and API key. See the command output for their locations and values.

## Documentation

- [Quick start](docs/quick-start.md) - run Aegis and connect a client.
- [Server deployment](docs/deployment/README.md) - run the container and operate the service.
- [Server configuration](docs/configuration/README.md) - config file fields and environment variables.
- [Client configuration](docs/clients.md) - configure Claude Code, Codex, and pi.

## License

MIT.
