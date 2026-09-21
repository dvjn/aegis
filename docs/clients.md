# Client configuration

Aegis is bring your own credentials. Provider credentials stay in the client and pass through Aegis to the provider. Aegis does not store provider credentials.

Create an Aegis API key in the web interface before configuring a client. Grant the key access to the provider ID used in the gateway URL.

## Claude Code

Set the gateway URL and Aegis header, then run Claude Code with its existing provider authentication:

```sh
export ANTHROPIC_BASE_URL="http://127.0.0.1:8765/providers/claude"
export ANTHROPIC_CUSTOM_HEADERS="x-aegis-api-key: YOUR_AEGIS_KEY"
claude
```

Replace `claude` in the URL when the configured provider uses another ID.

## Codex

Store the Aegis key in the environment:

```sh
export AEGIS_API_KEY="YOUR_AEGIS_KEY"
```

Add this provider to `~/.codex/config.toml`:

```toml
model_provider = "aegis"

[model_providers.aegis]
name = "Aegis"
base_url = "http://127.0.0.1:8765/providers/codex"
wire_api = "responses"
requires_openai_auth = true

[model_providers.aegis.env_http_headers]
"x-aegis-api-key" = "AEGIS_API_KEY"
```

Codex continues to use its existing provider authentication. Replace `codex` in the URL when the configured provider uses another ID.

## pi

Store the Aegis key in the environment:

```sh
export AEGIS_API_KEY="YOUR_AEGIS_KEY"
```

Add this override to `~/.pi/agent/models.json`:

```json
{
  "providers": {
    "openai-codex": {
      "baseUrl": "http://127.0.0.1:8765/providers/codex",
      "headers": {
        "x-aegis-api-key": "$AEGIS_API_KEY"
      }
    }
  }
}
```

The override keeps pi's built-in models and provider authentication. Replace `codex` in the URL when the configured provider uses another ID.

## TypeSafe

Both official SDKs read the API root from `TYPESAFE_BASE_URL`, so the gateway URL is set once for every program on the machine:

```sh
export TYPESAFE_BASE_URL="http://127.0.0.1:8765/providers/typesafe"
export TYPESAFE_API_KEY="YOUR_TYPESAFE_KEY"
export AEGIS_API_KEY="YOUR_AEGIS_KEY"
```

The SDKs read the first two. The Aegis key has no environment variable of its own, so each client adds it as a header when it constructs its client:

```python
from typesafe import TypeSafeClient

client = TypeSafeClient(headers={"x-aegis-api-key": os.environ["AEGIS_API_KEY"]})
```

```javascript
const client = new TypeSafeClient({
  defaultHeaders: { "x-aegis-api-key": process.env.AEGIS_API_KEY },
});
```

Without an SDK the same request is two headers and a body:

```sh
curl "http://127.0.0.1:8765/providers/typesafe/v1/systemone" \
  --header "x-aegis-api-key: YOUR_AEGIS_KEY" \
  --header "Authorization: Bearer YOUR_TYPESAFE_KEY" \
  --header "Content-Type: application/json" \
  --data '{"model":"jev-latest","state":"the sky at noon","questions":{"blue":{"type":"noul","instructions":"Is it blue?"}}}'
```

Replace `typesafe` in the URL when the configured provider uses another ID.

A coding agent writing TypeSafe code can install the [`jev`](../skills/jev/SKILL.md) skill, which documents the request and answer shapes and this header:

```sh
npx skills add dvjn/aegis --skill jev
```

## Aegis authentication

The `x-aegis-api-key` header authenticates the client to Aegis. Aegis removes this header before forwarding the request. It does not replace or store the client's provider credentials.
