//! SQLite (runtime `sqlx` queries, embedded migrations): targets, reviews and their
//! history, captures, runs. PNGs live beside it in [`blobs`].

pub mod blobs;
pub mod export;

use std::{collections::HashMap, path::Path, str::FromStr, time::Duration};

use eyre::WrapErr;
use jiff::{Timestamp, civil::Date};
use review_archive_core::{
	GbpLocation, Known, KnownReview, Observed, ReviewId, Target, TargetId, TargetKind,
	dto::{Counts, DayStats, ReviewDto, RunStatus},
	fmt_ts,
	reconcile::Plan,
	schedule::LastRun,
};
use sqlx::{
	FromRow, SqliteConnection,
	sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
};

fn parse_ts(s: &str) -> eyre::Result<Timestamp> {
	s.parse().wrap_err_with(|| format!("stored timestamp {s:?}"))
}

/// The archive's database.
#[derive(Clone, Debug)]
pub struct Store {
	pool: sqlx::SqlitePool,
}

/// A target to add.
#[derive(Clone, Debug)]
pub struct NewTarget {
	/// What people call it.
	pub label: String,
	/// Where its reviews are read from.
	pub kind: TargetKind,
	/// The Google place id.
	pub place_id: String,
	/// Required for `gbp`.
	pub gbp: Option<GbpLocation>,
	/// UI language of the Maps page.
	pub lang: String,
	/// At least an hour.
	pub interval: Duration,
}

/// A screenshot already written to the blob store, ready to be recorded.
#[derive(Clone, Debug)]
pub struct StoredCapture {
	/// Its blob name.
	pub sha256: String,
	/// Pixels.
	pub width: u32,
	/// Pixels.
	pub height: u32,
	/// When it was taken.
	pub captured_at: Timestamp,
	/// The page it was taken on.
	pub page_url: String,
}

/// A run's row id.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RunId(pub i64);

#[derive(Clone, Debug, FromRow)]
struct ReviewRow {
	id: i64,
	target_id: i64,
	source_review_id: String,
	author: String,
	author_url: Option<String>,
	rating: Option<i64>,
	text: Option<String>,
	reply: Option<String>,
	photo_count: i64,
	published_raw: Option<String>,
	published_est: Option<String>,
	first_seen: String,
	last_seen: String,
	gone_at: Option<String>,
	capture_pending: bool,
	capture_sha256: Option<String>,
	captured_at: Option<String>,
}

impl From<ReviewRow> for ReviewDto {
	fn from(r: ReviewRow) -> Self {
		Self {
			id: r.id,
			target_id: r.target_id,
			source_review_id: r.source_review_id,
			author: r.author,
			author_url: r.author_url,
			rating: r.rating,
			text: r.text,
			reply: r.reply,
			photo_count: r.photo_count,
			published_raw: r.published_raw,
			published_est: r.published_est,
			first_seen: r.first_seen,
			last_seen: r.last_seen,
			gone_at: r.gone_at,
			capture_pending: r.capture_pending,
			capture_sha256: r.capture_sha256,
			captured_at: r.captured_at,
		}
	}
}

#[derive(FromRow)]
struct TargetRow {
	id: i64,
	label: String,
	kind: String,
	place_id: String,
	gbp_account: Option<String>,
	gbp_location: Option<String>,
	lang: String,
	interval_secs: i64,
	enabled: bool,
	created_at: String,
}

impl TryFrom<TargetRow> for Target {
	type Error = eyre::Report;

	fn try_from(r: TargetRow) -> eyre::Result<Self> {
		let kind = TargetKind::from_str(&r.kind)?;
		let gbp = match (r.gbp_account, r.gbp_location) {
			(Some(account), Some(location)) => Some(GbpLocation { account, location }),
			_ => None,
		};
		Ok(Self {
			id: TargetId(r.id),
			label: r.label,
			kind,
			place_id: r.place_id,
			gbp,
			lang: r.lang,
			interval: Duration::from_secs(u64::try_from(r.interval_secs).wrap_err("negative interval_secs")?),
			enabled: r.enabled,
			created_at: parse_ts(&r.created_at)?,
		})
	}
}

#[derive(FromRow)]
struct KnownRow {
	id: i64,
	source_review_id: String,
	content_hash: String,
	capture_pending: bool,
	gone_at: Option<String>,
	published_est: Option<String>,
	published_raw: Option<String>,
	author: String,
	rating: Option<i64>,
	text: Option<String>,
}

