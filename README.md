# mimeclip

MIME-aware clipboard history daemon for Wayland.

Most clipboard managers store one "best" representation per entry — usually the plain-text preview. This can make copying a file and copying its path produce the same-looking history entry; in some setups they collapse into one entry, and restore pastes the wrong thing.

mimeclip stores **every MIME type offered** per clipboard event and restores them all simultaneously. Copying a file and copying its path are separate entries with distinct icons and distinct paste behavior.

## Why

Copying `notes.txt` from a file manager and copying its path as text can look identical in many clipboard history tools:

```
~/documents/notes.txt
~/documents/notes.txt
```

They may even collapse into one entry. On restore, the file MIME types are gone — paste in a file manager gives a path string instead of a file.

mimeclip hashes the full set of MIME types and payloads, so these are always two distinct entries:

```
 42  file   2026-04-28 10:14:05  notes.txt
 41  text   2026-04-28 10:13:52  ~/documents/notes.txt
```

Restoring the `file` entry offers all three MIME types (`x-special/gnome-copied-files`, `text/uri-list`, `text/plain`) — paste in Nautilus or Dolphin creates a real file paste operation instead of inserting a path string. Restoring the `text` entry offers only plain text.

## Requirements

- Wayland compositor with `zwlr_data_control_manager_v1` support (Hyprland, Sway, and most wlroots-based compositors)
- Rust toolchain to build

## Build

```bash
git clone https://github.com/MiguelRegueiro/mimeclip
cd mimeclip
cargo build --release
```

Binaries land in `target/release/`:
- `mimeclipd` — the daemon
- `mimeclip` — the CLI client

## Install or update

From a checkout, run one command:

```bash
./scripts/install.sh
```

It installs both binaries with Cargo to `~/.cargo/bin`, installs/updates the
current user's `mimeclipd` systemd service, starts it, and verifies it responds.
It uses neither `sudo` nor a distribution package manager, so the same command
works on Arch and Fedora. It requires a Rust toolchain and a systemd user
session.

## Running as a systemd user service

`scripts/install.sh` handles this automatically. The installed service is at
`~/.config/systemd/user/mimeclipd.service` and runs
`~/.cargo/bin/mimeclipd`.

Check it started:

```bash
mimeclip ping   # → pong
```

## CLI

```
mimeclip list [--limit N] [--json]   list history, newest first
mimeclip restore <id>                restore entry to clipboard (all MIME types)
mimeclip delete <id>                 remove an entry
mimeclip decode <id>                 dump all MIME payloads as base64 JSON
mimeclip clear                       wipe all history and compact its database
mimeclip ping                        check daemon is alive
```

`list` default limit is 50. `--json` emits the full entry array for use in scripts and UIs.

## IPC / UI integration

For frontends such as Quickshell, the daemon exposes a Unix domain socket at `$XDG_RUNTIME_DIR/mimeclipd.sock`. Send newline-terminated JSON requests, receive newline-terminated JSON responses.

**Request format:**

```json
{ "cmd": "list",    "limit": 100 }
{ "cmd": "restore", "id": 42 }
{ "cmd": "delete",  "id": 42 }
{ "cmd": "decode",  "id": 42 }
{ "cmd": "clear" }
{ "cmd": "ping" }
```

**List response:**

```json
{
  "status": "list",
  "entries": [
    {
      "id": 7,
      "hash": "a3f9...",
      "kind": "file",
      "label": "mouse.rs",
      "preview": "mouse.rs",
      "size": 1024,
      "timestamp": "2025-04-27T18:00:00Z",
      "mime_types": [
        "x-special/gnome-copied-files",
        "text/uri-list",
        "text/plain"
      ]
    }
  ]
}
```

The `kind` field is one of `text`, `uri`, `file`, `image`, or `other` — use it to show the right icon. A `file` entry and a `text` entry can coexist even when their `label` looks identical, because they hash differently (different MIME type sets).

**Restore response:** `{ "status": "ok" }`

**Error response:** `{ "status": "error", "message": "..." }`

## How it works

### The MIME type problem in detail

A Wayland clipboard offer can advertise many MIME types at once. Copying a file from Nautilus typically offers:

```
x-special/gnome-copied-files
text/uri-list
text/plain
```

