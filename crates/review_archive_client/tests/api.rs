//! The client against the real router, served in-process on a temp archive. The browser
//! is never started: the worker's part is played by recording a scripted scan and
//! finishing the job through the store.

use std::{io::Read, sync::Arc, time::Duration};

use axum::response::IntoResponse;
use jiff::Timestamp;
use reqwest::StatusCode;
use review_archive::{
	Archive,
	config::Config,
	core::{
		Coverage, Known, Observed, OwnerPost, Scan, Target,
		dto::{BalanceChange, CaptureRequest, Event, JobStatus, Me, MemberDto, NewTarget, NewTgChannel, NewTrack, NewWebhook, TargetPatch, TokenKind, TokensChange, TokensDto},
		tokens::Meter,
	},
	sources::ReviewSource,
};
use review_archive_client::{Captured, Client};
use review_archive_server::{
	auth::{Auth, SsoSite},
	http::{AppState, HttpConfig, router},
	worker::Signals,
};

const TOKEN: &str = "test-token-0123456789";
/// valeratrades.com's side: the key its `va_access` cookies are signed with.
const SSO_PRIVATE: &str = "-----BEGIN PRIVATE KEY-----\nMC4CAQAwBQYDK2VwBCIEIA3bBKSXvm87i5bc706Y1QG1uj5EmbgUZygHJGfO1XYj\n-----END PRIVATE KEY-----\n";
const SSO_PUBLIC: &str = "-----BEGIN PUBLIC KEY-----\nMCowBQYDK2VwAyEAws8sYuYGZt4/OjCm05rzUQYOTAWBxVHPL1Fdg74KyV4=\n-----END PUBLIC KEY-----\n";
const ALICE: &str = "Alice@x.com";
const BOB: &str = "bob@x.com";
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
	// valeratrades.com's `/auth/members`, as far as the archive sees it: answers whoever forwards a cookie
	let site = axum::Router::new().route(
		"/auth/members",
		axum::routing::get(|headers: axum::http::HeaderMap| async move {
			match headers.get("cookie").is_some_and(|c| c.to_str().unwrap().starts_with("va_access=")) {
				true => axum::Json(serde_json::json!([{"email": BOB, "username": "bob", "display_name": "Bob B"}])).into_response(),
				false => axum::http::StatusCode::UNAUTHORIZED.into_response(),
			}
		}),
	);
	let site_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
	let refresh = format!("http://{}/auth/refresh", site_listener.local_addr().unwrap());
	tokio::spawn(async move { axum::serve(site_listener, site).await.unwrap() });
	let auth = Auth::new(TOKEN, Some(SsoSite::new(va_sso::Verifier::try_new(SSO_PUBLIC).unwrap(), &refresh).unwrap()), None);
	let app = router(AppState::new(archive.clone(), auth, signals.clone(), HttpConfig::default()), None, None);
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

impl Env {
	/// The client as a browser on the archive's pages, signed in as a `service-arb` member.
	fn member(&self, email: &str) -> Client {
		self.signed_in(email, false, &["service-arb"], "same-origin")
	}

	fn signed_in(&self, email: &str, admin: bool, groups: &[&str], fetch_site: &str) -> Client {
		let claims = va_sso::Claims {
			sub: email.into(),
			email: email.into(),
			username: email.into(),
			admin,
			groups: groups.iter().map(|g| (*g).to_owned()).collect(),
			exp: Timestamp::now().as_second() + 900,
		};
		let cookie = format!("{}={}", va_sso::COOKIE, va_sso::mint(SSO_PRIVATE, claims).unwrap());
		let headers = reqwest::header::HeaderMap::from_iter([
			(reqwest::header::COOKIE, cookie.parse().unwrap()),
			("sec-fetch-site".parse().unwrap(), fetch_site.parse().unwrap()),
		]);
		Client::ambient(reqwest::Client::builder().default_headers(headers).build().unwrap(), &self.base).unwrap()
	}
}

struct Listed(Vec<Observed>);

impl ReviewSource for Listed {
	async fn scan(&self, _: &Target, _: &Known, _: &mut Meter) -> eyre::Result<Scan> {
		Ok(Scan {
			reviews: self.0.clone(),
			coverage: Coverage::Complete,
			warnings: vec![],
			cut_after: None,
			listed: Some(u64::try_from(self.0.len()).unwrap()),
			post: None,
		})
	}
}