const TARGET_COLUMNS: &str = "id, label, kind, place_id, gbp_account, gbp_location, lang, interval_secs, enabled, created_at";

impl Store {
	/// Opens (creating if need be) and migrates the database at `db_path`.
	pub async fn open(db_path: &Path) -> eyre::Result<Self> {
		if let Some(dir) = db_path.parent() {
			tokio::fs::create_dir_all(dir).await.wrap_err_with(|| format!("creating {}", dir.display()))?;
		}
		let opts = SqliteConnectOptions::new()
			.filename(db_path)
			.create_if_missing(true)
			.journal_mode(SqliteJournalMode::Wal)
			.foreign_keys(true)
			// `serve` and a hand-run `scan` may write at the same time
			.busy_timeout(Duration::from_secs(30));
		let pool = SqlitePoolOptions::new()
			.max_connections(4)
			.connect_with(opts)
			.await
			.wrap_err_with(|| format!("opening {}", db_path.display()))?;
		sqlx::migrate!("./migrations").run(&pool).await.wrap_err("applying migrations")?;
		Ok(Self { pool })
	}

	/// Adds a target, enabled.
	pub async fn add_target(&self, t: &NewTarget, now: Timestamp) -> eyre::Result<TargetId> {
		let id: i64 = sqlx::query_scalar(
			"INSERT INTO targets (label, kind, place_id, gbp_account, gbp_location, lang, interval_secs, enabled, created_at)
			 VALUES (?, ?, ?, ?, ?, ?, ?, 1, ?) RETURNING id",
		)
		.bind(&t.label)
		.bind(t.kind.as_str())
		.bind(&t.place_id)
		.bind(t.gbp.as_ref().map(|g| g.account.as_str()))
		.bind(t.gbp.as_ref().map(|g| g.location.as_str()))
		.bind(&t.lang)
		.bind(i64::try_from(t.interval.as_secs()).wrap_err("interval too large")?)
		.bind(fmt_ts(now))
		.fetch_one(&self.pool)
		.await
		.wrap_err("inserting target")?;
		Ok(TargetId(id))
	}

