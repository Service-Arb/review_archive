//! The client against the real router, served in-process on a temp archive. The browser
//! is never started: the worker's part is played by recording a scripted scan and
//! finishing the job through the store.

use std::{io::Read, sync::Arc, time::Duration};

use jiff::Timestamp;
use reqwest::StatusCode;
use review_archive::{
	Archive,
	config::Config,
	core::{
		Coverage, Known, Observed, Scan, Target,
		dto::{CaptureRequest, Event, JobStatus, NewTarget, NewWebhook, TargetPatch},
	},
	sources::ReviewSource,
};
use review_archive_client::{Captured, Client};
use review_archive_server::{
	http::{AppState, router},
	worker::Signals,
};

const TOKEN: &str = "test-token-0123456789";
const PLACE: &str = "ChIJLU7jZClu5kcR4PcOOO6p3I0";

struct Env {
	_dir: tempfile::TempDir,
	archive: Archive,
	signals: Arc<Signals>,
	base: String,
	client: Client,
	server: tokio::task::JoinHandle<()>,
}

async fn env() -> Env {
	let dir = tempfile::tempdir().unwrap();
	let mut config = Config {
		data_dir: Some(dir.path().to_owned()),
		..Config::default()
	};
	// the webhooks here point at loopback, which only a listed host may
	config.webhooks.allowed_hosts = vec!["127.0.0.1".into()];
	let archive = Archive::open(config).await.unwrap();
	let signals = Arc::new(Signals::default());
	let app = router(AppState::new(archive.clone(), TOKEN, signals.clone()));
	let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
	let base = format!("http://{}", listener.local_addr().unwrap());
	let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
	let client = Client::new(&base, TOKEN).unwrap();
	Env {
		_dir: dir,
		archive,
		signals,
		base,
		client,
		server,
	}
}

struct Listed(Vec<Observed>);

impl ReviewSource for Listed {
	async fn scan(&self, _: &Target, _: &Known) -> eyre::Result<Scan> {
		Ok(Scan {
			reviews: self.0.clone(),
			coverage: Coverage::Complete,
			warnings: vec![],
			cut_after: None,
		})
	}
}

fn review(id: &str) -> Observed {
	Observed {
		source_review_id: id.into(),
		author: format!("author {id}"),
		rating: Some(5),
		text: Some("Lovely".into()),
		..Default::default()
	}
}

/// What the worker would do with the oldest queued job: scan (scripted here) and finish it.
async fn work_one(e: &Env, reviews: Vec<Observed>) -> i64 {
	let store = e.archive.store().unwrap();
	let job = store.claim_job(Timestamp::now()).await.unwrap().expect("a queued job");
	let target = store.target(job.target).await.unwrap();
	let rec = e.archive.record(&Listed(reviews), &target).await.unwrap();
	store.finish_job(job.id, JobStatus::Done, Some(rec.run), None, &rec.seen, Timestamp::now()).await.unwrap();
	e.signals.job_finished.send_modify(|n| *n += 1);
	job.id
}

