# Client configuration

Aegis is bring your own credentials. Provider credentials stay in the client and pass through Aegis to the provider. Aegis does not store provider credentials.

AGS handles Aegis authentication automatically. For manual configuration, create an Aegis API key in the web interface and grant it access to the provider ID used in the gateway URL.

## AGS

Install `ags` from your Aegis server using mise (Linux x86_64 or ARM64):

```sh
curl -fsSL https://aegis.example.com/install-mise.sh | sh
```

The server's installer registers mise's built-in `http:ags` backend with that server's version discovery, binary, and checksum URLs. It selects the exact version the server currently serves, then opens `ags setup`. Release binaries are downloaded from your Aegis server. Re-run the installer when changing server versions, or use `mise upgrade http:ags` to check that same server for an update. Installed CLI wrappers follow the version selected by mise. Use mise to manage the executable and `ags disconnect` or `ags uninstall` to revoke the connection or remove wrappers.

Open `/connect` on your Aegis server and run its installation command. On a terminal, `ags setup` opens a full-screen setup you can re-run any time. It has three steps, shown as tabs: **Aegis**, **Accounts**, and **Profiles**. Every element is edited in place and every change is saved at once, so you can quit at any point.

- **Aegis** pairs through browser/device-code OAuth (Enter on the server field; the verification URL and code appear on screen; Esc cancels). Once paired, it shows the server (Enter changes it, after revoking this machine on the current one), the sign-in state (Enter signs in again with the same client, keeping accounts and profiles), one checkbox per subscription (route) the server offers (Space toggles; only checked routes are granted, and each provider type's first checked route is its default), and Disconnect.
- **Accounts** lists your accounts and their sign-in state. Enter or `i` signs in (the vendor CLI takes over the terminal, then setup returns), `o` signs out, `r` renames in place, `d` removes after a y/n question, and `a` or the last row adds an account through a small inline form (product, separate or existing sign-in, name, and route when several are granted).
- **Profiles** shows each profile with its Claude and ChatGPT accounts. Enter on a product row opens a list of that product's accounts; on a profile, `d` toggles the global default, `h` toggles the rule for this directory, `r` renames, and `x` removes. `n` adds a profile.

Tab, Shift-Tab, 1/2/3, or ←/→ switch steps; ↑/↓ or j/k move; the bottom line lists the keys for the focused element, and `?` shows them all. q, Esc, or Ctrl-C quits (Esc first cancels an edit or question). `ags setup --step accounts` or `--step profiles` opens on that step. After the first pairing, setup installs executable wrappers for `claude`, `codex`, `pi`, and `agy`; start a new shell afterwards. Default subscriptions per provider type (Claude, Codex, and Antigravity) and per-CLI overrides are set with `ags configure`.

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

`ags login` aliases `ags setup`, and `ags config` aliases `ags configure`. `ags setup --no-wrappers` keeps explicit launching only. Without a terminal, `ags setup` pairs or re-validates, applies `--provider` choices, and installs wrappers, then points to the terminal setup while you have no accounts. A wrapped CLI or `ags run` pairs and selects providers before launching when setup is missing. Set `AGS_SERVER_URL` for first-run discovery, or enter the server URL when prompted; the selected URL is remembered for future logins. Use repeatable `--provider CLI=ID` options with setup/configure to select defaults without terminal prompts. For example: `ags configure --provider codex=work --provider pi=personal`.

The initial installer supports Linux x86_64 and ARM64. Aegis release containers include both relay builds; other deployments set `AEGIS_RELAY_DIR` to a directory containing `<rust-target>/ags`. The installation script is served at `/install.sh`. The configured `public_url` must be the URL clients use. Downloads are verified using a checksum obtained from that same HTTPS server.

AGS stores its Aegis OAuth access and refresh tokens, provider defaults, and connection metadata in an owner-only `connection.json`, and account and profile definitions in `accounts.json` and `profiles.json`, under `${XDG_DATA_HOME:-$HOME/.local/share}/ags`; `AGS_HOME` overrides that directory. No provider credentials are imported; managed accounts hold the vendor's own sign-in in their account directory. Relays appear under Connected apps in Account, with granted providers and last use. The dashboard combines their usage with API keys in one Clients table. Revoking the app blocks subsequent gateway requests immediately; no secondary API key is issued. Disconnect revokes remotely before removing local state; `ags disconnect --offline` and `ags uninstall --offline` remove local state and tell you to revoke the client separately. Run `ags configure` to change enabled subscriptions, provider defaults, or CLI overrides, including newly added server providers. After a revocation or expired login, `ags setup` pairs again under the same client.

Wrappers use a managed PATH entry for bash, zsh, fish, or a POSIX login shell. Aliases, functions, absolute executable paths, or IDE environments can bypass them. Vendor executables are resolved again on each launch so version-manager upgrades keep working. AGS does not silently fall back to the provider if Aegis is unavailable.

### Accounts and profiles

An account is one vendor sign-in: a Claude subscription (used by Claude Code) or a ChatGPT subscription (used by Codex), bound to a granted route. A profile is a named context that picks at most one Claude account and one ChatGPT account. Profiles can share accounts, so one sign-in can serve several contexts.

```sh
ags setup --step accounts                                              # the setup screen, on Accounts
ags account new claude --claude --native                               # reuse your current Claude sign-in
ags account new work-claude --claude                                   # a separate sign-in, then `ags account login work-claude`
ags profile new work --claude work-claude --chatgpt work-chatgpt       # profiles only reference existing accounts
cd ~/Workspace/work-repo && ags profile use work --here                # work applies inside this repository
ags profile                                                            # what applies here, and why
claude                                                                 # launches with the work account
ags run codex -P personal -- exec "Explain this project"              # one-off override
```

Accounts are created only by `ags account new` and by the setup screen's Accounts step; profiles never create them. `ags account new [NAME] [--claude|--chatgpt] [--native] [--route ID]` asks for whatever is missing on a terminal; without one, NAME and the product are required. `ags profile new [NAME] [--claude ACCOUNT] [--chatgpt ACCOUNT] [--default] [--here]` takes an existing account name or `none`, and on a terminal asks per product (skipping products with no accounts yet) and where the profile applies. `ags profile set PROFILE claude|chatgpt ACCOUNT|none` changes one product. Short aliases: `ags p` (profile), `ags a` (account), `ags r` (run), `ags st` (status); `n`/`c` (new), `ls`, `u` (use), `s` (set), `mv` (rename), and `in`/`out` (account login/logout), as in `ags p c work` or `ags a in work-claude`. Other commands: `ags profile list`, `ags profile use NAME` (global default), `ags profile use --clear [--here]`, `ags profile rename`, `ags profile rm`, and `ags account list|login|logout|rename|rm`.

A launch picks its profile from `-P/--profile`, then `AGS_PROFILE`, then the longest matching directory rule (`--here` uses the git repository root, or the current directory outside git), then the global default. If none applies, the launch behaves as before, and `--provider ID` always launches without a profile. If the profile has no account for the CLI, an interactive launch offers your existing accounts for that product (or launching without a profile this time), and fails with the commands that add one when there are none; if the account is not signed in, an interactive launch asks to sign in. A non-interactive launch fails with the command that fixes it. On a terminal, each launch prints the profile and account it uses. Running processes keep their account when defaults or rules change.

You choose each account's name; a native account defaults to the product name (`claude`, `chatgpt`). A separate account has its own vendor directory under `accounts/<id>` in the AGS home, passed as `CLAUDE_CONFIG_DIR` or `CODEX_HOME`; Codex accounts store their login in that directory (`cli_auth_credentials_store="file"`). `ags account login` and `ags account logout` run the vendor's own `claude auth login`/`codex login` inside the account; AGS never handles provider credentials. Credentials and conversation history stay with the account, so profiles sharing an account also share its history. Settings, instruction files, skills, agents, commands, plugins, and hooks are symlinked from your normal configuration; replace a symlink with a real file or directory to override it for one account. Native accounts use your existing configuration, and removing one never deletes it. `ags account rm` refuses while profiles use the account unless `--force` detaches it.

Accounts and profiles are registered with Aegis by ID and label. Requests from a profile launch carry the account, the profile, and a per-launch run ID, which Aegis validates against the connected app and strips before forwarding. The dashboard shows usage by account and by profile and lists recent sessions, taken from Claude Code's `X-Claude-Code-Session-Id` and Codex's `session-id` request headers. For Codex, Aegis records the `chatgpt-account-id` and flags an account whose upstream account changes. Labels are display metadata only. agy and pi do not use profiles yet.

pi is launched with a temporary extension that preserves its native authentication, model catalog, `models.json`, settings, and installed subagent extensions. Its native `anthropic` and `openai-codex` provider names use the selected provider defaults, including any pi overrides. Every enabled subscription also appears as `ags-<provider-id>`, with that provider type's existing models. For example, `ags-claude-personal/<claude-model>` and `ags-codex-work/<codex-model>` can run concurrently in one pi session. The installed `pi-subagents` extension shares the parent's model registry, so its parallel calls use those same proxy routes. Choose named subscription models from pi's model picker or subagent configuration after opening pi. At startup, a model on a native provider name switches to the same model on that type's default subscription, so `openai-codex/<model>` opens as `ags-<default codex subscription>/<model>`; pi's saved default model is not changed.

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