	/// Every target, by id.
	pub async fn targets(&self) -> eyre::Result<Vec<Target>> {
		let rows: Vec<TargetRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!("SELECT {TARGET_COLUMNS} FROM targets ORDER BY id")))
			.fetch_all(&self.pool)
			.await
			.wrap_err("listing targets")?;
		rows.into_iter().map(Target::try_from).collect()
	}

	/// One target; an error when there is none.
	pub async fn target(&self, id: TargetId) -> eyre::Result<Target> {
		let row: Option<TargetRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!("SELECT {TARGET_COLUMNS} FROM targets WHERE id = ?")))
			.bind(id.0)
			.fetch_optional(&self.pool)
			.await
			.wrap_err_with(|| format!("loading target {id}"))?;
		row.ok_or_else(|| eyre::eyre!("no target {id}"))?.try_into()
	}

	/// Enables or disables a target. Its archive stays either way.
	pub async fn set_enabled(&self, id: TargetId, enabled: bool) -> eyre::Result<()> {
		let done = sqlx::query("UPDATE targets SET enabled = ? WHERE id = ?")
			.bind(enabled)
			.bind(id.0)
			.execute(&self.pool)
			.await
			.wrap_err_with(|| format!("updating target {id}"))?;
		eyre::ensure!(done.rows_affected() == 1, "no target {id}");
		Ok(())
	}

	/// What the archive holds for a target, for reconciling.
	pub async fn known(&self, target: TargetId) -> eyre::Result<Known> {
		let rows: Vec<KnownRow> = sqlx::query_as(
			"SELECT id, source_review_id, content_hash, capture_pending, gone_at, published_est, published_raw, author, rating, text
			 FROM reviews WHERE target_id = ?",
		)
		.bind(target.0)
		.fetch_all(&self.pool)
		.await
		.wrap_err("loading known reviews")?;
		let mut reviews = HashMap::with_capacity(rows.len());
		for r in rows {
			reviews.insert(
				r.source_review_id,
				KnownReview {
					id: ReviewId(r.id),
					content_hash: r.content_hash,
					capture_pending: r.capture_pending,
					gone: r.gone_at.is_some(),
					published_est: r.published_est.as_deref().map(parse_ts).transpose()?,
					published_raw: r.published_raw,
					author: r.author,
					rating: r.rating.and_then(|r| u8::try_from(r).ok()),
					text: r.text,
				},
			);
		}
		Ok(Known { reviews })
	}

	/// Records that a run began.
	pub async fn start_run(&self, target: TargetId, now: Timestamp) -> eyre::Result<RunId> {
		let id: i64 = sqlx::query_scalar("INSERT INTO runs (target_id, started_at) VALUES (?, ?) RETURNING id")
			.bind(target.0)
			.bind(fmt_ts(now))
			.fetch_one(&self.pool)
			.await
			.wrap_err("recording run start")?;
		Ok(RunId(id))
	}

	/// Records how a run ended.
	pub async fn finish_run(&self, run: RunId, now: Timestamp, status: RunStatus, error: Option<&str>, counts: Counts) -> eyre::Result<()> {
		sqlx::query("UPDATE runs SET finished_at = ?, status = ?, error = ?, n_seen = ?, n_new = ?, n_changed = ?, n_gone = ? WHERE id = ?")
			.bind(fmt_ts(now))
			.bind(status.as_str())
			.bind(error)
			.bind(counts.seen)
			.bind(counts.new)
			.bind(counts.changed)
			.bind(counts.gone)
			.bind(run.0)
			.execute(&self.pool)
			.await
			.wrap_err("recording run end")?;
		Ok(())
	}

	/// The last finished run and how many failed in a row up to it.
	pub async fn last_run(&self, target: TargetId) -> eyre::Result<Option<LastRun>> {
		let rows: Vec<(String, String)> = sqlx::query_as("SELECT finished_at, status FROM runs WHERE target_id = ? AND finished_at IS NOT NULL ORDER BY id DESC LIMIT 64")
			.bind(target.0)
			.fetch_all(&self.pool)
			.await
			.wrap_err("loading run history")?;
		let Some((finished_at, _)) = rows.first() else {
			return Ok(None);
		};
		let consecutive_failures = rows.iter().take_while(|(_, s)| s == "failed").count();
		Ok(Some(LastRun {
			finished_at: parse_ts(finished_at)?,
			consecutive_failures: u32::try_from(consecutive_failures).expect("at most 64"),
		}))
	}

	/// Applies a reconciled scan in one transaction. `captures` is keyed by `source_review_id`.
	pub async fn apply(&self, target: TargetId, plan: &Plan<'_>, captures: &HashMap<String, StoredCapture>, scanner_version: &str, now: Timestamp) -> eyre::Result<Counts> {
		let now_s = fmt_ts(now);
		let mut tx = self.pool.begin().await?;

		for obs in &plan.new {
			let hash = obs.content_hash();
			let id: i64 = sqlx::query_scalar(
				"INSERT INTO reviews (target_id, source_review_id, author, author_url, rating, text, reply, photo_count,
				                      published_raw, published_est, first_seen, last_seen, content_hash, capture_pending)
				 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 1) RETURNING id",
			)
			.bind(target.0)
			.bind(&obs.source_review_id)
			.bind(&obs.author)
			.bind(&obs.author_url)
			.bind(obs.rating)
			.bind(&obs.text)
			.bind(&obs.reply)
			.bind(obs.photo_count)
			.bind(&obs.published_raw)
			.bind(obs.published_est.map(fmt_ts))
			.bind(&now_s)
			.bind(&now_s)
			.bind(&hash)
			.fetch_one(&mut *tx)
			.await
			.wrap_err_with(|| format!("inserting review {}", obs.source_review_id))?;
			insert_version(&mut tx, ReviewId(id), &now_s, &hash, obs).await?;
			record_capture(&mut tx, ReviewId(id), captures.get(&obs.source_review_id), scanner_version).await?;
		}

		for (id, obs) in &plan.changed {
			let hash = obs.content_hash();
			sqlx::query(
				"UPDATE reviews SET author = ?, author_url = ?, rating = ?, text = ?, reply = ?, photo_count = ?,
				                    published_raw = ?, published_est = COALESCE(published_est, ?),
				                    last_seen = ?, gone_at = NULL, content_hash = ?
				 WHERE id = ?",
			)
			.bind(&obs.author)
			.bind(&obs.author_url)
			.bind(obs.rating)
			.bind(&obs.text)
			.bind(&obs.reply)
			.bind(obs.photo_count)
			.bind(&obs.published_raw)
			.bind(obs.published_est.map(fmt_ts))
			.bind(&now_s)
			.bind(&hash)
			.bind(id.0)
			.execute(&mut *tx)
			.await
			.wrap_err("updating changed review")?;
			insert_version(&mut tx, *id, &now_s, &hash, obs).await?;
			record_capture(&mut tx, *id, captures.get(&obs.source_review_id), scanner_version).await?;
		}

		for (id, obs) in &plan.unchanged {
			// Only what is not content: the relative date text moves on by itself as time passes,
			// and the first estimate, made closest to publication, is the one kept.
			sqlx::query(
				"UPDATE reviews SET last_seen = ?, gone_at = NULL, photo_count = ?, author_url = COALESCE(?, author_url),
				                    published_est = COALESCE(published_est, ?)
				 WHERE id = ?",
			)
			.bind(&now_s)
			.bind(obs.photo_count)
			.bind(&obs.author_url)
			.bind(obs.published_est.map(fmt_ts))
			.bind(id.0)
			.execute(&mut *tx)
			.await
			.wrap_err("touching seen review")?;
			record_capture(&mut tx, *id, captures.get(&obs.source_review_id), scanner_version).await?;
		}

		for id in &plan.gone {
			sqlx::query("UPDATE reviews SET gone_at = ? WHERE id = ? AND gone_at IS NULL")
				.bind(&now_s)
				.bind(id.0)
				.execute(&mut *tx)
				.await
				.wrap_err("marking review gone")?;
		}

		tx.commit().await.wrap_err("committing scan")?;
		Ok(Counts {
			seen: u32::try_from(plan.seen()).unwrap_or(u32::MAX),
			new: u32::try_from(plan.new.len()).unwrap_or(u32::MAX),
			changed: u32::try_from(plan.changed.len()).unwrap_or(u32::MAX),
			gone: u32::try_from(plan.gone.len()).unwrap_or(u32::MAX),
		})
	}

	/// Reviews of a target, newest sighting first. `since` filters on `first_seen`.
	pub async fn reviews(&self, target: TargetId, since: Option<Timestamp>, gone: Option<bool>) -> eyre::Result<Vec<ReviewDto>> {
		let rows: Vec<ReviewRow> = sqlx::query_as(
			"SELECT r.id, r.target_id, r.source_review_id, r.author, r.author_url, r.rating, r.text, r.reply, r.photo_count,
			        r.published_raw, r.published_est, r.first_seen, r.last_seen, r.gone_at, r.capture_pending,
			        c.sha256 AS capture_sha256, c.captured_at AS captured_at
			 FROM reviews r
			 LEFT JOIN captures c ON c.id = (SELECT MIN(id) FROM captures WHERE review_id = r.id)
			 WHERE r.target_id = ?1
			   AND (?2 IS NULL OR r.first_seen >= ?2)
			   AND (?3 IS NULL OR (r.gone_at IS NOT NULL) = ?3)
			 ORDER BY r.first_seen DESC, r.id DESC",
		)
		.bind(target.0)
		.bind(since.map(fmt_ts))
		.bind(gone)
		.fetch_all(&self.pool)
		.await
		.wrap_err("listing reviews")?;
		Ok(rows.into_iter().map(ReviewDto::from).collect())
	}

	/// Whether a capture with this hash was recorded.
	pub async fn capture_exists(&self, sha256: &str) -> eyre::Result<bool> {
		let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM captures WHERE sha256 = ?")
			.bind(sha256)
			.fetch_one(&self.pool)
			.await
			.wrap_err("looking up capture")?;
		Ok(n > 0)
	}

	/// Per target and UTC day, over `[from, to]` inclusive.
	pub async fn stats(&self, target: Option<TargetId>, from: Option<Date>, to: Option<Date>) -> eyre::Result<Vec<DayStats>> {
		#[derive(FromRow)]
		struct Row {
			target_id: i64,
			day: String,
			new: i64,
			changed: i64,
			gone: i64,
			mean_rating: Option<f64>,
			r1: i64,
			r2: i64,
			r3: i64,
			r4: i64,
			r5: i64,
		}
		// `changed` counts versions past each review's first; `new`, the mean and the histogram
		// are over reviews by the day they were first seen, with the rating they had then —
		// a later edit is that later day's `changed`, not a rewrite of the day it arrived.
		let rows: Vec<Row> = sqlx::query_as(
			"WITH events AS (
			     SELECT r.target_id, date(r.first_seen) AS day, 1 AS new, 0 AS changed, 0 AS gone,
			            (SELECT v.rating FROM review_versions v WHERE v.review_id = r.id ORDER BY v.id LIMIT 1) AS rating
			       FROM reviews r
			     UNION ALL
			     SELECT r.target_id, date(v.seen_at), 0, 1, 0, NULL
			       FROM review_versions v JOIN reviews r ON r.id = v.review_id
			      WHERE v.id <> (SELECT MIN(id) FROM review_versions WHERE review_id = v.review_id)
			     UNION ALL
			     SELECT target_id, date(gone_at), 0, 0, 1, NULL FROM reviews WHERE gone_at IS NOT NULL
			 )
			 SELECT target_id, day, SUM(new) AS new, SUM(changed) AS changed, SUM(gone) AS gone,
			        AVG(CASE WHEN new = 1 THEN rating END) AS mean_rating,
			        SUM(new = 1 AND rating = 1) AS r1, SUM(new = 1 AND rating = 2) AS r2, SUM(new = 1 AND rating = 3) AS r3,
			        SUM(new = 1 AND rating = 4) AS r4, SUM(new = 1 AND rating = 5) AS r5
			 FROM events
			 WHERE (?1 IS NULL OR target_id = ?1) AND (?2 IS NULL OR day >= ?2) AND (?3 IS NULL OR day <= ?3)
			 GROUP BY target_id, day
			 ORDER BY target_id, day",
		)
		.bind(target.map(|t| t.0))
		.bind(from.map(|d| d.to_string()))
		.bind(to.map(|d| d.to_string()))
		.fetch_all(&self.pool)
		.await
		.wrap_err("computing stats")?;
		Ok(rows
			.into_iter()
			.map(|r| DayStats {
				target_id: r.target_id,
				day: r.day,
				new: r.new,
				changed: r.changed,
				gone: r.gone,
				mean_rating: r.mean_rating,
				histogram: [r.r1, r.r2, r.r3, r.r4, r.r5],
			})
			.collect())
	}

	/// The version history of one review, oldest first: `(seen_at, content_hash, rating, text, reply)`.
	pub async fn versions(&self, review: ReviewId) -> eyre::Result<Vec<(String, String, Option<i64>, Option<String>, Option<String>)>> {
		sqlx::query_as("SELECT seen_at, content_hash, rating, text, reply FROM review_versions WHERE review_id = ? ORDER BY id")
			.bind(review.0)
			.fetch_all(&self.pool)
			.await
			.wrap_err("loading versions")
	}

	/// How many captures a review has.
	pub async fn capture_count(&self, review: ReviewId) -> eyre::Result<i64> {
		sqlx::query_scalar("SELECT COUNT(*) FROM captures WHERE review_id = ?")
			.bind(review.0)
			.fetch_one(&self.pool)
			.await
			.wrap_err("counting captures")
	}
}