In common `wl-paste --watch cliphist store` setups, the history backend receives one selected representation — often `text/plain` — instead of the full MIME offer. Copying the literal file path produces a `text/plain` entry with the same content, so the two entries hash to the same value and one overwrites the other. After restore, the file MIME types are gone and paste behaves like plain text.

### What mimeclip does instead

1. On every clipboard change, enumerate all offered MIME types via `zwlr_data_control_manager_v1`.
2. Read the raw bytes for each MIME type through a pipe.
3. Hash the full set of `(mime_type, bytes)` pairs — order-independent. Two entries share a hash only if they offer exactly the same MIME types with exactly the same content.
4. Store all payloads as blobs in SQLite.
5. On restore, create a new Wayland data source that offers all stored MIME types and serves each one from the stored bytes until the source is cancelled.

### Database location

`$XDG_DATA_HOME/mimeclip/history.db` (defaults to `~/.local/share/mimeclip/history.db`)

## Configuration

Run this in a terminal for a small interactive setup:

```bash
mimeclip config
```

It persists settings in `$XDG_CONFIG_HOME/mimeclip/config.toml` (normally
`~/.config/mimeclip/config.toml`) and applies them to the running daemon
immediately. The default limits are **256 MiB total payload data** and **500
entries**. The daemon checks limits only after storing a new clipboard entry;
there is no timer or background maintenance process. When a limit is exceeded,
the least recently used entries are removed. SQLite reuses that space normally;
only `mimeclip clear` performs the heavier physical compaction.

For viewing or scripting:

```bash
mimeclip config show
mimeclip config set max-history-size 512MiB
mimeclip config set max-entries 750
```

The total-size limit counts stored clipboard payload bytes, rather than the
SQLite file size (which can temporarily include reusable pages and WAL data).

Environment variables remain available for service setups and override the
saved file:

| Variable | Default | Effect |
|---|---|---|
| `RUST_LOG` | `info` | Log level (`error`, `warn`, `info`, `debug`) |
| `XDG_RUNTIME_DIR` | `/tmp` | Socket location |
| `XDG_DATA_HOME` | `~/.local/share` | Database location |
| `MIMECLIP_MAX_ENTRIES` | saved setting | Maximum entries kept; least recently used entries are pruned automatically |
| `MIMECLIP_MAX_HISTORY_SIZE` | saved setting | Total stored payload limit, e.g. `256MiB` |

## Privacy and security

mimeclip records clipboard history locally. Be aware of the following:

- **Owner-only local storage.** The database directory is set to `0700`; the SQLite database and its WAL/SHM sidecars are set to `0600` whenever the daemon starts. Other local user accounts cannot read history through normal filesystem access.
- **Storage is not encrypted.** The SQLite database at `~/.local/share/mimeclip/history.db` remains plaintext to your own account and to any process that can act as it. Use full-disk encryption to protect a powered-off machine or its backups.
- **Password-manager hints are honored.** A clipboard selection offering the KDE-standard `x-kde-passwordManagerHint` MIME type is treated as sensitive and discarded before mimeclip requests or stores any payload. KeePassXC and other password managers commonly use this hint.
- **Unmarked secrets are still captured.** No clipboard manager can safely infer that arbitrary text is a password. If an application does not mark its clipboard data sensitive, pause mimeclip before copying the secret or clear/delete it afterward.
- **History is persistent across reboots.** Entries remain until explicitly deleted or until the configured count or total-size limit is reached and they are pushed out.

To delete a specific entry:

```bash
mimeclip delete <id>
```

To wipe all history:

```bash
mimeclip clear
```

Clearing checkpoints the WAL and vacuums the SQLite database, reclaiming space
from deleted image and blob payloads. This runs only for an explicit full clear,
not during ordinary clipboard operations. It is not a secure erase: storage
media, filesystem snapshots, or SSD wear-leveling may retain old bytes.

To stop recording temporarily:

```bash
systemctl --user stop mimeclipd
```

## Limitations

- Primary clipboard only (not the selection/middle-click buffer).
- Entries over 64 MiB are skipped.
- `application/vnd.portal.filetransfer` and similar portal session MIME types are filtered out — they represent live transfer sessions that cannot be replayed from stored bytes.
- The compositor must support `zwlr_data_control_manager_v1`. GNOME/Mutter does not; this tool targets Hyprland, Sway, and other wlroots compositors.
- mimeclip is early software. Expect rough edges.
