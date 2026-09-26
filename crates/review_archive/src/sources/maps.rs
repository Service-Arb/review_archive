//! `maps`: any public place, read from its Google Maps page in a headless browser.

use jiff::Timestamp;
use review_archive_core::{
	Known, Scan, Target,
	maps::{NewestFirst, scan_of},
};

use super::ReviewSource;
use crate::{browser::Browser, config::Defaults};

/// Scans a target's public Maps page: newest first, stopping at a screen of archived
/// cards, screenshotting what is new or still pending.
#[derive(Debug)]
pub struct MapsSource<'a> {
	/// The browser to walk in.
	pub browser: &'a Browser,
	/// The walk's limits.
	pub defaults: &'a Defaults,
}

impl ReviewSource for MapsSource<'_> {
	async fn scan(&self, target: &Target, known: &Known) -> eyre::Result<Scan> {
		let max = if known.is_empty() {
			self.defaults.max_reviews_initial
		} else {
			self.defaults.max_reviews_per_scan
		};
		let mut policy = NewestFirst::new(known);
		let walked = self.browser.walk(&target.place_id, &target.lang, &mut policy, max).await?;
		Ok(scan_of(walked, max, Timestamp::now()))
	}
}