#[tokio::test]
async fn targets_crud_and_errors() {
	let e = env().await;
	let t = e
		.client
		.add_target(&NewTarget {
			place: Some(PLACE.into()),
			label: Some("Tour Eiffel".into()),
			lang: Some("fr".into()),
			interval: Some("12h".into()),
			..Default::default()
		})
		.await
		.unwrap();
	assert_eq!((t.label.as_str(), t.interval_secs, t.enabled), ("Tour Eiffel", 12 * 3600, true));
	assert_eq!(e.client.targets().await.unwrap(), vec![t.clone()]);

	let detail = e.client.target(t.id).await.unwrap();
	assert_eq!((detail.reviews, detail.last_run), (0, None));
	assert!(detail.next_scan_at.is_some(), "never scanned: due now");

	let patched = e
		.client
		.update_target(
			t.id,
			&TargetPatch {
				label: Some("Eiffel".into()),
				interval: Some("1d".into()),
				..Default::default()
			},
		)
		.await
		.unwrap();
	assert_eq!((patched.label.as_str(), patched.interval_secs, patched.lang.as_str()), ("Eiffel", 86_400, "fr"));
	let disabled = e.client.disable_target(t.id).await.unwrap();
	assert!(!disabled.enabled);
	assert_eq!(e.client.target(t.id).await.unwrap().next_scan_at, None);

	// the caller's mistakes are 4xx with a reason, not 500s
	let too_often = e
		.client
		.update_target(
			t.id,
			&TargetPatch {
				interval: Some("10m".into()),
				..Default::default()
			},
		)
		.await
		.unwrap_err();
	assert_eq!(too_often.status(), Some(StatusCode::BAD_REQUEST));
	assert!(too_often.to_string().contains("at least 1h"), "{too_often}");
	assert_eq!(e.client.target(999).await.unwrap_err().status(), Some(StatusCode::NOT_FOUND));
	assert_eq!(e.client.runs(999, None).await.unwrap_err().status(), Some(StatusCode::NOT_FOUND));
	assert_eq!(e.client.scan(999).await.unwrap_err().status(), Some(StatusCode::NOT_FOUND));
	let not_a_place = e
		.client
		.add_target(&NewTarget {
			place: Some("cafe".into()),
			..Default::default()
		})
		.await
		.unwrap_err();
	assert_eq!(not_a_place.status(), Some(StatusCode::BAD_REQUEST));

	let stranger = Client::new(&e.base, "wrong-token-0000000000").unwrap();
	assert_eq!(stranger.targets().await.unwrap_err().status(), Some(StatusCode::UNAUTHORIZED));
	e.server.abort();
}

#[tokio::test]
async fn scans_and_captures_are_jobs_with_their_reviews() {
	let e = env().await;
	let t = e
		.client
		.add_target(&NewTarget {
			place: Some(PLACE.into()),
			..Default::default()
		})
		.await
		.unwrap();

	// a scan now: queued until the worker takes it
	let job = e.client.scan(t.id).await.unwrap();
	assert_eq!(e.client.job(job).await.unwrap().status, JobStatus::Queued);
	work_one(&e, vec![review("a"), review("b")]).await;
	let done = e.client.job(job).await.unwrap();
	assert_eq!(done.status, JobStatus::Done);
	let reviews = done.reviews.unwrap();
	assert_eq!(reviews.iter().map(|r| r.source_review_id.as_str()).collect::<Vec<_>>(), ["a", "b"]);
	assert_eq!(done.run.unwrap().counts.new, 2);
	let runs = e.client.runs(t.id, Some(5)).await.unwrap();
	assert_eq!(runs.len(), 1);

	let detail = e.client.review(reviews[0].id).await.unwrap();
	assert_eq!(detail.versions.len(), 1);
	assert!(detail.captures.is_empty());

	// an ad-hoc capture of the same place goes under the same target, and ?wait returns it
	let req = CaptureRequest {
		place: Some(PLACE.into()),
		limits: review_archive::core::dto::CaptureLimits {
			max_reviews: Some(5),
			..Default::default()
		},
		..Default::default()
	};
	let waiting = {
		let client = e.client.clone();
		let req = req.clone();
		tokio::spawn(async move { client.capture(&req, Some(10)).await })
	};
	// wait for the capture to be queued, then do the worker's part
	let mut queued = false;
	for _ in 0..100 {
		if e.archive.store().unwrap().job(job + 1).await.unwrap().is_some() {
			queued = true;
			break;
		}
		tokio::time::sleep(Duration::from_millis(20)).await;
	}
	assert!(queued);
	work_one(&e, vec![review("c"), review("a")]).await;
	let Captured::Done(capture) = waiting.await.unwrap().unwrap() else {
		panic!("finished within the wait")
	};
	assert_eq!(capture.target_id, t.id);
	assert_eq!(capture.reviews.unwrap().iter().map(|r| r.source_review_id.as_str()).collect::<Vec<_>>(), ["c", "a"]);

	// no wait: 202 and a job id; a new place gets an implicit, disabled target
	let other = CaptureRequest {
		place: Some("ChIJotherotherotherother".into()),
		..Default::default()
	};
	let Captured::Queued(id) = e.client.capture(&other, None).await.unwrap() else {
		panic!("nothing works the queue here")
	};
	let implicit = e.client.target(e.client.job(id).await.unwrap().target_id).await.unwrap();
	assert!(!implicit.target.enabled && implicit.target.label.starts_with("ad hoc"));
	let bad = e.client.capture(&CaptureRequest::default(), None).await.unwrap_err();
	assert_eq!(bad.status(), Some(StatusCode::BAD_REQUEST));

	// export.zip: the manifest lists the target's reviews
	let zip = e.client.export_zip(t.id, None).await.unwrap();
	let mut archive = zip::ZipArchive::new(std::io::Cursor::new(zip)).unwrap();
	let mut manifest = String::new();
	archive.by_name("manifest.json").unwrap().read_to_string(&mut manifest).unwrap();
	let manifest: serde_json::Value = serde_json::from_str(&manifest).unwrap();
	assert_eq!(manifest["reviews"].as_array().unwrap().len(), 3);
	assert_eq!(e.client.export_zip(999, None).await.unwrap_err().status(), Some(StatusCode::NOT_FOUND));
	e.server.abort();
}

