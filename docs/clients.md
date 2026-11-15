# Client configuration

Aegis is bring your own credentials. Provider credentials stay in the client and pass through Aegis to the provider. Aegis does not store provider credentials.

Create an Aegis API key in the web interface before configuring a client. Grant the key access to the provider ID used in the gateway URL.

To change a key later, use the buttons next to it under **Account**. **Edit** renames the key or changes its providers; the change applies to its current secret from the next request. **Rotate** issues a new secret with the same name and providers. After a rotation, the previous secret keeps working for one hour so clients can be updated, unless **Revoke the old secret now** is checked.

## Claude Code

Set the gateway URL and Aegis header, then run Claude Code with its existing provider authentication:

```sh
export ANTHROPIC_BASE_URL="http://127.0.0.1:8765/providers/claude"
export ANTHROPIC_CUSTOM_HEADERS="x-aegis-api-key: YOUR_AEGIS_KEY"
claude
```

Replace `claude` in the URL when the provider uses another ID.

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

## Amazon Bedrock and Google Vertex AI

A `bedrock` or `vertex` provider serves Claude Code, Codex and pi. Each client keeps its own cloud credentials and sends the Aegis key as an extra header. Claude Code runs Claude models only, and Codex runs OpenAI models only. pi can use any model the cloud offers.

Bedrock needs a Bedrock API key in `AWS_BEARER_TOKEN_BEDROCK`; other AWS credentials fail. Vertex uses your Google Cloud sign-in, such as `gcloud auth application-default login`. The examples use provider IDs `aws` and `gcp`.

### Claude Code

```sh
export ANTHROPIC_CUSTOM_HEADERS="x-aegis-api-key: YOUR_AEGIS_KEY"

# Bedrock
export CLAUDE_CODE_USE_BEDROCK=1
export ANTHROPIC_BEDROCK_BASE_URL="http://127.0.0.1:8765/providers/aws"
export AWS_REGION=eu-west-1   # the provider's region

# Vertex
export CLAUDE_CODE_USE_VERTEX=1
export ANTHROPIC_VERTEX_BASE_URL="http://127.0.0.1:8765/providers/gcp"
export ANTHROPIC_VERTEX_PROJECT_ID=your-project
export CLOUD_ML_REGION=global
```

### Codex

Codex's built-in `amazon-bedrock` and `gcp` providers keep their authentication; only the endpoint moves. They accept only static `http_headers`, not `env_http_headers`, so the Aegis key goes into the file. Add one of these to `~/.codex/config.toml`:

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
# Vertex: the base URL names your project and location.
model_provider = "gcp"

[model_providers.gcp]
base_url = "http://127.0.0.1:8765/providers/gcp/v1/projects/your-project/locations/global/publishers/openai/v1"
http_headers = { "x-aegis-api-key" = "YOUR_AEGIS_KEY" }
```

### pi

With `AEGIS_API_KEY` set, route pi's built-in providers in `~/.pi/agent/models.json`. pi leaves the project and location out of Vertex paths when its endpoint is replaced, so the Vertex URL names them:

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

Requests, usage and guardrails work as for other providers. Usage records the model named in the request path, and cloud model IDs such as `eu.anthropic.claude-sonnet-4-6-v1:0` are priced as their plain model.

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
