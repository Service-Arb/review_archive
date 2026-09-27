//! Webhook events and the job queue on a temp SQLite: events land in the outbox with the
//! scan that caused them, deliveries are signed, retried and survive a reopen.

use std::{
	collections::HashMap,
	sync::{Arc, Mutex},
	time::Duration,
};

use axum::{
	Router,
	body::Bytes,
	extract::State,
	http::{HeaderMap, StatusCode},
	routing::post,
};
use jiff::Timestamp;
use review_archive::{
	core::{
		Coverage, Known, Observed, ReviewId, Scan, Target, TargetKind,
		dto::{Event, EventPayload, JobKind, JobStatus, NewWebhook, RunStatus},
	},
	record::{Recorder, SystemClock},
	sources::ReviewSource,
	store::{NewTarget, Store, blobs::BlobStore},
	webhooks::{MAX_ATTEMPTS, deliver_due, signature},
};

struct Scripted(Mutex<Result<Scan, String>>);

impl ReviewSource for Scripted {
	async fn scan(&self, _: &Target, _: &Known) -> eyre::Result<Scan> {
		self.0.lock().unwrap().clone().map_err(|e| eyre::eyre!(e))
	}
}

fn review(id: &str, text: &str) -> Observed {
	Observed {
		source_review_id: id.into(),
		author: "A".into(),
		rating: Some(4),
		text: Some(text.into()),
		..Default::default()
	}
}

fn complete(reviews: Vec<Observed>) -> Result<Scan, String> {
	Ok(Scan {
		reviews,
		coverage: Coverage::Complete,
		warnings: vec![],
	})
}

/// Records what it is sent; answers with the next scripted status (200 once they run out).
#[derive(Clone, Default)]
struct Receiver {
	got: Arc<Mutex<Vec<(HeaderMap, Bytes)>>>,
	statuses: Arc<Mutex<Vec<StatusCode>>>,
}

async fn receive(State(r): State<Receiver>, headers: HeaderMap, body: Bytes) -> StatusCode {
	r.got.lock().unwrap().push((headers, body));
	let mut s = r.statuses.lock().unwrap();
	if s.is_empty() { StatusCode::OK } else { s.remove(0) }
}

async fn receiver() -> (Receiver, String, tokio::task::JoinHandle<()>) {
	let r = Receiver::default();
	let app = Router::new().route("/hook", post(receive)).with_state(r.clone());
	let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
	let url = format!("http://{}/hook", listener.local_addr().unwrap());
	let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
	(r, url, server)
}

async fn target(store: &Store) -> Target {
	let id = store
		.add_target(
			&NewTarget {
				label: "t".into(),
				kind: TargetKind::Maps,
				place_id: "ChIJtesttesttesttest".into(),
				gbp: None,
				lang: "en".into(),
				interval: Duration::from_secs(6 * 3600),
			},
			Timestamp::now(),
		)
		.await
		.unwrap();
	store.target(id).await.unwrap()
}