#[tokio::test]
async fn webhooks_and_the_openapi_document() {
	let e = env().await;
	let hook = e
		.client
		.add_webhook(&NewWebhook {
			url: "http://127.0.0.1:9/hook".into(),
			events: vec![Event::ReviewNew, Event::RunFailed],
			secret: "0123456789abcdef".into(),
		})
		.await
		.unwrap();
	assert_eq!(hook.events, [Event::ReviewNew, Event::RunFailed]);
	let listed = e.client.webhooks().await.unwrap();
	assert_eq!(listed, vec![hook.clone()]);
	assert!(!serde_json::to_string(&listed).unwrap().contains("0123456789abcdef"), "the secret is never returned");
	let weak = e
		.client
		.add_webhook(&NewWebhook {
			url: "ftp://example.com".into(),
			events: vec![Event::ReviewNew],
			secret: "0123456789abcdef".into(),
		})
		.await
		.unwrap_err();
	assert_eq!(weak.status(), Some(StatusCode::BAD_REQUEST));
	e.client.delete_webhook(hook.id).await.unwrap();
	assert_eq!(e.client.delete_webhook(hook.id).await.unwrap_err().status(), Some(StatusCode::NOT_FOUND));

	// the document is public and names every route the router serves
	let doc: serde_json::Value = reqwest::get(format!("{}/openapi.json", e.base)).await.unwrap().json().await.unwrap();
	let paths: Vec<&str> = doc["paths"].as_object().unwrap().keys().map(String::as_str).collect();
	for p in [
		"/targets",
		"/targets/{id}",
		"/targets/{id}/reviews",
		"/targets/{id}/runs",
		"/targets/{id}/scan",
		"/targets/{id}/export.zip",
		"/captures",
		"/captures/{sha256}.png",
		"/jobs/{id}",
		"/reviews/{id}",
		"/stats",
		"/webhooks",
		"/webhooks/{id}",
	] {
		assert!(paths.contains(&p), "{p} missing from {paths:?}");
	}
	assert!(doc["components"]["securitySchemes"]["bearer"].is_object());
	e.server.abort();
}

/// A 1×1 PNG: the archive only needs a valid signature and IHDR to tag it.
const PNG_1X1: &[u8] = &[
	0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15,
	0xc4, 0x89, 0x00, 0x00, 0x00, 0x0a, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x00, 0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d, 0xb4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e,
	0x44, 0xae, 0x42, 0x60, 0x82,
];

fn captured(id: &str) -> Observed {
	Observed {
		capture: Some(review_archive::core::Capture {
			png: PNG_1X1.to_vec(),
			captured_at: "2026-09-01T10:00:00Z".parse().unwrap(),
			page_url: "https://www.google.com/maps/place/x".into(),
		}),
		..review(id)
	}
}

async fn add_place(e: &Env) -> i64 {
	e.client
		.add_target(&NewTarget {
			place: Some(PLACE.into()),
			..Default::default()
		})
		.await
		.unwrap()
		.id
}

/// Records a scripted scan of the target, as the worker would.
async fn record(e: &Env, target: i64, reviews: Vec<Observed>) {
	let t = e.archive.target(review_archive::core::TargetId(target)).await.unwrap();
	e.archive.record(&Listed(reviews), &t).await.unwrap();
}

