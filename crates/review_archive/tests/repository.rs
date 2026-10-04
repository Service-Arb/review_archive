//! The archive end to end on a temp SQLite: a scripted source, real storage.

use std::{
	cell::Cell,
	collections::HashMap,
	sync::{LazyLock, Mutex},
};

use jiff::Timestamp;
use review_archive::{
	core::{
		Capture, Coverage, Known, Observed, ReviewId, Scan, Target, TargetId, TargetKind,
		dto::{RunStatus, TargetPatch},
		schedule::Schedule,
		tokens::{Meter, Tokens},
	},
	record::Recorder,
	sources::ReviewSource,
	store::{InsertTarget, Store, blobs::BlobStore},
};

static SCHEDULE: LazyLock<Schedule> = LazyLock::new(Schedule::default);
static TOKENS: LazyLock<Tokens> = LazyLock::new(Tokens::default);

thread_local! {
	// `#[tokio::test]` runs each test on a thread of its own
	static NOW: Cell<Timestamp> = const { Cell::new(Timestamp::UNIX_EPOCH) };
}

/// The pinned clock the recorder reads.
fn now() -> Timestamp {
	NOW.get()
}

struct FixedClock;

impl FixedClock {
	fn at(s: &str) -> Self {
		Self.set(s);
		Self
	}

	fn set(&self, s: &str) {
		NOW.set(s.parse().unwrap());
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
	async fn scan(&self, _: &Target, _: &Known, _: &mut Meter) -> eyre::Result<Scan> {
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

/// The stored capture's Exif text fields, by tag name.
fn exif_texts(blobs: &BlobStore, sha: &str) -> HashMap<String, String> {
	let bytes = std::fs::read(blobs.path_of(sha).unwrap()).unwrap();
	let exif = exif::Reader::new().read_from_container(&mut std::io::Cursor::new(bytes)).unwrap();
	exif.fields()
		.filter_map(|f| match &f.value {
			// kamadak-exif names no DocumentName
			exif::Value::Ascii(v) if f.tag == exif::Tag(exif::Context::Tiff, 0x010d) => Some(("DocumentName".into(), String::from_utf8(v[0].clone()).unwrap())),
			exif::Value::Ascii(v) => Some((f.tag.to_string(), String::from_utf8(v[0].clone()).unwrap())),
			_ => None,
		})
		.collect()
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
		cut_after: None,
		listed: None,
		post: None,
	})
}

struct Env {
	dir: tempfile::TempDir,
	store: Store,
	blobs: BlobStore,
	clock: FixedClock,
	target: Target,
}

async fn env() -> Env {
	let dir = tempfile::tempdir().unwrap();
	// laid out as a data dir, so an `Archive` can open it
	let store = Store::open(&dir.path().join("review_archive.db")).await.unwrap();
	let blobs = BlobStore::new(dir.path().join("blobs"));
	let clock = FixedClock::at("2026-09-01T10:00:00Z");
	let id = store
		.add_target(
			&InsertTarget {
				label: "Café test".into(),
				kind: TargetKind::Maps,
				place_id: "ChIJtesttesttesttest".into(),
				gbp: None,
				lang: "fr".into(),
				interval: v_utils::TF_6H,
				enabled: true,
			},
			now(),
		)
		.await
		.unwrap();
	let target = store.target(id).await.unwrap();
	Env { dir, store, blobs, clock, target }
}

impl Env {
	fn archive(&self) -> Recorder<'_> {
		Recorder {
			store: &self.store,
			blobs: &self.blobs,
			now,
			schedule: &SCHEDULE,
			tokens: &TOKENS,
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
	let texts = exif_texts(&e.blobs, &sha);
	assert_eq!(texts["DateTimeOriginal"], "2026:09:01 10:00:00");
	assert_eq!(texts["ImageUniqueID"], "a");
	assert_eq!(texts["ImageDescription"], "Café test");
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
		versions.iter().map(|v| (v.rating, v.text.clone())).collect::<Vec<_>>(),
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
	// a went missing on the 2nd and again on the 4th: both days count it
	assert_eq!(days, [("2026-09-01", 2, 0, 0), ("2026-09-02", 0, 1, 1), ("2026-09-04", 1, 0, 1)]);
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
		cut_after: None,
		listed: None,
		post: None,
	}));
	let s = e.archive().run(&src, &e.target).await.unwrap();
	assert_eq!(s.status, RunStatus::Partial);
	assert_eq!(e.store.last_run(e.target.id).await.unwrap().unwrap().consecutive_failures, 0);
	assert!(e.by_source_id().await["a"].capture_pending);
}

