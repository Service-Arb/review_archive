//! The composed scanner: picks the source for a target's kind and runs it through the
//! archive. Owns the one browser of a pass.

use std::{path::PathBuf, sync::OnceLock};

use crate::{
	archive::{Archive, RunSummary, SystemClock},
	config::Config,
	domain::{Known, ReviewSource, Scan, Target, TargetKind},
	sources::{
		gbp::{self, GbpSource},
		maps::{Browser, MapsSource},
	},
	store::{Store, blobs::BlobStore},
};

pub struct Runner {
	pub store: Store,
	pub blobs: BlobStore,
	pub config: Config,
	browser: Browser,
	http: reqwest::Client,
	gbp: OnceLock<gbp::Client>,
}

impl Runner {
	pub fn new(store: Store, config: Config, http: reqwest::Client, dump_html: Option<PathBuf>) -> Self {
		let browser = Browser::new(config.browser.clone(), config.profile_dir(), dump_html);
		Self {
			blobs: BlobStore::new(config.blob_dir()),
			store,
			config,
			browser,
			http,
			gbp: OnceLock::new(),
		}
	}

	pub async fn scan(&self, target: &Target) -> eyre::Result<RunSummary> {
		let archive = Archive {
			store: &self.store,
			blobs: &self.blobs,
			clock: &SystemClock,
		};
		match target.kind {
			TargetKind::Maps => {
				let source = MapsSource {
					browser: &self.browser,
					defaults: &self.config.defaults,
				};
				archive.run(&source, target).await
			}
			TargetKind::Gbp => {
				let client = match self.gbp.get() {
					Some(c) => c,
					// Read on first use, so a maps-only deployment needs no GBP secrets at all. Missing
					// ones fail the run like any source error, so the target backs off instead of spinning.
					None => match gbp::Credentials::from_env() {
						Ok(creds) => self.gbp.get_or_init(|| gbp::Client::new(self.http.clone(), creds)),
						Err(e) => return archive.run(&Unavailable(e), target).await,
					},
				};
				let source = GbpSource {
					client,
					browser: &self.browser,
					defaults: &self.config.defaults,
				};
				archive.run(&source, target).await
			}
		}
	}

	/// Ends the pass: the browser is not kept alive for hours between scans.
	pub async fn end_pass(&self) {
		self.browser.close().await;
	}
}

/// A source that could not be set up; scanning it fails with why.
struct Unavailable(eyre::Report);

impl ReviewSource for Unavailable {
	async fn scan(&self, _: &Target, _: &Known) -> eyre::Result<Scan> {
		Err(eyre::eyre!("{:#}", self.0))
	}
}