#[tokio::test]
async fn every_route_but_health_and_openapi_wants_the_token() {
	let e = env().await;
	let http = reqwest::Client::new();
	let routes = [
		("GET", "/targets"),
		("POST", "/targets"),
		("GET", "/targets/1"),
		("PATCH", "/targets/1"),
		("DELETE", "/targets/1"),
		("GET", "/targets/1/reviews"),
		("GET", "/targets/1/runs"),
		("POST", "/targets/1/scan"),
		("GET", "/targets/1/export.zip"),
		("POST", "/captures"),
		("GET", &format!("/captures/{}.png", "0".repeat(64))),
		("GET", "/jobs/1"),
		("GET", "/reviews/1"),
		("GET", "/stats"),
		("GET", "/webhooks"),
		("POST", "/webhooks"),
		("DELETE", "/webhooks/1"),
	];
	for (method, path) in routes {
		let resp = http
			.request(method.parse().unwrap(), format!("{}{path}", e.base))
			.bearer_auth("wrong-token-0000000000")
			.json(&serde_json::json!({}))
			.send()
			.await
			.unwrap();
		assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "{method} {path}");
		let resp = http.request(method.parse().unwrap(), format!("{}{path}", e.base)).send().await.unwrap();
		assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "{method} {path} without a token");
	}
	assert_eq!(reqwest::get(format!("{}/health", e.base)).await.unwrap().status(), StatusCode::OK);
	assert_eq!(reqwest::get(format!("{}/openapi.json", e.base)).await.unwrap().status(), StatusCode::OK);
	e.server.abort();
}

/// Every other `/targets/{id}/…` route says 404 for a target that does not exist.
#[tokio::test]
async fn reviews_of_an_unknown_target_are_a_404() {
	let e = env().await;
	let err = e.client.reviews(999, None, None).await.unwrap_err();
	assert_eq!(err.status(), Some(StatusCode::NOT_FOUND), "{err}");
	e.server.abort();
}

/// An interval that parses but does not fit the database is the caller's mistake: 400 with
/// the reason, not a 500 reported to Sentry.
#[tokio::test]
async fn an_interval_too_large_to_store_is_a_400() {
	let e = env().await;
	let huge = "9223372036854775808"; // seconds; one past i64::MAX
	let add = e
		.client
		.add_target(&NewTarget {
			place: Some(PLACE.into()),
			interval: Some(huge.into()),
			..Default::default()
		})
		.await
		.unwrap_err();
	assert_eq!(add.status(), Some(StatusCode::BAD_REQUEST), "POST /targets: {add}");

	let id = add_place(&e).await;
	let patch = e
		.client
		.update_target(
			id,
			&TargetPatch {
				interval: Some(huge.into()),
				..Default::default()
			},
		)
		.await
		.unwrap_err();
	assert_eq!(patch.status(), Some(StatusCode::BAD_REQUEST), "PATCH /targets/{id}: {patch}");
	e.server.abort();
}

/// The API speaks JSON in and out, errors included: a body or a query it cannot read gets
/// the same `{"error": …}` as every other 4xx.
#[tokio::test]
async fn an_unreadable_body_or_query_gets_a_json_error_body() {
	let e = env().await;
	let http = reqwest::Client::new();
	let bad_body = http
		.post(format!("{}/targets", e.base))
		.bearer_auth(TOKEN)
		.header("content-type", "application/json")
		.body("{not json")
		.send()
		.await
		.unwrap();
	assert!(bad_body.status().is_client_error(), "{}", bad_body.status());
	let text = bad_body.text().await.unwrap();
	assert!(serde_json::from_str::<review_archive::core::dto::ErrorBody>(&text).is_ok(), "not an ErrorBody: {text:?}");

	let bad_query = http.get(format!("{}/targets/1/reviews?gone=maybe", e.base)).bearer_auth(TOKEN).send().await.unwrap();
	assert!(bad_query.status().is_client_error(), "{}", bad_query.status());
	let text = bad_query.text().await.unwrap();
	assert!(serde_json::from_str::<review_archive::core::dto::ErrorBody>(&text).is_ok(), "not an ErrorBody: {text:?}");
	e.server.abort();
}

