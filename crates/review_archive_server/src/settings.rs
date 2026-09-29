//! The environment: secrets and the deployment profile. Everything else — data dir, bind
//! address, browser, defaults — is the TOML config (`--config`).

use review_archive::{config::Secrets, sources::gbp::Credentials};

ev_lib::settings! {
	/// Each secret is needed only by what uses it; a missing one fails that, with an error
	/// naming it, rather than the boot. The exception is the API token in production, where
	/// the only thing this binary runs is `serve`.
	pub struct Settings {
		/// Bearer token of the HTTP API, 16+ characters. `serve` refuses to start without it.
		#[secret]
		#[required_in("production")]
		review_archive_token: Option<String>,
		/// Places API key: resolving a Maps URL that carries no place id.
		#[secret]
		google_maps_key: Option<String>,
		/// Business Profile OAuth client, for `gbp` targets. All three or none.
		gbp_client_id: Option<String>,
		#[secret]
		gbp_client_secret: Option<String>,
		#[secret]
		gbp_refresh_token: Option<String>,
		/// Playbook's token introspection (`{base}/introspect`): members' tokens are checked
		/// there. With its secret; both or neither. Unset: only the operator's token works.
		auth_introspect_url: Option<String>,
		#[secret]
		introspect_secret: Option<String>,
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

	/// How members' tokens are checked: `None` takes only the operator's.
	pub fn introspect(&self) -> eyre::Result<Option<review_archive_server::auth::Introspect>> {
		match (&self.auth_introspect_url, &self.introspect_secret) {
			(Some(url), Some(secret)) => Ok(Some(review_archive_server::auth::Introspect::new(
				url.parse().map_err(|e| eyre::eyre!("AUTH_INTROSPECT_URL is not a URL: {e}"))?,
				secret.clone(),
			)?)),
			(None, None) => Ok(None),
			_ => eyre::bail!("AUTH_INTROSPECT_URL and INTROSPECT_SECRET go together: set both, or neither"),
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

	/// The API token, checked; the error says what `serve` needs.
	pub fn api_token(&self) -> eyre::Result<&str> {
		let token = self.review_archive_token.as_deref().ok_or_else(|| eyre::eyre!("REVIEW_ARCHIVE_TOKEN must be set for serve"))?;
		eyre::ensure!(token.len() >= 16, "REVIEW_ARCHIVE_TOKEN is too short to be a secret (16+ characters)");
		Ok(token)
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
				"REVIEW_ARCHIVE_TOKEN",
				"GOOGLE_MAPS_KEY",
				"GBP_CLIENT_ID",
				"GBP_CLIENT_SECRET",
				"GBP_REFRESH_TOKEN",
				"AUTH_INTROSPECT_URL",
				"INTROSPECT_SECRET",
				"TELEGRAM_BOT_TOKEN",
				"SENTRY_DSN",
				"ALERT_WEBHOOK_ERROR",
				"ALERT_WEBHOOK_WARN",
				"APP_ENV"
			]
		);
		assert_eq!(Settings::required_var_names("production"), ["REVIEW_ARCHIVE_TOKEN"]);
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
		assert_eq!(format!("{:#}", s.api_token().unwrap_err()), "REVIEW_ARCHIVE_TOKEN must be set for serve");
		assert!(from(&[("REVIEW_ARCHIVE_TOKEN", "short")]).unwrap().api_token().is_err());

		let err = from(&[("APP_ENV", "production")]).unwrap_err();
		assert!(err.to_string().contains("REVIEW_ARCHIVE_TOKEN"), "{err}");
		// secrets never print
		let s = from(&[("REVIEW_ARCHIVE_TOKEN", "0123456789abcdef-xyzzy")]).unwrap();
		assert!(!format!("{s:?}").contains("xyzzy"));
	}
}
