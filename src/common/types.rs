use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum EntryKind {
    Text,
    Uri,
    File,
    Image,
    Other,
}

impl EntryKind {
    pub fn label(&self) -> &'static str {
        match self {
            EntryKind::Text => "text",
            EntryKind::Uri => "uri",
            EntryKind::File => "file",
            EntryKind::Image => "image",
            EntryKind::Other => "other",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MimePayload {
    pub mime_type: String,
    /// Base64-encoded raw bytes
    pub data_b64: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub id: i64,
    pub hash: String,
    pub kind: EntryKind,
    pub label: String,
    pub preview: String,
    pub size: usize,
    pub timestamp: DateTime<Utc>,
    pub mime_types: Vec<String>,
}

/// Classify an entry based on offered MIME types (checked in priority order).
pub fn classify_kind(mime_types: &[String]) -> EntryKind {
    let has = |prefix: &str| mime_types.iter().any(|m| m.starts_with(prefix));

    if mime_types.iter().any(|m| {
        m == "x-special/gnome-copied-files"
            || m == "x-kde-cut-selection"
            || m.starts_with("x-special/")
    }) || (mime_types.iter().any(|m| m == "text/uri-list")
        && mime_types
            .iter()
            .any(|m| m.starts_with("application/x-kde") || m.starts_with("x-special")))
    {
        return EntryKind::File;
    }

    if mime_types.iter().any(|m| m == "text/uri-list") {
        return EntryKind::Uri;
    }

    if has("image/") {
        return EntryKind::Image;
    }

    if has("text/") {
        return EntryKind::Text;
    }

    EntryKind::Other
}

/// Build a human-readable label from MIME payloads.
pub fn build_label(kind: &EntryKind, mime_payloads: &[(String, Vec<u8>)]) -> String {
    let get = |mime: &str| {
        mime_payloads
            .iter()
            .find(|(m, _)| m == mime)
            .map(|(_, d)| d.as_slice())
    };

    match kind {
        EntryKind::File | EntryKind::Uri => {
            if let Some(data) = get("text/uri-list") {
                let s = String::from_utf8_lossy(data);
                let paths: Vec<&str> = s
                    .lines()
                    .filter(|l| !l.starts_with('#') && !l.is_empty())
                    .collect();
                match paths.len() {
                    0 => "file".to_string(),
                    1 => {
                        let p = paths[0].trim_start_matches("file://");
                        let p = percent_decode(p);
                        std::path::Path::new(&p)
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or(p)
                    }
                    n => format!(
                        "{} and {} more",
                        {
                            let p = paths[0].trim_start_matches("file://");
                            let p = percent_decode(p);
                            std::path::Path::new(&p)
                                .file_name()
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or(p)
                        },
                        n - 1
                    ),
                }
            } else {
                "file".to_string()
            }
        }
        EntryKind::Text => {
            if let Some(data) = get("text/plain;charset=utf-8")
                .or_else(|| get("text/plain"))
                .or_else(|| {
                    mime_payloads
                        .iter()
                        .find(|(m, _)| m.starts_with("text/"))
                        .map(|(_, d)| d.as_slice())
                })
            {
                let s = String::from_utf8_lossy(data);
                let trimmed = s.trim();
                if trimmed.len() > 120 {
                    format!("{}…", &trimmed[..120])
                } else {
                    trimmed.to_string()
                }
            } else {
                "text".to_string()
            }
        }
        EntryKind::Image => "image".to_string(),
        EntryKind::Other => "binary".to_string(),
    }
}

fn percent_decode(s: &str) -> String {
    let mut bytes: Vec<u8> = Vec::with_capacity(s.len());
    let input = s.as_bytes();
    let mut i = 0;
    while i < input.len() {
        if input[i] == b'%' && i + 2 < input.len() {
            if let Ok(h) = std::str::from_utf8(&input[i + 1..i + 3]) {
                if let Ok(b) = u8::from_str_radix(h, 16) {
                    bytes.push(b);
                    i += 3;
                    continue;
                }
            }
        }
        bytes.push(input[i]);
        i += 1;
    }
    String::from_utf8_lossy(&bytes).into_owned()
}
