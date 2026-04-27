# mimeclip

MIME-aware clipboard history daemon for Wayland.

Most clipboard managers store one "best" representation per entry — usually the plain-text preview. This means copying a file and copying its path produce the same-looking history entry, overwriting each other, and restore pastes the wrong thing.

mimeclip stores **every MIME type offered** per clipboard event and restores them all simultaneously. Copying a file and copying its path are separate entries with distinct icons and distinct paste behavior.

## Requirements

- Wayland compositor with `zwlr_data_control_manager_v1` support (Hyprland, Sway, and most wlroots-based compositors)
- Rust toolchain to build

## Build

```bash
git clone <repo>
cd mimeclip
cargo build --release
```

Binaries land in `target/release/`:
- `mimeclipd` — the daemon
- `mimeclip` — the CLI client

## Install

Copy binaries to somewhere on your `$PATH`:

```bash
sudo cp target/release/mimeclipd target/release/mimeclip /usr/local/bin/
```

Or symlink from the build directory if you want to `cargo build --release` to update in place.

## Running as a systemd user service

A service file is included at `.config/systemd/user/mimeclipd.service`. Copy it:

```bash
cp .config/systemd/user/mimeclipd.service ~/.config/systemd/user/
# Edit ExecStart path if you installed the binary elsewhere
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

## Quickshell / UI integration

The daemon exposes a Unix domain socket at `$XDG_RUNTIME_DIR/mimeclipd.sock`. Send newline-terminated JSON requests, receive newline-terminated JSON responses.

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

### Why cliphist loses data

A Wayland clipboard offer can advertise many MIME types at once. Copying a file from Nautilus typically offers:

```
x-special/gnome-copied-files
text/uri-list
text/plain
```

cliphist reads one representation (usually `text/plain`) and stores that. Copying the literal file path also produces a `text/plain` entry with the same content, so the two entries hash to the same value and one overwrites the other. After restore, the file MIME types are gone and paste behaves like plain text.

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

## Limitations

- Primary clipboard only (not the selection/middle-click buffer).
- Entries over 64 MiB are skipped.
- `application/vnd.portal.filetransfer` and similar portal session MIME types are filtered out — they represent live transfer sessions that cannot be replayed from stored bytes.
- The compositor must support `zwlr_data_control_manager_v1`. GNOME/Mutter does not; this tool targets Hyprland, Sway, and other wlroots compositors.
