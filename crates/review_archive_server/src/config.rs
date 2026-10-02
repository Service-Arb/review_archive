//! The config: where things live, what a new target defaults to, how scans and deliveries
//! are paced. Secrets are never read from here, only from the environment.

use std::{net::SocketAddr, path::PathBuf};

use review_archive::config::{BrowserConfig, Defaults, WebhookConfig};
use review_archive_core::{maps::cost, schedule::Schedule, tokens::Tokens};
use review_archive_server::{http::HttpConfig, worker::WorkerConfig};
use smart_default::SmartDefault;
use v_utils::macros::{ConfigJsonSchema, MyConfigPrimitives, Settings};

#[derive(Clone, ConfigJsonSchema, Debug, MyConfigPrimitives, Settings, SmartDefault)]
#[settings(config_name = "review_archive")]
pub struct AppConfig {
	#[default(PathBuf::from("data"))]
	pub data_dir: PathBuf,
	#[default(SocketAddr::from(([127, 0, 0, 1], 59110)))]
	pub bind: SocketAddr,
	/// The dashboard's built bundle, served under `/mfe/`; not served when unset.
	pub mfe_dir: Option<PathBuf>,
	#[serde(default)]
	#[settings(flatten)]
	pub browser: BrowserConfig,
	#[serde(default)]
	#[settings(flatten)]
	pub defaults: Defaults,
	#[serde(default)]
	#[settings(flatten)]
	pub schedule: Schedule,
	#[serde(default)]
	#[settings(flatten)]
	pub tokens: Tokens,
	#[serde(default)]
	#[settings(flatten)]
	pub webhooks: WebhookConfig,
	#[serde(default)]
	#[settings(flatten)]
	pub worker: WorkerConfig,
	#[serde(default)]
	#[settings(flatten)]
	pub http: HttpConfig,
}

impl AppConfig {
	pub fn load(flags: SettingsFlags) -> eyre::Result<Self> {
		let cfg = Self::try_build(flags)?;
		eyre::ensure!(
			cfg.defaults.interval >= cfg.schedule.min_interval,
			"defaults.interval ({}) is below schedule.min_interval ({})",
			cfg.defaults.interval,
			cfg.schedule.min_interval
		);
		eyre::ensure!(
			cfg.tokens.per_hour >= cost::FIRST_SCREEN,
			"tokens.per_hour ({}) is below what a walk's first screen costs ({})",
			cfg.tokens.per_hour,
			cost::FIRST_SCREEN
		);
		Ok(cfg)
	}

	/// The library's view: this config's sections, plus the secrets from the environment.
	pub fn archive(&self, secrets: review_archive::config::Secrets) -> review_archive::config::Config {
		review_archive::config::Config {
			data_dir: Some(self.data_dir.clone()),
			browser: self.browser.clone(),
			defaults: self.defaults.clone(),
			schedule: self.schedule.clone(),
			tokens: self.tokens.clone(),
			webhooks: self.webhooks.clone(),
			secrets,
		}
	}
}

#[cfg(test)]
mod tests {
	use v_utils::TF_12H;

	use super::*;

	#[test]
	fn full_and_empty_configs() {
		let cfg: AppConfig = toml::from_str(
			r#"
			data_dir = "/data"
			bind = "0.0.0.0:59110"
			[browser]
			executable = "/bin/chromium"
			[defaults]
			lang = "fr"
			interval = "12h"
			[webhooks]
			allowed_hosts = ["concierge"]
			"#,
		)
		.unwrap();
		assert_eq!(cfg.defaults.interval, TF_12H);
		assert_eq!(cfg.defaults.max_reviews_per_scan, 200);
		assert_eq!(cfg.webhooks.allowed_hosts, ["concierge"]);
		assert!(toml::from_str::<AppConfig>("[defaults]\nlang = \"fr&q=x\"").is_err(), "a lang goes into a URL");

		let empty: AppConfig = toml::from_str("").unwrap();
		assert_eq!(empty.bind.to_string(), "127.0.0.1:59110");
		assert!(toml::from_str::<AppConfig>("[schedule]\ntypo = 1").is_err());
	}
}
