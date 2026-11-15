# Configuration

Aegis reads server settings from `config.toml` in the working directory. Environment variables select the file, override selected settings, and configure mail and logs. Providers and guardrails are stored in the database; see [Providers and guardrails](#providers-and-guardrails).

## Config file

Create `config.toml` from this block. Uncomment settings that need non-default values.

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

## Providers and guardrails

Providers and guardrails live in the database at two layers:

- **Instance:** set by superusers under **Instance settings** (the gear button in the top bar, shown only to superusers), or with the `aegis provider` and `aegis guardrails` commands. Every user can use instance providers, and instance guardrails apply to every request.
- **User:** each user adds their own under **Providers** and **Guardrails** in their account settings (the person button in the top bar). Nobody else sees or edits them.

A running server picks up changes made in the web interface immediately, and changes made with the CLI within 15 seconds.

### Providers

A provider has an ID, a type, and an optional upstream base URL. Clients send requests to `/providers/<id>/...` with an `x-aegis-api-key` header, and the key must allow that provider.

- IDs contain only letters, digits, `-` or `_`, up to 64 characters. A user's provider cannot reuse an instance provider's ID, and an instance provider cannot take an ID a user already has.
- Types: `claude_subscription`, `codex_subscription`, `typesafe`, `bedrock`, `vertex`. Each type has a default upstream.
- `bedrock` is Amazon Bedrock in one region, set with the **Region** field or `--region` (default `us-east-1`). It serves Claude Code (InvokeModel), pi (Converse) and Codex (Bedrock's OpenAI-compatible Responses endpoint). Clients must authenticate with a [Bedrock API key](https://docs.aws.amazon.com/bedrock/latest/userguide/api-keys.html) in `AWS_BEARER_TOKEN_BEDROCK`: AWS credentials that sign requests cover the host and body, which Aegis changes, so Bedrock rejects them.
- `vertex` is Google Vertex AI. Each request goes to the Google host for the location in its path, so one provider serves every project and location the client's own Google credentials can reach. It serves Claude Code (Anthropic models), Codex (OpenAI models) and pi (Gemini and other models).
- The ID and type cannot change after creation, because client URLs and usage history refer to them. Only the upstream can be edited.
- Deleting a provider removes it from every API key that was granted it. Usage history stays.
- Deleting an instance provider that users' active API keys use offers two choices: **Delete for everyone**, or **Keep for users**, which gives each of those users a copy of their own with the same ID, type and upstream, then removes it from the instance. Their keys keep working, and they can edit or delete their copy. Other users lose access.

```sh
aegis provider add claude --type claude_subscription
aegis provider add work --type codex_subscription --base-url https://proxy.example/codex
aegis provider add aws --type bedrock --region eu-west-1
aegis provider add gcp --type vertex
aegis provider update work --default-url
aegis provider list
aegis provider keep-for-users work   # delete from the instance, keeping a copy for each user of it
aegis provider remove work
```

### Guardrails

Guardrails scan request bodies before they reach a provider. Each layer turns on secret detection (built-in detectors) and custom regex patterns, and chooses a mode: `observe` records matches; `mask` replaces them before forwarding and restores them in the response.

The instance layer is a minimum that users can only add to. On a user's request, a layer that is on stays on, `mask` wins over `observe`, and the detectors and patterns of both layers run. A user's pattern cannot reuse the name of an instance pattern. The **Guardrails** page has a test box that shows what the saved settings would find in a sample.

```sh
aegis guardrails show
aegis guardrails set --enabled true --mode mask
aegis guardrails secrets all                   # every built-in detector
aegis guardrails secrets disable jwt           # all except jwt
aegis guardrails secrets disable               # secret detection off
aegis guardrails regex add employee_id '\bEMP-\d{6}\b'
aegis guardrails regex remove employee_id
```

Built-in secret detectors: `anthropic_api_key`, `openai_api_key`, `github_token`, `gitlab_token`, `aws_access_key_id`, `slack_token`, `google_api_key`, `stripe_key`, `npm_token`, `jwt`, `pem_private_key`, `aegis_api_key`. Pattern names are unique lowercase snake_case.

### Upgrading from configuration-file settings

Earlier releases read `[[providers]]` and `[guardrails]` from `config.toml`, with `GUARDRAILS_ENABLED`, `GUARDRAILS_MODE` and `GUARDRAILS_SECRETS_ENABLED` as overrides. On the first start after upgrading, Aegis copies those settings once into the instance layer. After that, and on any new installation, those sections and variables are ignored, and Aegis logs a warning if they are still set. Remove them once the import is done.

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

This task refreshes the bundled model catalog. The running server automatically backfills unknown costs on startup and after adopting refreshed prices. Already calculated costs are preserved; updated dashboard totals are queued for rebuilding. Models with no matching price remain unknown.

### Logs

`RUST_LOG` is optional and controls log filtering:

```sh
RUST_LOG=aegis=debug aegis serve
```

The default filter logs Aegis at `info` and its HTTP dependencies at `warn`.
