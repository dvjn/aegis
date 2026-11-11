#!/bin/sh
set -eu
server=@SERVER@
case "$server" in
  https://*|http://127.0.0.1|http://127.0.0.1:*|http://localhost|http://localhost:*|http://\[::1\]|http://\[::1\]:*) ;;
  *) echo 'AGS installation requires HTTPS (HTTP is allowed on loopback only).' >&2; exit 1 ;;
esac
case "$(uname -s):$(uname -m)" in
  Linux:x86_64) target=x86_64-unknown-linux-musl ;;
  Linux:aarch64|Linux:arm64) target=aarch64-unknown-linux-musl ;;
  *) echo 'AGS currently supports Linux x86_64 and ARM64.' >&2; exit 1 ;;
esac
ags_home="${AGS_HOME:-${XDG_DATA_HOME:-$HOME/.local/share}/ags}"
mkdir -p "$ags_home/bin"
chmod 700 "$ags_home"
if [ -e "$ags_home/bin/ags" ] && [ ! -f "$ags_home/.installed" ]; then
  echo 'Refusing to overwrite an unmanaged AGS installation.' >&2; exit 1
fi
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT HUP INT TERM
version=$(curl -fsS --proto '=http,https' "$server/downloads/ags/version")
case "$version" in ''|*[!0-9a-zA-Z.+-]*) echo 'Invalid server release version.' >&2; exit 1 ;; esac
curl -fsS --proto '=http,https' "$server/downloads/ags/$version/$target/ags" -o "$tmp/ags"
curl -fsS --proto '=http,https' "$server/downloads/ags/$version/$target/ags.sha256" -o "$tmp/ags.sha256"
(cd "$tmp" && sha256sum -c ags.sha256)
chmod 700 "$tmp/ags"
[ "$("$tmp/ags" --version)" = "ags $version" ] || { echo 'Relay artifact version does not match this server.' >&2; exit 1; }
install -m 700 "$tmp/ags" "$ags_home/bin/ags.next"
mv "$ags_home/bin/ags.next" "$ags_home/bin/ags"
touch "$ags_home/.installed"
rm -rf "$tmp"
trap - EXIT HUP INT TERM
if ( : </dev/tty ) 2>/dev/null; then
  exec "$ags_home/bin/ags" setup "$server" "$@" </dev/tty
fi
exec "$ags_home/bin/ags" setup "$server" "$@"