/// Lists nothing it could judge by, and shows an owner's post.
struct Posted(OwnerPost);

impl ReviewSource for Posted {
	async fn scan(&self, _: &Target, _: &Known, _: &mut Meter) -> eyre::Result<Scan> {
		Ok(Scan {
			reviews: vec![],
			coverage: Coverage::DownTo(None),
			warnings: vec![],
			cut_after: None,
			listed: None,
			post: Some(self.0.clone()),
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
		("GET", "/me/overview"),
		("POST", "/me/gmails"),
		("DELETE", "/me/gmails/1"),
		("POST", "/me/gmails/1/tracks"),
		("DELETE", "/me/gmails/1/tracks/1"),
		("GET", "/me/gmails/1/locations/1/board"),
		("PUT", "/me/gmails/1/reinstatements/1"),
		("DELETE", "/me/gmails/1/reinstatements/1"),
		("GET", "/me/tg-channels"),
		("POST", "/me/tg-channels"),
		("DELETE", "/me/tg-channels/1"),
		("POST", "/me/tg-channels/1/test"),
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

/// The operator's routes are not a member's, and `/me` is not the operator token's; a
/// site admin signed in gets both. Someone signed in outside `service-arb` gets neither.
#[tokio::test]
async fn members_and_the_operator_keep_to_their_routes() {
	let e = env().await;
	let alice = e.member(ALICE);
	assert_eq!(alice.targets().await.unwrap_err().status(), Some(StatusCode::FORBIDDEN));
	assert_eq!(alice.webhooks().await.unwrap_err().status(), Some(StatusCode::FORBIDDEN));
	assert_eq!(e.client.overview().await.unwrap_err().status(), Some(StatusCode::FORBIDDEN));
	assert_eq!(alice.overview().await.unwrap(), vec![]);

	let admin = e.signed_in("root@x.com", true, &[], "same-origin");
	assert_eq!(admin.targets().await.unwrap(), vec![]);
	assert_eq!(admin.overview().await.unwrap(), vec![]);

	let stranger = e.signed_in("stranger@x.com", false, &["another-group"], "same-origin");
	assert_eq!(stranger.overview().await.unwrap_err().status(), Some(StatusCode::FORBIDDEN));
	e.server.abort();
}

/// An admin acts as any member through `X-Member`: their overview, their writes. No one else
/// may; without it, everyone is themselves; `/me` is always who signed in.
#[tokio::test]
async fn an_admin_acts_as_a_member_and_no_one_else_may() {
	let e = env().await;
	let alice = e.member(ALICE);
	alice.add_gmail("alice@gmail.com").await.unwrap();
	let admin = e.signed_in("root@x.com", true, &[], "same-origin");
	assert_eq!(admin.overview().await.unwrap(), vec![], "without the header, the admin's own");
	let as_alice = admin.clone().as_member(ALICE);
	assert_eq!(as_alice.overview().await.unwrap(), alice.overview().await.unwrap());
	as_alice.add_gmail("ops@gmail.com").await.unwrap();
	assert_eq!(alice.overview().await.unwrap().len(), 2, "the admin's write is alice's");
	assert_eq!(e.client.clone().as_member(ALICE).overview().await.unwrap().len(), 2, "the operator's token acts too");
	assert_eq!(e.member(BOB).as_member(ALICE).overview().await.unwrap_err().status(), Some(StatusCode::FORBIDDEN));

	let root = Me {
		email: "root@x.com".into(),
		username: "root@x.com".into(),
		admin: true,
		tokens: TokensDto { balance: 15, daily: 15, cap: 300 },
	};
	assert_eq!(as_alice.me().await.unwrap(), root);
	assert!(!alice.me().await.unwrap().admin);
	assert_eq!(e.client.me().await.unwrap_err().status(), Some(StatusCode::FORBIDDEN), "the token is no one");

	let bob = MemberDto {
		email: BOB.into(),
		username: Some("bob".into()),
		display_name: Some("Bob B".into()),
		balance: 15,
	};
	assert_eq!(admin.members().await.unwrap(), vec![bob]);
	assert_eq!(alice.members().await.unwrap_err().status(), Some(StatusCode::FORBIDDEN));
	assert_eq!(e.client.members().await.unwrap_err().status(), Some(StatusCode::FORBIDDEN), "no cookie to ask the site with");
	e.server.abort();
}

/// A member's balance is an admin's to set, and theirs to read with every change that made it.
#[tokio::test]
async fn an_admin_sets_a_members_tokens() {
	let e = env().await;
	let alice = e.member(ALICE);
	assert_eq!(alice.me().await.unwrap().tokens.balance, 15, "a day's worth on first sight");
	let set = |n| TokensChange {
		change: BalanceChange::Set(n),
		note: Some("trial".into()),
	};
	assert_eq!(alice.change_tokens(ALICE, &set(1000)).await.unwrap_err().status(), Some(StatusCode::FORBIDDEN));
	let admin = e.signed_in("root@x.com", true, &[], "same-origin");
	assert_eq!(admin.change_tokens(ALICE, &set(8)).await.unwrap().balance, 8);
	assert_eq!(alice.me().await.unwrap().tokens.balance, 8);
	let ledger = alice.ledger().await.unwrap();
	assert_eq!(
		ledger.iter().map(|l| (l.kind, l.delta, l.by.as_deref())).collect::<Vec<_>>(),
		[(TokenKind::Set, -7, Some("root@x.com")), (TokenKind::Accrual, 15, None)]
	);
	let bad = TokensChange {
		change: BalanceChange::Grant(0),
		note: None,
	};
	assert_eq!(admin.change_tokens(ALICE, &bad).await.unwrap_err().status(), Some(StatusCode::BAD_REQUEST));
	e.server.abort();
}

/// `serve --dev-member`: a browser with no sign-in is that member, so a dashboard runs
/// without valeratrades.com; a token or a cookie still says who it is.
#[tokio::test]
async fn a_dev_member_stands_in_for_a_missing_sign_in() {
	let dir = tempfile::tempdir().unwrap();
	let archive = Archive::open(Config {
		data_dir: Some(dir.path().to_owned()),
		..Config::default()
	})
	.await
	.unwrap();
	archive
		.add_gmail("test@x.com", &review_archive::core::dto::NewGmail { gmail: "ops@gmail.com".into() })
		.await
		.unwrap();
	let auth = Auth::new(TOKEN, None, Some("test@x.com".into()));
	let mfe = tempfile::tempdir().unwrap();
	let app = router(AppState::new(archive, auth, Arc::new(Signals::default()), HttpConfig::default()), Some(mfe.path()), Some("/"));
	let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
	let base = format!("http://{}", listener.local_addr().unwrap());
	let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

	let nobody = Client::ambient(reqwest::Client::new(), &base).unwrap();
	assert_eq!(nobody.overview().await.unwrap()[0].gmail.gmail, "ops@gmail.com");
	assert_eq!(nobody.me().await.unwrap().email, "test@x.com");
	assert_eq!(nobody.targets().await.unwrap_err().status(), Some(StatusCode::FORBIDDEN), "a member, not the operator");
	assert_eq!(
		Client::new(&base, TOKEN).unwrap().overview().await.unwrap_err().status(),
		Some(StatusCode::FORBIDDEN),
		"the token is still the token"
	);
	for view in [
		"/",
		"/telegram",
		"/tokens",
		"/gmails/1",
		"/gmails/1/places/2",
		"/members/bob@x.com",
		"/members/bob@x.com/",
		"/members/bob@x.com/gmails/1/places/2",
	] {
		let page = reqwest::get(format!("{base}{view}")).await.unwrap();
		assert_eq!(page.status(), StatusCode::OK, "{view} is the dashboard's page");
		assert!(page.text().await.unwrap().contains("mfe-review-archive-dashboard"));
	}
	server.abort();
}

/// A sign-in cookie reads from anywhere it is sent, but writes only from the archive's own
/// pages; one signed by another key, or expired, is no sign-in.
#[tokio::test]
async fn a_cookie_writes_only_from_this_origin_and_only_while_valid() {
	let e = env().await;
	let elsewhere = e.signed_in(ALICE, false, &["service-arb"], "cross-site");
	assert_eq!(elsewhere.overview().await.unwrap(), vec![]);
	assert_eq!(elsewhere.add_gmail("ops@gmail.com").await.unwrap_err().status(), Some(StatusCode::FORBIDDEN));
	e.member(ALICE).add_gmail("ops@gmail.com").await.unwrap();

	let http = reqwest::Client::new();
	let ask = async |cookie: String| http.get(format!("{}/me/overview", e.base)).header("cookie", cookie).send().await.unwrap().status();
	let claims = |exp: i64| va_sso::Claims {
		sub: "a".into(),
		email: ALICE.into(),
		username: "a".into(),
		admin: false,
		groups: vec!["service-arb".into()],
		exp,
	};
	let other_key = "-----BEGIN PRIVATE KEY-----\nMC4CAQAwBQYDK2VwBCIEIHz7B0H6oZ1y3Yb8hB5o2c0X9m1wV6o1b8wXcY0f3s1a\n-----END PRIVATE KEY-----\n";
	let forged = va_sso::mint(other_key, claims(Timestamp::now().as_second() + 900)).unwrap();
	assert_eq!(ask(format!("va_access={forged}")).await, StatusCode::UNAUTHORIZED);
	let expired = va_sso::mint(SSO_PRIVATE, claims(Timestamp::now().as_second() - 3600)).unwrap();
	assert_eq!(ask(format!("va_access={expired}")).await, StatusCode::UNAUTHORIZED);
	e.server.abort();
}

/// Two members on one place share its target and its scans; each sees only their own
/// gmails, boards and screenshots, and the operator sees every screenshot.
#[tokio::test]
async fn members_share_places_but_see_only_their_own() {
	let e = env().await;
	let (alice, bob) = (e.member(ALICE), e.member(BOB));
	let a = alice.add_gmail(" Ops.Paris@gmail.com ").await.unwrap();
	assert_eq!(a.gmail, "ops.paris@gmail.com");
	assert_eq!(alice.add_gmail("ops.paris@gmail.com").await.unwrap_err().status(), Some(StatusCode::BAD_REQUEST), "twice");
	assert_eq!(alice.add_gmail("two words").await.unwrap_err().status(), Some(StatusCode::BAD_REQUEST));
	assert_eq!(
		e.member("carol@x.com").add_gmail(" tg:@Owner ").await.unwrap().gmail,
		"tg:@owner",
		"an alias groups as well as an address"
	);
	let b = bob.add_gmail("bob@gmail.com").await.unwrap();

	let track = NewTrack {
		place: PLACE.into(),
		lang: Some("fr".into()),
		..Default::default()
	};
	let t = alice.track(a.id, &track).await.unwrap();
	assert_eq!(bob.track(b.id, &track).await.unwrap().id, t.id, "one place, one target");
	assert_eq!(e.client.targets().await.unwrap().len(), 1);
	assert_eq!(bob.track(a.id, &track).await.unwrap_err().status(), Some(StatusCode::NOT_FOUND), "alice's gmail is not bob's");

	// the overview counts screenshots by when they were taken: one today, one a fortnight ago
	let taken = |id: &str, days_ago: i64| {
		let mut o = captured(id);
		o.capture.as_mut().unwrap().captured_at = Timestamp::now() - jiff::SignedDuration::from_hours(24 * days_ago);
		o
	};
	let replied = Observed {
		reply: Some("Thanks".into()),
		published_est: Some(Timestamp::now() - jiff::SignedDuration::from_hours(24 * 3)),
		..review("r3")
	};
	record(&e, t.id, vec![taken("r1", 0), taken("r2", 14), replied]).await;
	let overview = alice.overview().await.unwrap();
	assert_eq!(overview.len(), 1);
	let loc = &overview[0].locations[0];
	assert_eq!(
		(
			loc.target.id, loc.snapshots_7d, loc.snapshots_30d, loc.new_7d, loc.live, loc.responded, loc.removed, loc.reinstating
		),
		(t.id, 1, 2, 1, 3, 1, 0, 0)
	);
	assert_eq!(loc.listed, Some(3), "what the source says the place has");
	assert_eq!((loc.posts_7d, loc.latest_post.clone()), (0, None));

	let target = e.archive.target(review_archive::core::TargetId(t.id)).await.unwrap();
	let post = |text: &str, days_ago: i64| {
		Posted(OwnerPost {
			text: text.into(),
			published_raw: Some(format!("{days_ago} days ago")),
			published_est: Some(Timestamp::now() - jiff::SignedDuration::from_hours(24 * days_ago)),
		})
	};
	for p in [post("Closed for renovation", 12), post("Open on Sunday", 1), post("Open on Sunday", 1)] {
		e.archive.record(&p, &target).await.unwrap();
	}
	let loc = alice.overview().await.unwrap()[0].locations[0].clone();
	assert_eq!(loc.posts_7d, 1, "one post, seen twice, in the last week");
	assert_eq!(loc.latest_post.map(|p| p.text).as_deref(), Some("Open on Sunday"));
	assert_eq!(loc.last_run_status, Some(review_archive::core::dto::RunStatus::Ok));

	let board = alice.board(a.id, t.id).await.unwrap();
	let url = board.snapshotted.iter().find_map(|c| c.review.capture_url.clone()).expect("r1 was captured");
	assert!(alice.capture_png(&url).await.unwrap().starts_with(b"\x89PNG"));
	assert!(e.client.capture_png(&url).await.is_ok(), "the operator sees everything");
	assert_eq!(bob.board(a.id, t.id).await.unwrap_err().status(), Some(StatusCode::NOT_FOUND));

	// once bob stops tracking the place, its screenshots are no longer his to see
	assert!(bob.capture_png(&url).await.is_ok());
	bob.untrack(b.id, t.id).await.unwrap();
	assert_eq!(bob.capture_png(&url).await.unwrap_err().status(), Some(StatusCode::NOT_FOUND));
	assert_eq!(bob.overview().await.unwrap()[0].locations, vec![]);
	assert_eq!(alice.overview().await.unwrap()[0].locations.len(), 1, "alice's track stays");
	e.server.abort();
}

/// A removed review moves to Reinstating when asked, back when withdrawn — on record
/// either way — and returns to Snapshotted, with its appeal, once a scan lists it again.
#[tokio::test]
async fn a_reinstatement_is_asked_withdrawn_and_answered_by_a_scan() {
	let e = env().await;
	let alice = e.member(ALICE);
	let g = alice.add_gmail("ops@gmail.com").await.unwrap();
	let t = alice
		.track(
			g.id,
			&NewTrack {
				place: PLACE.into(),
				..Default::default()
			},
		)
		.await
		.unwrap();
	record(&e, t.id, vec![review("kept"), review("removed")]).await;
	record(&e, t.id, vec![review("kept")]).await;

	let columns = |b: &review_archive::core::dto::Board| {
		let ids = |cs: &[review_archive::core::dto::BoardCard]| cs.iter().map(|c| c.review.source_review_id.clone()).collect::<Vec<_>>();
		(ids(&b.snapshotted), ids(&b.removed), ids(&b.reinstating))
	};
	let board = alice.board(g.id, t.id).await.unwrap();
	assert_eq!(columns(&board), (vec!["kept".into()], vec!["removed".into()], vec![]));
	let (kept, removed) = (board.snapshotted[0].review.id, board.removed[0].review.id);

	assert_eq!(
		alice.reinstate(g.id, kept).await.unwrap_err().status(),
		Some(StatusCode::BAD_REQUEST),
		"a listed review has nothing to appeal"
	);
	let asked = alice.reinstate(g.id, removed).await.unwrap();
	assert_eq!(alice.reinstate(g.id, removed).await.unwrap(), asked, "asking twice is the one appeal");
	assert_eq!(columns(&alice.board(g.id, t.id).await.unwrap()), (vec!["kept".into()], vec![], vec!["removed".into()]));
	assert_eq!(alice.overview().await.unwrap()[0].locations[0].reinstating, 1);

	alice.withdraw_reinstatement(g.id, removed).await.unwrap();
	assert_eq!(alice.withdraw_reinstatement(g.id, removed).await.unwrap_err().status(), Some(StatusCode::NOT_FOUND));
	assert_eq!(columns(&alice.board(g.id, t.id).await.unwrap()), (vec!["kept".into()], vec!["removed".into()], vec![]));
	alice.reinstate(g.id, removed).await.unwrap();
	assert_eq!(alice.delete_gmail(g.id).await.unwrap_err().status(), Some(StatusCode::BAD_REQUEST), "appeals are kept");

	record(&e, t.id, vec![review("kept"), review("removed")]).await;
	let board = alice.board(g.id, t.id).await.unwrap();
	assert_eq!(
		columns(&board),
		(vec!["removed".into(), "kept".into()], vec![], vec![]),
		"newest first sighting first, then newest row"
	);
	let back = board.snapshotted.iter().find(|c| c.review.id == removed).unwrap();
	assert!(back.reinstatement.as_ref().unwrap().reinstated_at.is_some(), "the badge: reinstated after its appeal");
	e.server.abort();
}

/// A place is scanned while any track of it is on under a gmail that is on; turning
/// tracks off is the member's own and leaves the shared target as it was.
#[tokio::test]
async fn a_place_is_scanned_while_anyone_has_it_on() {
	let e = env().await;
	let (alice, bob) = (e.member(ALICE), e.member(BOB));
	let (a, b) = (alice.add_gmail("a@gmail.com").await.unwrap(), bob.add_gmail("b@gmail.com").await.unwrap());
	let track = NewTrack {
		place: PLACE.into(),
		..Default::default()
	};
	let t = alice.track(a.id, &track).await.unwrap().id;
	bob.track(b.id, &track).await.unwrap();
	let untracked = e
		.client
		.add_target(&NewTarget {
			place: Some(PLACE.into()),
			lang: Some("de".into()),
			..Default::default()
		})
		.await
		.unwrap()
		.id;
	let scanned = async || {
		let due = e.archive.due(Timestamp::now()).await.unwrap().into_iter().map(|t| t.id.0).collect::<Vec<_>>();
		let next = e.client.target(t).await.unwrap().next_scan_at.is_some();
		assert_eq!(due.contains(&t), next, "the schedule and the target's detail agree");
		assert!(due.contains(&untracked), "a target no one tracks keeps its own flag");
		next
	};
	assert!(scanned().await);
	let overview = alice.overview().await.unwrap();
	assert!(overview[0].gmail.enabled && overview[0].locations[0].enabled, "on by default");

	alice.set_track_enabled(a.id, t, false).await.unwrap();
	assert!(!alice.overview().await.unwrap()[0].locations[0].enabled);
	assert!(scanned().await, "bob still has it on");
	bob.set_gmail_enabled(b.id, false).await.unwrap();
	assert!(!scanned().await, "bob's track is on, under a gmail that is off");
	assert!(bob.overview().await.unwrap()[0].locations[0].enabled, "a gmail's switch leaves its tracks' own");
	assert!(e.client.target(t).await.unwrap().target.enabled, "the shared target is untouched");

	assert_eq!(
		bob.set_track_enabled(a.id, t, true).await.unwrap_err().status(),
		Some(StatusCode::NOT_FOUND),
		"alice's gmail is not bob's"
	);
	assert_eq!(bob.set_gmail_enabled(a.id, true).await.unwrap_err().status(), Some(StatusCode::NOT_FOUND));
	bob.set_gmail_enabled(b.id, true).await.unwrap();
	assert!(scanned().await);
	e.server.abort();
}

/// A channel's destination has to be one Telegram can take, and one member's channels are
/// not another's.
#[tokio::test]
async fn telegram_channels_are_the_members_own() {
	let e = env().await;
	let (alice, bob) = (e.member(ALICE), e.member(BOB));
	let ch = |destination: &str| NewTgChannel {
		destination: destination.into(),
		gmail_id: None,
		events: vec![Event::ReviewGone],
	};
	let added = alice.add_tg_channel(&ch("-1002244305221/7")).await.unwrap();
	assert_eq!(alice.add_tg_channel(&ch("#nope!")).await.unwrap_err().status(), Some(StatusCode::BAD_REQUEST));
	assert_eq!(
		alice.add_tg_channel(&NewTgChannel { events: vec![], ..ch("@chan") }).await.unwrap_err().status(),
		Some(StatusCode::BAD_REQUEST)
	);
	assert_eq!(bob.tg_channels().await.unwrap(), vec![]);
	assert_eq!(bob.delete_tg_channel(added.id).await.unwrap_err().status(), Some(StatusCode::NOT_FOUND));
	assert_eq!(bob.test_tg_channel(added.id).await.unwrap_err().status(), Some(StatusCode::NOT_FOUND));
	assert_eq!(alice.tg_channels().await.unwrap(), vec![added.clone()]);
	alice.delete_tg_channel(added.id).await.unwrap();
	assert_eq!(alice.tg_channels().await.unwrap(), vec![]);
	e.server.abort();
}
