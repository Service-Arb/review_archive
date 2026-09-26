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
	let archive = Archive::open(Config {
		data_dir: Some(dir.path().to_owned()),
		..Config::default()
	})
	.await
	.unwrap();
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
		max_reviews: Some(5),
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
