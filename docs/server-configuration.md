# Server Configuration

## Config file

| Variable | Required | Default | Sets |
| --- | --- | --- | --- |
| `AEGIS_CONFIG` | no | `config.toml` | path to the config file |

## Server overrides

| Variable | Required | Default | Overrides |
| --- | --- | --- | --- |
| `HTTP_ADDR` | no | config file | `http_addr` |
| `PUBLIC_URL` | no | config file | `public_url` |
| `DATABASE_URL` | no | config file | `database_url` |
| `MAX_CAPTURE_BYTES` | no | config file | `max_capture_bytes` |

## Mail

| Variable | Required | Default | Sets |
| --- | --- | --- | --- |
| `SMTP_HOST` | no | unset | SMTP server for sending mail |
| `SMTP_PORT` | no | `587` | SMTP server port |
| `SMTP_FROM` | with `SMTP_HOST` | unset | sender address |
| `SMTP_USERNAME`, `SMTP_PASSWORD` | no | unset | credentials; set both or neither |

Without `SMTP_HOST`, Aegis writes emails like password-reset links to the log.

## Authentication

| Variable | Required | Default | Sets |
| --- | --- | --- | --- |
| `ACCESS_TOKEN_MINUTES` | no | `5` | lifetime in minutes of access tokens issued with a refresh token; others last 1 hour |

## Pricing overrides

| Variable | Required | Default | Overrides |
| --- | --- | --- | --- |
| `PRICING_ENABLED` | no | config file | `pricing.enabled` |
| `PRICING_REFRESH_HOURS` | no | config file | `pricing.refresh_hours` |
| `PRICING_URL` | no | config file | `pricing.url` |

## Logs

`RUST_LOG` is optional and controls log filtering:

```sh
RUST_LOG=aegis=debug aegis serve
```

The default filter logs Aegis at `info` and its HTTP dependencies at `warn`.
