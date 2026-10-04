#!/usr/bin/env bash
# Install or update MimeClip for the current user.
# This intentionally does not use a distribution package manager: the same
# script works on Arch, Fedora, and other systemd-based Linux desktops.

set -euo pipefail

repo_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)"
service_dir="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
service_file="$service_dir/mimeclipd.service"

if ! command -v cargo >/dev/null 2>&1; then
    echo "error: Rust Cargo is required. Install the Rust toolchain, then rerun this script." >&2
    exit 1
fi

if ! command -v systemctl >/dev/null 2>&1; then
    echo "error: systemd user services are required to run mimeclipd." >&2
    exit 1
fi

echo "Installing MimeClip to $HOME/.cargo/bin ..."
cargo install --path "$repo_dir" --locked --force

mkdir -p "$service_dir"
install -m 0644 "$repo_dir/systemd/mimeclipd.service" "$service_file"

systemctl --user daemon-reload
systemctl --user enable mimeclipd.service
systemctl --user restart mimeclipd.service

if ! "$HOME/.cargo/bin/mimeclip" ping >/dev/null; then
    echo "error: mimeclipd did not respond after installation." >&2
    echo "Check: systemctl --user status mimeclipd.service" >&2
    exit 1
fi

echo "MimeClip installed and mimeclipd is running."
