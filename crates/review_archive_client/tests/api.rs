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
		Coverage, Known, Observed, OwnerPost, Scan, Target,
		dto::{BalanceChange, CaptureRequest, Event, JobStatus, Me, MemberDto, NewTarget, NewTgChannel, NewTrack, NewWebhook, TargetPatch, TokenKind, TokensChange, TokensDto},
		tokens::Meter,
	},
	sources::ReviewSource,
	store::people::{Claim, Seen},
};
use review_archive_client::{Captured, Client};
use review_archive_server::{
	auth::Auth,
	http::{AppState, HttpConfig, router},
	worker::Signals,
};
use sa_auth::Signer;

/// The panel's signing key, and another under the same id that the archive does not hold.
const PANEL_KEY: &str = "panel:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";
const FOREIGN_KEY: &str = "panel:AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=";
/// Who the test panel vouches for on a request: it signs for them and strips this.
const CALLER_HEADER: &str = "x-test-caller";
const ALICE: &str = "alice";
const BOB: &str = "bob";
const PLACE: &str = "ChIJLU7jZClu5kcR4PcOOO6p3I0";

/// A concierge account, as the panel's assertion names it.
#[derive(Clone, serde::Deserialize, serde::Serialize)]
struct Who {
	sub: String,
	email: String,
	verified: bool,
	name: String,
	permissions: Vec<String>,
}

impl Who {
	/// `{sub}@x.com`, verified, with no permissions.
	fn member(sub: &str) -> Self {
		Self {
			sub: sub.into(),
			email: format!("{sub}@x.com"),
			verified: true,
			name: sub.into(),
			permissions: vec![],
		}
	}

	fn may(self, permissions: &[&str]) -> Self {
		Self {
			permissions: permissions.iter().map(|p| (*p).to_owned()).collect(),
			..self
		}
	}
}

fn operator() -> Who {
	Who::member("root").may(&[sa_auth::Archive::Operate.as_str()])
}

fn admin() -> Who {
	Who::member("root").may(&[sa_auth::Archive::Operate.as_str(), sa_auth::Tokens::Grant.as_str(), sa_auth::Members::ActAs.as_str()])
}

fn sign(signer: &Signer, who: &Who, method: &str, path: &str, exp: i64) -> String {
	signer.sign(&sa_auth::Assertion {
		aud: sa_auth::Service::ReviewArchive,
		sub: who.sub.clone(),
		email: who.email.clone(),
		email_verified: who.verified,
		name: who.name.clone(),
		permissions: who.permissions.iter().cloned().collect(),
		method: method.into(),
		path: path.into(),
		exp,
	})
}

fn alive() -> i64 {
	Timestamp::now().as_second() + sa_auth::TTL
}

/// An HTTP client whose every request the test panel signs for `who`.
fn http_as(who: &Who) -> reqwest::Client {
	let headers = reqwest::header::HeaderMap::from_iter([(CALLER_HEADER.parse().unwrap(), serde_json::to_string(who).unwrap().parse().unwrap())]);
	reqwest::Client::builder().default_headers(headers).build().unwrap()
}

struct Env {
	dir: tempfile::TempDir,
	archive: Archive,
	signals: Arc<Signals>,
	signer: Arc<Signer>,
	base: String,
	/// The operator: may operate the archive, and nothing else.
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
	let signer: Arc<Signer> = Arc::new(PANEL_KEY.parse().unwrap());
	let panel = {
		let signer = signer.clone();
		axum::middleware::from_fn(move |mut req: axum::extract::Request, next: axum::middleware::Next| {
			let signer = signer.clone();
			async move {
				if let Some(who) = req.headers_mut().remove(CALLER_HEADER) {
					let who: Who = serde_json::from_slice(who.as_bytes()).expect("http_as serialized it");
					let token = sign(&signer, &who, req.method().as_str(), req.uri().path(), alive());
					req.headers_mut().insert(sa_auth::HEADER, token.parse().expect("a JWS is base64url and dots"));
				}
				next.run(req).await
			}
		})
	};
	let auth = Auth::Panel(signer.public().parse().unwrap());
	let app = router(AppState::new(archive.clone(), auth, signals.clone(), HttpConfig::default()), None, None).layer(panel);
	let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
	let base = format!("http://{}", listener.local_addr().unwrap());
	let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
	let client = Client::ambient(http_as(&operator()), &base).unwrap();
	Env {
		dir,
		archive,
		signals,
		signer,
		base,
		client,
		server,
	}
}

