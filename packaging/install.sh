#!/bin/sh
# Install or update quickshare-daemon from a GitHub release.
#
# For Fedora, prefer the COPR repository -- `dnf upgrade` then keeps the daemon
# current with no further effort, which is the whole update story. This script
# exists for distributions without a package, and is deliberately readable:
# read it before running it, as you should with anything piped to a shell.
#
# Usage:
#   ./install.sh                 install or update to the latest release
#   ./install.sh --uninstall     remove everything it installed
set -eu

REPO="${QUICKSHARE_REPO:-LXK-98/quickshare-daemon}"
BIN_DIR="${XDG_BIN_HOME:-$HOME/.local/bin}"
UNIT_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/systemd/user"
EXT_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/nautilus-python/extensions"
BIN="$BIN_DIR/quickshare-daemon"

say() { printf '%s\n' "$*" >&2; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

uninstall() {
    systemctl --user disable --now quickshare-daemon.service 2>/dev/null || true
    rm -f "$BIN" "$UNIT_DIR/quickshare-daemon.service" \
          "$EXT_DIR/quickshare_nautilus.py"
    systemctl --user daemon-reload 2>/dev/null || true
    say "Removed. Your config at ${XDG_CONFIG_HOME:-$HOME/.config}/quickshare-daemon/"
    say "and received files were left alone."
    exit 0
}

[ "${1:-}" = "--uninstall" ] && uninstall

for tool in curl tar sha256sum; do
    command -v "$tool" >/dev/null || die "$tool is required"
done

[ "$(uname -s)" = "Linux" ] || die "Linux only"
[ "$(uname -m)" = "x86_64" ] || die "only x86_64 release builds are published; build from source with 'cargo install --path .'"

say "Finding the latest release of $REPO..."
tag=$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" |
      sed -n 's/.*"tag_name" *: *"\([^"]*\)".*/\1/p' | head -1)
[ -n "$tag" ] || die "could not determine the latest release"

if [ -x "$BIN" ] && "$BIN" --version 2>/dev/null | grep -q "${tag#v}"; then
    say "Already on $tag."
    exit 0
fi

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
base="https://github.com/$REPO/releases/download/$tag"
archive="quickshare-daemon-$tag-x86_64-linux.tar.gz"

say "Downloading $tag..."
curl -fsSL "$base/$archive" -o "$work/$archive"
curl -fsSL "$base/$archive.sha256" -o "$work/$archive.sha256"

# Verified before anything is unpacked or run.
say "Verifying checksum..."
( cd "$work" && sha256sum -c "$archive.sha256" >/dev/null ) ||
    die "checksum mismatch -- refusing to install"

tar -xzf "$work/$archive" -C "$work"

mkdir -p "$BIN_DIR" "$UNIT_DIR"
install -m0755 "$work/quickshare-daemon" "$BIN"
# %h in the packaged unit resolves to the home directory, but a user install
# lives in ~/.local/bin rather than ~/.cargo/bin.
sed "s|^ExecStart=.*|ExecStart=$BIN|" "$work/quickshare-daemon.service" \
    > "$UNIT_DIR/quickshare-daemon.service"
systemctl --user daemon-reload 2>/dev/null || true

# Only when nautilus-python is present: without it the file does nothing, and
# installing it anyway would imply the menu works.
if [ -e /usr/lib64/nautilus/extensions-4/libnautilus-python.so ] ||
   [ -e /usr/lib/nautilus/extensions-4/libnautilus-python.so ]; then
    mkdir -p "$EXT_DIR"
    install -m0644 "$work/quickshare_nautilus.py" "$EXT_DIR/"
    say "Installed the Nautilus menu (restart Nautilus with 'nautilus -q')."
else
    say "Skipped the Nautilus menu: nautilus-python is not installed."
fi

say ""
say "Installed $tag to $BIN"
case ":$PATH:" in
    *":$BIN_DIR:"*) ;;
    *) say "Note: $BIN_DIR is not on your PATH." ;;
esac
say ""
say "Start it now:      systemctl --user start quickshare-daemon"
say "Start at login:    systemctl --user enable --now quickshare-daemon"
say "Update later:      re-run this script"
