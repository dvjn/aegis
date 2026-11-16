# Client Setup

The Aegis client, `ags`, runs Claude Code, Codex and pi through Aegis. It pairs
a machine with one Aegis server, and each provider gets its own sign-in. There
is nothing to configure.

## Install

```sh
mise use -g "github:dvjn/aegis[matching=ags-]"
```

<details>
<summary>Without mise</summary>

From the GitHub release, for Linux on `x86_64` and `aarch64`:

```sh
tag=vX.Y.Z                         # the release
target=x86_64-unknown-linux-musl   # or aarch64-unknown-linux-musl
name="ags-$tag-$target"
curl -fLO "https://github.com/dvjn/aegis/releases/download/$tag/$name.tar.gz"
curl -fLO "https://github.com/dvjn/aegis/releases/download/$tag/$name.tar.gz.sha256"
sha256sum -c "$name.tar.gz.sha256" && tar -xzf "$name.tar.gz"
install -Dm755 "$name/ags" ~/.local/bin/ags
```

From source, on any system:

```sh
cargo install --locked --git https://github.com/dvjn/aegis ags
```

</details>

## Set up

1. Pair this machine with your Aegis server:

   ```sh
   ags login https://aegis.example
   ```

   Approve the machine in the browser and choose its providers. ags then signs in to each provider in the terminal.

2. Add the line for your shell to its startup file, then open a new shell:

   ```sh
   eval "$(ags init bash)"    # ~/.bashrc
   eval "$(ags init zsh)"     # ~/.zshrc
   ags init fish | source     # ~/.config/fish/config.fish
   ```

3. Run the CLI as usual:

   ```sh
   claude
   ```

   When several providers fit, ags asks once which to use. Change it with `ags use claude <provider>`, or pick one for a single run with `ags -p <provider> claude`.

`AGS_BYPASS=1 claude` runs the CLI without Aegis. `ags help` lists every command.

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