impl Env {
	fn caller(&self, who: &Who) -> Client {
		Client::ambient(http_as(who), &self.base).unwrap()
	}

	/// Someone signed in with no permissions here.
	fn member(&self, sub: &str) -> Client {
		self.caller(&Who::member(sub))
	}

	fn admin(&self) -> Client {
		self.caller(&admin())
	}

	/// Pre-existing rows under `email`: a person nobody signed in as yet, and a gmail of theirs.
	async fn unclaimed(&self, email: &str, gmail: &str) {
		let db = sqlx::SqlitePool::connect(&format!("sqlite://{}", self.dir.path().join("review_archive.db").display()))
			.await
			.unwrap();
		let id: i64 = sqlx::query_scalar("INSERT INTO people (email, name, first_seen) VALUES (?, '', '2026-01-01T00:00:00Z') RETURNING id")
			.bind(email)
			.fetch_one(&db)
			.await
			.unwrap();
		sqlx::query("INSERT INTO managing_gmails (person_id, gmail, created_at) VALUES (?, ?, '2026-01-01T00:00:00Z')")
			.bind(id)
			.bind(gmail)
			.execute(&db)
			.await
			.unwrap();
	}
}

async fn id(c: &Client) -> i64 {
	c.me().await.unwrap().id
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
		"/captures/{sha256}.avif",
		"/jobs/{id}",
		"/reviews/{id}",
		"/stats",
		"/webhooks",
		"/webhooks/{id}",
	] {
		assert!(paths.contains(&p), "{p} missing from {paths:?}");
	}
	assert!(doc["components"]["securitySchemes"]["panel_assertion"].is_object());
	e.server.abort();
}

/// A 1×1 PNG.
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
async fn every_route_but_health_and_openapi_wants_an_assertion() {
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
		("GET", &format!("/captures/{}.avif", "0".repeat(64))),
		("GET", "/jobs/1"),
		("GET", "/reviews/1"),
		("GET", "/stats"),
		("GET", "/webhooks"),
		("POST", "/webhooks"),
		("DELETE", "/webhooks/1"),
		("GET", "/members"),
		("POST", "/members/1/tokens"),
		("GET", "/me"),
		("GET", "/me/tokens"),
		("GET", "/me/overview"),
		("POST", "/me/gmails"),
		("DELETE", "/me/gmails/1"),
		("PATCH", "/me/gmails/1"),
		("POST", "/me/gmails/1/tracks"),
		("DELETE", "/me/gmails/1/tracks/1"),
		("PATCH", "/me/gmails/1/tracks/1"),
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
			.json(&serde_json::json!({}))
			.send()
			.await
			.unwrap();
		assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "{method} {path}");
	}
	assert_eq!(reqwest::get(format!("{}/health", e.base)).await.unwrap().status(), StatusCode::OK);
	assert_eq!(reqwest::get(format!("{}/openapi.json", e.base)).await.unwrap().status(), StatusCode::OK);
	e.server.abort();
}

/// An assertion is for the one request it was signed for: not another path, not another method.
#[tokio::test]
async fn an_assertion_replayed_on_another_request_is_refused() {
	let e = env().await;
	let alice = Who::member(ALICE);
	let http = reqwest::Client::new();
	let send = async |method: &str, path: &str, token: &str| {
		http.request(method.parse().unwrap(), format!("{}{path}", e.base))
			.header(sa_auth::HEADER, token)
			.json(&serde_json::json!({}))
			.send()
			.await
			.unwrap()
			.status()
	};
	let me = sign(&e.signer, &alice, "GET", "/me", alive());
	assert_eq!(send("GET", "/me", &me).await, StatusCode::OK);
	assert_eq!(send("GET", "/me/overview", &me).await, StatusCode::UNAUTHORIZED);
	let channels = sign(&e.signer, &alice, "GET", "/me/tg-channels", alive());
	assert_eq!(send("POST", "/me/tg-channels", &channels).await, StatusCode::UNAUTHORIZED);
	e.server.abort();
}

