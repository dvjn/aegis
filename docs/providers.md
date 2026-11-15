# Providers, guardrails and API keys

Providers, guardrails and API keys are stored in the database and managed in the web interface or with the `aegis` command line. A running server applies web changes immediately and command-line changes within 15 seconds.

## Providers

A provider is an upstream that clients reach at `/providers/<id>/…`. Clients keep their own provider credentials; Aegis passes them through and never stores them.

| Type | Upstream |
| --- | --- |
| `claude_subscription` | Anthropic API |
| `codex_subscription` | ChatGPT's Codex backend |
| `typesafe` | TypeSafe API |
| `bedrock` | Amazon Bedrock in one region (default `us-east-1`). Clients must use a Bedrock API key; AWS credentials that sign requests are rejected, because Aegis changes the signed host and body. |
| `vertex` | Google Vertex AI, at the location each request names |

Every type has a default upstream. A custom base URL replaces it, for a proxy or a private endpoint.

Providers exist at two levels:

- **Instance** providers are available to every user and managed by superusers.
- **Personal** providers belong to one user and are visible only to them.

IDs are shared across both levels: a personal provider can't take an instance provider's ID, and the reverse. An ID uses letters, digits, `-` and `_`, up to 64 characters.

The ID and type are fixed after creation, because client URLs and usage history refer to them; only the upstream can change. Deleting a provider removes it from every API key granted it, and usage history stays. An instance provider that is still in use can instead be kept for its users: each user whose active keys use it gets a personal copy with the same ID, so their keys keep working.

## Guardrails

Guardrails scan each request for secrets and custom patterns before it leaves Aegis:

- `observe` records matches and sends the request unchanged;
- `mask` replaces matches with placeholders and restores the originals in the response.

Instance guardrails apply to every request, and each user can add personal ones. Personal guardrails only tighten the instance's: if either is on, guardrails run; if either masks, matches are masked; the detectors and patterns of both run.

The built-in secret detectors are `anthropic_api_key`, `openai_api_key`, `github_token`, `gitlab_token`, `aws_access_key_id`, `slack_token`, `google_api_key`, `stripe_key`, `npm_token`, `jwt`, `pem_private_key` and `aegis_api_key`. Custom patterns are regular expressions with a unique lowercase `snake_case` name.

Every request is scanned except `GET`, `HEAD` and `OPTIONS` requests, which carry no prompt. Content a provider signs, such as reasoning blocks, is never edited.

## API keys

Clients authenticate with an API key in the `x-aegis-api-key` header, which Aegis removes before forwarding. A key reaches only the providers it is granted.

Changing a key's name or providers applies to its current secret from the next request. Rotating issues a new secret and keeps the previous one working for one hour, unless it is revoked immediately. Revoking a key disables all its secrets at once.

## Command line

The commands manage instance providers and guardrails, and work on the server's database:

```sh
aegis provider add claude --type claude_subscription
aegis provider add aws --type bedrock --region eu-west-1
aegis provider add work --type codex_subscription --base-url https://proxy.example.com/codex
aegis provider update work --default-url
aegis provider list
aegis provider keep-for-users work
aegis provider remove work

aegis guardrails show
aegis guardrails set --enabled true --mode mask
aegis guardrails secrets all
aegis guardrails secrets disable jwt
aegis guardrails regex add employee_id '\bEMP-\d{6}\b'
aegis guardrails regex remove employee_id

aegis key create --user USER_ID --name ci --provider claude
aegis key list --user USER_ID
aegis key revoke --user USER_ID KEY_ID
```
