# Server Configuration

Aegis reads environment variables, and optionally a config file. A variable overrides its config key.

## Server

- `HTTP_ADDR` (`http_addr`): listen address, default `127.0.0.1:8765` (`0.0.0.0:8765` in the container)
- `PUBLIC_URL` (`public_url`): the URL clients reach Aegis at; set it behind a reverse proxy
- `DATABASE_URL` (`database_url`): SQLite URL, default `sqlite://data/aegis.db?mode=rwc` (`/data/aegis.db` in the container)
- `MAX_CAPTURE_BYTES` (`max_capture_bytes`): largest response body kept in usage records, default 16 MiB
- `REGISTRATION_ENABLED` (`registration_enabled`): allow sign-up through the web form, default `false`

## Mail

- `SMTP_HOST` (`smtp.host`): SMTP server for sending mail
- `SMTP_PORT` (`smtp.port`): SMTP server port, default `587`
- `SMTP_FROM` (`smtp.from`): sender address, required with `SMTP_HOST`
- `SMTP_USERNAME` (`smtp.username`), `SMTP_PASSWORD` (`smtp.password`): credentials; set both or neither

Without an SMTP host, Aegis writes emails like password-reset links to the log.

## Authentication

- `AUTH_ACCESS_TOKEN_MINUTES` (`auth.access_token_minutes`): lifetime of access tokens issued with a refresh token, default `5`; others last 1 hour

## Pricing

- `PRICING_ENABLED` (`pricing.enabled`): refresh model prices, default `true`
- `PRICING_REFRESH_HOURS` (`pricing.refresh_hours`): hours between refreshes, default `12`
- `PRICING_URL` (`pricing.url`): price map URL, default the one in this repository

## Logs

- `RUST_LOG`: log filter, such as `aegis=debug`; default logs Aegis at `info` and its HTTP dependencies at `warn`

## Config file

- `AEGIS_CONFIG`: path to the config file, default `config.toml` in the working directory

Example `config.toml`:

```toml
public_url = "https://aegis.example.com"
database_url = "sqlite://data/aegis.db?mode=rwc"
http_addr = "127.0.0.1:8765"   # 0.0.0.0:8765 for listening on all interfaces
max_capture_bytes = 16777216   # 16MiB
registration_enabled = false   # allow sign-up through the web form

[smtp]
host = "smtp.example.com"
port = 587
from = "aegis@example.com"
username = "aegis"             # set SMTP_PASSWORD in the environment

[auth]
access_token_minutes = 5

[pricing]
enabled = true
refresh_hours = 12
url = "https://raw.githubusercontent.com/dvjn/aegis/main/pricing/model_prices.json"
```