#[tokio::test]
async fn a_forged_or_expired_assertion_is_no_sign_in() {
	let e = env().await;
	let alice = Who::member(ALICE);
	let foreign: Signer = FOREIGN_KEY.parse().unwrap();
	let unknown: Signer = FOREIGN_KEY.replacen("panel", "other", 1).parse().unwrap();
	for token in [
		sign(&foreign, &alice, "GET", "/me", alive()),
		sign(&unknown, &alice, "GET", "/me", alive()),
		sign(&e.signer, &alice, "GET", "/me", Timestamp::now().as_second() - 1),
		"not.a.jws".to_owned(),
	] {
		let resp = reqwest::Client::new().get(format!("{}/me", e.base)).header(sa_auth::HEADER, &token).send().await.unwrap();
		assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "{token}");
	}
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
	let http = http_as(&operator());
	let bad_body = http
		.post(format!("{}/targets", e.base))
		.header("content-type", "application/json")
		.body("{not json")
		.send()
		.await
		.unwrap();
	assert!(bad_body.status().is_client_error(), "{}", bad_body.status());
	let text = bad_body.text().await.unwrap();
	assert!(serde_json::from_str::<review_archive::core::dto::ErrorBody>(&text).is_ok(), "not an ErrorBody: {text:?}");

	let bad_query = http.get(format!("{}/targets/1/reviews?gone=maybe", e.base)).send().await.unwrap();
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

	let avif = e.client.capture_avif(&url).await.unwrap();
	assert_eq!(&avif[4..12], b"ftypavif");
	let blobs = review_archive::store::blobs::BlobStore::new(e.dir.path().join("blobs"));
	assert_eq!(avif, std::fs::read(blobs.path_of(&sha).unwrap()).unwrap());

	// a file in the blob dir that no capture row names is not served
	let stray = blobs.put(b"not a recorded capture").await.unwrap();
	assert_eq!(e.client.capture_avif(&format!("/captures/{stray}.avif")).await.unwrap_err().status(), Some(StatusCode::NOT_FOUND));
	let never_recorded = format!("/captures/{}.avif", "0".repeat(64));
	assert_eq!(e.client.capture_avif(&never_recorded).await.unwrap_err().status(), Some(StatusCode::NOT_FOUND));
	assert_eq!(e.client.capture_avif(&format!("/captures/{sha}")).await.unwrap_err().status(), Some(StatusCode::NOT_FOUND));
	assert_eq!(
		e.client.capture_avif(&format!("/captures/{}.avif", sha.to_uppercase())).await.unwrap_err().status(),
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
	let files: Vec<(String, Option<String>)> = manifest["reviews"]
		.as_array()
		.unwrap()
		.iter()
		.map(|r| (r["source_review_id"].as_str().unwrap().to_owned(), r["capture"].as_str().map(str::to_owned)))
		.collect();
	let a_file = files.iter().find(|(id, _)| id == "a").unwrap().1.clone().expect("a has a capture in the export");
	assert!(a_file.starts_with("captures/") && a_file.ends_with(".avif"), "{a_file}");
	assert_eq!(files.iter().find(|(id, _)| id == "b").unwrap().1, None, "no capture, no file");
	assert_eq!(archive.len(), 2, "the manifest and a's capture");

	let mut in_zip = Vec::new();
	archive.by_name(&a_file).unwrap().read_to_end(&mut in_zip).unwrap();
	assert_eq!(in_zip, e.client.capture_avif(a.capture_url.as_deref().unwrap()).await.unwrap());

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

	let csv = http_as(&operator())
		.get(format!("{}/stats?target={t}", e.base))
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

/// The archive's own routes are for whoever may operate it, whatever else they may do.
#[tokio::test]
async fn archive_routes_need_archive_operate() {
	let e = env().await;
	let all_but = e.caller(&Who::member("root").may(&[sa_auth::Tokens::Grant.as_str(), sa_auth::Members::ActAs.as_str()]));
	for c in [all_but, e.member(ALICE)] {
		assert_eq!(c.targets().await.unwrap_err().status(), Some(StatusCode::FORBIDDEN));
		assert_eq!(c.webhooks().await.unwrap_err().status(), Some(StatusCode::FORBIDDEN));
		assert_eq!(c.export_zip(1, None).await.unwrap_err().status(), Some(StatusCode::FORBIDDEN));
	}
	assert_eq!(e.client.targets().await.unwrap(), vec![]);
	e.server.abort();
}

/// `/me` is anyone signed in, with no permission at all.
#[tokio::test]
async fn me_is_open_to_anyone_signed_in() {
	let e = env().await;
	let alice = e.member(ALICE);
	let me = alice.me().await.unwrap();
	assert_eq!(
		me,
		Me {
			id: me.id,
			email: "alice@x.com".into(),
			name: ALICE.into(),
			permissions: vec![],
			tokens: TokensDto { balance: 15, daily: 15, cap: 300 },
		}
	);
	assert_eq!(alice.overview().await.unwrap(), vec![]);
	e.server.abort();
}

/// The members' list and their balances are for whoever may grant tokens.
#[tokio::test]
async fn members_and_their_balances_need_tokens_grant() {
	let e = env().await;
	let alice = e.member(ALICE);
	let a = id(&alice).await;
	let set = TokensChange {
		change: BalanceChange::Set(8),
		note: None,
	};
	for c in [&e.client, &alice] {
		assert_eq!(c.members().await.unwrap_err().status(), Some(StatusCode::FORBIDDEN));
		assert_eq!(c.change_tokens(a, &set).await.unwrap_err().status(), Some(StatusCode::FORBIDDEN));
	}
	let granter = e.caller(&Who::member("root").may(&[sa_auth::Tokens::Grant.as_str()]));
	assert_eq!(granter.change_tokens(a, &set).await.unwrap().balance, 8);
	assert_eq!(granter.change_tokens(9999, &set).await.unwrap_err().status(), Some(StatusCode::NOT_FOUND));
	let root = id(&granter).await;
	let member = |id, sub: &str, balance| MemberDto {
		id,
		email: format!("{sub}@x.com"),
		name: sub.into(),
		claimed: true,
		balance,
	};
	assert_eq!(granter.members().await.unwrap(), vec![member(a, ALICE, 8), member(root, "root", 15)]);
	e.server.abort();
}

/// Who may act as others does, through `X-Member`: their overview, their writes; `/me` is
/// still who signed in. No one else may.
#[tokio::test]
async fn acting_as_a_member_needs_members_act_as() {
	let e = env().await;
	let alice = e.member(ALICE);
	alice.add_gmail("alice@gmail.com").await.unwrap();
	let a = id(&alice).await;
	let admin = e.admin();
	assert_eq!(admin.overview().await.unwrap(), vec![], "without the header, the admin's own");
	let as_alice = admin.clone().as_member(a);
	assert_eq!(as_alice.overview().await.unwrap(), alice.overview().await.unwrap());
	as_alice.add_gmail("ops@gmail.com").await.unwrap();
	assert_eq!(alice.overview().await.unwrap().len(), 2, "the admin's write is alice's");
	assert_eq!(as_alice.me().await.unwrap().email, "root@x.com");
	assert_eq!(admin.as_member(9999).overview().await.unwrap_err().status(), Some(StatusCode::NOT_FOUND));
	for c in [e.client.clone(), e.member(BOB)] {
		assert_eq!(c.as_member(a).overview().await.unwrap_err().status(), Some(StatusCode::FORBIDDEN));
	}
	e.server.abort();
}

/// Rows from before people had ids go to the first sign-in with their address that concierge
/// verified.
#[tokio::test]
async fn an_unverified_sign_in_cannot_claim_its_addresss_rows() {
	let e = env().await;
	e.unclaimed("carol@x.com", "carol.ops@gmail.com").await;
	let unverified = Who {
		verified: false,
		..Who::member("carol")
	};
	assert_eq!(e.caller(&unverified).me().await.unwrap_err().status(), Some(StatusCode::FORBIDDEN));
	let overview = e.member("carol").overview().await.unwrap();
	assert_eq!(overview.iter().map(|g| g.gmail.gmail.as_str()).collect::<Vec<_>>(), ["carol.ops@gmail.com"]);
	e.server.abort();
}

/// With another account already holding the address, whose the rows are is not for a sign-in to say.
#[tokio::test]
async fn an_address_another_account_holds_cannot_be_claimed() {
	let e = env().await;
	e.member("dan").me().await.unwrap();
	e.unclaimed("dan@x.com", "dan.ops@gmail.com").await;
	let twin = Who {
		sub: "dan-twin".into(),
		..Who::member("dan")
	};
	assert_eq!(e.caller(&twin).me().await.unwrap_err().status(), Some(StatusCode::FORBIDDEN));
	e.server.abort();
}

/// A member's balance is an admin's to set, and theirs to read with every change that made it.
#[tokio::test]
async fn an_admin_sets_a_members_tokens() {
	let e = env().await;
	let alice = e.member(ALICE);
	let me = alice.me().await.unwrap();
	assert_eq!(me.tokens.balance, 15, "a day's worth on first sight");
	let set = |n| TokensChange {
		change: BalanceChange::Set(n),
		note: Some("trial".into()),
	};
	let admin = e.admin();
	assert_eq!(admin.change_tokens(me.id, &set(8)).await.unwrap().balance, 8);
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
	assert_eq!(admin.change_tokens(me.id, &bad).await.unwrap_err().status(), Some(StatusCode::BAD_REQUEST));
	e.server.abort();
}

/// `serve --dev-member`: every request is that member, so a dashboard runs without the panel.
#[tokio::test]
async fn a_dev_member_stands_in_for_a_missing_sign_in() {
	let dir = tempfile::tempdir().unwrap();
	let archive = Archive::open(Config {
		data_dir: Some(dir.path().to_owned()),
		..Config::default()
	})
	.await
	.unwrap();
	let seen = Seen {
		sub: "test",
		email: "test@localhost",
		email_verified: true,
		name: "test",
	};
	let Claim::Person(test) = archive.person(&seen).await.unwrap() else {
		panic!("nobody else holds the address")
	};
	archive.add_gmail(test, &review_archive::core::dto::NewGmail { gmail: "ops@gmail.com".into() }).await.unwrap();
	let auth = Auth::Dev {
		sub: "test".into(),
		permissions: Vec::<String>::new().into_iter().collect(),
	};
	let mfe = tempfile::tempdir().unwrap();
	let app = router(AppState::new(archive, auth, Arc::new(Signals::default()), HttpConfig::default()), Some(mfe.path()), Some("/"));
	let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
	let base = format!("http://{}", listener.local_addr().unwrap());
	let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

	let nobody = Client::ambient(reqwest::Client::new(), &base).unwrap();
	assert_eq!(nobody.overview().await.unwrap()[0].gmail.gmail, "ops@gmail.com");
	assert_eq!(nobody.me().await.unwrap().email, "test@localhost");
	assert_eq!(nobody.targets().await.unwrap_err().status(), Some(StatusCode::FORBIDDEN), "a member, not the operator");
	for view in [
		"/",
		"/telegram",
		"/tokens",
		"/gmails/1",
		"/gmails/1/places/2",
		"/members/3",
		"/members/3/",
		"/members/3/gmails/1/places/2",
	] {
		let page = reqwest::get(format!("{base}{view}")).await.unwrap();
		assert_eq!(page.status(), StatusCode::OK, "{view} is the dashboard's page");
		assert!(page.text().await.unwrap().contains("mfe-review-archive-dashboard"));
	}
	server.abort();
}

/// Two builds of the bundle share their file names and, from the nix store, their mtime: what a
/// browser or the edge kept from the first must not pass for the second.
#[tokio::test]
async fn a_new_bundle_is_never_answered_from_an_old_ones_cache() {
	let mfe = tempfile::tempdir().unwrap();
	let glue = mfe.path().join("review_archive_web.js");
	let build = |body: &str| {
		std::fs::write(&glue, body).unwrap();
		std::fs::File::options()
			.write(true)
			.open(&glue)
			.unwrap()
			.set_modified(std::time::UNIX_EPOCH + Duration::from_secs(1))
			.unwrap();
	};
	let serve = || async {
		let dir = tempfile::tempdir().unwrap();
		let archive = Archive::open(Config {
			data_dir: Some(dir.path().to_owned()),
			..Config::default()
		})
		.await
		.unwrap();
		let auth = Auth::Dev {
			sub: "test".into(),
			permissions: Vec::<String>::new().into_iter().collect(),
		};
		let app = router(AppState::new(archive, auth, Arc::new(Signals::default()), HttpConfig::default()), Some(mfe.path()), Some("/"));
		let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
		let base = format!("http://{}", listener.local_addr().unwrap());
		(dir, base, tokio::spawn(async move { axum::serve(listener, app).await.unwrap() }))
	};
	let http = reqwest::Client::new();

	build("first");
	let (_d1, base, server) = serve().await;
	let first = http.get(format!("{base}/mfe/review_archive_web.js")).send().await.unwrap();
	let cache_control = first.headers().get("cache-control").map(|v| v.to_str().unwrap().to_owned());
	assert_eq!(cache_control.as_deref(), Some("no-cache"), "kept only as long as it is asked about again");
	let validators: Vec<(String, String)> = ["etag", "last-modified"]
		.into_iter()
		.filter_map(|h| first.headers().get(h).map(|v| (h.to_owned(), v.to_str().unwrap().to_owned())))
		.collect();
	assert_eq!(first.text().await.unwrap(), "first");
	let again = validators.iter().fold(http.get(format!("{base}/mfe/review_archive_web.js")), |r, (h, v)| match h.as_str() {
		"etag" => r.header("if-none-match", v),
		_ => r.header("if-modified-since", v),
	});
	assert_eq!(again.send().await.unwrap().status(), StatusCode::NOT_MODIFIED, "the same build is not sent twice");
	server.abort();

	build("second");
	let (_d2, base, server) = serve().await;
	let revalidated = validators
		.iter()
		.fold(http.get(format!("{base}/mfe/review_archive_web.js")), |r, (h, v)| match h.as_str() {
			"etag" => r.header("if-none-match", v),
			_ => r.header("if-modified-since", v),
		})
		.send()
		.await
		.unwrap();
	assert_eq!(revalidated.status(), StatusCode::OK);
	assert_eq!(revalidated.text().await.unwrap(), "second");
	server.abort();
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
		e.member("carol").add_gmail(" tg:@Owner ").await.unwrap().gmail,
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
	assert_eq!(&alice.capture_avif(&url).await.unwrap()[4..12], b"ftypavif");
	assert!(e.client.capture_avif(&url).await.is_ok(), "the operator sees everything");
	assert_eq!(bob.board(a.id, t.id).await.unwrap_err().status(), Some(StatusCode::NOT_FOUND));

	// once bob stops tracking the place, its screenshots are no longer his to see
	assert!(bob.capture_avif(&url).await.is_ok());
	bob.untrack(b.id, t.id).await.unwrap();
	assert_eq!(bob.capture_avif(&url).await.unwrap_err().status(), Some(StatusCode::NOT_FOUND));
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

/// A test post is one a minute per person, whatever became of the last.
#[tokio::test]
async fn a_second_test_post_within_a_minute_is_a_429() {
	let e = env().await;
	let alice = e.member(ALICE);
	let ch = alice
		.add_tg_channel(&NewTgChannel {
			destination: "@chan".into(),
			gmail_id: None,
			events: vec![Event::ReviewGone],
		})
		.await
		.unwrap();
	// no bot is configured here, which the archive also answers 429, so the limit is told by its reason
	let limited = |r: &Result<(), review_archive_client::Error>| matches!(r, Err(review_archive_client::Error::Api { status: StatusCode::TOO_MANY_REQUESTS, message }) if message == "one test post a minute");
	let first = alice.test_tg_channel(ch.id).await;
	assert!(!limited(&first), "{first:?}");
	let second = alice.test_tg_channel(ch.id).await;
	assert!(limited(&second), "{second:?}");
	e.server.abort();
}
