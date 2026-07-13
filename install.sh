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

os="$(uname -s)"
arch="$(uname -m)"

# Linux binaries are static (musl), so they run on any distro. Windows is not
# supported at all: vmux is built on Unix domain sockets, fork/setsid and POSIX
# signals, so there is nothing to install there yet.
case "${os}/${arch}" in
  Linux/x86_64 | Linux/amd64)   target="x86_64-unknown-linux-musl" ;;
  Linux/aarch64 | Linux/arm64)  target="aarch64-unknown-linux-musl" ;;
  Darwin/arm64)                 target="aarch64-apple-darwin" ;;
  Darwin/x86_64)                target="x86_64-apple-darwin" ;;
  Linux/* | Darwin/*)
    fail "no prebuilt binary for ${os} ${arch} yet — build from source: cargo install vmux-tui" ;;
  *)
    fail "vmux supports Linux and macOS (got ${os})" ;;
esac

command -v curl >/dev/null 2>&1 || fail "curl is required"
command -v tar >/dev/null 2>&1 || fail "tar is required"

# Checksum verification is mandatory, so resolve the tool up front rather than
# discovering it is missing after the download.
if command -v sha256sum >/dev/null 2>&1; then
  sha256_check() { sha256sum -c "$1"; }
elif command -v shasum >/dev/null 2>&1; then
  sha256_check() { shasum -a 256 -c "$1"; }
else
  fail "sha256sum (or shasum) is required to verify the download"
fi

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

# Every release ships a .sha256 alongside the tarball, so a missing or
# unreadable one means something is wrong with the download — never install
# unverified. Failing closed is the whole point of checksumming a `curl | sh`.
curl -fsSL "${url}.sha256" -o "$tmp/${name}.tar.gz.sha256" \
  || fail "could not fetch the checksum: ${url}.sha256"
(cd "$tmp" && sha256_check "${name}.tar.gz.sha256" >/dev/null 2>&1) \
  || fail "checksum verification failed — refusing to install ${name}.tar.gz"

tar -C "$tmp" -xzf "$tmp/${name}.tar.gz"
mkdir -p "$VMUX_INSTALL_DIR"
install -m 0755 "$tmp/${name}/${BIN}" "${VMUX_INSTALL_DIR}/${BIN}"

printf 'Installed %s to %s/%s\n' "$BIN" "$VMUX_INSTALL_DIR" "$BIN"
case ":${PATH}:" in
  *":${VMUX_INSTALL_DIR}:"*) ;;
  *) printf 'Note: %s is not on your PATH. Add this to your shell profile:\n  export PATH="%s:$PATH"\n' "$VMUX_INSTALL_DIR" "$VMUX_INSTALL_DIR" ;;
esac
