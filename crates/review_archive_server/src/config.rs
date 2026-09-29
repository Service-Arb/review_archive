//! The TOML config: where things live and what a new target defaults to. Secrets are
//! never read from here, only from the environment.

use std::{
	net::SocketAddr,
	path::{Path, PathBuf},
};

use eyre::WrapErr;
use review_archive::config::{BrowserConfig, Defaults, WebhookConfig};
use review_archive_core::schedule;
use serde::Deserialize;

pub const DEFAULT_BIND: &str = "127.0.0.1:59110";

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
	pub data_dir: PathBuf,
	pub bind: SocketAddr,
	pub browser: BrowserConfig,
	pub defaults: Defaults,
	pub webhooks: WebhookConfig,
	/// The dashboard's built bundle, served under `/mfe/`; not served when unset.
	pub mfe_dir: Option<PathBuf>,
}

impl Default for Config {
	fn default() -> Self {
		Self {
			data_dir: PathBuf::from("data"),
			bind: DEFAULT_BIND.parse().expect("DEFAULT_BIND is a valid socket address"),
			browser: BrowserConfig::default(),
			defaults: Defaults::default(),
			webhooks: WebhookConfig::default(),
			mfe_dir: None,
		}
	}
}

impl Config {
	pub fn load(path: Option<&Path>) -> eyre::Result<Self> {
		let Some(path) = path else {
			return Ok(Self::default());
		};
		let raw = std::fs::read_to_string(path).wrap_err_with(|| format!("reading config at {}", path.display()))?;
		let cfg: Self = toml::from_str(&raw).wrap_err_with(|| format!("parsing config at {}", path.display()))?;
		eyre::ensure!(
			cfg.defaults.interval >= schedule::MIN_INTERVAL,
			"defaults.interval must be at least {}s",
			schedule::MIN_INTERVAL.as_secs()
		);
		Ok(cfg)
	}

	/// The library's view: this file's sections, plus the secrets from the environment.
	pub fn archive(&self, secrets: review_archive::config::Secrets) -> review_archive::config::Config {
		review_archive::config::Config {
			data_dir: Some(self.data_dir.clone()),
			browser: self.browser.clone(),
			defaults: self.defaults.clone(),
			webhooks: self.webhooks.clone(),
			secrets,
		}
	}
}

#[cfg(test)]
mod tests {
	use std::time::Duration;

	use super::*;

	#[test]
	fn full_and_empty_configs() {
		let cfg: Config = toml::from_str(
			r#"
			data_dir = "/data"
			bind = "0.0.0.0:59110"
			[browser]
			executable = "/bin/chromium"
			no_sandbox = true
			[defaults]
			lang = "fr"
			interval = "12h"
			[webhooks]
			allowed_hosts = ["concierge"]
			"#,
		)
		.unwrap();
		assert_eq!(cfg.defaults.interval, Duration::from_secs(12 * 3600));
		assert_eq!(cfg.defaults.max_reviews_per_scan, 200);
		assert!(cfg.browser.no_sandbox);
		assert_eq!(cfg.webhooks.allowed_hosts, ["concierge"]);
		assert!(toml::from_str::<Config>("[defaults]\nlang = \"fr&q=x\"").is_err(), "a lang goes into a URL");

		let empty: Config = toml::from_str("").unwrap();
		assert_eq!(empty.bind.to_string(), DEFAULT_BIND);
		assert!(toml::from_str::<Config>("typo = 1").is_err());
	}
}
