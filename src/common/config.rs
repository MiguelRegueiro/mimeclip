use std::fmt::Write as _;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

pub const DEFAULT_MAX_ENTRIES: usize = 500;
pub const DEFAULT_MAX_HISTORY_SIZE: u64 = 256 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Config {
    pub max_entries: usize,
    /// Kept as a human-readable value in TOML, such as "256 MiB".
    pub max_history_size: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub max_entries: usize,
    pub max_history_size: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_entries: DEFAULT_MAX_ENTRIES,
            max_history_size: format_size(DEFAULT_MAX_HISTORY_SIZE),
        }
    }
}

impl Config {
    pub fn limits(&self) -> Result<Limits> {
        if self.max_entries == 0 {
            bail!("max_entries must be at least 1");
        }
        Ok(Limits {
            max_entries: self.max_entries,
            max_history_size: parse_size(&self.max_history_size)?,
        })
    }

    pub fn load() -> Result<Self> {
        let path = config_path()?;
        let mut config = if path.exists() {
            let source = std::fs::read_to_string(&path)
                .with_context(|| format!("reading configuration {}", path.display()))?;
            toml::from_str(&source)
                .with_context(|| format!("parsing configuration {}", path.display()))?
        } else {
            Self::default()
        };

        // Keep the original environment override working for service users.
        if let Ok(value) = std::env::var("MIMECLIP_MAX_ENTRIES") {
            config.max_entries = value
                .parse()
                .context("parsing MIMECLIP_MAX_ENTRIES as a positive integer")?;
        }
        if let Ok(value) = std::env::var("MIMECLIP_MAX_HISTORY_SIZE") {
            config.max_history_size = value;
        }
        config.limits()?;
        Ok(config)
    }
}

pub fn config_path() -> Result<PathBuf> {
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|_| {
            std::env::var("HOME")
                .map(PathBuf::from)
                .map(|home| home.join(".config"))
        })
        .unwrap_or_else(|_| PathBuf::from("/tmp"));
    Ok(base.join("mimeclip/config.toml"))
}

pub fn save(config: &Config) -> Result<()> {
    config.limits()?;
    let path = config_path()?;
    let parent = path.parent().expect("configuration path has a parent");
    std::fs::create_dir_all(parent)
        .with_context(|| format!("creating configuration directory {}", parent.display()))?;
    restrict_directory_permissions(parent)?;

    let contents = toml::to_string_pretty(config)?;
    let temporary = path.with_extension("toml.tmp");
    std::fs::write(&temporary, contents)
        .with_context(|| format!("writing configuration {}", temporary.display()))?;
    restrict_file_permissions(&temporary)?;
    std::fs::rename(&temporary, &path)
        .with_context(|| format!("saving configuration {}", path.display()))?;
    Ok(())
}

pub fn parse_size(value: &str) -> Result<u64> {
    let normalized = value.trim().to_ascii_lowercase().replace(' ', "");
    let units = [
        ("gib", 1024_u64 * 1024 * 1024),
        ("gb", 1024_u64 * 1024 * 1024),
        ("mib", 1024_u64 * 1024),
        ("mb", 1024_u64 * 1024),
        ("kib", 1024_u64),
        ("kb", 1024_u64),
        ("b", 1_u64),
    ];
    let (number, multiplier) = units
        .iter()
        .find_map(|(suffix, multiplier)| normalized.strip_suffix(suffix).map(|n| (n, *multiplier)))
        .unwrap_or((normalized.as_str(), 1));
    let number: u64 = number
        .parse()
        .with_context(|| format!("invalid history size {value:?}; use values such as 256MiB"))?;
    let bytes = number
        .checked_mul(multiplier)
        .context("history size is too large")?;
    if bytes == 0 {
        bail!("history size must be at least 1 byte");
    }
    Ok(bytes)
}

pub fn format_size(bytes: u64) -> String {
    for (unit, divisor) in [
        ("GiB", 1024_u64 * 1024 * 1024),
        ("MiB", 1024_u64 * 1024),
        ("KiB", 1024_u64),
    ] {
        if bytes >= divisor && bytes % divisor == 0 {
            return format!("{} {unit}", bytes / divisor);
        }
    }
    let mut output = String::new();
    let _ = write!(output, "{bytes} B");
    output
}

#[cfg(unix)]
fn restrict_directory_permissions(path: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
fn restrict_directory_permissions(_: &std::path::Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn restrict_file_permissions(path: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn restrict_file_permissions(_: &std::path::Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{format_size, parse_size, DEFAULT_MAX_HISTORY_SIZE};

    #[test]
    fn parses_human_history_sizes() {
        assert_eq!(parse_size("256MiB").unwrap(), DEFAULT_MAX_HISTORY_SIZE);
        assert_eq!(parse_size("2 GiB").unwrap(), 2 * 1024 * 1024 * 1024);
        assert!(parse_size("0MiB").is_err());
        assert!(parse_size("lots").is_err());
    }

    #[test]
    fn formats_exact_binary_sizes() {
        assert_eq!(format_size(DEFAULT_MAX_HISTORY_SIZE), "256 MiB");
    }
}
