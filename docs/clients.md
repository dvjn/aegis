# Client configuration

Aegis is bring your own credentials. Provider credentials stay in the client and pass through Aegis to the provider. Aegis does not store provider credentials.

AGS handles Aegis authentication automatically. For manual configuration, create an Aegis API key in the web interface and grant it access to the provider ID used in the gateway URL.

## AGS

Install `ags` from your Aegis server using mise (Linux x86_64 or ARM64):

```sh
curl -fsSL https://aegis.example.com/install-mise.sh | sh
```

The server's installer registers mise's built-in `http:ags` backend with that server's version discovery, binary, and checksum URLs. It selects the exact version the server currently serves, then opens the connection wizard. Release binaries are downloaded from your Aegis server. Re-run the installer when changing server versions, or use `mise upgrade http:ags` to check that same server for an update. Installed CLI wrappers follow the version selected by mise. Use mise to manage the executable and `ags disconnect` or `ags uninstall` to revoke the connection or remove wrappers.

Open `/connect` on your Aegis server and run its installation command. AGS pairs through browser/device-code OAuth and opens an inline setup wizard. Select the subscriptions you use with Space, choose the default subscription for each provider type (Claude, Codex, and Antigravity), then optionally override those defaults for particular CLIs. Only selected subscriptions are granted to this machine. AGS installs executable wrappers for `claude`, `codex`, `pi`, and `agy`. Start a new shell after setup.

Each launch starts private loopback proxies and stops them when the CLI exits. pi gets a separate proxy for every enabled subscription. Provider sign-ins stay in the original CLI. The relay sends its Aegis OAuth access token in `x-aegis-access-token`, separately from provider authentication. It refreshes that token before requests when needed, including during long-running CLI sessions. Vendor configuration files are not edited, and Aegis tokens are not exposed in the child environment. Antigravity uses the CLI's existing Google subscription sign-in.

```sh
ags setup https://aegis.example.com
ags configure
ags run codex --provider work -- exec "Explain this project"
ags status
ags doctor
ags update
ags update --rollback
ags disconnect
ags uninstall
```

`ags login` aliases `ags setup`, and `ags config` aliases `ags configure`. `ags setup --no-wrappers` keeps explicit launching only. A wrapped CLI or `ags run` starts the login/provider wizard before launching when setup is missing. Set `AGS_SERVER_URL` for first-run discovery, or enter the server URL when prompted; the selected URL is remembered for future logins. Use repeatable `--provider CLI=ID` options with setup/configure to select defaults without terminal prompts. For example: `ags configure --provider codex=work --provider pi=personal`.

The initial installer supports Linux x86_64 and ARM64. Aegis release containers include both relay builds; other deployments set `AEGIS_RELAY_DIR` to a directory containing `<rust-target>/ags`. The installation script is served at `/install.sh`. The configured `public_url` must be the URL clients use. Downloads are verified using a checksum obtained from that same HTTPS server.

AGS stores only its Aegis OAuth access and refresh tokens, provider defaults, and connection metadata in an owner-only `connection.json` under `${XDG_DATA_HOME:-$HOME/.local/share}/ags`; `AGS_HOME` overrides that directory. No provider credentials are imported. Relays appear under Connected apps in Account, with granted providers and last use. The dashboard combines their usage with API keys in one Clients table. Revoking the app blocks subsequent gateway requests immediately; no secondary API key is issued. Disconnect revokes remotely before removing local state; `ags disconnect --offline` and `ags uninstall --offline` remove local state and tell you to revoke the client separately. Run `ags configure` to change enabled subscriptions, provider defaults, or CLI overrides, including newly added server providers. To re-pair after revocation, disconnect and run setup again.

Wrappers use a managed PATH entry for bash, zsh, fish, or a POSIX login shell. Aliases, functions, absolute executable paths, or IDE environments can bypass them. Vendor executables are resolved again on each launch so version-manager upgrades keep working. AGS does not silently fall back to the provider if Aegis is unavailable.

pi is launched with a temporary extension that preserves its native authentication, model catalog, `models.json`, settings, and installed subagent extensions. Its native `anthropic` and `openai-codex` provider names use the selected provider defaults, including any pi overrides. Every enabled subscription also appears as `ags-<provider-id>`, with that provider type's existing models. For example, `ags-claude-personal/<claude-model>` and `ags-codex-work/<codex-model>` can run concurrently in one pi session. The installed `pi-subagents` extension shares the parent's model registry, so its parallel calls use those same proxy routes. Choose named subscription models from pi's model picker or subagent configuration after opening pi. pi keeps its own startup model choice; AGS does not ask for a single pi provider.

The default subscription for each provider type reuses that native pi sign-in. For an additional account of the same type, use pi's `/login` menu and select the entry labeled with that subscription ID. Each named subscription keeps its own OAuth credentials in pi's native credential storage.

Existing `models.json` gateway endpoint overrides are redirected at the final streaming boundary, and legacy Aegis key headers are removed. Named pi providers and model-specific endpoints referencing an enabled Aegis subscription retain that route. `ags run pi --provider ID` changes the native default for that subscription type during this launch; every enabled subscription remains available by its `ags-<provider-id>` name. Other extensions must not replace AGS's streaming overrides.

The remaining sections describe manual configuration without AGS.

## Antigravity CLI

Add an Antigravity subscription provider to the server configuration:

```toml
[[providers]]
id = "antigravity"
type = "antigravity_subscription"
```

After restarting the server, run `ags configure`, enable this provider, and choose the default subscription. Then run `agy` through its installed wrapper, or `ags run agy -- --model gemini-3.8-flash-low`. In development, use `mise run ags -- config` and `mise run ags:run -- agy`.

AGS sets `CLOUD_CODE_URL` only in the launched process, starts its private proxy, and authenticates to Aegis with the connected app's OAuth token. Google's authorization stays in the original CLI. Account, model-catalog, and quota endpoints are proxied but excluded from captured model-request history. Generate-content requests record the requested/resolved model and cached, output, and thinking token usage. Cached input is counted once; output includes thinking, with thinking also recorded as its subset. Token counts describe model usage; subscription quotas and charges are not derived from token prices. Unpriced models retain token counts and an unknown cost.

This subscription override was verified with `agy` 1.5.3 and is an internal CLI interface. `AGY_LLM_GATEWAY_URL`/`AGY_LLM_GATEWAY_API_KEY`, a nonempty `modelProvider`, and explicit `auth.type` settings conflict with this mode. Session settings can be reset with `--settings '{"modelProvider":"","auth":{"type":""}}'`. Remote agents execute outside the local relay and are unsupported.

Guardrails scan nested Gemini prompts, system instructions, and tool declarations. Signed parts and inline attachment bytes are preserved. Normal requests and observe mode stream immediately. When mask mode finds a secret, response restoration buffers up to 8 MiB so late thought signatures can be recognized; signed candidates remain unchanged. Larger responses pass through with placeholders intact. Unsigned text split across SSE frames and function arguments are restored after the response completes.

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
