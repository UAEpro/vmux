#!/bin/sh
# vmux installer — download the latest prebuilt static binary from GitHub.
#
#   curl -fsSL https://raw.githubusercontent.com/UAEpro/vmux/main/install.sh | sh
#
# Override the install directory with VMUX_INSTALL_DIR (default: ~/.local/bin).
set -eu

REPO="UAEpro/vmux"
BIN="vmux"
: "${VMUX_INSTALL_DIR:=${HOME}/.local/bin}"

fail() { printf 'install: %s\n' "$1" >&2; exit 1; }

[ "$(uname -s)" = "Linux" ] || fail "vmux currently supports Linux only (got $(uname -s))"

case "$(uname -m)" in
  x86_64 | amd64) target="x86_64-unknown-linux-musl" ;;
  *) fail "no prebuilt binary for '$(uname -m)' yet — build from source: cargo install --git https://github.com/${REPO}" ;;
esac

command -v curl >/dev/null 2>&1 || fail "curl is required"
command -v tar >/dev/null 2>&1 || fail "tar is required"

# Resolve the latest published release tag. Capture the full response first so
# the JSON parse can't SIGPIPE curl mid-write.
api="$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest")" \
  || fail "could not reach the GitHub releases API"
tag="$(printf '%s' "$api" | grep '"tag_name"' | head -n1 \
  | sed -E 's/.*"tag_name":[[:space:]]*"([^"]+)".*/\1/')"
[ -n "$tag" ] || fail "could not find a published release yet"
version="${tag#v}"

name="${BIN}-${version}-${target}"
url="https://github.com/${REPO}/releases/download/${tag}/${name}.tar.gz"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

printf 'Downloading %s %s...\n' "$BIN" "$tag"
curl -fsSL "$url" -o "$tmp/${name}.tar.gz" || fail "download failed: $url"

# Verify the checksum when the release ships one.
if curl -fsSL "${url}.sha256" -o "$tmp/${name}.tar.gz.sha256" 2>/dev/null; then
  (cd "$tmp" && sha256sum -c "${name}.tar.gz.sha256" >/dev/null 2>&1) \
    || fail "checksum verification failed"
fi

tar -C "$tmp" -xzf "$tmp/${name}.tar.gz"
mkdir -p "$VMUX_INSTALL_DIR"
install -m 0755 "$tmp/${name}/${BIN}" "${VMUX_INSTALL_DIR}/${BIN}"

printf 'Installed %s to %s/%s\n' "$BIN" "$VMUX_INSTALL_DIR" "$BIN"
case ":${PATH}:" in
  *":${VMUX_INSTALL_DIR}:"*) ;;
  *) printf 'Note: %s is not on your PATH. Add this to your shell profile:\n  export PATH="%s:$PATH"\n' "$VMUX_INSTALL_DIR" "$VMUX_INSTALL_DIR" ;;
esac
