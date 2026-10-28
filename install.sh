#!/bin/sh
# Builds discommand and its WhatsApp helper and installs both into ~/.cargo/bin.
# Works on Linux, macOS and Termux. Re-run after changes to update.
set -e
cd "$(dirname "$0")"
for tool in cargo go; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        echo "$tool not found. Install Rust (https://rustup.rs) and Go (https://go.dev/dl), then open a new terminal." >&2
        exit 1
    fi
done
bin="${CARGO_HOME:-$HOME/.cargo}/bin"

if [ -f keys/keys.env ]; then
    echo "Building with keys/keys.env (one-step Google/Microsoft sign-in)."
else
    echo "No keys/keys.env: building without built-in Google/Microsoft app IDs (see keys/README.md)."
fi

# The default TLS is the OS's own (Security.framework on macOS, OpenSSL on
# Linux). Use pure-Rust TLS on Termux or when OpenSSL headers are missing.
features=""
if [ "$(uname -s)" != Darwin ]; then
    if [ -n "$TERMUX_VERSION" ] || ! pkg-config --exists openssl 2>/dev/null; then
        features="--no-default-features --features rustls"
    fi
fi
# shellcheck disable=SC2086
cargo install --path . --force $features

mkdir -p "$bin"
(cd wa && CGO_ENABLED=0 go build -trimpath -ldflags "-s -w" -o "$bin/discommand-wa" .)
echo "Installed discommand and discommand-wa to $bin"
case ":$PATH:" in
    *":$bin:"*) ;;
    *) echo "Add $bin to your PATH (or open a new terminal) to run discommand." ;;
esac
