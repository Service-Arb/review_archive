//! One real scan of one real place. Needs a browser and the network, so it only runs
//! by hand: `cargo test --test live -- --ignored`.
//!
//! `REVIEW_ARCHIVE_CHROME` points at the browser (else it is looked up on PATH),
//! `REVIEW_ARCHIVE_HEADFUL=1` opens a window, and `REVIEW_ARCHIVE_PROFILE` reuses a browser
//! profile instead of a fresh one. Google may answer with a "limited view" of Maps that
//! has no reviews (headless, or a fresh profile); the scan then fails and says so.

use std::time::Duration;

use jiff::Timestamp;
use review_archive::{
	config::Config,
	domain::TargetKind,
	runner::Runner,
	store::{NewTarget, RunStatus, Store},
};

/// The Eiffel Tower: public, and reviewed every few minutes.
const PLACE_ID: &str = "ChIJLU7jZClu5kcR4PcOOO6p3I0";

#[tokio::test]
#[ignore = "live: needs Chrome and the network"]
async fn live_scan_of_a_real_place() {
	let dir = tempfile::tempdir().unwrap();
	let mut config = Config {
		data_dir: dir.path().to_owned(),
		..Config::default()
	};
	config.browser.executable = std::env::var_os("REVIEW_ARCHIVE_CHROME").map(Into::into);
	config.browser.headful = std::env::var("REVIEW_ARCHIVE_HEADFUL").is_ok_and(|v| v == "1");
	config.browser.profile_dir = std::env::var_os("REVIEW_ARCHIVE_PROFILE").map(Into::into);
	config.defaults.max_reviews_initial = 12;
	config.defaults.max_reviews_per_scan = 12;

	let store = Store::open(&config.db_path()).await.unwrap();
	let id = store
		.add_target(
			&NewTarget {
				label: "Tour Eiffel".into(),
				kind: TargetKind::Maps,
				place_id: PLACE_ID.into(),
				gbp: None,
				lang: "fr".into(),
				interval: Duration::from_secs(6 * 3600),
			},
			Timestamp::now(),
		)
		.await
		.unwrap();
	let target = store.target(id).await.unwrap();
	let runner = Runner::new(store.clone(), config, reqwest::Client::new(), None);
	let summary = runner.scan(&target).await.unwrap();
	runner.end_pass().await;
	println!("{summary}");

	assert_ne!(summary.status, RunStatus::Failed, "{summary}");
	assert_eq!(summary.counts.new, 12);
	assert!(summary.captured >= 10, "{summary}");
	let reviews = store.reviews(id, None, None).await.unwrap();
	assert!(reviews.iter().all(|r| r.rating.is_some() && r.published_est.is_some()));
	let sha = reviews.iter().find_map(|r| r.capture_sha256.clone()).unwrap();
	let png = std::fs::read(runner.blobs.path_of(&sha).unwrap()).unwrap();
	assert!(review_archive::png_meta::dimensions(&png).unwrap().0 >= 400);
}
