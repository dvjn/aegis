# Configuration

Aegis reads settings from `config.toml` in the working directory. Environment variables select the file, override selected settings, and configure mail and logs.

## Config file

Create `config.toml` from this block. Uncomment settings that need non-default values and add one `[[providers]]` table per provider.

```toml
# Optional. Listener address and port.
# Default: "127.0.0.1:8765".
# http_addr = "127.0.0.1:8765"

# Optional. SQLite connection URL.
# Default: "sqlite://data/aegis.db?mode=rwc".
# database_url = "sqlite://data/aegis.db?mode=rwc"

# Optional. Maximum captured request or response body in bytes. Must be greater than zero.
# Default: 16777216.
# max_capture_bytes = 16777216

# Optional. External origin for OAuth and MCP security checks.
# Set this to the exact external URL when Aegis runs behind a reverse proxy.
# Default: unset.
# public_url = "https://aegis.example.com"

# Optional. Allows account creation through the web form.
# Default: false.
# registration_enabled = false

# Optional. Scans request bodies for secrets. Default: disabled.
# [guardrails]
# enabled = false
# mode = "observe" # observe | mask. Observe records matches; mask replaces them before forwarding.
#
# Optional. Enabled by default when guardrails are enabled. Omit detectors to run all built-ins.
# [guardrails.secrets]
# enabled = true
# detectors = [
#   "anthropic_api_key", "openai_api_key", "github_token", "gitlab_token",
#   "aws_access_key_id", "slack_token", "google_api_key", "stripe_key",
#   "npm_token", "jwt", "pem_private_key", "aegis_api_key",
# ]
#
# Optional. User-defined patterns masked like secrets.
# [guardrails.regex]
# enabled = true
# [guardrails.regex.detectors]
# internal_token = '\bint_[a-z0-9]{32}\b' # Names must be unique lowercase snake_case.
# employee_id = '\bEMP-\d{6}\b'

# Optional. Add one table per upstream provider.
# [[providers]]
# id = "provider-name" # Required. Unique URL segment and API-key permission name. Maximum 64 characters: letters, digits, - or _.
# type = "claude_subscription" # Required: claude_subscription | codex_subscription.
# base_url = "https://provider.example/v1" # Optional. The provider type supplies a default.

# Optional. Controls model-price refreshes.
# [pricing]
# enabled = true # Default: true.
# refresh_hours = 12 # Default: 12. Must be at least 1.
# url = "https://raw.githubusercontent.com/dvjn/aegis/main/src/pricing/model_prices.json" # Default shown. Must use HTTPS.
#
# Optional. Add one table per local model-price override.
# [[pricing.overrides]]
# model = "example-model" # Required.
# input_per_mtok = 1.0 # Required.
# output_per_mtok = 5.0 # Required.
# cache_read_per_mtok = 0.1 # Optional. Default: unset.
# cache_write_per_mtok = 1.25 # Optional. Default: unset.
```

Aegis creates `data/root.key` on first start. The file must contain 32 bytes and must not be accessible by group or other users.

Clients send requests to `/providers/<id>/...` with an `x-aegis-api-key` header. The API key must allow the selected provider.

## Environment variables

### Config file

| Variable | Required | Default | Sets |
| --- | --- | --- | --- |
| `AEGIS_CONFIG` | no | `config.toml` | path to the config file |

### Server overrides

| Variable | Required | Default | Overrides |
| --- | --- | --- | --- |
| `HTTP_ADDR` | no | config file | `http_addr` |
| `DATABASE_URL` | no | config file | `database_url` |
| `MAX_CAPTURE_BYTES` | no | config file | `max_capture_bytes` |

### Guardrail overrides

| Variable | Required | Default | Overrides |
| --- | --- | --- | --- |
| `GUARDRAILS_ENABLED` | no | config file | `guardrails.enabled` |
| `GUARDRAILS_MODE` | no | config file | `guardrails.mode` |
| `GUARDRAILS_SECRETS_ENABLED` | no | config file | `guardrails.secrets.enabled` |

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

### Logs

`RUST_LOG` is optional and controls log filtering:

```sh
RUST_LOG=aegis=debug aegis serve
```

The default filter logs Aegis at `info` and its HTTP dependencies at `warn`.
