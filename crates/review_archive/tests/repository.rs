//! The archive end to end on a temp SQLite: a scripted source, real storage.

use std::{collections::HashMap, sync::Mutex, time::Duration};

use jiff::Timestamp;
use review_archive::{
	core::{Capture, Coverage, Known, Observed, ReviewId, Scan, Target, TargetId, TargetKind, dto::RunStatus},
	record::{Clock, Recorder},
	sources::ReviewSource,
	store::{NewTarget, Store, blobs::BlobStore},
};

struct FixedClock(Mutex<Timestamp>);

impl FixedClock {
	fn at(s: &str) -> Self {
		Self(Mutex::new(s.parse().unwrap()))
	}

	fn set(&self, s: &str) {
		*self.0.lock().unwrap() = s.parse().unwrap();
	}
}

impl Clock for FixedClock {
	fn now(&self) -> Timestamp {
		*self.0.lock().unwrap()
	}
}

/// Returns whatever scan it was last given, or an error.
struct Scripted(Mutex<Result<Scan, String>>);

impl Scripted {
	fn set(&self, scan: Result<Scan, String>) {
		*self.0.lock().unwrap() = scan;
	}
}

impl ReviewSource for Scripted {
	async fn scan(&self, _: &Target, _: &Known) -> eyre::Result<Scan> {
		self.0.lock().unwrap().clone().map_err(|e| eyre::eyre!(e))
	}
}

fn png() -> Vec<u8> {
	let mut out = Vec::new();
	let mut enc = png::Encoder::new(&mut out, 4, 3);
	enc.set_color(png::ColorType::Rgb);
	enc.write_header().unwrap().write_image_data(&[90; 36]).unwrap();
	out
}

fn review(id: &str, rating: u8, text: &str, est: Option<&str>, capture: bool) -> Observed {
	Observed {
		source_review_id: id.into(),
		author: format!("author of {id}"),
		rating: Some(rating),
		text: Some(text.into()),
		published_raw: Some("a week ago".into()),
		published_est: est.map(|e| e.parse().unwrap()),
		capture: capture.then(|| Capture {
			png: png(),
			captured_at: "2026-09-01T10:00:00Z".parse().unwrap(),
			page_url: "https://www.google.com/maps/place/x".into(),
		}),
		..Default::default()
	}
}

fn scan(reviews: Vec<Observed>, coverage: Coverage) -> Result<Scan, String> {
	Ok(Scan {
		reviews,
		coverage,
		warnings: vec![],
	})
}

struct Env {
	_dir: tempfile::TempDir,
	store: Store,
	blobs: BlobStore,
	clock: FixedClock,
	target: Target,
}

async fn env() -> Env {
	let dir = tempfile::tempdir().unwrap();
	let store = Store::open(&dir.path().join("db.sqlite")).await.unwrap();
	let blobs = BlobStore::new(dir.path().join("blobs"));
	let clock = FixedClock::at("2026-09-01T10:00:00Z");
	let id = store
		.add_target(
			&NewTarget {
				label: "Café test".into(),
				kind: TargetKind::Maps,
				place_id: "ChIJtesttesttesttest".into(),
				gbp: None,
				lang: "fr".into(),
				interval: Duration::from_secs(6 * 3600),
			},
			clock.now(),
		)
		.await
		.unwrap();
	let target = store.target(id).await.unwrap();
	Env {
		_dir: dir,
		store,
		blobs,
		clock,
		target,
	}
}

impl Env {
	fn archive(&self) -> Recorder<'_, FixedClock> {
		Recorder {
			store: &self.store,
			blobs: &self.blobs,
			clock: &self.clock,
		}
	}

	async fn by_source_id(&self) -> HashMap<String, review_archive::core::dto::ReviewDto> {
		self.store
			.reviews(self.target.id, None, None)
			.await
			.unwrap()
			.into_iter()
			.map(|r| (r.source_review_id.clone(), r))
			.collect()
	}
}

