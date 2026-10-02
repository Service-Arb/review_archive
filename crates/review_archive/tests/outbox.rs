//! Webhook events and the job queue on a temp SQLite: events land in the outbox with the
//! scan that caused them, deliveries are signed, retried and survive a reopen.

use std::{
	collections::HashMap,
	sync::{Arc, Mutex},
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
	config::WebhookConfig,
	core::{
		Coverage, Known, Observed, ReviewId, Scan, Target, TargetKind,
		dto::{Event, EventPayload, JobKind, JobStatus, NewTgChannel, NewWebhook, RunStatus},
		schedule::Schedule,
		tokens::{Meter, Tokens},
	},
	record::Recorder,
	sources::ReviewSource,
	store::{InsertTarget, Recipient, Store, blobs::BlobStore},
	webhooks::{Deliverer, Telegram, signature},
};

/// The receivers here listen on loopback, which only a listed host may reach.
fn deliverer() -> Deliverer {
	Deliverer::new(
		&WebhookConfig {
			allowed_hosts: vec!["127.0.0.1".into()],
			..WebhookConfig::default()
		},
		None,
	)
	.unwrap()
}

struct Scripted(Mutex<Result<Scan, String>>);

impl ReviewSource for Scripted {
	async fn scan(&self, _: &Target, _: &Known, _: &mut Meter) -> eyre::Result<Scan> {
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
		cut_after: None,
		listed: None,
		post: None,
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
			&InsertTarget {
				label: "t".into(),
				kind: TargetKind::Maps,
				place_id: "ChIJtesttesttesttest".into(),
				gbp: None,
				lang: "en".into(),
				interval: v_utils::TF_6H,
				enabled: true,
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
	let hook = store
		.add_webhook(
			&NewWebhook {
				url: url.clone(),
				events: vec![Event::ReviewNew, Event::ReviewGone, Event::RunFailed],
				secret: secret.into(),
			},
			Timestamp::now(),
		)
		.await
		.unwrap()
		.id;
	let rec = Recorder {
		store: &store,
		blobs: &blobs,
		now: Timestamp::now,
		schedule: &Schedule::default(),
		tokens: &Tokens::default(),
	};

	// two new reviews, then one of them gone, then a failed run; `changed` is not subscribed
	let src = Scripted(Mutex::new(complete(vec![review("a", "x"), review("b", "y")])));
	let first = rec.record(&src, &t, None).await.unwrap();
	assert_eq!(first.seen.len(), 2);
	*src.0.lock().unwrap() = complete(vec![review("a", "edited")]);
	rec.run(&src, &t).await.unwrap();
	*src.0.lock().unwrap() = Err("blocked by Google".into());
	assert_eq!(rec.run(&src, &t).await.unwrap().status, RunStatus::Failed);

	// the receiver fails the first try of the first delivery
	rx.statuses.lock().unwrap().push(StatusCode::INTERNAL_SERVER_ERROR);
	let hooks = deliverer();
	let now = Timestamp::now();
	let report = hooks.deliver_due(&store, now).await.unwrap();
	assert_eq!((report.delivered, report.retrying, report.gave_up), (3, 1, 0));

	// a restart: a new store on the same file still owes the failed one, due after the backoff
	drop(store);
	let store = Store::open(&db).await.unwrap();
	assert_eq!(hooks.deliver_due(&store, now).await.unwrap().delivered, 0, "not due yet");
	let later = now.checked_add(jiff::SignedDuration::from_secs(31)).unwrap();
	assert_eq!(hooks.deliver_due(&store, later).await.unwrap().delivered, 1);
	server.abort();

	let got = rx.got.lock().unwrap().clone();
	assert_eq!(got.len(), 5, "4 events, one of them tried twice");
	let mut events: HashMap<Event, usize> = HashMap::new();
	for (headers, body) in &got {
		assert_eq!(headers["x-signature"].to_str().unwrap(), signature(secret, body), "signed over the raw body");
		let p: EventPayload = serde_json::from_slice(body).unwrap();
		assert_eq!(headers["x-event"].to_str().unwrap(), p.event.as_ref());
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
	assert_eq!(store.delivery_counts(hook).await.unwrap(), (0, 4, 0));
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
		now: Timestamp::now,
		schedule: &Schedule::default(),
		tokens: &Tokens::default(),
	}
	.run(&src, &t)
	.await
	.unwrap();

	let hooks = deliverer();
	let mut at = Timestamp::now();
	for _ in 0..WebhookConfig::default().max_attempts {
		hooks.deliver_due(&store, at).await.unwrap();
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
	let a = store.enqueue_job(JobKind::Scan, t.id, None, 20, now).await.unwrap();
	let b = store.enqueue_job(JobKind::Capture, t.id, None, 20, now).await.unwrap();

	let claimed = store.claim_job(now).await.unwrap().unwrap();
	assert_eq!((claimed.id, claimed.kind), (a, JobKind::Scan));
	assert_eq!(store.job(a).await.unwrap().unwrap().status, JobStatus::Running);
	store.finish_job(a, JobStatus::Done, None, None, &[ReviewId(99)], now).await.unwrap();
	let done = store.job(a).await.unwrap().unwrap();
	assert_eq!(done.status, JobStatus::Done);
	assert_eq!(done.reviews.unwrap().len(), 0, "unknown review ids are left out");

	// b is taken, then the process dies
	assert_eq!(store.claim_job(now).await.unwrap().unwrap().id, b);
	assert_eq!(store.fail_interrupted(now).await.unwrap(), 1);
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
		now: Timestamp::now,
		schedule: &Schedule::default(),
		tokens: &Tokens::default(),
	};
	let src = Scripted(Mutex::new(complete(vec![review("a", "x"), review("b", "y")])));
	rec.run(&src, &t).await.unwrap();
	*src.0.lock().unwrap() = complete(vec![review("b", "y")]);
	assert_eq!(rec.run(&src, &t).await.unwrap().counts.gone, 1);
	*src.0.lock().unwrap() = complete(vec![review("a", "x, edited"), review("b", "y")]);
	rec.run(&src, &t).await.unwrap();

	let report = deliverer().deliver_due(&store, Timestamp::now()).await.unwrap();
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

/// A Telegram Bot API stand-in: records `(method, content type, body)`; a chat named
/// `@nochat` is refused the way Telegram refuses it.
#[derive(Clone, Default)]
struct Bot {
	got: Arc<Mutex<Vec<(String, String, Bytes)>>>,
}

async fn bot_call(State(b): State<Bot>, axum::extract::Path((token, method)): axum::extract::Path<(String, String)>, headers: HeaderMap, body: Bytes) -> (StatusCode, String) {
	assert_eq!(token, "bot123:abc");
	if String::from_utf8_lossy(&body).contains("nochat") {
		return (StatusCode::BAD_REQUEST, r#"{"ok":false,"error_code":400,"description":"Bad Request: chat not found"}"#.into());
	}
	let ct = headers[axum::http::header::CONTENT_TYPE].to_str().unwrap().to_owned();
	b.got.lock().unwrap().push((method, ct, body));
	(StatusCode::OK, r#"{"ok":true,"result":{}}"#.into())
}

fn png() -> Vec<u8> {
	let mut out = Vec::new();
	let mut enc = png::Encoder::new(&mut out, 4, 3);
	enc.set_color(png::ColorType::Rgb);
	enc.write_header().unwrap().write_image_data(&[90; 36]).unwrap();
	out
}

/// A member's channel hears about the places the member tracks, and only those; a removed
/// review arrives as its screenshot; what Telegram refuses is retried, with its reason.
#[tokio::test]
async fn telegram_channels_get_their_members_places_only() {
	let dir = tempfile::tempdir().unwrap();
	let store = Store::open(&dir.path().join("db.sqlite")).await.unwrap();
	let blobs = BlobStore::new(dir.path().join("blobs"));
	let now = Timestamp::now();
	let watched = target(&store).await;
	let other = store
		.add_target(
			&InsertTarget {
				label: "elsewhere".into(),
				kind: TargetKind::Maps,
				place_id: "ChIJelsewhereelsewhere".into(),
				gbp: None,
				lang: "en".into(),
				interval: v_utils::TF_6H,
				enabled: true,
			},
			now,
		)
		.await
		.unwrap();
	let events = vec![Event::ReviewNew, Event::ReviewGone];
	let channel = |dest: &str, gmail: Option<i64>| NewTgChannel {
		destination: dest.into(),
		gmail_id: gmail,
		events: events.clone(),
	};
	let alice = store.add_gmail("alice@x.com", "ops@gmail.com", now).await.unwrap();
	store.track(Some("alice@x.com"), alice.id, watched.id, now).await.unwrap();
	store.add_tg_channel("alice@x.com", &channel("@alice_chan", None), now).await.unwrap();
	let bob = store.add_gmail("bob@x.com", "bob@gmail.com", now).await.unwrap();
	store.track(Some("bob@x.com"), bob.id, other, now).await.unwrap();
	store.add_tg_channel("bob@x.com", &channel("@bob_chan", None), now).await.unwrap();
	// carol tracks the same place, but her channel's gmail tracks nothing
	let carol = store.add_gmail("carol@x.com", "c1@gmail.com", now).await.unwrap();
	let carol_empty = store.add_gmail("carol@x.com", "c2@gmail.com", now).await.unwrap();
	store.track(Some("carol@x.com"), carol.id, watched.id, now).await.unwrap();
	store.add_tg_channel("carol@x.com", &channel("@carol_chan", Some(carol_empty.id)), now).await.unwrap();
	// dave's chat does not have the bot
	let dave = store.add_gmail("dave@x.com", "d@gmail.com", now).await.unwrap();
	store.track(Some("dave@x.com"), dave.id, watched.id, now).await.unwrap();
	store.add_tg_channel("dave@x.com", &channel("@nochat", None), now).await.unwrap();

	let rec = Recorder {
		store: &store,
		blobs: &blobs,
		now: Timestamp::now,
		schedule: &Schedule::default(),
		tokens: &Tokens::default(),
	};
	let mut shot = review("b", "rude");
	shot.capture = Some(review_archive::core::Capture {
		png: png(),
		captured_at: now,
		page_url: "https://maps.example/b".into(),
	});
	let src = Scripted(Mutex::new(complete(vec![review("a", "fine"), shot])));
	rec.run(&src, &watched).await.unwrap();
	*src.0.lock().unwrap() = complete(vec![review("a", "fine")]);
	assert_eq!(rec.run(&src, &watched).await.unwrap().counts.gone, 1);

	let b = Bot::default();
	let app = Router::new().route("/{token}/{method}", post(bot_call)).with_state(b.clone());
	let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
	let api = format!("http://{}/", listener.local_addr().unwrap());
	let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
	let d = Deliverer::new(
		&WebhookConfig::default(),
		Some(Telegram {
			api: api.parse().unwrap(),
			token: "123:abc".into(),
			blobs: blobs.clone(),
		}),
	)
	.unwrap();
	let report = d.deliver_due(&store, Timestamp::now()).await.unwrap();
	let tested = d.test_telegram("@nochat", "hello".into()).await.unwrap_err();
	server.abort();
	assert_eq!(tested.to_string(), "Telegram answered 400 Bad Request: Bad Request: chat not found");

	assert_eq!((report.delivered, report.retrying), (3, 3), "alice's three; dave's three refused");
	let got = b.got.lock().unwrap().clone();
	let calls: Vec<(&str, bool)> = got.iter().map(|(m, _, body)| (m.as_str(), String::from_utf8_lossy(body).contains("alice_chan"))).collect();
	assert_eq!(calls, [("sendMessage", true), ("sendMessage", true), ("sendPhoto", true)]);
	let (_, ct, photo) = &got[2];
	assert!(ct.starts_with("multipart/form-data"));
	let photo = String::from_utf8_lossy(photo);
	assert!(photo.contains("Review removed · t\n★★★★ A\nrude"), "the caption: {photo}");
	assert!(photo.contains("filename=\"review.png\"") && photo.contains("PNG"), "the screenshot");
	let refused = store.due_deliveries(Timestamp::MAX, 50).await.unwrap();
	assert_eq!(refused.len(), 3);
	assert!(refused.iter().all(|r| matches!(&r.to, Recipient::Telegram { destination, .. } if destination == "@nochat")));
}
