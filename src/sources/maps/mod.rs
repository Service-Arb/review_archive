//! `maps`: any public place, read from its Google Maps page in a headless browser.

pub mod browser;
pub mod parse;
pub mod selectors;

use std::path::PathBuf;

use jiff::Timestamp;

use self::{
	browser::{SCREEN, Session, WalkEnd, WalkPolicy, Walked},
	parse::Card,
};
use crate::{
	config::{BrowserConfig, Defaults},
	domain::{Coverage, Known, Observed, ReviewSource, Scan, Target, relative_date},
};

/// A browser launched on first use and shared by every target of one pass.
pub struct Browser {
	cfg: BrowserConfig,
	profile_dir: PathBuf,
	dump_html: Option<PathBuf>,
	session: tokio::sync::Mutex<Option<Session>>,
}

impl Browser {
	pub fn new(cfg: BrowserConfig, profile_dir: PathBuf, dump_html: Option<PathBuf>) -> Self {
		Self {
			cfg,
			profile_dir,
			dump_html,
			session: tokio::sync::Mutex::new(None),
		}
	}

	/// Opens the place's review list and walks it.
	pub async fn walk(&self, place_id: &str, lang: &str, policy: &(dyn WalkPolicy + Sync), max: usize) -> eyre::Result<Walked> {
		let mut guard = self.session.lock().await;
		if guard.is_none() {
			let mut s = Session::launch(&self.cfg, &self.profile_dir).await?;
			s.dump_html = self.dump_html.clone();
			*guard = Some(s);
		}
		let session = guard.as_ref().expect("launched just above");
		let page_url = session.open_reviews(place_id, lang).await?;
		session.walk(policy, max, &page_url).await
	}

	pub async fn close(&self) {
		if let Some(s) = self.session.lock().await.take() {
			s.close().await;
		}
	}
}

pub struct MapsSource<'a> {
	pub browser: &'a Browser,
	pub defaults: &'a Defaults,
}

struct NewestFirst<'a> {
	known: &'a Known,
}

impl WalkPolicy for NewestFirst<'_> {
	fn is_known(&self, card: &Card) -> bool {
		self.known.contains(&card.id)
	}

	fn wants_capture(&self, card: &Card) -> bool {
		self.known.wants_capture(&card.id)
	}

	/// A full screen of archived cards in a row: everything older is archived too.
	fn satisfied(&self, cards: &[Card]) -> bool {
		cards.len() >= SCREEN && cards[cards.len() - SCREEN..].iter().all(|c| self.is_known(c))
	}
}

impl ReviewSource for MapsSource<'_> {
	async fn scan(&self, target: &Target, known: &Known) -> eyre::Result<Scan> {
		let max = if known.is_empty() {
			self.defaults.max_reviews_initial
		} else {
			self.defaults.max_reviews_per_scan
		};
		let walked = self.browser.walk(&target.place_id, &target.lang, &NewestFirst { known }, max).await?;
		Ok(to_scan(walked, max, Timestamp::now()))
	}
}

pub fn observed(card: Card, now: Timestamp) -> Observed {
	let published_est = card.date_raw.as_deref().and_then(|d| relative_date::estimate(d, now));
	Observed {
		source_review_id: card.id,
		author: card.author,
		author_url: card.author_url,
		rating: card.rating,
		text: card.text,
		reply: card.reply,
		photo_count: card.photo_count,
		published_raw: card.date_raw,
		published_est,
		capture: None,
	}
}

fn to_scan(walked: Walked, max: usize, now: Timestamp) -> Scan {
	let Walked { cards, end, mut warnings, .. } = walked;
	if end == WalkEnd::Cap {
		warnings.push(format!("stopped after {max} reviews without reaching archived ones or the end of the list"));
	}
	let reviews: Vec<Observed> = cards.into_iter().map(|(card, capture)| Observed { capture, ..observed(card, now) }).collect();
	let coverage = match end {
		WalkEnd::ReachedEnd => Coverage::Complete,
		WalkEnd::Satisfied | WalkEnd::Cap => Coverage::DownTo(reviews.iter().filter_map(|r| r.published_est).min()),
	};
	Scan { reviews, coverage, warnings }
}
