//! Settings, stored in the corpus itself.
//!
//! Kept in the `meta` table rather than a separate file so a corpus stays one
//! self-describing directory: copy it to another machine and its settings come
//! with it.

use anyhow::{Result, bail};
use rusqlite::params;

use crate::store::Store;

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    /// Drop repositories with no upstream commit in this many months.
    /// Zero disables decay.
    pub decay_months: u32,
    /// Also drop repositories the owner has archived.
    pub decay_archived: bool,
    /// After an update, top the corpus back up from `discover_query`.
    pub auto_discover: bool,
    /// GitHub search qualifiers used by `discover` and auto-discovery.
    pub discover_query: String,
    /// How many repositories a discovery run may add.
    pub discover_limit: usize,
    /// Skip repositories below this star count.
    pub min_stars: u32,
    /// Skip repositories with no upstream commit in this many months. Zero
    /// accepts any age.
    pub max_age_months: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            // Off by default: silently deleting a user's data is not a sane
            // out-of-the-box behaviour.
            decay_months: 0,
            // An archived repository is frozen upstream: it will never gain a
            // fix or a new pattern, so keeping it only feeds the agent code
            // that is guaranteed not to improve.
            decay_archived: true,
            auto_discover: false,
            discover_query: "topic:ai-agents".into(),
            discover_limit: 25,
            min_stars: 100,
            // Two years. Old enough to keep stable, finished libraries;
            // recent enough to exclude code that predates current practice.
            max_age_months: 24,
        }
    }
}

/// Every setting, with the help text shown by `steroids config`.
pub const KEYS: &[(&str, &str)] = &[
    (
        "decay_months",
        "remove repos with no upstream commit in N months (0 = never)",
    ),
    ("decay_archived", "also remove archived repos (true/false)"),
    (
        "auto_discover",
        "top up from discover_query after each update (true/false)",
    ),
    (
        "discover_query",
        "GitHub search qualifiers, e.g. 'topic:ai-agents language:python'",
    ),
    ("discover_limit", "how many repos a discovery run may add"),
    ("min_stars", "ignore repos with fewer stars than this"),
    (
        "max_age_months",
        "ignore repos with no commit in N months (0 = any age)",
    ),
];

impl Config {
    pub fn load(store: &Store) -> Result<Self> {
        let mut config = Config::default();
        let mut statement = store
            .db
            .prepare("SELECT key, value FROM meta WHERE key LIKE 'config.%'")?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
        })?;
        for row in rows {
            let (key, value) = row?;
            let key = key.trim_start_matches("config.");
            let value = String::from_utf8_lossy(&value).into_owned();
            // A stored value that no longer parses (hand-edited, or written by
            // a newer version) falls back to the default, but silently
            // reverting a setting the user chose would be worse than saying so.
            if let Err(error) = config.set(key, &value) {
                eprintln!("  ignoring stored setting {key}: {error}");
            }
        }
        Ok(config)
    }

    pub fn save(&self, store: &Store) -> Result<()> {
        for (key, _) in KEYS {
            store.db.execute(
                "INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)",
                params![format!("config.{key}"), self.get(key).into_bytes()],
            )?;
        }
        Ok(())
    }

    pub fn get(&self, key: &str) -> String {
        match key {
            "decay_months" => self.decay_months.to_string(),
            "decay_archived" => self.decay_archived.to_string(),
            "auto_discover" => self.auto_discover.to_string(),
            "discover_query" => self.discover_query.clone(),
            "discover_limit" => self.discover_limit.to_string(),
            "min_stars" => self.min_stars.to_string(),
            "max_age_months" => self.max_age_months.to_string(),
            _ => String::new(),
        }
    }

    pub fn set(&mut self, key: &str, value: &str) -> Result<()> {
        let value = value.trim();
        match key {
            "decay_months" => self.decay_months = parse_number(value, "decay_months")? as u32,
            "decay_archived" => self.decay_archived = parse_bool(value, "decay_archived")?,
            "auto_discover" => self.auto_discover = parse_bool(value, "auto_discover")?,
            "discover_query" => {
                if value.is_empty() {
                    bail!("discover_query cannot be empty");
                }
                value.clone_into(&mut self.discover_query);
            }
            "discover_limit" => {
                let limit = parse_number(value, "discover_limit")?;
                // The GitHub search API caps a page at 100, and a run that adds
                // more than that in one go is better done deliberately.
                if !(1..=100).contains(&limit) {
                    bail!("discover_limit must be between 1 and 100");
                }
                self.discover_limit = limit as usize;
            }
            "min_stars" => self.min_stars = parse_number(value, "min_stars")? as u32,
            "max_age_months" => self.max_age_months = parse_number(value, "max_age_months")? as u32,
            other => bail!(
                "unknown setting {other:?}. Known: {}",
                KEYS.iter().map(|(k, _)| *k).collect::<Vec<_>>().join(", ")
            ),
        }
        Ok(())
    }
}

fn parse_number(value: &str, key: &str) -> Result<u64> {
    value
        .parse()
        .map_err(|_| anyhow::anyhow!("{key} must be a whole number, got {value:?}"))
}

fn parse_bool(value: &str, key: &str) -> Result<bool> {
    match value.to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Ok(true),
        "false" | "no" | "off" | "0" => Ok(false),
        _ => bail!("{key} must be true or false, got {value:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_a_store() -> Result<()> {
        let directory =
            std::env::temp_dir().join(format!("steroids-config-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        let store = Store::open(&directory)?;

        let mut config = Config::default();
        config.set("decay_months", "6")?;
        config.set("auto_discover", "yes")?;
        config.set("discover_query", "topic:llm language:rust")?;
        config.save(&store)?;

        let loaded = Config::load(&store)?;
        assert_eq!(loaded, config);
        assert_eq!(loaded.decay_months, 6);
        assert!(loaded.auto_discover);

        std::fs::remove_dir_all(&directory)?;
        Ok(())
    }

    #[test]
    fn rejects_nonsense() {
        let mut config = Config::default();
        assert!(config.set("decay_months", "soon").is_err());
        assert!(config.set("decay_archived", "maybe").is_err());
        assert!(config.set("discover_limit", "0").is_err());
        assert!(config.set("discover_limit", "500").is_err());
        assert!(config.set("nonexistent", "1").is_err());
        assert!(config.set("discover_query", "").is_err());
    }
}
