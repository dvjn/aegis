# ags

`ags` runs Claude Code, Codex and pi through Aegis. It pairs a machine with one Aegis server, and each provider gets its own sign-in. There is nothing to configure.

## Install

GitHub releases include an `ags` tarball for Linux on `x86_64` and `aarch64`:

```sh
tag=vX.Y.Z                         # the release
target=x86_64-unknown-linux-musl   # or aarch64-unknown-linux-musl
name="ags-$tag-$target"
curl -fLO "https://github.com/dvjn/aegis/releases/download/$tag/$name.tar.gz"
curl -fLO "https://github.com/dvjn/aegis/releases/download/$tag/$name.tar.gz.sha256"
sha256sum -c "$name.tar.gz.sha256" && tar -xzf "$name.tar.gz"
install -Dm755 "$name/ags" ~/.local/bin/ags
```

On other systems, build it from source:

```sh
cargo install --locked --git https://github.com/dvjn/aegis ags
```

## Commands

| Command | Does |
| --- | --- |
| `ags login <URL>` | Pair this machine with an Aegis server |
| `ags logout` | Unpair this machine |
| `ags status` | Show the server, account and providers |
| `ags setup [provider]` | Sign in to providers that need it, or again to one |
| `ags use [cli [provider]]` | Set the provider a CLI uses, or list them |
| `ags [-p provider] <claude\|codex\|pi> [args…]` | Run the CLI through Aegis |
| `ags init <bash\|zsh\|fish>` | Print the shell line that runs plain `claude`, `codex` and `pi` through ags |

### login

```sh
ags login https://aegis.example
```

Approve the machine in the browser and choose its providers. ags then signs in to each provider in the terminal. Change a machine's providers later under **Connected apps** on your account page.

### Choosing a provider

```sh
ags -p aws claude
ags use claude aws   # from now on
```

Without `-p`, ags uses the CLI's remembered provider, or the only one that fits. When several fit, it asks once.

### init

Add the line for your shell to its startup file:

| Shell | Line |
| --- | --- |
| bash | `eval "$(ags init bash)"` |
| zsh | `eval "$(ags init zsh)"` |
| fish | `ags init fish \| source` |

Plain `claude`, `codex` and `pi` then run through ags. If a launch can't go through Aegis, ags offers a fix, then offers to run the CLI without it. `AGS_BYPASS=1 claude` skips Aegis for one run.

## Providers

| CLI | Claude subscription | ChatGPT subscription | Bedrock | Vertex |
| --- | --- | --- | --- | --- |
| `claude` | yes | | yes | yes |
| `codex` | | yes | yes | yes |
| `pi` | yes | yes | yes | yes |

Bedrock needs a Bedrock API key and Vertex a service account key file; `ags setup` asks for them. Codex on Vertex needs `gcloud` on `PATH` and OpenAI partner models enabled in the Google Cloud project.

`ags pi` reaches every granted provider in one launch, each under its own name. pi's other providers are off during it.

## Limits

- Windows has no shims; run `ags claude` and the others directly.
- pi providers added by another extension, or to `models.json` while pi runs, don't go through Aegis.
