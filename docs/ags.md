# ags

`ags` is the command-line client for Aegis. It pairs a machine with one Aegis server, then launches Claude Code, Codex and pi through it. Each provider gets its own sign-in, and your own settings stay shared. There is nothing to configure: no base URLs, headers or API keys.

## Install

GitHub releases include an `ags` tarball for Linux on `x86_64` and `aarch64`, with a SHA-256 checksum beside it:

```sh
tag=vX.Y.Z                         # the release
target=x86_64-unknown-linux-musl   # or aarch64-unknown-linux-musl
name="ags-$tag-$target"
curl -fLO "https://github.com/dvjn/aegis/releases/download/$tag/$name.tar.gz"
curl -fLO "https://github.com/dvjn/aegis/releases/download/$tag/$name.tar.gz.sha256"
sha256sum -c "$name.tar.gz.sha256"
tar -xzf "$name.tar.gz"
install -Dm755 "$name/ags" ~/.local/bin/ags
```

Any directory on `PATH` works. On other systems, build it from source:

```sh
cargo install --locked --git https://github.com/dvjn/aegis ags
```

## Commands

| Command | Does |
| --- | --- |
| `ags login <URL>` | Pair this machine with an Aegis server |
| `ags logout` | Revoke this machine's access and forget the server |
| `ags status` | Show the server, account and providers of this machine |
| `ags setup [provider]` | Sign in to each provider that needs it, or again to one provider |
| `ags use [cli [provider]]` | Set the provider a CLI uses, or list the provider of each CLI |
| `ags [-p provider] <claude\|codex\|pi> [args…]` | Run the CLI through Aegis |
| `ags init <bash\|zsh\|fish>` | Print the shell code that runs plain `claude`, `codex` and `pi` through ags |

### login

```sh
ags login https://aegis.example
```

ags prints an approval URL and a code, and opens the browser when it can. On the approval page, check the code and choose the providers this machine may use. The machine then appears under **Connected apps** on your account page, as `ags on <hostname>`.

The URL must be the server's own address, its [`public_url`](configuration/README.md#config-file), and must use `https` unless it is on loopback.

