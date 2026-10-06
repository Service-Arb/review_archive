//! Members pay for the walks of the places they track: a ledger renewed daily up to a cap,
//! charges split across trackers with the run's end, and no walk for a place whose trackers
//! are out of tokens, nor past the account's hourly limit. What a member paid adds up by day.

use std::path::Path;

use jiff::{SignedDuration, Timestamp};
use review_archive::{
	Archive,
	config::Config,
	core::{
		Coverage, Known, PersonId, Scan, Target, TargetId,
		dto::{BalanceChange, NewGmail, NewTarget, TokenKind, UsageDay},
		schedule::Schedule,
		tokens::{Meter, Tokens},
	},
	record::Recorder,
	sources::ReviewSource,
	store::{
		Store,
		blobs::BlobStore,
		people::{Claim, Seen},
	},
};

/// A walk that costs `.0` tokens.
struct Spends(i64);

impl ReviewSource for Spends {
	async fn scan(&self, _: &Target, _: &Known, meter: &mut Meter) -> eyre::Result<Scan> {
		meter.spend(self.0);
		Ok(Scan {
			reviews: vec![],
			coverage: Coverage::DownTo(None),
			warnings: vec![],
			cut_after: None,
			listed: None,
			post: None,
		})
	}
}

async fn open(dir: &Path) -> Archive {
	Archive::open(Config {
		data_dir: Some(dir.to_owned()),
		..Config::default()
	})
	.await
	.unwrap()
}

async fn target(archive: &Archive, place: &str) -> Target {
	let t = NewTarget {
		place: Some(place.to_owned()),
		..Default::default()
	};
	archive.add_target(&t).await.unwrap().target
}

async fn person(store: &Store, sub: &str) -> PersonId {
	let email = format!("{sub}@x");
	let seen = Seen {
		sub,
		email: &email,
		email_verified: true,
		name: sub,
	};
	match store.person(&seen, Timestamp::now()).await.unwrap() {
		Claim::Person(p) => p,
		refused => panic!("{sub}: {refused:?}"),
	}
}

/// `member` tracks `t` under a gmail of their own.
async fn tracks(archive: &Archive, member: PersonId, t: TargetId) {
	let g = archive.add_gmail(member, &NewGmail { gmail: format!("{member}.ops") }).await.unwrap();
	archive.assign(g.id, t).await.unwrap();
}

async fn set(archive: &Archive, member: PersonId, n: i64) {
	archive
		.store()
		.unwrap()
		.change_balance(member, BalanceChange::Set(n), "root", None, Timestamp::now(), &Tokens::default())
		.await
		.unwrap();
}

#[tokio::test]
async fn renewal_stops_at_the_cap_and_never_takes_back() {
	let dir = tempfile::tempdir().unwrap();
	let store = open(dir.path()).await.store().unwrap().clone();
	let cfg = Tokens::default();
	let a = person(&store, "a").await;
	let t0: Timestamp = "2026-10-01T09:00:00Z".parse().unwrap();
	let day = |n: i64| t0 + SignedDuration::from_hours(24 * n);
	assert_eq!(store.balance(a, t0, &cfg).await.unwrap(), 15, "a day's worth on first sight");
	assert_eq!(store.balance(a, day(3), &cfg).await.unwrap(), 60);
	assert_eq!(store.balance(a, day(3) + SignedDuration::from_hours(5), &cfg).await.unwrap(), 60, "once a day");
	assert_eq!(store.balance(a, day(100), &cfg).await.unwrap(), 300);

	let bought = store.change_balance(a, BalanceChange::Purchase(200), "root", Some("inv-1"), day(100), &cfg).await.unwrap();
	assert_eq!(bought, 500);
	assert_eq!(store.balance(a, day(130), &cfg).await.unwrap(), 500, "above the cap stays");

	assert_eq!(store.change_balance(a, BalanceChange::Set(8), "root", None, day(130), &cfg).await.unwrap(), 8);
	assert_eq!(store.balance(a, day(130), &cfg).await.unwrap(), 8);
	// days spent at the cap renew nothing afterwards: renewal counts from the last day it ran
	assert_eq!(store.balance(a, day(131), &cfg).await.unwrap(), 23);
}

#[tokio::test]
async fn trackers_split_a_walk_within_their_balances() {
	let dir = tempfile::tempdir().unwrap();
	let archive = open(dir.path()).await;
	let t = target(&archive, "ChIJtokenstest000000split").await;
	let poor = person(archive.store().unwrap(), "poor").await;
	let rich = person(archive.store().unwrap(), "rich").await;
	tracks(&archive, poor, t.id).await;
	tracks(&archive, rich, t.id).await;
	set(&archive, poor, 5).await;
	set(&archive, rich, 100).await;

	let rec = archive.record(&Spends(20), &t).await.unwrap();
	assert_eq!(archive.runs(t.id, &Default::default()).await.unwrap()[0].tokens, 20);
	for (member, left, paid) in [(poor, 0, -5), (rich, 85, -15)] {
		assert_eq!(archive.tokens(member).await.unwrap().balance, left, "{member}");
		let charge = archive.ledger(member).await.unwrap().into_iter().find(|l| l.kind == TokenKind::Charge).unwrap();
		assert_eq!((charge.delta, charge.run_id, charge.target_id), (paid, Some(rec.run.0), Some(t.id.0)), "{member}");
	}

	// the operator's own look is no member's to pay for
	let store = archive.store().unwrap();
	let job = archive.enqueue_scan(t.id).await.unwrap();
	store.claim_job(Timestamp::now()).await.unwrap().unwrap();
	let rec = Recorder {
		store,
		blobs: &BlobStore::new(dir.path().join("blobs")),
		now: Timestamp::now,
		schedule: &Schedule::default(),
		tokens: &Tokens::default(),
	};
	rec.record(&Spends(10), &t, Some(job)).await.unwrap();
	assert_eq!(archive.tokens(rich).await.unwrap().balance, 85);
}