#[tokio::test]
async fn a_capture_is_served_only_once_recorded_and_by_its_exact_name() {
	let e = env().await;
	let t = add_place(&e).await;
	record(&e, t, vec![captured("a")]).await;
	let a = e.client.reviews(t, None, None).await.unwrap().remove(0);
	let url = a.capture_url.clone().expect("a capture was recorded");
	let sha = a.capture_sha256.clone().unwrap();

	let png = e.client.capture_png(&url).await.unwrap();
	assert!(png.starts_with(b"\x89PNG"), "a PNG");
	let blobs = review_archive::store::blobs::BlobStore::new(e._dir.path().join("blobs"));
	assert_eq!(png, std::fs::read(blobs.path_of(&sha).unwrap()).unwrap());

	// a file in the blob dir that no capture row names is not served
	let stray = blobs.put(b"not a recorded capture").await.unwrap();
	assert_eq!(e.client.capture_png(&format!("/captures/{stray}.png")).await.unwrap_err().status(), Some(StatusCode::NOT_FOUND));
	let never_recorded = format!("/captures/{}.png", "0".repeat(64));
	assert_eq!(e.client.capture_png(&never_recorded).await.unwrap_err().status(), Some(StatusCode::NOT_FOUND));
	assert_eq!(e.client.capture_png(&format!("/captures/{sha}")).await.unwrap_err().status(), Some(StatusCode::NOT_FOUND));
	assert_eq!(
		e.client.capture_png(&format!("/captures/{}.png", sha.to_uppercase())).await.unwrap_err().status(),
		Some(StatusCode::NOT_FOUND)
	);
	e.server.abort();
}

#[tokio::test]
async fn export_zip_holds_each_first_capture_under_its_manifest_name() {
	let e = env().await;
	let t = add_place(&e).await;
	record(&e, t, vec![captured("a"), review("b")]).await;
	let a = e.client.reviews(t, None, None).await.unwrap().into_iter().find(|r| r.source_review_id == "a").unwrap();

	let zip = e.client.export_zip(t, None).await.unwrap();
	let mut archive = zip::ZipArchive::new(std::io::Cursor::new(zip)).unwrap();
	let mut manifest = String::new();
	archive.by_name("manifest.json").unwrap().read_to_string(&mut manifest).unwrap();
	let manifest: serde_json::Value = serde_json::from_str(&manifest).unwrap();
	let pngs: Vec<(String, Option<String>)> = manifest["reviews"]
		.as_array()
		.unwrap()
		.iter()
		.map(|r| (r["source_review_id"].as_str().unwrap().to_owned(), r["png"].as_str().map(str::to_owned)))
		.collect();
	let a_png = pngs.iter().find(|(id, _)| id == "a").unwrap().1.clone().expect("a has a PNG in the export");
	assert!(a_png.starts_with("captures/") && a_png.ends_with(".png"), "{a_png}");
	assert_eq!(pngs.iter().find(|(id, _)| id == "b").unwrap().1, None, "no capture, no file");
	assert_eq!(archive.len(), 2, "the manifest and a's PNG");

	let mut in_zip = Vec::new();
	archive.by_name(&a_png).unwrap().read_to_end(&mut in_zip).unwrap();
	assert_eq!(in_zip, e.client.capture_png(a.capture_url.as_deref().unwrap()).await.unwrap());

	// `since` filters on first sight: nothing was first seen in the future
	let later = e.client.export_zip(t, Some("2999-01-01")).await.unwrap();
	let mut later = zip::ZipArchive::new(std::io::Cursor::new(later)).unwrap();
	let mut manifest = String::new();
	later.by_name("manifest.json").unwrap().read_to_string(&mut manifest).unwrap();
	let manifest: serde_json::Value = serde_json::from_str(&manifest).unwrap();
	assert_eq!(manifest["reviews"].as_array().unwrap().len(), 0);
	assert_eq!(e.client.export_zip(t, Some("yesterday")).await.unwrap_err().status(), Some(StatusCode::BAD_REQUEST));
	e.server.abort();
}