async fn insert_version(tx: &mut SqliteConnection, review: ReviewId, seen_at: &str, hash: &str, obs: &Observed) -> eyre::Result<()> {
	sqlx::query("INSERT INTO review_versions (review_id, seen_at, content_hash, rating, text, reply) VALUES (?, ?, ?, ?, ?, ?)")
		.bind(review.0)
		.bind(seen_at)
		.bind(hash)
		.bind(obs.rating)
		.bind(&obs.text)
		.bind(&obs.reply)
		.execute(tx)
		.await
		.wrap_err("inserting review version")?;
	Ok(())
}

async fn record_capture(tx: &mut SqliteConnection, review: ReviewId, capture: Option<&StoredCapture>, scanner_version: &str) -> eyre::Result<()> {
	let Some(c) = capture else {
		return Ok(());
	};
	sqlx::query("INSERT INTO captures (review_id, captured_at, sha256, width, height, page_url, scanner_version) VALUES (?, ?, ?, ?, ?, ?, ?)")
		.bind(review.0)
		.bind(fmt_ts(c.captured_at))
		.bind(&c.sha256)
		.bind(c.width)
		.bind(c.height)
		.bind(&c.page_url)
		.bind(scanner_version)
		.execute(&mut *tx)
		.await
		.wrap_err("inserting capture")?;
	sqlx::query("UPDATE reviews SET capture_pending = 0 WHERE id = ?")
		.bind(review.0)
		.execute(&mut *tx)
		.await
		.wrap_err("clearing capture_pending")?;
	Ok(())
}
