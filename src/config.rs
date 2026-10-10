use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

/// Everything that is not a secret lives in the TOML file. Secrets come from the environment.
#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub actual: ActualConfig,
    #[serde(default)]
    pub jev: JevConfig,
    #[serde(default)]
    pub policy: PolicyConfig,
    #[serde(default)]
    pub tags: TagConfig,
    /// Needed only by `jevcat import`.
    #[serde(default)]
    pub import: Option<crate::import::ImportConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ActualConfig {
    /// Base URL of actual-http-api, for example `http://localhost:5007`.
    pub base_url: String,
    /// The budget's Sync ID from Settings → Advanced settings in Actual.
    pub budget_sync_id: String,
    /// How far back to fetch transactions on each run.
    #[serde(default = "default_since_days")]
    pub since_days: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct JevConfig {
    #[serde(default = "default_jev_base_url")]
    pub base_url: String,
    #[serde(default = "default_model")]
    pub model: String,
    #[serde(default = "default_instructions")]
    pub instructions: String,
    /// Label of the escape-hatch option every question carries.
    #[serde(default = "default_unsure_label")]
    pub unsure_label: String,
    /// Rubric text per category, keyed by category name. Categories without an entry
    /// get their group name as the hint.
    #[serde(default)]
    pub descriptions: HashMap<String, String>,
    #[serde(default = "default_concurrency")]
    pub concurrency: usize,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PolicyConfig {
    /// Confidence at or above which a category is written without review.
    #[serde(default = "default_apply_threshold")]
    pub apply_threshold: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TagConfig {
    #[serde(default = "default_applied_tag")]
    pub applied: String,
    #[serde(default = "default_flagged_tag")]
    pub flagged: String,
    #[serde(default = "default_missed_tag")]
    pub missed: String,
}

/// Secrets, read from the environment so they never sit in the config file.
#[derive(Debug, Clone)]
pub struct Secrets {
    pub actual_api_key: String,
    pub actual_encryption_password: Option<String>,
    pub typesafe_api_key: String,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        let config: Config =
            toml::from_str(&text).with_context(|| format!("parsing config {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            (0.0..=1.0).contains(&self.policy.apply_threshold),
            "policy.apply_threshold must be between 0 and 1"
        );
        anyhow::ensure!(
            self.jev.concurrency >= 1,
            "jev.concurrency must be at least 1"
        );
        anyhow::ensure!(
            !self.actual.budget_sync_id.trim().is_empty(),
            "actual.budget_sync_id is required"
        );
        Ok(())
    }
}

impl Secrets {
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            actual_api_key: required_env("ACTUAL_HTTP_API_KEY")?,
            actual_encryption_password: std::env::var("ACTUAL_ENCRYPTION_PASSWORD")
                .ok()
                .filter(|value| !value.is_empty()),
            typesafe_api_key: required_env("TYPESAFE_API_KEY")?,
        })
    }
}

fn required_env(name: &str) -> Result<String> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .with_context(|| format!("environment variable {name} is required"))
}

impl Default for JevConfig {
    fn default() -> Self {
        Self {
            base_url: default_jev_base_url(),
            model: default_model(),
            instructions: default_instructions(),
            unsure_label: default_unsure_label(),
            descriptions: HashMap::new(),
            concurrency: default_concurrency(),
            timeout_secs: default_timeout_secs(),
            max_retries: default_max_retries(),
        }
    }
}

impl Default for PolicyConfig {
    fn default() -> Self {
        Self {
            apply_threshold: default_apply_threshold(),
        }
    }
}

impl Default for TagConfig {
    fn default() -> Self {
        Self {
            applied: default_applied_tag(),
            flagged: default_flagged_tag(),
            missed: default_missed_tag(),
        }
    }
}

fn default_since_days() -> u32 {
    90
}
fn default_jev_base_url() -> String {
    "https://api.typesafe.ai".to_string()
}
fn default_model() -> String {
    "jev-latest".to_string()
}
fn default_instructions() -> String {
    "Which budget category does this bank transaction belong to?".to_string()
}
fn default_unsure_label() -> String {
    "Unsure".to_string()
}
fn default_concurrency() -> usize {
    4
}
fn default_timeout_secs() -> u64 {
    30
}
fn default_max_retries() -> u32 {
    2
}
fn default_apply_threshold() -> f64 {
    0.85
}
fn default_applied_tag() -> String {
    "#jev".to_string()
}
fn default_flagged_tag() -> String {
    "#jev-flag".to_string()
}
fn default_missed_tag() -> String {
    "#jev-miss".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimal_config_fills_defaults() {
        let config: Config = toml::from_str(
            r#"
            [actual]
            base_url = "http://localhost:5007"
            budget_sync_id = "abc"
            "#,
        )
        .unwrap();
        config.validate().unwrap();
        assert_eq!(config.actual.since_days, 90);
        assert_eq!(config.jev.model, "jev-latest");
        assert_eq!(config.policy.apply_threshold, 0.85);
        assert_eq!(config.tags.flagged, "#jev-flag");
    }

    #[test]
    fn threshold_out_of_range_is_rejected() {
        let config: Config = toml::from_str(
            r#"
            [actual]
            base_url = "http://localhost:5007"
            budget_sync_id = "abc"
            [policy]
            apply_threshold = 1.5
            "#,
        )
        .unwrap();
        assert!(config.validate().is_err());
    }
}