#[tokio::test]
async fn events_are_queued_with_the_scan_signed_and_retried_across_a_reopen() {
	let dir = tempfile::tempdir().unwrap();
	let db = dir.path().join("db.sqlite");
	let store = Store::open(&db).await.unwrap();
	let blobs = BlobStore::new(dir.path().join("blobs"));
	let t = target(&store).await;
	let (rx, url, server) = receiver().await;
	let secret = "0123456789abcdef-shh";
	store
		.add_webhook(
			&NewWebhook {
				url: url.clone(),
				events: vec![Event::ReviewNew, Event::ReviewGone, Event::RunFailed],
				secret: secret.into(),
			},
			Timestamp::now(),
		)
		.await
		.unwrap();
	let rec = Recorder {
		store: &store,
		blobs: &blobs,
		clock: &SystemClock,
	};

	// two new reviews, then one of them gone, then a failed run; `changed` is not subscribed
	let src = Scripted(Mutex::new(complete(vec![review("a", "x"), review("b", "y")])));
	let first = rec.record(&src, &t).await.unwrap();
	assert_eq!(first.seen.len(), 2);
	*src.0.lock().unwrap() = complete(vec![review("a", "edited")]);
	rec.run(&src, &t).await.unwrap();
	*src.0.lock().unwrap() = Err("blocked by Google".into());
	assert_eq!(rec.run(&src, &t).await.unwrap().status, RunStatus::Failed);

	// the receiver fails the first try of the first delivery
	rx.statuses.lock().unwrap().push(StatusCode::INTERNAL_SERVER_ERROR);
	let http = reqwest::Client::new();
	let now = Timestamp::now();
	let report = deliver_due(&store, &http, now).await.unwrap();
	assert_eq!((report.delivered, report.retrying, report.gave_up), (3, 1, 0));

	// a restart: a new store on the same file still owes the failed one, due after the backoff
	drop(store);
	let store = Store::open(&db).await.unwrap();
	assert_eq!(deliver_due(&store, &http, now).await.unwrap().delivered, 0, "not due yet");
	let later = now.checked_add(jiff::SignedDuration::from_secs(31)).unwrap();
	assert_eq!(deliver_due(&store, &http, later).await.unwrap().delivered, 1);
	server.abort();

	let got = rx.got.lock().unwrap().clone();
	assert_eq!(got.len(), 5, "4 events, one of them tried twice");
	let mut events: HashMap<Event, usize> = HashMap::new();
	for (headers, body) in &got {
		assert_eq!(headers["x-signature"].to_str().unwrap(), signature(secret, body), "signed over the raw body");
		let p: EventPayload = serde_json::from_slice(body).unwrap();
		assert_eq!(headers["x-event"].to_str().unwrap(), p.event.as_str());
		assert_eq!(p.target_id, t.id.0);
		*events.entry(p.event).or_default() += 1;
		match p.event {
			Event::ReviewNew | Event::ReviewGone => assert!(p.review.is_some()),
			Event::RunFailed => assert!(p.run.unwrap().error.unwrap().contains("blocked by Google")),
			other => panic!("not subscribed: {other:?}"),
		}
	}
	assert_eq!(events[&Event::ReviewNew], 3, "a and b, a retried once");
	assert_eq!(events[&Event::ReviewGone], 1);
	assert_eq!(events[&Event::RunFailed], 1);
	assert_eq!(store.delivery_counts(1).await.unwrap(), (0, 4, 0));
}

#[tokio::test]
async fn a_dead_receiver_is_given_up_on_and_a_removed_hook_is_owed_nothing() {
	let dir = tempfile::tempdir().unwrap();
	let store = Store::open(&dir.path().join("db.sqlite")).await.unwrap();
	let blobs = BlobStore::new(dir.path().join("blobs"));
	let t = target(&store).await;
	// nothing listens here
	let dead = store
		.add_webhook(
			&NewWebhook {
				url: "http://127.0.0.1:9/hook".into(),
				events: vec![Event::ReviewNew],
				secret: "0123456789abcdef".into(),
			},
			Timestamp::now(),
		)
		.await
		.unwrap();
	let src = Scripted(Mutex::new(complete(vec![review("a", "x")])));
	Recorder {
		store: &store,
		blobs: &blobs,
		clock: &SystemClock,
	}
	.run(&src, &t)
	.await
	.unwrap();

	let http = reqwest::Client::new();
	let mut at = Timestamp::now();
	for _ in 0..MAX_ATTEMPTS {
		deliver_due(&store, &http, at).await.unwrap();
		at = at.checked_add(jiff::SignedDuration::from_hours(7)).unwrap();
	}
	assert_eq!(store.delivery_counts(dead.id).await.unwrap(), (0, 0, 1));

	assert!(store.delete_webhook(dead.id).await.unwrap());
	assert_eq!(store.delivery_counts(dead.id).await.unwrap(), (0, 0, 0), "deliveries go with the hook");
	assert!(!store.delete_webhook(dead.id).await.unwrap());
}