After approval, login installs the [shims](#shims), then runs `ags setup` in a terminal. A machine pairs with one server at a time; run `ags logout` before logging in to another. Change a machine's providers with **Edit** under **Connected apps**.

ags refreshes the machine's token by itself. If the server revokes the machine, ags forgets the login and asks you to log in again.

### logout

Revokes the machine on the server, forgets the login and removes the shims. Provider sign-ins, keys and remembered providers are kept for the next login.

If the server can't revoke the machine, ags still forgets the login, exits with an error, and asks you to disconnect the machine under **Connected apps** on the server.

### status

Prints the server, account and granted providers. Exits with an error when not logged in.

### setup

Without an argument, walks every credential that is missing, one prompt each:

- a sign-in to each CLI that can use a subscription provider, run with that CLI's own login command;
- a Bedrock API key, typed without echo;
- a path to a Vertex service account key file, which ags copies.

Declining a sign-in, or leaving a key empty, skips that credential; `ags setup` doesn't ask about it again. For a CLI that isn't installed, ags asks whether to sign in once it is, and prints what to run after installing it.

With a provider, asks for each of its credentials again, including skipped ones. Declining keeps the current one.

A launch that finds its credential missing offers to set it up first.

### use

```sh
ags use claude aws   # claude uses aws from now on
ags use              # list each CLI's provider
```

The list shows each CLI with its provider and type, and pi with one row per provider type:

```text
CLI     PROVIDER  TYPE
claude  claude    claude_subscription
codex   —
pi      claude    claude_subscription
pi      —         codex_subscription
pi      aws       bedrock
pi      —         vertex (none granted)
```

### claude, codex, pi

```sh
ags claude --resume
ags -p aws claude
AGS_PROVIDER=aws claude
```

Everything after the CLI's name goes to the CLI unchanged, so `-p` or `--provider` goes before it. `AGS_PROVIDER` does the same for one launch; empty counts as unset.

Without either, ags uses the CLI's remembered provider, or the only provider that fits. When several fit, the first launch asks once and remembers the answer; change it with `ags use`. Without a terminal, ags fails instead and names the options.

ags exits as the CLI exits.

### init

Prints shell code that puts the shims first on `PATH`. When logged in, it also rewrites the shims if they don't run the current ags. Add the line for your shell to its startup file:

| Shell | Startup file | Line |
| --- | --- | --- |
| bash | `~/.bashrc` | `eval "$(ags init bash)"` |
| zsh | `~/.zshrc` | `eval "$(ags init zsh)"` |
| fish | `~/.config/fish/config.fish` | `ags init fish \| source` |

`ags login` and `ags status` print the line for your shell while the shims are not on `PATH`.

## Shims

`ags login` writes `claude`, `codex` and `pi` shims to the `bin` directory of the [ags directory](#files). With the init line in place, plain `claude` runs as `ags claude`. ags runs the real CLI, the first one on `PATH` that is not a shim.

When a shim can't go through Aegis, for example when not logged in or a provider is not set up:

- in a terminal, ags shows the error, offers a fix when there is one (log in, set up providers, or pair again), then offers to open the CLI without Aegis;
- without a terminal, ags prints the error and runs the CLI without Aegis.

`ags claude` never falls back; it fails instead.

Set `AGS_BYPASS=1` to run the real CLI without Aegis for one run:

```sh
AGS_BYPASS=1 claude
```

A CLI started from inside a launch of the same CLI runs in that launch.

## Providers and CLIs

Each CLI runs on these provider types:

| CLI | `claude_subscription` | `codex_subscription` | `bedrock` | `vertex` |
| --- | --- | --- | --- | --- |
| `claude` | yes | | yes | yes |
| `codex` | | yes | yes | yes |
| `pi` | yes | yes | yes | yes |

Codex has [limits on Bedrock and Vertex](#cloud-providers).

### Sign-ins

Claude Code and Codex run in a config directory of the provider's own, `providers/<id>/claude` or `providers/<id>/codex` in the [ags directory](#files). A subscription provider's sign-in runs there, so two providers of the same type can use two accounts. Session history stays there too. pi keeps each provider's sign-in in `providers/<id>/pi`.

On macOS, Claude Code keeps the sign-in itself in the keychain.

During a launch, ags unsets the variables a CLI would use in place of the provider's sign-in, such as `ANTHROPIC_API_KEY`, `OPENAI_API_KEY` or `AWS_PROFILE`. It warns once when your Claude Code `settings.json` sets `apiKeyHelper`, or sets such a variable under `env`, which ags can't override.

### Shared settings

Your own settings are linked into each provider's directory, from `~/.claude`, `~/.codex` and `~/.pi/agent`, or from `CLAUDE_CONFIG_DIR`, `CODEX_HOME` and `PI_CODING_AGENT_DIR` when set:

| CLI | Shared |
| --- | --- |
| Claude Code | `settings.json`, `CLAUDE.md`, `rules`, `agents`, `commands`, `skills`, `output-styles`, `workflows`, `keybindings.json`, `themes`, `hooks`, `plugins`, `statusline.py`, `statusline.sh`; copied from `.claude.json`: MCP servers, onboarding state, and each project's MCP and trust settings |
| Codex | `config.toml`, `AGENTS.md`, `AGENTS.override.md`, `rules`, `prompts`, `hooks.json`, `.hook-trust`, `skills`, `plugins` |
| pi | `settings.json`, `keybindings.json`, `mcp.json`, `models.json`, `AGENTS.md`, `CLAUDE.md`, `SYSTEM.md`, `APPEND_SYSTEM.md`, `extensions`, `skills`, `prompts`, `themes`, `npm` |

When a provider's directory holds a different copy of a shared file, for example after a CLI saved over the link, ags keeps that copy and says once which file to delete to share yours again.

### Cloud providers

| Type | Credential | Where the request goes |
| --- | --- | --- |
| `bedrock` | Bedrock API key | The provider's region, else `AWS_REGION` or `AWS_DEFAULT_REGION` |
| `vertex` | Service account key file; its `project_id` is the project | The provider's location if it has one, else `CLOUD_ML_REGION` for Claude Code or `GOOGLE_CLOUD_LOCATION` for Codex and pi, else `global` |

ags keeps the credential in the provider's directory and gives it only to that provider's launches. Your own AWS credentials and `GOOGLE_CLOUD_API_KEY` are unset during them.

Codex on Bedrock needs a provider whose endpoint names its region. Codex on Vertex runs on Linux and macOS only, needs `gcloud` on `PATH`, and needs OpenAI partner models enabled in the Google Cloud project.

### pi

`ags pi` reaches every granted provider pi can use in one launch. pi lists each under its own provider ID.

pi's own `anthropic`, `openai-codex`, `amazon-bedrock` and `google-vertex` providers go to one provider of their type: the only one, or the one chosen with `ags use pi <provider>`, which sets the provider for that type. The first launch asks once for each type with several. `-p` sets the provider of its type for one launch.

pi's other providers, including those in `models.json` when pi starts, are off during `ags pi`. Its sessions are kept in the `pi` directory of the ags directory.

## Environment variables

| Variable | Read for |
| --- | --- |
| `AGS_PROVIDER` | the provider of one launch, like `-p` |
| `AGS_BYPASS` | any value but empty or `0`: a shim runs the CLI without Aegis |
| `XDG_CONFIG_HOME` | the ags directory on Linux |
| `CLAUDE_CONFIG_DIR`, `CODEX_HOME`, `PI_CODING_AGENT_DIR` | your own CLI settings to share |
| `AWS_REGION`, `AWS_DEFAULT_REGION` | the Bedrock region, when the provider names none |
| `CLOUD_ML_REGION`, `GOOGLE_CLOUD_LOCATION` | the Vertex location, when the provider has none |

During a launch, ags points `CLAUDE_CONFIG_DIR`, `CODEX_HOME` or `PI_CODING_AGENT_DIR` at the provider's directory, and adds `127.0.0.1` and `localhost` to `NO_PROXY`.

## Files

The ags directory is:

| System | Directory |
| --- | --- |
| Linux | `$XDG_CONFIG_HOME/ags`, else `~/.config/ags` |
| macOS | `~/Library/Application Support/ags` |
| Windows | `%APPDATA%\ags` |

It holds, readable only by you:

| Path | Holds |
| --- | --- |
| `login.json` | the server and the machine's tokens |
| `defaults.json` | each CLI's remembered provider |
| `bin/` | the shims |
| `providers/<id>/` | the provider's sign-ins, Bedrock API key or Vertex service account |
| `providers/state.json` | skipped credentials and warnings already shown |
| `pi/` | pi's directory during `ags pi` |
| `gcloud/`, `pi-extension.js` | used by Codex on Vertex and by `ags pi` |

## Limits

- During `ags pi`, providers that another pi extension registers, or that are added to `models.json` while pi runs, don't go through Aegis.
- Windows has no shims and doesn't share your settings files with providers; run `ags claude` and the others directly.
- ags launches only Claude Code, Codex and pi. Configure other clients, including TypeSafe, [by hand](clients.md).
