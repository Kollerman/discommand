#!/bin/sh
# Builds discommand and its WhatsApp helper and installs both into ~/.cargo/bin.
# Re-run after changes to update.
set -e
cd "$(dirname "$0")"
bin="${CARGO_HOME:-$HOME/.cargo}/bin"

if [ -f keys/keys.env ]; then
    echo "Building with keys/keys.env (one-step Google/Microsoft sign-in)."
else
    echo "No keys/keys.env: building without built-in Google/Microsoft app IDs (see keys/README.md)."
fi

# Pure-Rust TLS on Termux or when OpenSSL headers are missing.
features=""
if [ -n "$TERMUX_VERSION" ] || ! pkg-config --exists openssl 2>/dev/null; then
    features="--no-default-features --features rustls"
fi
# shellcheck disable=SC2086
cargo install --path . --force $features

(cd wa && CGO_ENABLED=0 go build -trimpath -ldflags "-s -w" -o "$bin/discommand-wa" .)
echo "Installed discommand and discommand-wa to $bin"