#[tokio::test]
async fn jobs_are_taken_oldest_first_and_a_crash_fails_the_running_one() {
	let dir = tempfile::tempdir().unwrap();
	let store = Store::open(&dir.path().join("db.sqlite")).await.unwrap();
	let t = target(&store).await;
	let now = Timestamp::now();
	let a = store.enqueue_job(JobKind::Scan, t.id, None, now).await.unwrap();
	let b = store.enqueue_job(JobKind::Capture, t.id, None, now).await.unwrap();

	let claimed = store.claim_job(now).await.unwrap().unwrap();
	assert_eq!((claimed.id, claimed.kind), (a, JobKind::Scan));
	assert_eq!(store.job(a).await.unwrap().unwrap().status, JobStatus::Running);
	store.finish_job(a, JobStatus::Done, None, None, &[ReviewId(99)], now).await.unwrap();
	let done = store.job(a).await.unwrap().unwrap();
	assert_eq!(done.status, JobStatus::Done);
	assert_eq!(done.reviews.unwrap().len(), 0, "unknown review ids are left out");

	// b is taken, then the process dies
	assert_eq!(store.claim_job(now).await.unwrap().unwrap().id, b);
	assert_eq!(store.fail_interrupted_jobs(now).await.unwrap(), 1);
	let b = store.job(b).await.unwrap().unwrap();
	assert_eq!(b.status, JobStatus::Failed);
	assert!(b.error.unwrap().contains("interrupted"));
	assert!(store.claim_job(now).await.unwrap().is_none());
	assert!(store.job(12345).await.unwrap().is_none());
}

/// `review.changed` and `review.reappeared` carry the review as the scan left it: the new
/// text, and no longer gone.
#[tokio::test]
async fn an_edited_review_that_is_back_sends_changed_and_reappeared() {
	let dir = tempfile::tempdir().unwrap();
	let store = Store::open(&dir.path().join("db.sqlite")).await.unwrap();
	let blobs = BlobStore::new(dir.path().join("blobs"));
	let t = target(&store).await;
	let (rx, url, server) = receiver().await;
	store
		.add_webhook(
			&NewWebhook {
				url,
				events: vec![Event::ReviewChanged, Event::ReviewReappeared],
				secret: "0123456789abcdef".into(),
			},
			Timestamp::now(),
		)
		.await
		.unwrap();
	let rec = Recorder {
		store: &store,
		blobs: &blobs,
		clock: &SystemClock,
	};
	let src = Scripted(Mutex::new(complete(vec![review("a", "x"), review("b", "y")])));
	rec.run(&src, &t).await.unwrap();
	*src.0.lock().unwrap() = complete(vec![review("b", "y")]);
	assert_eq!(rec.run(&src, &t).await.unwrap().counts.gone, 1);
	*src.0.lock().unwrap() = complete(vec![review("a", "x, edited"), review("b", "y")]);
	rec.run(&src, &t).await.unwrap();

	let report = deliver_due(&store, &reqwest::Client::new(), Timestamp::now()).await.unwrap();
	server.abort();
	assert_eq!(report.delivered, 2);
	let got: Vec<EventPayload> = rx.got.lock().unwrap().iter().map(|(_, body)| serde_json::from_slice(body).unwrap()).collect();
	let summary: Vec<(Event, Option<String>, Option<String>)> = got
		.into_iter()
		.map(|p| {
			let r = p.review.unwrap();
			(p.event, r.text, r.gone_at)
		})
		.collect();
	assert_eq!(
		summary,
		[(Event::ReviewChanged, Some("x, edited".into()), None), (Event::ReviewReappeared, Some("x, edited".into()), None),]
	);
}