#[tokio::test]
async fn targets_enable_disable() {
	let e = env().await;
	let enabled = |on| TargetPatch {
		enabled: Some(on),
		..Default::default()
	};
	e.store.update_target(e.target.id, &enabled(false)).await.unwrap();
	assert!(!e.store.target(e.target.id).await.unwrap().enabled);
	assert!(e.store.update_target(TargetId(999), &enabled(true)).await.is_err());
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

/// A review as Maps printed its date on one scan.
fn dated(id: &str, text: &str, raw: &str, est: &str) -> Observed {
	Observed {
		source_review_id: id.into(),
		author: format!("author of {id}"),
		rating: Some(4),
		text: Some(text.into()),
		published_raw: Some(raw.into()),
		published_est: Some(est.parse().unwrap()),
		..Default::default()
	}
}

/// First seen as "a year ago" on 2026-09-01, the review was published somewhere in
/// (2024-09-01, 2025-09-01]. Its author edits it; Maps then prints "Edited a day ago". The
/// estimate on record is still the first one (2025-09-01), so the text that says how precise
/// that estimate is must still be the year-wide phrase — otherwise the review is taken to be
/// no older than 2025-08-31 and a walk that stopped at 2025-08-15 "walked past" it.
#[tokio::test]
async fn an_edit_does_not_narrow_how_old_a_review_can_be() {
	let e = env().await;
	let src = Scripted(Mutex::new(scan(vec![dated("old", "Nice", "a year ago", "2025-09-01T10:00:00Z")], Coverage::DownTo(None))));
	e.archive().run(&src, &e.target).await.unwrap();

	e.clock.set("2026-09-02T10:00:00Z");
	src.set(scan(vec![dated("old", "Nice, edited", "Edited a day ago", "2026-09-01T10:00:00Z")], Coverage::DownTo(None)));
	let s = e.archive().run(&src, &e.target).await.unwrap();
	assert_eq!(s.counts.changed, 1);

	// a walk down to a card estimated at 2025-08-15 that did not list it
	e.clock.set("2026-09-03T10:00:00Z");
	src.set(scan(
		vec![dated("other", "Hi", "a day ago", "2026-09-02T10:00:00Z")],
		Coverage::DownTo(Some("2025-08-15T00:00:00Z".parse().unwrap())),
	));
	let s = e.archive().run(&src, &e.target).await.unwrap();
	assert_eq!(s.counts.gone, 0, "the review may be as old as 2024-09-01: the walk did not reach it");
	assert_eq!(e.by_source_id().await["old"].gone_at, None);
}

/// Stats are history: a day on which a review went missing keeps its `gone` count when the
/// review is listed again later (the `review.gone` webhook for that day was sent, too).
#[tokio::test]
async fn a_gone_day_keeps_its_count_after_the_review_reappears() {
	let e = env().await;
	let src = Scripted(Mutex::new(scan(vec![review("a", 5, "x", None, false), review("b", 4, "y", None, false)], Coverage::Complete)));
	e.archive().run(&src, &e.target).await.unwrap();

	e.clock.set("2026-09-02T10:00:00Z");
	src.set(scan(vec![review("b", 4, "y", None, false)], Coverage::Complete));
	assert_eq!(e.archive().run(&src, &e.target).await.unwrap().counts.gone, 1);

	e.clock.set("2026-09-03T10:00:00Z");
	src.set(scan(vec![review("a", 5, "x", None, false), review("b", 4, "y", None, false)], Coverage::Complete));
	e.archive().run(&src, &e.target).await.unwrap();

	let stats = e.store.stats(Some(e.target.id), None, None).await.unwrap();
	let gone_on_the_2nd = stats.iter().find(|d| d.day == "2026-09-02").map(|d| d.gone);
	assert_eq!(gone_on_the_2nd, Some(1), "{stats:?}");
}

/// [`Scripted`] as an ad-hoc capture.
struct AdHoc<'a>(&'a Scripted);

impl ReviewSource for AdHoc<'_> {
	async fn scan(&self, t: &Target, k: &Known, m: &mut Meter) -> eyre::Result<Scan> {
		self.0.scan(t, k, m).await
	}

	fn ad_hoc(&self) -> bool {
		true
	}
}

fn cut(reviews: Vec<Observed>, cut_after: Option<&str>) -> Result<Scan, String> {
	Ok(Scan {
		reviews,
		coverage: Coverage::DownTo(None),
		warnings: vec![],
		cut_after: cut_after.map(str::to_owned),
		listed: None,
		post: None,
	})
}

/// A capture neither ends a target's first, whole-list walk nor moves its schedule.
#[tokio::test]
async fn an_ad_hoc_capture_is_not_the_targets_scan() {
	let e = env().await;
	let src = Scripted(Mutex::new(scan(vec![review("a", 5, "x", None, false)], Coverage::DownTo(None))));
	e.archive().run(&AdHoc(&src), &e.target).await.unwrap();
	assert!(e.store.known(e.target.id).await.unwrap().initial, "still to be walked whole");
	assert_eq!(e.store.last_run(e.target.id).await.unwrap(), None, "not scheduled");

	e.archive().run(&src, &e.target).await.unwrap();
	assert!(!e.store.known(e.target.id).await.unwrap().initial);
	assert!(e.store.last_run(e.target.id).await.unwrap().is_some());
}

