#!/usr/bin/env bash
# Install pw, with its recorder (`pw record`) running as a service.
#
# Usage: ./install.sh [--system | --prefix DIR] [--no-service]
#   --system      Install to /usr/local/bin (with sudo); default ~/.local/bin
#   --prefix DIR  Install to DIR/bin
#   --no-service  Install the binary only; do not start the recorder
#
# From a release archive it installs the pw beside it; from a source checkout
# it builds pw first, which needs cargo. The recorder runs as a systemd user
# service on Linux and a launchd agent on macOS; elsewhere the script prints
# how to start it.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

die() { echo "error: $*" >&2; exit 1; }
usage() { sed -n '2,12p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; }

prefix="$HOME/.local"
sudo=""
service=1
while [ "$#" -gt 0 ]; do
    case "$1" in
        --system) prefix=/usr/local; sudo=sudo ;;
        --prefix)
            [ "$#" -ge 2 ] || die "--prefix needs a folder"
            prefix="$2"
            shift
            ;;
        --no-service) service=0 ;;
        -h | --help) usage; exit 0 ;;
        *) die "unknown argument: $1" ;;
    esac
    shift
done

os="$(uname -s)"
case "$os" in
    Linux | FreeBSD | Darwin) ;;
    *) die "unsupported OS: $os (on Windows, see README.md)" ;;
esac

# The pw to install: the one a release archive carries, or a fresh build.
if [ -f "$here/pw" ] && [ ! -f "$here/Cargo.toml" ]; then
    built="$here/pw"
else
    command -v cargo >/dev/null 2>&1 || die "cargo is not on the PATH; install Rust (https://rustup.rs)"
    echo "Building pw with $(cargo --version)..."
    cargo build --release --locked --manifest-path "$here/Cargo.toml"
    built="$here/target/release/pw"
fi
[ -x "$built" ] || die "no pw to install at $built"

bindir="$prefix/bin"
$sudo install -d "$bindir"
# Replacing the file instead of rewriting it in place leaves a running
# recorder on the old binary until the service restarts it below.
$sudo install -m 0755 "$built" "$bindir/pw.new"
$sudo mv -f "$bindir/pw.new" "$bindir/pw"
if [ "$os" = Darwin ]; then
    # A binary unpacked from a download is quarantined, and Gatekeeper
    # refuses to run an unsigned one; this copy came from a trusted install.
    $sudo xattr -c "$bindir/pw"
fi
echo "Installed $bindir/pw"
case ":${PATH}:" in
    *":$bindir:"*) ;;
    *) echo "Note: $bindir is not in PATH; add it to your shell profile." ;;
esac

[ "$service" -eq 1 ] || exit 0

systemd_service() {
    local unit_dir="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
    install -d "$unit_dir"
    sed "s|^ExecStart=.*|ExecStart=$bindir/pw record|" \
        "$here/contrib/systemd/process-watch.service" > "$unit_dir/process-watch.service"
    systemctl --user daemon-reload
    systemctl --user enable process-watch.service >/dev/null
    systemctl --user restart process-watch.service
    echo "Recorder running: systemctl --user status process-watch"
}

launchd_agent() {
    local agent="$HOME/Library/LaunchAgents/local.process-watch.plist"
    install -d "$(dirname "$agent")"
    sed "s|<string>/usr/local/bin/pw</string>|<string>$bindir/pw</string>|" \
        "$here/contrib/launchd/local.process-watch.plist" > "$agent"
    if launchctl print "gui/$(id -u)/local.process-watch" >/dev/null 2>&1; then
        launchctl bootout "gui/$(id -u)" "$agent"
    fi
    launchctl bootstrap "gui/$(id -u)" "$agent"
    echo "Recorder running: launchctl print gui/$(id -u)/local.process-watch"
}

case "$os" in
    Linux)
        if command -v systemctl >/dev/null 2>&1 && systemctl --user show-environment >/dev/null 2>&1; then
            systemd_service
            exit 0
        fi
        ;;
    Darwin)
        launchd_agent
        exit 0
        ;;
esac

echo "No service manager to run the recorder; start it at login with this crontab line:"
echo "  @reboot $bindir/pw record"
