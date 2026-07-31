//! TOML configuration for the health check.
//!
//! ```toml
//! admin_url = "https://pulsar-admin.example.net"   # optional; PULSAR_ADMIN_URL / --admin-url override
//! subscription = "my-consumer-subscription"
//! backlog_threshold = 100                          # optional; default 100
//!
//! topics = [
//!   "persistent://widgetco/toybox/marbles",
//!   "persistent://widgetco/toybox/robots-partition-3",  # explicit partition is fine
//! ]
//!
//! # Optional colour thresholds for the BACKLOG, SIZE and UNACKED columns.
//! # Each column turns yellow at `warn` and red at `crit`; below `warn` it is
//! # green. Omit a column to leave it uncoloured. `size_*` are in bytes.
//! [colors]
//! backlog_warn = 100000
//! backlog_crit = 1000000
//! size_warn    = 1073741824      # 1 GiB
//! size_crit    = 5368709120      # 5 GiB
//! unacked_warn = 50000
//! unacked_crit = 200000
//!
//! # Optional Kubernetes correlation. `--kube` or `enabled = true` activates it;
//! # CLI --kube-* flags override any value here.
//! [kube]
//! enabled   = true
//! namespace = "my-ns"
//! selector  = "app=my-consumer"
//! configmap = "my-consumer-config"
//! log_tail  = 200
//! assert    = { worker_count = "24", batch_size = "30" }
//! ```

use std::fs;
use std::path::Path;

use serde::Deserialize;

fn default_threshold() -> i64 {
    100
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Pulsar admin service URL. Optional here because it can come from
    /// `PULSAR_ADMIN_URL` or `--admin-url` instead.
    pub admin_url: Option<String>,

    /// Subscription whose presence and backlog we are checking.
    pub subscription: String,

    /// Per-partition backlog above which a partition is reported as hot.
    #[serde(default = "default_threshold")]
    pub backlog_threshold: i64,

    /// Topics to check. Entries may be base topics (partitioned or not) or
    /// explicit `-partition-N` names. `persistent://` is assumed if the
    /// scheme is omitted.
    pub topics: Vec<String>,

    /// Optional per-column colour thresholds for table output.
    #[serde(default)]
    pub colors: ColorThresholds,

    /// Optional Kubernetes correlation settings. CLI `--kube-*` flags override
    /// these; `--kube` or `kube.enabled = true` activates the correlation.
    #[serde(default)]
    pub kube: KubeConfig,
}

/// Kubernetes correlation settings from the config file. Every field is
/// optional; CLI flags override any value set here. `assert` is an inline table
/// of `config.toml` key → expected-value pairs.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KubeConfig {
    /// Activate correlation from config alone (equivalent to passing `--kube`).
    #[serde(default)]
    pub enabled: bool,
    pub namespace: Option<String>,
    pub selector: Option<String>,
    pub configmap: Option<String>,
    pub log_tail: Option<i64>,
    /// config.toml assertions as an inline table: `assert = { key = "v", ... }`.
    #[serde(default)]
    pub assert: std::collections::BTreeMap<String, String>,
}

/// Warn/crit thresholds that drive the colour of the BACKLOG, SIZE and UNACKED
/// columns. A column with no thresholds set is left uncoloured. A value at or
/// above `crit` is red; at or above `warn` (but below `crit`) is yellow; below
/// `warn` is green. `size_*` are in bytes.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ColorThresholds {
    pub backlog_warn: Option<i64>,
    pub backlog_crit: Option<i64>,
    pub size_warn: Option<i64>,
    pub size_crit: Option<i64>,
    pub unacked_warn: Option<i64>,
    pub unacked_crit: Option<i64>,
}

/// The colour a value should render in, given optional warn/crit thresholds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThresholdLevel {
    None,
    Ok,
    Warn,
    Crit,
}

impl ColorThresholds {
    fn level(value: i64, warn: Option<i64>, crit: Option<i64>) -> ThresholdLevel {
        // Crit takes precedence; either threshold may be set independently.
        if let Some(c) = crit
            && value >= c {
                return ThresholdLevel::Crit;
            }
        if let Some(w) = warn
            && value >= w {
                return ThresholdLevel::Warn;
            }
        if warn.is_some() || crit.is_some() {
            ThresholdLevel::Ok
        } else {
            ThresholdLevel::None
        }
    }

    pub fn backlog_level(&self, value: i64) -> ThresholdLevel {
        Self::level(value, self.backlog_warn, self.backlog_crit)
    }

