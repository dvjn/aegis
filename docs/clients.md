# Client configuration

Configure each client for its provider as usual; its credentials pass through Aegis, which never stores them. Pointing a client at Aegis needs only its base URL and the `x-aegis-api-key` header, with an API key granted that provider (see [API keys](providers.md#api-keys)).

The examples assume Aegis at `http://127.0.0.1:8765`, providers named `claude`, `codex`, `aws` (Bedrock), `gcp` (Vertex) and `typesafe`, and the key in the environment:

```sh
export AEGIS_API_KEY="YOUR_AEGIS_KEY"
```

## Claude Code

```sh
export ANTHROPIC_CUSTOM_HEADERS="x-aegis-api-key: $AEGIS_API_KEY"

export ANTHROPIC_BASE_URL="http://127.0.0.1:8765/providers/claude"        # Claude subscription
export ANTHROPIC_BEDROCK_BASE_URL="http://127.0.0.1:8765/providers/aws"   # Bedrock
export ANTHROPIC_VERTEX_BASE_URL="http://127.0.0.1:8765/providers/gcp"    # Vertex
```

## Codex

In `~/.codex/config.toml`, for a ChatGPT subscription:

```toml
model_provider = "aegis"

[model_providers.aegis]
name = "Aegis"
base_url = "http://127.0.0.1:8765/providers/codex"
wire_api = "responses"
requires_openai_auth = true
env_http_headers = { "x-aegis-api-key" = "AEGIS_API_KEY" }
```

For Bedrock or Vertex, on Codex's built-in providers. These accept only fixed headers:

```toml
[model_providers.amazon-bedrock]
base_url = "http://127.0.0.1:8765/providers/aws/openai/v1"
http_headers = { "x-aegis-api-key" = "YOUR_AEGIS_KEY" }

[model_providers.gcp]
base_url = "http://127.0.0.1:8765/providers/gcp/v1/projects/your-project/locations/global/publishers/openai/v1"
http_headers = { "x-aegis-api-key" = "YOUR_AEGIS_KEY" }
```

## pi

In `~/.pi/agent/models.json`:

```json
{
  "providers": {
    "anthropic": {
      "baseUrl": "http://127.0.0.1:8765/providers/claude",
      "headers": { "x-aegis-api-key": "$AEGIS_API_KEY" }
    },
    "openai-codex": {
      "baseUrl": "http://127.0.0.1:8765/providers/codex",
      "headers": { "x-aegis-api-key": "$AEGIS_API_KEY" }
    },
    "amazon-bedrock": {
      "baseUrl": "http://127.0.0.1:8765/providers/aws",
      "headers": { "x-aegis-api-key": "$AEGIS_API_KEY" }
    },
    "google-vertex": {
      "baseUrl": "http://127.0.0.1:8765/providers/gcp/v1/projects/your-project/locations/global",
      "headers": { "x-aegis-api-key": "$AEGIS_API_KEY" }
    }
  }
}
```

For Vertex, Codex and pi drop the project and location from request paths once their base URL changes, so their URLs include them.

## TypeSafe

Both official SDKs read the API root from `TYPESAFE_BASE_URL`, so the gateway URL is set once for every program on the machine:

```sh
export TYPESAFE_BASE_URL="http://127.0.0.1:8765/providers/typesafe"
export TYPESAFE_API_KEY="YOUR_TYPESAFE_KEY"
```

The Aegis key has no environment variable of its own, so each client adds it as a header when it constructs its client:

```python
import os
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
  --header "x-aegis-api-key: $AEGIS_API_KEY" \
  --header "Authorization: Bearer $TYPESAFE_API_KEY" \
  --header "Content-Type: application/json" \
  --data '{"model":"jev-latest","state":"the sky at noon","questions":{"blue":{"type":"noul","instructions":"Is it blue?"}}}'
```

A coding agent writing TypeSafe code can install the [`jev`](../skills/jev/SKILL.md) skill, which documents the request and answer shapes and this header:

```sh
npx skills add dvjn/aegis --skill jev
```
