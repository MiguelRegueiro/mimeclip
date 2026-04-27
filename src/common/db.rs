use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection};

use crate::common::types::{Entry, EntryKind, MimePayload};

const DEFAULT_MAX_ENTRIES: usize = 500;

pub struct Database {
    conn: Connection,
    max_entries: usize,
}

pub struct NewEntry<'a> {
    pub hash: &'a str,
    pub kind: &'a EntryKind,
    pub label: &'a str,
    pub preview: &'a str,
    pub size: usize,
    pub timestamp: DateTime<Utc>,
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
        let db = Self { conn, max_entries };
        db.init()?;
        Ok(db)
    }

    fn init(&self) -> Result<()> {
        self.conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA foreign_keys=ON;

             CREATE TABLE IF NOT EXISTS entries (
                 id        INTEGER PRIMARY KEY AUTOINCREMENT,
                 hash      TEXT    NOT NULL UNIQUE,
                 kind      TEXT    NOT NULL,
                 label     TEXT    NOT NULL,
                 preview   TEXT    NOT NULL,
                 size      INTEGER NOT NULL,
                 timestamp TEXT    NOT NULL
             );

             CREATE TABLE IF NOT EXISTS payloads (
                 id        INTEGER PRIMARY KEY AUTOINCREMENT,
                 entry_id  INTEGER NOT NULL REFERENCES entries(id) ON DELETE CASCADE,
                 mime_type TEXT    NOT NULL,
                 data      BLOB    NOT NULL
             );

             CREATE INDEX IF NOT EXISTS idx_payloads_entry ON payloads(entry_id);
             CREATE INDEX IF NOT EXISTS idx_entries_ts ON entries(timestamp DESC);
            ",
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
            timestamp,
            payloads,
        } = entry;

        // If hash already exists, bump timestamp and return existing id.
        if let Some(id) = self.find_by_hash(hash)? {
            self.conn.execute(
                "UPDATE entries SET timestamp = ?1 WHERE id = ?2",
                params![timestamp.to_rfc3339(), id],
            )?;
            return Ok(id);
        }

        self.conn.execute(
            "INSERT INTO entries (hash, kind, label, preview, size, timestamp)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                hash,
                kind.label(),
                label,
                preview,
                size as i64,
                timestamp.to_rfc3339(),
            ],
        )?;
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
                SELECT id FROM entries ORDER BY timestamp DESC LIMIT ?1
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
            "SELECT id, hash, kind, label, preview, size, timestamp
             FROM entries
             ORDER BY timestamp DESC
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
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        let mut result = Vec::with_capacity(entries.len());
        for (id, hash, kind_str, label, preview, size, ts_str) in entries {
            let mime_types = self.mime_types_for(id)?;
            let kind = parse_kind(&kind_str);
            let timestamp = ts_str
                .parse::<DateTime<Utc>>()
                .unwrap_or_else(|_| Utc::now());
            result.push(Entry {
                id,
                hash,
                kind,
                label,
                preview,
                size: size as usize,
                timestamp,
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
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn duplicate_hash_updates_timestamp_for_all_entry_kinds() {
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
                    timestamp: first_ts,
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
                    timestamp: second_ts,
                    payloads: &payloads,
                })
                .expect("insert duplicate entry");

            assert_eq!(first_id, second_id, "{}", kind.label());

            let entries = db.list(10).expect("list entries");
            assert_eq!(entries.len(), 1, "{}", kind.label());
            assert_eq!(entries[0].id, first_id, "{}", kind.label());
            assert_eq!(entries[0].hash, hash, "{}", kind.label());
            assert_eq!(entries[0].kind, kind, "{}", kind.label());
            assert_eq!(entries[0].timestamp, second_ts, "{}", kind.label());

            std::fs::remove_file(&db_path).ok();
        }
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
