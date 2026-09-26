//! The TOML config: where things live and what a new target defaults to. Secrets are
//! never read from here, only from the environment.

use std::{
	net::SocketAddr,
	path::{Path, PathBuf},
	time::Duration,
};

use eyre::WrapErr;
use serde::Deserialize;

use crate::domain::{parse_interval, schedule};

pub const DEFAULT_BIND: &str = "127.0.0.1:59110";

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
	pub data_dir: PathBuf,
	pub bind: SocketAddr,
	pub browser: BrowserConfig,
	pub defaults: Defaults,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BrowserConfig {
	/// Chrome or Chromium binary; found on PATH when unset.
	pub executable: Option<PathBuf>,
	/// For containers that run as an unprivileged user without user namespaces.
	pub no_sandbox: bool,
	/// Shows the window, for watching a scan by hand.
	pub headful: bool,
	/// Browser profile; `<data_dir>/chromium-profile` when unset.
	pub profile_dir: Option<PathBuf>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Defaults {
	pub lang: String,
	#[serde(deserialize_with = "de_interval")]
	pub interval: Duration,
	pub max_reviews_per_scan: usize,
	pub max_reviews_initial: usize,
}

impl Default for Config {
	fn default() -> Self {
		Self {
			data_dir: PathBuf::from("data"),
			bind: DEFAULT_BIND.parse().expect("DEFAULT_BIND is a valid socket address"),
			browser: BrowserConfig::default(),
			defaults: Defaults::default(),
		}
	}
}

impl Default for Defaults {
	fn default() -> Self {
		Self {
			lang: "en".into(),
			interval: schedule::DEFAULT_INTERVAL,
			max_reviews_per_scan: 200,
			max_reviews_initial: 2000,
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

	pub fn db_path(&self) -> PathBuf {
		self.data_dir.join("review_archive.db")
	}

	pub fn blob_dir(&self) -> PathBuf {
		self.data_dir.join("blobs")
	}

	/// The browser profile: it is what remembers the consent answer between runs.
	pub fn profile_dir(&self) -> PathBuf {
		self.browser.profile_dir.clone().unwrap_or_else(|| self.data_dir.join("chromium-profile"))
	}
}

fn de_interval<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
	let s = String::deserialize(d)?;
	parse_interval(&s).map_err(serde::de::Error::custom)
}

#[cfg(test)]
mod tests {
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
			"#,
		)
		.unwrap();
		assert_eq!(cfg.defaults.interval, Duration::from_secs(12 * 3600));
		assert_eq!(cfg.defaults.max_reviews_per_scan, 200);
		assert!(cfg.browser.no_sandbox);

		let empty: Config = toml::from_str("").unwrap();
		assert_eq!(empty.bind.to_string(), DEFAULT_BIND);
		assert!(toml::from_str::<Config>("typo = 1").is_err());
	}
}
