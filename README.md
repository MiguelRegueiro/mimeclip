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

## Install

Install to `~/.cargo/bin` (recommended — matches the path the service file expects):

```bash
cargo install --path . --locked
```

Or copy binaries manually to somewhere on your `$PATH`:

```bash
cargo build --release
sudo cp target/release/mimeclipd target/release/mimeclip /usr/local/bin/
```

## Running as a systemd user service

A service file is included at `systemd/mimeclipd.service`. It assumes the binary is at `~/.cargo/bin/mimeclipd` (the default for `cargo install`). Edit `ExecStart` if you installed elsewhere.

```bash
mkdir -p ~/.config/systemd/user
cp systemd/mimeclipd.service ~/.config/systemd/user/
systemctl --user enable --now mimeclipd
```

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
mimeclip clear                       wipe all history
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

No config file. Behavior is controlled by environment variables:

| Variable | Default | Effect |
|---|---|---|
| `RUST_LOG` | `info` | Log level (`error`, `warn`, `info`, `debug`) |
| `XDG_RUNTIME_DIR` | `/tmp` | Socket location |
| `XDG_DATA_HOME` | `~/.local/share` | Database location |
| `MIMECLIP_MAX_ENTRIES` | `500` | Maximum entries kept; oldest are pruned automatically |

## Privacy and security

mimeclip records everything that passes through the clipboard: passwords, authentication tokens, private messages, file contents, screenshots, and anything else you copy. Be aware of the following:

- **Storage is unencrypted.** The SQLite database at `~/.local/share/mimeclip/history.db` is a plain file. Anyone with read access to your home directory can read your clipboard history.
- **Password manager copies are captured.** Most password managers clear the clipboard after a short timeout, but mimeclip will have already stored the entry. Delete it manually with `mimeclip delete <id>` or pause the daemon before copying secrets.
- **History is persistent across reboots.** Entries remain until explicitly deleted or until the `MIMECLIP_MAX_ENTRIES` limit is reached and they are pushed out.

To delete a specific entry:

```bash
mimeclip delete <id>
```

To wipe all history:

```bash
mimeclip clear
```

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