#[tokio::test]
async fn patch_changes_only_what_it_names_and_delete_only_disables() {
	let e = env().await;
	let t = e
		.client
		.add_target(&NewTarget {
			place: Some(PLACE.into()),
			label: Some("Eiffel".into()),
			lang: Some("fr".into()),
			..Default::default()
		})
		.await
		.unwrap();

	let same = e.client.update_target(t.id, &TargetPatch::default()).await.unwrap();
	assert_eq!(same, t, "an empty patch changes nothing");

	let gone = e.client.disable_target(t.id).await.unwrap();
	let gone_again = e.client.disable_target(t.id).await.unwrap();
	assert_eq!((gone.enabled, gone_again.enabled), (false, false), "DELETE is idempotent");
	assert_eq!(e.client.targets().await.unwrap().len(), 1, "a deleted target is kept");

	let back = e
		.client
		.update_target(
			t.id,
			&TargetPatch {
				enabled: Some(true),
				..Default::default()
			},
		)
		.await
		.unwrap();
	assert_eq!((back.enabled, back.label.as_str(), back.lang.as_str()), (true, "Eiffel", "fr"));

	assert_eq!(e.client.update_target(999, &TargetPatch::default()).await.unwrap_err().status(), Some(StatusCode::NOT_FOUND));
	assert_eq!(e.client.disable_target(999).await.unwrap_err().status(), Some(StatusCode::NOT_FOUND));
	let bad = e
		.client
		.update_target(
			t.id,
			&TargetPatch {
				interval: Some("soon".into()),
				..Default::default()
			},
		)
		.await
		.unwrap_err();
	assert_eq!(bad.status(), Some(StatusCode::BAD_REQUEST));
	e.server.abort();
}

#[tokio::test]
async fn stats_come_back_as_json_through_the_client_and_as_csv_on_request() {
	let e = env().await;
	let t = add_place(&e).await;
	record(&e, t, vec![review("a"), review("b")]).await;
	// the UTC day the archive recorded them on
	let today = e.client.reviews(t, None, None).await.unwrap()[0].first_seen[..10].to_owned();

	let rows = e.client.stats(Some(t), Some(&today), Some(&today)).await.unwrap();
	assert_eq!(rows.len(), 1, "{rows:?}");
	assert_eq!((rows[0].day.as_str(), rows[0].new, rows[0].histogram), (today.as_str(), 2, [0, 0, 0, 0, 2]));
	assert_eq!(rows[0].mean_rating, Some(5.0));
	assert!(e.client.stats(Some(t), Some("2999-01-01"), None).await.unwrap().is_empty());

	let csv = reqwest::Client::new()
		.get(format!("{}/stats?target={t}", e.base))
		.bearer_auth(TOKEN)
		.header("accept", "text/csv")
		.send()
		.await
		.unwrap();
	assert!(csv.headers()["content-type"].to_str().unwrap().starts_with("text/csv"));
	let csv = csv.text().await.unwrap();
	assert_eq!(
		csv.lines().collect::<Vec<_>>(),
		[
			"target_id,day,new,changed,gone,mean_rating,stars_1,stars_2,stars_3,stars_4,stars_5",
			&format!("{t},{today},2,0,0,5.000,0,0,0,0,2"),
		]
	);
	e.server.abort();
}

/// `?wait=` far beyond the 120 s cap is capped, not added to the clock as is.
#[tokio::test]
async fn a_wait_beyond_the_cap_still_answers_when_the_job_ends() {
	let e = env().await;
	add_place(&e).await;
	let req = CaptureRequest {
		place: Some(PLACE.into()),
		..Default::default()
	};
	let waiting = {
		let client = e.client.clone();
		tokio::spawn(async move { client.capture(&req, Some(u64::MAX)).await })
	};
	let store = e.archive.store().unwrap();
	let mut queued = false;
	for _ in 0..100 {
		if store.job(1).await.unwrap().is_some() {
			queued = true;
			break;
		}
		tokio::time::sleep(Duration::from_millis(20)).await;
	}
	assert!(queued, "the capture was queued");
	work_one(&e, vec![review("a")]).await;
	let got = tokio::time::timeout(Duration::from_secs(10), waiting).await.expect("answered once the job ended").unwrap();
	assert!(matches!(got, Ok(Captured::Done(ref job)) if job.status == JobStatus::Done), "{got:?}");
	e.server.abort();
}