#[tokio::test]
async fn a_place_whose_trackers_are_out_of_tokens_waits() {
	let dir = tempfile::tempdir().unwrap();
	let archive = open(dir.path()).await;
	let t = target(&archive, "ChIJtokenstest00000broke").await;
	let broke = person(archive.store().unwrap(), "broke").await;
	tracks(&archive, broke, t.id).await;
	set(&archive, broke, 0).await;
	let now = Timestamp::now();
	assert!(archive.due(now).await.unwrap().is_empty());
	assert_eq!(archive.next_due(now).await.unwrap(), None, "nothing to wake up for until a balance renews");

	tracks(&archive, person(archive.store().unwrap(), "flush").await, t.id).await;
	assert_eq!(archive.due(now).await.unwrap().iter().map(|t| t.id).collect::<Vec<_>>(), [t.id]);
}

#[tokio::test]
async fn the_hour_closes_maps_whoever_pays() {
	let dir = tempfile::tempdir().unwrap();
	let archive = open(dir.path()).await;
	let spent = target(&archive, "ChIJtokenstest00000spent").await;
	let waiting = target(&archive, "ChIJtokenstest0000waiting").await;
	archive.record(&Spends(Tokens::default().per_hour), &spent).await.unwrap();
	let now = Timestamp::now();
	assert!(archive.due(now).await.unwrap().is_empty());
	assert!(archive.enqueue_scan(waiting.id).await.is_err());
	let reopens = archive.next_due(now).await.unwrap().unwrap();
	assert!(reopens > now + SignedDuration::from_mins(59), "{reopens}");
}

#[tokio::test]
async fn usage_sums_a_members_own_charges_by_utc_day() {
	let dir = tempfile::tempdir().unwrap();
	let archive = open(dir.path()).await;
	let store = archive.store().unwrap();
	let shared = target(&archive, "ChIJtokenstest000usage01").await;
	let own = target(&archive, "ChIJtokenstest000usage02").await;
	let me = person(store, "me").await;
	let other = person(store, "other").await;
	let g = archive.add_gmail(me, &NewGmail { gmail: "me.ops".into() }).await.unwrap();
	archive.assign(g.id, shared.id).await.unwrap();
	archive.assign(g.id, own.id).await.unwrap();
	tracks(&archive, other, shared.id).await;
	set(&archive, me, 1000).await;
	set(&archive, other, 1000).await;

	let blobs = BlobStore::new(dir.path().join("blobs"));
	let walk = |now: fn() -> Timestamp, t: Target, cost: i64| {
		let (archive, blobs) = (&archive, &blobs);
		async move {
			let rec = Recorder {
				store: archive.store().unwrap(),
				blobs,
				now,
				schedule: &Schedule::default(),
				tokens: &Tokens::default(),
			};
			rec.record(&Spends(cost), &t, None).await.unwrap();
		}
	};
	walk(|| "2026-10-04T23:30:00Z".parse().unwrap(), own.clone(), 7).await;
	walk(|| "2026-10-05T00:30:00Z".parse().unwrap(), shared.clone(), 10).await; // split with `other`
	walk(|| "2026-10-05T02:00:00Z".parse().unwrap(), own.clone(), 3).await;
	walk(|| "2026-08-01T12:00:00Z".parse().unwrap(), own.clone(), 50).await; // past the window

	let usage = store.usage(me, "2026-10-06T08:00:00Z".parse().unwrap()).await.unwrap();
	assert_eq!(usage.days.len(), 30);
	assert_eq!(usage.days[0].day, "2026-09-07");
	let charged: Vec<&UsageDay> = usage.days.iter().filter(|d| d.walks > 0).collect();
	assert_eq!(
		charged,
		[
			&UsageDay {
				day: "2026-10-04".into(),
				walks: 1,
				tokens: 7
			},
			&UsageDay {
				day: "2026-10-05".into(),
				walks: 2,
				tokens: 8
			},
		]
	);
	assert_eq!(
		usage.days.last().unwrap(),
		&UsageDay {
			day: "2026-10-06".into(),
			walks: 0,
			tokens: 0
		}
	);
	assert_eq!(usage.places_tracked, 2);
	assert_eq!(store.usage(other, "2026-10-06T08:00:00Z".parse().unwrap()).await.unwrap().places_tracked, 1);
}
