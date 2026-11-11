#!/bin/sh
set -eu
server=@SERVER@
case "$server" in
  https://*|http://127.0.0.1|http://127.0.0.1:*|http://localhost|http://localhost:*|http://\[::1\]|http://\[::1\]:*) ;;
  *) echo 'ags installation requires HTTPS (HTTP is allowed on loopback only).' >&2; exit 1 ;;
esac
command -v mise >/dev/null 2>&1 || { echo 'Install mise first: https://mise.jdx.dev' >&2; exit 1; }
case "$(uname -s):$(uname -m)" in
  Linux:x86_64) platform=linux-x64 ;;
  Linux:aarch64|Linux:arm64) platform=linux-arm64 ;;
  *) echo 'ags currently supports Linux x86_64 and ARM64.' >&2; exit 1 ;;
esac
version=$(curl -fsS --proto '=http,https' "$server/downloads/ags/version")
case "$version" in ''|*[!0-9a-zA-Z.+-]*) echo 'Invalid server release version.' >&2; exit 1 ;; esac
base="$server/downloads/ags/{{version}}/{{os()}}-{{arch()}}"
checksum_file=$(curl -fsS --proto '=http,https' "$server/downloads/ags/$version/$platform/ags.sha256") || {
  echo "ags download unavailable for $platform (server $version). The server operator must publish the binary; for local development, run mise run ags:stage." >&2
  exit 1
}
checksum=$(printf '%s\n' "$checksum_file" | awk 'NR == 1 {print $1}')
case "$checksum" in ''|*[!0-9a-fA-F]*) echo 'Invalid server checksum.' >&2; exit 1 ;; esac
[ "${#checksum}" -eq 64 ] || { echo 'Invalid server checksum.' >&2; exit 1; }
options="url=$base/ags,version_list_url=$server/downloads/ags/version,checksum_url=$base/ags.sha256,format=raw,bin=ags"
tool="http:ags[$options]@$version"
verified_tool="http:ags[$options,checksum=sha256:$checksum]@$version"
mise install --force "$verified_tool"
[ "$(mise exec "$verified_tool" -- ags --version)" = "ags $version" ] || { echo 'Installed ags version does not match this server.' >&2; exit 1; }
mise use --global "$tool"
# curl | sh leaves stdin attached to the script pipe; the wizard needs the TTY.
if ( : </dev/tty ) 2>/dev/null; then
  exec mise exec "$tool" -- ags setup "$server" "$@" </dev/tty
fi
exec mise exec "$tool" -- ags setup "$server" "$@"
