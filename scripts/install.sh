#!/bin/sh
# Install or update MimeClip for the current user.
# This intentionally does not use a distribution package manager or init system.

set -eu

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)
repo_dir=$(CDPATH= cd -- "$script_dir/.." && pwd -P)

if ! command -v cargo >/dev/null 2>&1; then
    echo "error: Rust Cargo is required. Install the Rust toolchain, then rerun this script." >&2
    exit 1
fi

echo "Installing MimeClip to $HOME/.cargo/bin ..."
cargo install --path "$repo_dir" --locked --force

echo "MimeClip installed. Start mimeclipd from your compositor session."