#[tokio::test]
async fn new_changed_gone_reappeared() {
	let e = env().await;
	let src = Scripted(Mutex::new(Err(String::new())));

	// 1. first sight of two reviews, one of them captured
	src.set(scan(
		vec![
			review("a", 5, "Great", Some("2026-08-25T10:00:00Z"), true),
			review("b", 2, "Slow", Some("2026-08-01T10:00:00Z"), false),
		],
		Coverage::Complete,
	));
	let s = e.archive().run(&src, &e.target).await.unwrap();
	assert_eq!((s.status, s.counts.new, s.counts.seen, s.captured), (RunStatus::Ok, 2, 2, 1));
	let rows = e.by_source_id().await;
	assert!(!rows["a"].capture_pending && rows["b"].capture_pending);
	let sha = rows["a"].capture_sha256.clone().unwrap();
	let stored = std::fs::read(e.blobs.path_of(&sha).unwrap()).unwrap();
	let info = png::Decoder::new(std::io::Cursor::new(stored)).read_info().unwrap();
	let texts: HashMap<String, String> = info.info().uncompressed_latin1_text.iter().map(|t| (t.keyword.clone(), t.text.clone())).collect();
	assert_eq!(texts["Creation Time"], "2026-09-01T10:00:00Z");
	assert_eq!(texts["Review ID"], "a");
	assert_eq!(texts["Title"], "Café test");
	assert!(e.store.capture_exists(&sha).await.unwrap());

	// 2. b is edited and finally captured; a is not listed on a complete scan → gone
	e.clock.set("2026-09-02T10:00:00Z");
	src.set(scan(vec![review("b", 4, "Slow but kind", Some("2026-08-01T10:00:00Z"), true)], Coverage::Complete));
	let s = e.archive().run(&src, &e.target).await.unwrap();
	assert_eq!((s.counts.new, s.counts.changed, s.counts.gone, s.captured), (0, 1, 1, 1));
	let rows = e.by_source_id().await;
	assert_eq!(rows["a"].gone_at.as_deref(), Some("2026-09-02T10:00:00Z"));
	assert_eq!(rows["b"].rating, Some(4));
	assert!(!rows["b"].capture_pending);
	let versions = e.store.versions(ReviewId(rows["b"].id)).await.unwrap();
	assert_eq!(
		versions.iter().map(|v| (v.2, v.3.clone())).collect::<Vec<_>>(),
		[(Some(2), Some("Slow".into())), (Some(4), Some("Slow but kind".into()))],
		"history keeps the original"
	);

	// 3. a is back: gone cleared, no new version, not re-captured
	e.clock.set("2026-09-03T10:00:00Z");
	src.set(scan(
		vec![review("a", 5, "Great", Some("2026-08-25T10:00:00Z"), true), review("b", 4, "Slow but kind", None, false)],
		Coverage::Complete,
	));
	let s = e.archive().run(&src, &e.target).await.unwrap();
	assert_eq!((s.counts.new, s.counts.changed, s.counts.gone, s.captured), (0, 0, 0, 0));
	let rows = e.by_source_id().await;
	assert_eq!(rows["a"].gone_at, None);
	assert_eq!(rows["a"].last_seen, "2026-09-03T10:00:00Z");
	assert_eq!(e.store.capture_count(ReviewId(rows["a"].id)).await.unwrap(), 1);
	assert_eq!(e.store.versions(ReviewId(rows["a"].id)).await.unwrap().len(), 1);

	// 4. a partial walk that only reached 2026-08-10 says nothing about older b
	e.clock.set("2026-09-04T10:00:00Z");
	src.set(scan(
		vec![review("c", 3, "Ok", Some("2026-09-03T10:00:00Z"), false)],
		Coverage::DownTo(Some("2026-08-10T00:00:00Z".parse().unwrap())),
	));
	let s = e.archive().run(&src, &e.target).await.unwrap();
	// a ("a week ago" as of 2026-08-25, so 2026-08-18 at the earliest) was walked past and is
	// missing → gone; b (2026-08-01) was not reached
	assert_eq!((s.counts.new, s.counts.gone), (1, 1));
	let rows = e.by_source_id().await;
	assert!(rows["a"].gone_at.is_some() && rows["b"].gone_at.is_none());

	// filters used by the API
	let gone = e.store.reviews(e.target.id, None, Some(true)).await.unwrap();
	assert_eq!(gone.iter().map(|r| r.source_review_id.as_str()).collect::<Vec<_>>(), ["a"]);
	let recent = e.store.reviews(e.target.id, Some("2026-09-04T00:00:00Z".parse().unwrap()), None).await.unwrap();
	assert_eq!(recent.iter().map(|r| r.source_review_id.as_str()).collect::<Vec<_>>(), ["c"]);

	// stats per day
	let stats = e.store.stats(Some(e.target.id), None, None).await.unwrap();
	let days: Vec<(&str, i64, i64, i64)> = stats.iter().map(|d| (d.day.as_str(), d.new, d.changed, d.gone)).collect();
	assert_eq!(days, [("2026-09-01", 2, 0, 0), ("2026-09-02", 0, 1, 0), ("2026-09-04", 1, 0, 1)]);
	assert_eq!(stats[0].histogram, [0, 1, 0, 0, 1]);
	assert_eq!(stats[0].mean_rating, Some(3.5));
}

#[tokio::test]
async fn failures_are_runs_and_back_off() {
	let e = env().await;
	let src = Scripted(Mutex::new(Err("blocked by Google".into())));
	for _ in 0..3 {
		let s = e.archive().run(&src, &e.target).await.unwrap();
		assert_eq!(s.status, RunStatus::Failed);
		assert!(s.error.unwrap().contains("blocked by Google"));
	}
	assert_eq!(e.store.last_run(e.target.id).await.unwrap().unwrap().consecutive_failures, 3);

	src.set(Ok(Scan {
		reviews: vec![review("a", 5, "x", None, false)],
		coverage: Coverage::DownTo(None),
		warnings: vec!["screenshot of a failed".into()],
	}));
	let s = e.archive().run(&src, &e.target).await.unwrap();
	assert_eq!(s.status, RunStatus::Partial);
	assert_eq!(e.store.last_run(e.target.id).await.unwrap().unwrap().consecutive_failures, 0);
	assert!(e.by_source_id().await["a"].capture_pending);
}

#[tokio::test]
async fn targets_enable_disable() {
	let e = env().await;
	e.store.set_enabled(e.target.id, false).await.unwrap();
	assert!(!e.store.target(e.target.id).await.unwrap().enabled);
	assert!(e.store.set_enabled(TargetId(999), true).await.is_err());
}

#[tokio::test]
async fn an_empty_complete_scan_is_partial_and_keeps_the_archive() {
	let e = env().await;
	let src = Scripted(Mutex::new(scan(vec![review("a", 5, "x", None, false)], Coverage::Complete)));
	e.archive().run(&src, &e.target).await.unwrap();

	e.clock.set("2026-09-02T10:00:00Z");
	src.set(scan(vec![], Coverage::Complete));
	let s = e.archive().run(&src, &e.target).await.unwrap();
	assert_eq!((s.status, s.counts.gone), (RunStatus::Partial, 0));
	assert!(s.error.unwrap().contains("no reviews at all"));
	assert_eq!(e.by_source_id().await["a"].gone_at, None);
}