/// The count a scan compares its own against is the last scan's: a capture that read a
/// newer one has not archived what that newer count holds.
#[tokio::test]
async fn the_count_to_compare_is_the_last_scans() {
	let e = env().await;
	let listed = || async { e.store.known(e.target.id).await.unwrap().listed };
	let counted = |n| {
		Ok(Scan {
			listed: Some(n),
			..cut(vec![], None).unwrap()
		})
	};
	let src = Scripted(Mutex::new(counted(3)));
	e.archive().run(&src, &e.target).await.unwrap();
	assert_eq!(listed().await, Some(3));

	src.set(counted(4));
	e.archive().run(&AdHoc(&src), &e.target).await.unwrap();
	assert_eq!(listed().await, Some(3));

	e.archive().run(&src, &e.target).await.unwrap();
	assert_eq!(listed().await, Some(4));
}

/// Where a walk was cut short is kept until a scan gets past it; a capture may leave a
/// gap of its own but never covers up one.
#[tokio::test]
async fn a_cut_walk_is_remembered_until_a_scan_gets_past_it() {
	let e = env().await;
	let marker = || async { e.store.known(e.target.id).await.unwrap().cut_after };
	let src = Scripted(Mutex::new(cut(vec![review("a", 5, "x", None, false)], Some("a"))));
	e.archive().run(&src, &e.target).await.unwrap();
	assert_eq!(marker().await.as_deref(), Some("a"));

	src.set(cut(vec![review("b", 5, "y", None, false)], Some("b")));
	e.archive().run(&AdHoc(&src), &e.target).await.unwrap();
	assert_eq!(marker().await.as_deref(), Some("a"), "the older gap is the one to fill");

	src.set(Err("blocked".into()));
	e.archive().run(&src, &e.target).await.unwrap();
	assert_eq!(marker().await.as_deref(), Some("a"), "a failed run changes nothing");

	src.set(cut(vec![], None));
	e.archive().run(&src, &e.target).await.unwrap();
	assert_eq!(marker().await, None);
}

/// A job queue that only grows is refused at its limit; asking twice for the same job
/// queues it once.
#[tokio::test]
async fn the_job_queue_is_bounded_and_deduplicated() {
	use review_archive::core::{
		Rejected,
		dto::{CaptureLimits, JobKind},
	};

	let e = env().await;
	let at = now();
	let scan = e.store.enqueue_job(JobKind::Scan, e.target.id, None, 2, at).await.unwrap();
	assert_eq!(e.store.enqueue_job(JobKind::Scan, e.target.id, None, 2, at).await.unwrap(), scan);
	let five = CaptureLimits {
		max_reviews: Some(5),
		..Default::default()
	};
	e.store.enqueue_job(JobKind::Capture, e.target.id, Some(&five), 2, at).await.unwrap();
	let busy = e.store.enqueue_job(JobKind::Capture, e.target.id, None, 2, at).await.unwrap_err();
	assert!(matches!(busy.downcast_ref::<Rejected>(), Some(Rejected::Busy(_))), "{busy:#}");
}

/// A capture of named reviews answers with those, and its run and job end together.
#[tokio::test]
async fn a_capture_of_named_reviews_returns_those() {
	use review_archive::core::dto::{CaptureLimits, JobKind, JobStatus};

	let e = env().await;
	let only_b = CaptureLimits {
		review_ids: Some(vec!["b".into()]),
		..Default::default()
	};
	let id = e.store.enqueue_job(JobKind::Capture, e.target.id, Some(&only_b), 20, now()).await.unwrap();
	e.store.claim_job(now()).await.unwrap();
	let src = Scripted(Mutex::new(scan(vec![review("a", 5, "x", None, false), review("b", 4, "y", None, false)], Coverage::DownTo(None))));
	e.archive().record(&AdHoc(&src), &e.target, Some(id)).await.unwrap();
	let job = e.store.job(id).await.unwrap().unwrap();
	assert_eq!(job.status, JobStatus::Done);
	assert_eq!(job.reviews.unwrap().iter().map(|r| r.source_review_id.as_str()).collect::<Vec<_>>(), ["b"]);
}

/// A run the process died in is failed on the next start, not left open forever.
#[tokio::test]
async fn an_interrupted_run_is_failed_on_start() {
	let e = env().await;
	let run = e.store.start_run(e.target.id, false, now()).await.unwrap();
	e.store.fail_interrupted(now()).await.unwrap();
	let run = e.store.run(run).await.unwrap().unwrap();
	assert_eq!(run.status, Some(RunStatus::Failed));
	assert!(run.error.unwrap().contains("interrupted"));
}

