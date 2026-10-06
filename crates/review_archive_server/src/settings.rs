//! The environment: secrets and the deployment profile. Everything else — data dir, bind
//! address, browser, defaults — is the TOML config (`--config`).

use review_archive::{config::Secrets, sources::gbp::Credentials};

ev_lib::settings! {
	/// Each secret is needed only by what uses it; a missing one fails that, with an error
	/// naming it, rather than the boot. The exception is the panel's keys in production, where
	/// the only thing this binary runs is `serve`.
	pub struct Settings {
		/// The Service-Arb panel's public keys, `<kid>:<base64>` comma-separated: whose
		/// assertion names the caller ([`sa_auth`]). `serve` refuses to start without them.
		#[required_in("production")]
		panel_assertion_keys: Option<String>,
		/// Places API key: resolving a Maps URL that carries no place id.
		#[secret]
		google_maps_key: Option<String>,
		/// Business Profile OAuth client, for `gbp` targets. All three or none.
		gbp_client_id: Option<String>,
		#[secret]
		gbp_client_secret: Option<String>,
		#[secret]
		gbp_refresh_token: Option<String>,
		/// The bot members' Telegram channels are posted by.
		#[secret]
		telegram_bot_token: Option<String>,
		/// Unset: errors are logged, not reported.
		#[secret]
		sentry_dsn: Option<String>,
		/// Discord webhooks the files an error or warning points at go to. Both or neither.
		#[secret]
		alert_webhook_error: Option<String>,
		#[secret]
		alert_webhook_warn: Option<String>,
		app_env: String = "development",
	}
}

impl Settings {
	/// What the library needs of the environment.
	pub fn secrets(&self) -> Secrets {
		let gbp = match (&self.gbp_client_id, &self.gbp_client_secret, &self.gbp_refresh_token) {
			(Some(client_id), Some(client_secret), Some(refresh_token)) => Some(Credentials {
				client_id: client_id.clone(),
				client_secret: client_secret.clone(),
				refresh_token: refresh_token.clone(),
			}),
			_ => None,
		};
		Secrets {
			google_maps_key: self.google_maps_key.clone(),
			gbp,
			telegram_bot_token: self.telegram_bot_token.clone(),
		}
	}

	/// Where alerts go: `None` sends none.
	pub fn alert_webhooks(&self) -> eyre::Result<Option<ev_lib::alerts::Webhooks>> {
		match (&self.alert_webhook_error, &self.alert_webhook_warn) {
			(Some(error), Some(warn)) => Ok(Some(ev_lib::alerts::Webhooks {
				error: error.parse().map_err(|e| eyre::eyre!("ALERT_WEBHOOK_ERROR is not a URL: {e}"))?,
				warn: warn.parse().map_err(|e| eyre::eyre!("ALERT_WEBHOOK_WARN is not a URL: {e}"))?,
			})),
			(None, None) => Ok(None),
			_ => eyre::bail!("ALERT_WEBHOOK_ERROR and ALERT_WEBHOOK_WARN go together: set both, or neither"),
		}
	}

	/// The panel's keys, checked; the error says what `serve` needs.
	pub fn panel_keys(&self) -> eyre::Result<sa_auth::Keys> {
		let keys = self.panel_assertion_keys.as_deref().ok_or_else(|| eyre::eyre!("PANEL_ASSERTION_KEYS must be set for serve"))?;
		keys.parse().map_err(|e| eyre::eyre!("PANEL_ASSERTION_KEYS: {e}"))
	}
}

/// `--print-required-vars[=PROFILE]` (default `production`): the variables a deploy into
/// that profile must provide, one per line, for the gitops preflight. Read before clap,
/// which would otherwise demand a subcommand.
pub fn print_required_vars_for() -> Option<String> {
	const FLAG: &str = "--print-required-vars";
	let mut args = std::env::args().skip(1);
	let arg = args.next()?;
	match arg.split_once('=') {
		Some((FLAG, profile)) => Some(profile.to_owned()),
		Some(_) => None,
		None if arg == FLAG => Some(args.next().unwrap_or_else(|| "production".to_owned())),
		None => None,
	}
}

#[cfg(test)]
mod tests {
	use std::collections::HashMap;

	use super::*;

	/// The env surface is the deploy contract (the image env and the cluster Secret use these
	/// names); a rename here must be deliberate.
	#[test]
	fn env_surface_matches_the_deploy_contract() {
		assert_eq!(
			Settings::var_names(),
			[
				"PANEL_ASSERTION_KEYS",
				"GOOGLE_MAPS_KEY",
				"GBP_CLIENT_ID",
				"GBP_CLIENT_SECRET",
				"GBP_REFRESH_TOKEN",
				"TELEGRAM_BOT_TOKEN",
				"SENTRY_DSN",
				"ALERT_WEBHOOK_ERROR",
				"ALERT_WEBHOOK_WARN",
				"APP_ENV"
			]
		);
		assert_eq!(Settings::required_var_names("production"), ["PANEL_ASSERTION_KEYS"]);
		assert!(Settings::required_var_names("development").is_empty());
	}

	fn from(vars: &[(&str, &str)]) -> Result<Settings, ev_lib::settings::SettingsError> {
		let map: HashMap<String, String> = vars.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect();
		Settings::from_source(|k| map.get(k).cloned())
	}

	#[test]
	fn secrets_are_lazy_and_named_when_missing() {
		let s = from(&[("GBP_CLIENT_ID", "id")]).unwrap();
		assert!(s.secrets().gbp.is_none(), "a partial GBP triple is none of it");
		assert_eq!(format!("{:#}", s.panel_keys().unwrap_err()), "PANEL_ASSERTION_KEYS must be set for serve");
		assert!(from(&[("PANEL_ASSERTION_KEYS", "k1:short")]).unwrap().panel_keys().is_err());

		let err = from(&[("APP_ENV", "production")]).unwrap_err();
		assert!(err.to_string().contains("PANEL_ASSERTION_KEYS"), "{err}");
		// secrets never print
		let s = from(&[("TELEGRAM_BOT_TOKEN", "0123456789abcdef-xyzzy")]).unwrap();
		assert!(!format!("{s:?}").contains("xyzzy"));
	}
}
