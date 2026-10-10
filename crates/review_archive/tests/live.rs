//! One real scan of one real place. Needs a browser and the network, so it only runs
//! by hand: `cargo test --test live -- --ignored`.
//!
//! `REVIEW_ARCHIVE_CHROME` points at the browser (the devShell sets it),
//! `REVIEW_ARCHIVE_HEADFUL=1` opens a window, and `REVIEW_ARCHIVE_PROFILE` reuses a browser
//! profile instead of a fresh one. Google may answer with a "limited view" of Maps that
//! has no reviews (headless, or a fresh profile), and a profile signed out of Google fails;
//! the scan then says so with the page saved.

use review_archive::{
	Archive, CaptureRequest, SessionError,
	config::Config,
	core::dto::{NewTarget, ReviewsQuery, RunStatus},
};

/// The Eiffel Tower: public, and reviewed every few minutes.
const PLACE_ID: &str = "ChIJLU7jZClu5kcR4PcOOO6p3I0";

fn config(dir: &std::path::Path) -> Config {
	let mut config = Config {
		data_dir: Some(dir.to_owned()),
		..Config::default()
	};
	config.browser.executable = std::env::var_os("REVIEW_ARCHIVE_CHROME").map(Into::into);
	config.browser.headful = std::env::var("REVIEW_ARCHIVE_HEADFUL").is_ok_and(|v| v == "1");
	config.browser.profile_dir = std::env::var_os("REVIEW_ARCHIVE_PROFILE").map(Into::into);
	config.defaults.max_reviews_initial = 12;
	config.defaults.max_reviews_per_scan = 12;
	config.defaults.lang = "fr".into();
	config
}

#[tokio::test]
#[ignore = "live: needs Chrome and the network"]
async fn live_scan_of_a_real_place() {
	let dir = tempfile::tempdir().unwrap();
	let archive = Archive::open(config(dir.path())).await.unwrap();
	let added = archive
		.add_target(&NewTarget {
			place: Some(PLACE_ID.into()),
			label: Some("Tour Eiffel".into()),
			interval: Some("6h".into()),
			..Default::default()
		})
		.await
		.unwrap();
	let id = added.target.id;
	let summary = archive.scan_target(id).await.unwrap();
	archive.close().await;
	println!("{summary}");

	// What Google serves the browser is its call; each answer must come out as ARCHITECTURE's invariants say.
	let error = summary.error.as_deref();
	if summary.status == RunStatus::Failed {
		let error = error.expect("a failed run says why");
		assert!(error.contains("code: review_archive::google::"), "{summary}");
		assert!(error.contains("-walk-failed.png]"), "{summary}");
		return;
	}
	assert_eq!(error, None, "{summary}");
	assert_eq!(summary.counts.new, 12, "{summary}");
	assert!(summary.captured >= summary.counts.new * 5 / 6, "{summary}");
	let reviews = archive.reviews(id, &ReviewsQuery::default()).await.unwrap();
	assert!(reviews.iter().all(|r| r.rating.is_some() && r.published_est.is_some()));
	let captured = reviews.iter().find(|r| r.capture_sha256.is_some()).unwrap();
	assert!(archive.review(review_archive::core::ReviewId(captured.id)).await.unwrap().captures[0].width >= 600);
}

/// The library without a store: reviews and WebPs in memory.
#[tokio::test]
#[ignore = "live: needs Chrome and the network"]
async fn live_capture_without_a_store() {
	let dir = tempfile::tempdir().unwrap();
	let mut config = config(dir.path());
	config.browser.profile_dir = config.browser.profile_dir.or_else(|| Some(dir.path().join("profile")));
	config.data_dir = None;
	let archive = Archive::open(config).await.unwrap();
	let got = archive.capture_place(&CaptureRequest::new(PLACE_ID).lang("fr").max_reviews(5)).await;
	archive.close().await;
	let got = match got {
		Ok(got) => got,
		Err(e) => {
			assert!(
				matches!(
					e.downcast_ref::<SessionError>(),
					Some(SessionError::LimitedView { .. } | SessionError::Blocked { .. } | SessionError::SignedOut { .. })
				),
				"{}",
				review_archive::describe(&e)
			);
			return;
		}
	};
	assert_eq!(got.scan.reviews.len(), 5, "{:?}", got.scan.warnings);
	assert!(got.scan.reviews.iter().all(|r| r.capture.is_none()), "taken out into webps");
	assert!(got.webps.values().next().unwrap().width >= 600);
	assert!(archive.store().is_err());
}