/// A scan the store cannot write is an error for the caller, and a failed run for the
/// schedule: the target backs off instead of being scanned again at once.
#[tokio::test]
async fn a_scan_the_store_refuses_is_a_failed_run() {
	let e = env().await;
	// the schema allows 1–5 stars
	let src = Scripted(Mutex::new(scan(vec![review("a", 9, "x", None, false)], Coverage::DownTo(None))));
	assert!(e.archive().run(&src, &e.target).await.is_err());
	let last = e.store.last_run(e.target.id).await.unwrap().expect("the run is finished");
	assert_eq!(last.consecutive_failures, 1);
	assert!(e.by_source_id().await.is_empty(), "nothing of it was stored");
}

/// Captures stored as PNG, from before AVIF: converted when the archive opens, keeping the
/// provenance their `tEXt` chunks carry, and the outbox's payloads follow them.
#[tokio::test]
async fn png_captures_become_avif_on_open() {
	use review_archive::core::dto::{Event, NewWebhook};
	use sha2::Digest;

	let e = env().await;
	e.store
		.add_webhook(
			&NewWebhook {
				url: "https://hooks.example/x".into(),
				events: vec![Event::ReviewNew],
				secret: "0123456789abcdef-shh".into(),
			},
			now(),
		)
		.await
		.unwrap();
	let src = Scripted(Mutex::new(scan(vec![review("a", 5, "Great", None, true)], Coverage::DownTo(None))));
	e.archive().run(&src, &e.target).await.unwrap();
	let avif = e.by_source_id().await["a"].capture_sha256.clone().unwrap();

	// back to how a capture was stored before
	let mut old_png = Vec::new();
	let mut enc = png::Encoder::new(&mut old_png, 4, 3);
	enc.set_color(png::ColorType::Rgb);
	for (k, v) in [
		("Creation Time", "2026-08-31T09:00:00Z"),
		("Source", "https://www.google.com/maps/place/x"),
		("Title", "Café test"),
		("Review ID", "a"),
		("Software", "review_archive 0.1.0+old"),
	] {
		enc.add_text_chunk(k.into(), v.into()).unwrap();
	}
	enc.write_header().unwrap().write_image_data(&[90; 36]).unwrap();
	let old = review_archive::core::hex(&sha2::Sha256::digest(&old_png));
	let png_path = e.blobs.path_of(&old).unwrap().with_extension("png");
	std::fs::create_dir_all(png_path.parent().unwrap()).unwrap();
	std::fs::write(&png_path, &old_png).unwrap();
	std::fs::remove_file(e.blobs.path_of(&avif).unwrap()).unwrap();
	let db = sqlx::SqlitePool::connect(&format!("sqlite://{}", e.dir.path().join("review_archive.db").display()))
		.await
		.unwrap();
	sqlx::query("UPDATE captures SET sha256 = ?2 WHERE sha256 = ?1")
		.bind(&avif)
		.bind(&old)
		.execute(&db)
		.await
		.unwrap();
	sqlx::query("UPDATE webhook_deliveries SET payload = REPLACE(REPLACE(payload, '/captures/' || ?1 || '.avif', '/captures/' || ?2 || '.png'), ?1, ?2)")
		.bind(&avif)
		.bind(&old)
		.execute(&db)
		.await
		.unwrap();
	db.close().await;

	let open = || async {
		let config = review_archive::config::Config {
			data_dir: Some(e.dir.path().to_owned()),
			..Default::default()
		};
		review_archive::Archive::open(config).await.unwrap().close().await;
	};
	open().await;
	let new = e.by_source_id().await["a"].capture_sha256.clone().unwrap();
	assert_ne!(new, old);
	assert!(!png_path.exists(), "the PNG is gone");
	let texts = exif_texts(&e.blobs, &new);
	assert_eq!(texts["DateTimeOriginal"], "2026:08:31 09:00:00");
	assert_eq!(texts["DocumentName"], "https://www.google.com/maps/place/x");
	assert_eq!(texts["ImageDescription"], "Café test");
	assert_eq!(texts["ImageUniqueID"], "a");
	assert_eq!(texts["Software"], "review_archive 0.1.0+old");
	let payloads: Vec<String> = e.store.due_deliveries(Timestamp::MAX, 10).await.unwrap().into_iter().map(|d| d.payload).collect();
	assert_eq!(payloads.len(), 1);
	assert!(payloads[0].contains(&format!("\"/captures/{new}.avif\"")) && !payloads[0].contains(&old), "{}", payloads[0]);

	open().await;
	assert_eq!(e.by_source_id().await["a"].capture_sha256.as_deref(), Some(new.as_str()), "a second open does nothing");
}
