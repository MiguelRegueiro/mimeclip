use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection};

use crate::common::types::{Entry, EntryKind, MimePayload};

const DEFAULT_MAX_ENTRIES: usize = 500;

pub struct Database {
    conn: Connection,
    max_entries: usize,
    has_legacy_timestamp: bool,
}

pub struct NewEntry<'a> {
    pub hash: &'a str,
    pub kind: &'a EntryKind,
    pub label: &'a str,
    pub preview: &'a str,
    pub size: usize,
    pub created_at: DateTime<Utc>,
    pub payloads: &'a [(String, Vec<u8>)],
}

impl Database {
    pub fn open(path: &std::path::Path) -> Result<Self> {
        let conn = Connection::open(path)
            .with_context(|| format!("opening database at {}", path.display()))?;
        let max_entries = std::env::var("MIMECLIP_MAX_ENTRIES")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_MAX_ENTRIES);
        let mut db = Self {
            conn,
            max_entries,
            has_legacy_timestamp: false,
        };
        db.init()?;
        db.has_legacy_timestamp = db.entries_column_exists("timestamp")?;
        Ok(db)
    }

    fn init(&self) -> Result<()> {
        self.conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA foreign_keys=ON;

             CREATE TABLE IF NOT EXISTS entries (
                 id           INTEGER PRIMARY KEY AUTOINCREMENT,
                 hash         TEXT    NOT NULL UNIQUE,
                 kind         TEXT    NOT NULL,
                 label        TEXT    NOT NULL,
                 preview      TEXT    NOT NULL,
                 size         INTEGER NOT NULL,
                 created_at   TEXT    NOT NULL,
                 last_used_at TEXT    NOT NULL
             );

             CREATE TABLE IF NOT EXISTS payloads (
                 id        INTEGER PRIMARY KEY AUTOINCREMENT,
                 entry_id  INTEGER NOT NULL REFERENCES entries(id) ON DELETE CASCADE,
                 mime_type TEXT    NOT NULL,
                 data      BLOB    NOT NULL
             );

             CREATE INDEX IF NOT EXISTS idx_payloads_entry ON payloads(entry_id);",
        )?;
        self.migrate_entries_schema()?;
        self.conn.execute_batch(
            "DROP INDEX IF EXISTS idx_entries_ts;
             CREATE INDEX IF NOT EXISTS idx_entries_last_used_at
             ON entries(last_used_at DESC);",
        )?;
        Ok(())
    }

    /// Insert a new entry. Returns the new row id, or the existing id if hash already exists.
    pub fn insert(&self, entry: NewEntry<'_>) -> Result<i64> {
        let NewEntry {
            hash,
            kind,
            label,
            preview,
            size,
            created_at,
            payloads,
        } = entry;
        let created_at = created_at.to_rfc3339();

        // If hash already exists, keep its original creation time and update recency.
        if let Some(id) = self.find_by_hash(hash)? {
            self.conn.execute(
                "UPDATE entries
                 SET kind = ?1, label = ?2, preview = ?3, size = ?4, last_used_at = ?5
                 WHERE id = ?6",
                params![kind.label(), label, preview, size as i64, &created_at, id],
            )?;
            return Ok(id);
        }

        if self.has_legacy_timestamp {
            self.conn.execute(
                "INSERT INTO entries (
                    hash, kind, label, preview, size, created_at, last_used_at, timestamp
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    hash,
                    kind.label(),
                    label,
                    preview,
                    size as i64,
                    &created_at,
                    &created_at,
                    &created_at,
                ],
            )?;
        } else {
            self.conn.execute(
                "INSERT INTO entries (
                    hash, kind, label, preview, size, created_at, last_used_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    hash,
                    kind.label(),
                    label,
                    preview,
                    size as i64,
                    &created_at,
                    &created_at,
                ],
            )?;
        }
        let id = self.conn.last_insert_rowid();

        for (mime, data) in payloads {
            self.conn.execute(
                "INSERT INTO payloads (entry_id, mime_type, data) VALUES (?1, ?2, ?3)",
                params![id, mime, data],
            )?;
        }

        self.trim()?;
        Ok(id)
    }

    fn trim(&self) -> Result<()> {
        self.conn.execute(
            "DELETE FROM entries WHERE id NOT IN (
                SELECT id FROM entries ORDER BY last_used_at DESC LIMIT ?1
            )",
            params![self.max_entries as i64],
        )?;
        Ok(())
    }

    pub fn find_by_hash(&self, hash: &str) -> Result<Option<i64>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id FROM entries WHERE hash = ?1")?;
        let mut rows = stmt.query(params![hash])?;
        Ok(rows.next()?.map(|r| r.get(0).unwrap()))
    }

    pub fn list(&self, limit: usize) -> Result<Vec<Entry>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, hash, kind, label, preview, size, created_at, last_used_at
             FROM entries
             ORDER BY last_used_at DESC
             LIMIT ?1",
        )?;
        let entries = stmt
            .query_map(params![limit as i64], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, String>(7)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        let mut result = Vec::with_capacity(entries.len());
        for (id, hash, kind_str, label, preview, size, created_at_str, last_used_at_str) in entries
        {
            let mime_types = self.mime_types_for(id)?;
            let kind = parse_kind(&kind_str);
            let created_at = created_at_str
                .parse::<DateTime<Utc>>()
                .unwrap_or_else(|_| Utc::now());
            let last_used_at = last_used_at_str
                .parse::<DateTime<Utc>>()
                .unwrap_or_else(|_| Utc::now());
            result.push(Entry {
                id,
                hash,
                kind,
                label,
                preview,
                size: size as usize,
                created_at,
                last_used_at,
                mime_types,
            });
        }
        Ok(result)
    }

    pub fn get_payloads(&self, entry_id: i64) -> Result<Vec<MimePayload>> {
        let mut stmt = self
            .conn
            .prepare("SELECT mime_type, data FROM payloads WHERE entry_id = ?1 ORDER BY id")?;
        let rows = stmt
            .query_map(params![entry_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(rows
            .into_iter()
            .map(|(mime_type, data)| MimePayload {
                mime_type,
                data_b64: B64.encode(&data),
            })
            .collect())
    }

    pub fn get_hash(&self, entry_id: i64) -> Result<Option<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT hash FROM entries WHERE id = ?1")?;
        let mut rows = stmt.query(params![entry_id])?;
        Ok(rows.next()?.map(|r| r.get(0).unwrap()))
    }

    pub fn get_raw_payloads(&self, entry_id: i64) -> Result<Vec<(String, Vec<u8>)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT mime_type, data FROM payloads WHERE entry_id = ?1 ORDER BY id")?;
        let rows = stmt
            .query_map(params![entry_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn touch_last_used(&self, entry_id: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE entries SET last_used_at = ?1 WHERE id = ?2",
            params![Utc::now().to_rfc3339(), entry_id],
        )?;
        Ok(())
    }

    pub fn delete(&self, entry_id: i64) -> Result<bool> {
        let n = self
            .conn
            .execute("DELETE FROM entries WHERE id = ?1", params![entry_id])?;
        Ok(n > 0)
    }

    pub fn clear(&self) -> Result<usize> {
        let n = self.conn.execute("DELETE FROM entries", [])?;
        Ok(n)
    }

    fn mime_types_for(&self, entry_id: i64) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT mime_type FROM payloads WHERE entry_id = ?1 ORDER BY id")?;
        let rows = stmt
            .query_map(params![entry_id], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    fn migrate_entries_schema(&self) -> Result<()> {
        let has_created_at = self.entries_column_exists("created_at")?;
        let has_last_used_at = self.entries_column_exists("last_used_at")?;
        let has_timestamp = self.entries_column_exists("timestamp")?;

        if !has_created_at {
            self.conn
                .execute("ALTER TABLE entries ADD COLUMN created_at TEXT", [])?;
        }
        if !has_last_used_at {
            self.conn
                .execute("ALTER TABLE entries ADD COLUMN last_used_at TEXT", [])?;
        }

        if has_timestamp {
            self.conn.execute(
                "UPDATE entries
                 SET created_at = COALESCE(
                     created_at,
                     timestamp,
                     strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
                 )",
                [],
            )?;
            self.conn.execute(
                "UPDATE entries
                 SET last_used_at = COALESCE(
                     last_used_at,
                     timestamp,
                     created_at,
                     strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
                 )",
                [],
            )?;
        } else {
            self.conn.execute(
                "UPDATE entries
                 SET created_at = COALESCE(
                     created_at,
                     strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
                 )",
                [],
            )?;
            self.conn.execute(
                "UPDATE entries
                 SET last_used_at = COALESCE(
                     last_used_at,
                     created_at,
                     strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
                 )",
                [],
            )?;
        }

        Ok(())
    }

    fn entries_column_exists(&self, column_name: &str) -> Result<bool> {
        let mut stmt = self.conn.prepare("PRAGMA table_info(entries)")?;
        let columns = stmt
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(columns.iter().any(|column| column == column_name))
    }
}

fn parse_kind(s: &str) -> EntryKind {
    match s {
        "text" => EntryKind::Text,
        "uri" => EntryKind::Uri,
        "file" => EntryKind::File,
        "image" => EntryKind::Image,
        _ => EntryKind::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::{Database, NewEntry};
    use crate::common::types::EntryKind;
    use chrono::{Duration, TimeZone, Utc};
    use rusqlite::{params, Connection};
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn duplicate_hash_preserves_created_at_and_updates_last_used_at_for_all_entry_kinds() {
        let cases = [
            (
                EntryKind::Text,
                vec![("text/plain".to_string(), b"/tmp/example.txt".to_vec())],
            ),
            (
                EntryKind::File,
                vec![
                    (
                        "x-special/gnome-copied-files".to_string(),
                        b"copy\nfile:///tmp/example.txt\n".to_vec(),
                    ),
                    (
                        "text/uri-list".to_string(),
                        b"file:///tmp/example.txt\n".to_vec(),
                    ),
                ],
            ),
            (
                EntryKind::Image,
                vec![("image/png".to_string(), vec![0, 1, 2, 3])],
            ),
            (
                EntryKind::Uri,
                vec![(
                    "text/uri-list".to_string(),
                    b"https://example.com\n".to_vec(),
                )],
            ),
        ];

        for (kind, payloads) in cases {
            let db_path = unique_test_db_path(kind.label());
            let db = Database::open(&db_path).expect("open test database");
            let first_ts = Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap();
            let second_ts = first_ts + Duration::seconds(30);
            let hash = format!("dedupe-{}", kind.label());

            let first_id = db
                .insert(NewEntry {
                    hash: &hash,
                    kind: &kind,
                    label: "first label",
                    preview: "first preview",
                    size: payloads.iter().map(|(_, data)| data.len()).sum(),
                    created_at: first_ts,
                    payloads: &payloads,
                })
                .expect("insert first entry");

            let second_id = db
                .insert(NewEntry {
                    hash: &hash,
                    kind: &kind,
                    label: "second label",
                    preview: "second preview",
                    size: payloads.iter().map(|(_, data)| data.len()).sum(),
                    created_at: second_ts,
                    payloads: &payloads,
                })
                .expect("insert duplicate entry");

            assert_eq!(first_id, second_id, "{}", kind.label());

            let entries = db.list(10).expect("list entries");
            assert_eq!(entries.len(), 1, "{}", kind.label());
            assert_eq!(entries[0].id, first_id, "{}", kind.label());
            assert_eq!(entries[0].hash, hash, "{}", kind.label());
            assert_eq!(entries[0].kind, kind, "{}", kind.label());
            assert_eq!(entries[0].label, "second label", "{}", kind.label());
            assert_eq!(entries[0].preview, "second preview", "{}", kind.label());
            assert_eq!(entries[0].created_at, first_ts, "{}", kind.label());
            assert_eq!(entries[0].last_used_at, second_ts, "{}", kind.label());

            std::fs::remove_file(&db_path).ok();
        }
    }

    #[test]
    fn opening_legacy_database_backfills_created_and_last_used_timestamps() {
        let db_path = unique_test_db_path("legacy-migration");
        let legacy = Connection::open(&db_path).expect("open legacy database");
        legacy
            .execute_batch(
                "CREATE TABLE entries (
                    id        INTEGER PRIMARY KEY AUTOINCREMENT,
                    hash      TEXT    NOT NULL UNIQUE,
                    kind      TEXT    NOT NULL,
                    label     TEXT    NOT NULL,
                    preview   TEXT    NOT NULL,
                    size      INTEGER NOT NULL,
                    timestamp TEXT    NOT NULL
                );
                CREATE TABLE payloads (
                    id        INTEGER PRIMARY KEY AUTOINCREMENT,
                    entry_id  INTEGER NOT NULL,
                    mime_type TEXT    NOT NULL,
                    data      BLOB    NOT NULL
                );",
            )
            .expect("create legacy schema");

        let legacy_ts = Utc.with_ymd_and_hms(2026, 4, 29, 18, 30, 0).unwrap();
        legacy
            .execute(
                "INSERT INTO entries (hash, kind, label, preview, size, timestamp)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    "legacy-hash",
                    "image",
                    "PNG 1920x1080",
                    "PNG 1920x1080",
                    123_i64,
                    legacy_ts.to_rfc3339(),
                ],
            )
            .expect("insert legacy entry");
        legacy
            .execute(
                "INSERT INTO payloads (entry_id, mime_type, data) VALUES (?1, ?2, ?3)",
                params![1_i64, "image/png", vec![0_u8, 1, 2, 3]],
            )
            .expect("insert legacy payload");
        drop(legacy);

        let db = Database::open(&db_path).expect("open migrated database");
        let entries = db.list(10).expect("list migrated entries");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].created_at, legacy_ts);
        assert_eq!(entries[0].last_used_at, legacy_ts);

        std::fs::remove_file(&db_path).ok();
    }

    #[test]
    fn legacy_database_keeps_accepting_new_inserts_after_migration() {
        let db_path = unique_test_db_path("legacy-insert");
        let legacy = Connection::open(&db_path).expect("open legacy database");
        legacy
            .execute_batch(
                "CREATE TABLE entries (
                    id        INTEGER PRIMARY KEY AUTOINCREMENT,
                    hash      TEXT    NOT NULL UNIQUE,
                    kind      TEXT    NOT NULL,
                    label     TEXT    NOT NULL,
                    preview   TEXT    NOT NULL,
                    size      INTEGER NOT NULL,
                    timestamp TEXT    NOT NULL
                );
                CREATE TABLE payloads (
                    id        INTEGER PRIMARY KEY AUTOINCREMENT,
                    entry_id  INTEGER NOT NULL,
                    mime_type TEXT    NOT NULL,
                    data      BLOB    NOT NULL
                );",
            )
            .expect("create legacy schema");
        drop(legacy);

        let db = Database::open(&db_path).expect("open migrated database");
        let created_at = Utc.with_ymd_and_hms(2026, 4, 29, 19, 40, 0).unwrap();

        db.insert(NewEntry {
            hash: "legacy-new-hash",
            kind: &EntryKind::Image,
            label: "PNG 1920x1080",
            preview: "PNG 1920x1080",
            size: 4,
            created_at,
            payloads: &[("image/png".to_string(), vec![0_u8, 1, 2, 3])],
        })
        .expect("insert into migrated legacy database");

        let entries = db.list(10).expect("list migrated entries");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].created_at, created_at);
        assert_eq!(entries[0].last_used_at, created_at);

        std::fs::remove_file(&db_path).ok();
    }

    fn unique_test_db_path(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time before epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "mimeclip-db-test-{label}-{}-{nanos}.sqlite3",
            std::process::id()
        ))
    }
}
