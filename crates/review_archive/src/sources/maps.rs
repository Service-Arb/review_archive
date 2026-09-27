//! `maps`: any public place, read from its Google Maps page in a headless browser.

use jiff::Timestamp;
use review_archive_core::{
	Known, Scan, Target,
	dto::CaptureLimits,
	maps::{NewestFirst, Requested},
};

use super::ReviewSource;
use crate::{browser::Browser, config::Defaults};

/// Scans a target's public Maps page, newest first; see [`NewestFirst`] for how far.
#[derive(Debug)]
pub struct MapsSource<'a> {
	/// The browser to walk in.
	pub browser: &'a Browser,
	/// The walk's limits.
	pub defaults: &'a Defaults,
}

impl ReviewSource for MapsSource<'_> {
	async fn scan(&self, target: &Target, known: &Known) -> eyre::Result<Scan> {
		let max = self.defaults.max_for(known);
		let mut policy = NewestFirst::new(known, Timestamp::now());
		let walked = self.browser.walk(&target.place_id, &target.lang, &mut policy, max).await?;
		Ok(policy.conclude(walked, max, Timestamp::now()))
	}
}

/// An ad-hoc capture of a target's Maps page: down to the limit, or until the named
/// reviews are all found; screenshots what the archive still lacks.
#[derive(Debug)]
pub struct RequestedSource<'a> {
	/// The browser to walk in.
	pub browser: &'a Browser,
	/// Cards read at most.
	pub max: usize,
	/// What to capture.
	pub limits: &'a CaptureLimits,
}

impl ReviewSource for RequestedSource<'_> {
	async fn scan(&self, target: &Target, known: &Known) -> eyre::Result<Scan> {
		let mut policy = Requested::new(known, self.limits.review_ids.clone());
		let walked = self.browser.walk(&target.place_id, &target.lang, &mut policy, self.max).await?;
		Ok(policy.conclude(walked, Timestamp::now()))
	}

	fn ad_hoc(&self) -> bool {
		true
	}
}
