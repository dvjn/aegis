# Client configuration

Aegis is bring your own credentials. Provider credentials stay in the client and pass through Aegis to the provider. Aegis does not store provider credentials.

Each client needs an Aegis API key granted the provider in its URL; see [API keys](providers.md#api-keys). The examples assume Aegis at `http://127.0.0.1:8765` and the key in the environment:

```sh
export AEGIS_API_KEY="YOUR_AEGIS_KEY"
```

Replace the provider ID in a URL when yours differs.

## Claude Code

Set the gateway URL and Aegis header, then run Claude Code with its existing provider authentication:

```sh
export ANTHROPIC_BASE_URL="http://127.0.0.1:8765/providers/claude"
export ANTHROPIC_CUSTOM_HEADERS="x-aegis-api-key: $AEGIS_API_KEY"
claude
```

## Codex

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

Codex continues to use its existing provider authentication.

## pi

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

The override keeps pi's built-in models and provider authentication.

## Amazon Bedrock and Google Vertex AI

Each client keeps its own cloud credentials. Bedrock requires a [Bedrock API key](https://docs.aws.amazon.com/bedrock/latest/userguide/api-keys.html) in `AWS_BEARER_TOKEN_BEDROCK`: other AWS credentials sign the exact host and body, which Aegis changes. Vertex uses the client's Google Cloud credentials. The examples use providers `aws` and `gcp`.

| Client | Bedrock | Vertex |
| --- | --- | --- |
| Claude Code | Claude models | Claude models |
| Codex | OpenAI models | OpenAI models |
| pi | Any Bedrock model | Gemini models |

Claude Code:

```sh
export ANTHROPIC_CUSTOM_HEADERS="x-aegis-api-key: $AEGIS_API_KEY"

# Bedrock
export CLAUDE_CODE_USE_BEDROCK=1
export ANTHROPIC_BEDROCK_BASE_URL="http://127.0.0.1:8765/providers/aws"
export AWS_REGION=eu-west-1

# Vertex
export CLAUDE_CODE_USE_VERTEX=1
export ANTHROPIC_VERTEX_BASE_URL="http://127.0.0.1:8765/providers/gcp"
export ANTHROPIC_VERTEX_PROJECT_ID=your-project
export CLOUD_ML_REGION=global
```

Codex, in `~/.codex/config.toml`. Its built-in cloud providers accept only fixed headers, so the key is written in:

```toml
# Bedrock
model_provider = "amazon-bedrock"
model = "openai.gpt-5.6-sol"

[model_providers.amazon-bedrock]
base_url = "http://127.0.0.1:8765/providers/aws/openai/v1"
http_headers = { "x-aegis-api-key" = "YOUR_AEGIS_KEY" }

[model_providers.amazon-bedrock.aws]
region = "eu-west-1"
```

```toml
# Vertex
model_provider = "gcp"

[model_providers.gcp]
base_url = "http://127.0.0.1:8765/providers/gcp/v1/projects/your-project/locations/global/publishers/openai/v1"
http_headers = { "x-aegis-api-key" = "YOUR_AEGIS_KEY" }
```

pi, in `~/.pi/agent/models.json`:

```json
{
  "providers": {
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

For Vertex, Codex and pi leave the project and location out of request paths once their endpoint changes, so their URLs include them.

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

## Aegis authentication

The `x-aegis-api-key` header authenticates the client to Aegis. Aegis removes this header before forwarding the request. It does not replace or store the client's provider credentials.