    pub fn size_level(&self, value: i64) -> ThresholdLevel {
        Self::level(value, self.size_warn, self.size_crit)
    }

    pub fn unacked_level(&self, value: i64) -> ThresholdLevel {
        Self::level(value, self.unacked_warn, self.unacked_crit)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to read config file {path}: {source}")]
    Read {
        path: String,
        source: std::io::Error,
    },
    #[error("failed to parse config file {path}: {source}")]
    Parse { path: String, source: toml::de::Error },
    #[error("config file {path}: `topics` must not be empty")]
    NoTopics { path: String },
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let display = path.display().to_string();
        let raw = fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: display.clone(),
            source,
        })?;
        let config: Config = toml::from_str(&raw).map_err(|source| ConfigError::Parse {
            path: display.clone(),
            source,
        })?;
        if config.topics.is_empty() {
            return Err(ConfigError::NoTopics { path: display });
        }
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_config() {
        let config: Config = toml::from_str(
            r#"
            subscription = "sub"
            topics = ["persistent://a/b/c"]
            "#,
        )
        .expect("minimal config should parse");
        assert_eq!(config.backlog_threshold, 100);
        assert!(config.admin_url.is_none());
        // No [colors] table → every column uncoloured.
        assert_eq!(config.colors.backlog_level(999_999), ThresholdLevel::None);
    }

    #[test]
    fn parses_color_thresholds() {
        let config: Config = toml::from_str(
            r#"
            subscription = "sub"
            topics = ["a/b/c"]
            [colors]
            backlog_warn = 100000
            backlog_crit = 1000000
            size_warn = 1073741824
            "#,
        )
        .expect("config with colors should parse");
        assert_eq!(config.colors.backlog_level(50_000), ThresholdLevel::Ok);
        assert_eq!(config.colors.backlog_level(100_000), ThresholdLevel::Warn);
        assert_eq!(config.colors.backlog_level(2_000_000), ThresholdLevel::Crit);
        // size_crit unset → only warn applies, never crit.
        assert_eq!(config.colors.size_level(2_000_000_000), ThresholdLevel::Warn);
        // unacked has no thresholds → uncoloured.
        assert_eq!(config.colors.unacked_level(999_999), ThresholdLevel::None);
    }

    #[test]
    fn rejects_unknown_color_fields() {
        let result: Result<Config, _> = toml::from_str(
            r#"
            subscription = "sub"
            topics = ["a/b/c"]
            [colors]
            backlog_wrn = 5
            "#,
        );
        assert!(result.is_err(), "typoed colour field must be rejected");
    }

    #[test]
    fn rejects_unknown_fields() {
        let result: Result<Config, _> = toml::from_str(
            r#"
            subscription = "sub"
            topics = ["a/b/c"]
            backlog_treshold = 5
            "#,
        );
        assert!(result.is_err(), "typoed field names must be rejected");
    }

    #[test]
    fn parses_kube_section() {
        let config: Config = toml::from_str(
            r#"
            subscription = "sub"
            topics = ["a/b/c"]
            [kube]
            enabled = true
            namespace = "my-ns"
            selector = "app=my-consumer"
            configmap = "my-cm"
            log_tail = 500
            assert = { worker_count = "24", batch_size = "30" }
            "#,
        )
        .expect("kube section should parse");
        assert!(config.kube.enabled);
        assert_eq!(config.kube.namespace.as_deref(), Some("my-ns"));
        assert_eq!(config.kube.selector.as_deref(), Some("app=my-consumer"));
        assert_eq!(config.kube.log_tail, Some(500));
        assert_eq!(config.kube.assert.get("worker_count").map(String::as_str), Some("24"));
        assert_eq!(config.kube.assert.len(), 2);
    }

    #[test]
    fn kube_section_defaults_to_disabled() {
        let config: Config = toml::from_str(
            r#"
            subscription = "sub"
            topics = ["a/b/c"]
            "#,
        )
        .unwrap();
        assert!(!config.kube.enabled);
        assert!(config.kube.selector.is_none());
        assert!(config.kube.assert.is_empty());
    }

    #[test]
    fn rejects_unknown_kube_fields() {
        let result: Result<Config, _> = toml::from_str(
            r#"
            subscription = "sub"
            topics = ["a/b/c"]
            [kube]
            selctor = "app=x"
            "#,
        );
        assert!(result.is_err(), "typoed kube field must be rejected");
    }
}
