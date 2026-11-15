# Configuration

Aegis reads server settings from `config.toml` in the working directory. Environment variables select the file, override selected settings, and configure mail and logs. Providers, guardrails and API keys are stored in the database; see [Providers, guardrails and API keys](../providers.md).

## Config file

Create `config.toml` from this block. Uncomment settings that need non-default values.

```toml
# Optional. Listener address and port.
# Default: "127.0.0.1:8765".
# http_addr = "127.0.0.1:8765"

# Optional. SQLite connection URL.
# Default: "sqlite://data/aegis.db?mode=rwc".
# database_url = "sqlite://data/aegis.db?mode=rwc"

# Optional. Maximum response body kept in usage records, in bytes. Request bodies are
# kept whole. Must be greater than zero.
# Default: 16777216.
# max_capture_bytes = 16777216

# Optional. External origin for OAuth and MCP security checks.
# Set this to the exact external URL when Aegis runs behind a reverse proxy.
# Default: unset.
# public_url = "https://aegis.example.com"

# Optional. Allows account creation through the web form.
# Default: false.
# registration_enabled = false

# Optional. Controls model-price refreshes.
# [pricing]
# enabled = true # Default: true.
# refresh_hours = 12 # Default: 12. Must be at least 1.
# url = "https://raw.githubusercontent.com/dvjn/aegis/main/pricing/model_prices.json" # Default shown. Must use HTTPS.
#
# Optional. Add one table per local model-price override.
# [[pricing.overrides]]
# model = "example-model" # Required.
# input_per_mtok = 1.0 # Required.
# output_per_mtok = 5.0 # Required.
# cache_read_per_mtok = 0.1 # Optional. Default: unset.
# cache_write_per_mtok = 1.25 # Optional. Default: unset.
```

Aegis creates `data/root.key` under its working directory on first start (`/data/data/root.key` in the container). The file must contain 32 bytes and must not be accessible by group or other users.

## Settings from earlier releases

Earlier releases read `[[providers]]` and `[guardrails]` from this file, and the `GUARDRAILS_ENABLED`, `GUARDRAILS_MODE` and `GUARDRAILS_SECRETS_ENABLED` variables. On the first start after upgrading, Aegis imports them once as instance providers and guardrails. After that they are ignored, and Aegis logs a warning while they are still set. New installations never import them.

## Environment variables

### Config file

| Variable | Required | Default | Sets |
| --- | --- | --- | --- |
| `AEGIS_CONFIG` | no | `config.toml` | path to the config file |

### Server overrides

| Variable | Required | Default | Overrides |
| --- | --- | --- | --- |
| `HTTP_ADDR` | no | config file | `http_addr` |
| `PUBLIC_URL` | no | config file | `public_url` |
| `DATABASE_URL` | no | config file | `database_url` |
| `MAX_CAPTURE_BYTES` | no | config file | `max_capture_bytes` |

### Mail

| Variable | Required | Default | Sets |
| --- | --- | --- | --- |
| `SMTP_HOST` | no | unset | SMTP server for password-reset mail |
| `SMTP_PORT` | no | `587` | SMTP server port |
| `SMTP_FROM` | with `SMTP_HOST` | unset | sender address |
| `SMTP_USERNAME`, `SMTP_PASSWORD` | no | unset | credentials; set both or neither |

Without `SMTP_HOST`, Aegis writes password-reset links to the log.

### Pricing overrides

| Variable | Required | Default | Overrides |
| --- | --- | --- | --- |
| `PRICING_ENABLED` | no | config file | `pricing.enabled` |
| `PRICING_REFRESH_HOURS` | no | config file | `pricing.refresh_hours` |
| `PRICING_URL` | no | config file | `pricing.url` |

### Refreshing prices and backfilling costs

For local development:

```sh
mise run pricing:update
```

This task refreshes the bundled model catalog from [LiteLLM](https://github.com/BerriAI/litellm). Each request is priced with its provider's own prices, falling back to the model's first-party price. Prices fetched at runtime layer over the bundled ones. The running server automatically backfills unknown costs on startup and after adopting refreshed prices. Already calculated costs are preserved; updated dashboard totals are queued for rebuilding. Models with no matching price remain unknown.

### Logs

`RUST_LOG` is optional and controls log filtering:

```sh
RUST_LOG=aegis=debug aegis serve
```

The default filter logs Aegis at `info` and its HTTP dependencies at `warn`.
