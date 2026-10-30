#!/bin/sh
# Builds discommand and its WhatsApp helper and installs both into ~/.cargo/bin.
# Works on Linux, macOS and Termux. Re-run after changes to update.
#
#   sh install.sh                          asks which integrations to build
#   sh install.sh --with discord,calendar  only these: discord, whatsapp, matrix, calendar, todo, twos, moodle
#   sh install.sh --with all               back to all of them
set -e
cd "$(dirname "$0")"

# Every integration (the default features in Cargo.toml).
all="discord,whatsapp,matrix,calendar,todo,twos,moodle"
with=""
while [ $# -gt 0 ]; do
    case "$1" in
        --with) with="$2"; shift 2 ;;
        --with=*) with="${1#--with=}"; shift ;;
        -h|--help) sed -n '2,7p' "$0"; exit 0 ;;
        *) echo "unknown option: $1 (see --help)" >&2; exit 2 ;;
    esac
done
with=$(printf '%s' "$with" | tr 'A-Z ' 'a-z,' | tr -s ',' | sed 's/^,//; s/,$//')
ask() {
    printf '%s [Y/n] ' "$1"
    read -r answer || answer=n
    case "$answer" in ""|[Yy]*|[Jj]*) return 0 ;; *) return 1 ;; esac
}
# The choice is remembered in .integrations (not in git) for later updates.
last=""
[ -f .integrations ] && last=$(cat .integrations)
if [ -z "$with" ]; then
    if [ ! -t 0 ]; then
        with="$last"
    elif [ -n "$last" ] && ask "Build with the same integrations as last time ($last)?"; then
        with="$last"
    else
        while [ -z "$with" ]; do
            for name in discord whatsapp matrix calendar todo twos moodle; do
                case $name in
                    discord) label="Discord" ;;
                    whatsapp) label="WhatsApp (needs Go)" ;;
                    matrix) label="Matrix (adds a few minutes to the build)" ;;
                    calendar) label="Google Calendar" ;;
                    todo) label="Microsoft To Do" ;;
                    twos) label="Twos" ;;
                    moodle) label="Moodle (university / school)" ;;
                esac
                ask "  $label" && with="${with:+$with,}$name"
            done
            [ -z "$with" ] && echo "Choose at least one."
        done
        [ "$with" = "$all" ] && with=all
    fi
fi
if [ "$with" = all ]; then
    rm -f .integrations
    with=""
fi
case ",${with:-$all}," in *,whatsapp,*) whatsapp=1 ;; *) whatsapp="" ;; esac

for tool in cargo ${whatsapp:+go}; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        echo "$tool not found. Install Rust (https://rustup.rs) and Go (https://go.dev/dl), then open a new terminal." >&2
        exit 1
    fi
done
bin="${CARGO_HOME:-$HOME/.cargo}/bin"

if [ -n "$with" ]; then
    echo "Integrations: $with"
else
    echo "Integrations: all (choose with --with discord,whatsapp,matrix,calendar,todo,twos,moodle)"
fi
if [ -f keys/keys.env ]; then
    echo "Building with keys/keys.env (one-step Google/Microsoft sign-in)."
else
    echo "No keys/keys.env: building without built-in Google/Microsoft app IDs (see keys/README.md)."
fi

# The default TLS is the OS's own (Security.framework on macOS, OpenSSL on
# Linux). Use pure-Rust TLS on Termux or when OpenSSL headers are missing.
tls=native-tls
if [ "$(uname -s)" != Darwin ]; then
    if [ -n "$TERMUX_VERSION" ] || ! pkg-config --exists openssl 2>/dev/null; then
        tls=rustls
    fi
fi
cargo install --path . --force --no-default-features --features "$tls,${with:-$all}"
if [ -n "$with" ]; then echo "$with" > .integrations; else rm -f .integrations; fi

if [ -n "$whatsapp" ]; then
    mkdir -p "$bin"
    (cd wa && CGO_ENABLED=0 go build -trimpath -ldflags "-s -w" -o "$bin/discommand-wa" .)
    echo "Installed discommand and discommand-wa to $bin"
else
    echo "Installed discommand to $bin"
fi
case ":$PATH:" in
    *":$bin:"*) ;;
    *) echo "Add $bin to your PATH (or open a new terminal) to run discommand." ;;
esac
