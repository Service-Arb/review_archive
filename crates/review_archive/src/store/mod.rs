//! SQLite (runtime `sqlx` queries, embedded migrations): targets, reviews and their
//! history, captures, runs. PNGs live beside it in [`blobs`].
//!
//! Every write that reads first takes the database's write lock up front (`BEGIN
//! IMMEDIATE`): `serve`'s worker, its HTTP side and a hand-run `scan` write at once, and a
//! read-then-write transaction that only asks for the lock at its first write fails when
//! another writer got in between.

pub mod blobs;
mod events;
pub mod export;
mod jobs;
mod members;
mod webhooks;

use std::{collections::HashMap, path::Path, time::Duration};

use eyre::WrapErr;
use jiff::{Timestamp, civil::Date};
pub use jobs::ClaimedJob;
use review_archive_core::{
	GbpLocation, Known, KnownReview, Observed, Rejected, ReviewId, Target, TargetId, TargetKind, check_lang,
	dto::{CaptureDto, Counts, DayStats, Event, JobStatus, ReviewDetail, ReviewDto, RunDto, RunStatus, TargetPatch, VersionDto, capture_url},
	fmt_ts, parse_interval,
	reconcile::Plan,
	schedule::{Breaker, LastRun, Schedule},
};
use sqlx::{
	FromRow, SqliteConnection, Transaction,
	sqlite::{Sqlite, SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
};
use v_utils::Timeframe;
pub use webhooks::{Delivery, Recipient};

fn parse_ts(s: &str) -> eyre::Result<Timestamp> {
	s.parse().wrap_err_with(|| format!("stored timestamp {s:?}"))
}

/// A count as the API reports it; a count past `u32::MAX` says `u32::MAX`.
pub(crate) fn sat_u32(n: impl TryInto<u32>) -> u32 {
	n.try_into().unwrap_or(u32::MAX)
}

/// The archive's database.
#[derive(Clone, Debug)]
pub struct Store {
	pool: sqlx::SqlitePool,
}

/// A target to insert.
#[derive(Clone, Debug)]
pub struct InsertTarget {
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
	/// At least `schedule.min_interval`.
	pub interval: Timeframe,
	/// Disabled targets are kept but not scheduled (ad-hoc captures).
	pub enabled: bool,
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

/// How a run ended, and the job it ran for, if any: both are written in one transaction.
#[derive(Clone, Copy, Debug)]
pub struct RunEnd<'a> {
	/// The run.
	pub run: RunId,
	/// How it went.
	pub status: RunStatus,
	/// The failure, or the warnings of a partial run.
	pub error: Option<&'a str>,
	/// The job whose run this is.
	pub job: Option<i64>,
}

/// What a scan saw, ready to be written.
#[derive(Clone, Copy, Debug)]
pub struct ScanWrite<'a> {
	/// The reconciled scan.
	pub plan: &'a Plan<'a>,
	/// Screenshots, keyed by `source_review_id`.
	pub captures: &'a HashMap<String, StoredCapture>,
	/// See [`review_archive_core::Scan::cut_after`].
	pub cut_after: Option<&'a str>,
	/// See [`review_archive_core::Scan::listed`].
	pub listed: Option<u64>,
	/// An ad-hoc capture: it may record a gap but never closes one.
	pub ad_hoc: bool,
	/// Recorded with each capture.
	pub scanner_version: &'a str,
}

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
			capture_url: r.capture_sha256.as_deref().map(capture_url),
			capture_sha256: r.capture_sha256,
			captured_at: r.captured_at,
		}
	}
}

/// Every column a [`ReviewDto`] needs, the first capture joined in; filter with `WHERE`.
const REVIEW_SELECT: &str = "SELECT r.id, r.target_id, r.source_review_id, r.author, r.author_url, r.rating, r.text, r.reply, r.photo_count,
        r.published_raw, r.published_est, r.first_seen, r.last_seen, r.gone_at, r.capture_pending,
        c.sha256 AS capture_sha256, c.captured_at AS captured_at
 FROM reviews r
 LEFT JOIN captures c ON c.id = (SELECT MIN(id) FROM captures WHERE review_id = r.id)";

#[derive(FromRow)]
struct RunRow {
	id: i64,
	target_id: i64,
	started_at: String,
	finished_at: Option<String>,
	status: Option<String>,
	error: Option<String>,
	n_seen: i64,
	n_new: i64,
	n_changed: i64,
	n_gone: i64,
}

const RUN_COLUMNS: &str = "id, target_id, started_at, finished_at, status, error, n_seen, n_new, n_changed, n_gone";

impl TryFrom<RunRow> for RunDto {
	type Error = eyre::Report;

	fn try_from(r: RunRow) -> eyre::Result<Self> {
		Ok(Self {
			status: r.status.as_deref().map(str::parse).transpose().wrap_err_with(|| format!("run {} has an unknown status", r.id))?,
			id: r.id,
			target_id: r.target_id,
			started_at: r.started_at,
			finished_at: r.finished_at,
			error: r.error,
			counts: Counts {
				seen: sat_u32(r.n_seen),
				new: sat_u32(r.n_new),
				changed: sat_u32(r.n_changed),
				gone: sat_u32(r.n_gone),
			},
		})
	}
}

#[derive(FromRow)]
struct CaptureRow {
	sha256: String,
	captured_at: String,
	width: i64,
	height: i64,
	page_url: String,
	scanner_version: String,
}

impl From<CaptureRow> for CaptureDto {
	fn from(c: CaptureRow) -> Self {
		Self {
			url: capture_url(&c.sha256),
			sha256: c.sha256,
			captured_at: c.captured_at,
			width: c.width,
			height: c.height,
			page_url: c.page_url,
			scanner_version: c.scanner_version,
		}
	}
}

#[derive(FromRow)]
struct VersionRow {
	seen_at: String,
	content_hash: String,
	rating: Option<i64>,
	text: Option<String>,
	reply: Option<String>,
}

impl From<VersionRow> for VersionDto {
	fn from(v: VersionRow) -> Self {
		Self {
			seen_at: v.seen_at,
			content_hash: v.content_hash,
			rating: v.rating,
			text: v.text,
			reply: v.reply,
		}
	}
}

/// What [`Store::apply`] wrote.
#[derive(Clone, Debug, Default)]
pub struct Applied {
	/// The counts of the run.
	pub counts: Counts,
	/// Every review the scan listed, in its order.
	pub seen: Vec<ReviewId>,
}

/// Archived totals of a target.
#[derive(Clone, Copy, Debug, Default, FromRow)]
pub struct TargetCounts {
	/// Reviews archived.
	pub reviews: i64,
	/// Of which gone now.
	pub gone: i64,
	/// Screenshots.
	pub captures: i64,
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
		let gbp = r.gbp_account.zip(r.gbp_location).map(|(account, location)| GbpLocation { account, location });
		Ok(Self {
			id: TargetId(r.id),
			label: r.label,
			kind: r.kind.parse().wrap_err_with(|| format!("target {} has an unknown kind", r.id))?,
			place_id: r.place_id,
			gbp,
			lang: r.lang,
			interval: Timeframe(
				u64::try_from(r.interval_secs)
					.wrap_err("negative interval_secs")?
					.checked_mul(1000)
					.ok_or_else(|| eyre::eyre!("interval_secs {} overflows", r.interval_secs))?,
			),
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

fn no_target(id: TargetId) -> Rejected {
	Rejected::not_found(format!("no target {id}"))
}

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

	/// A write transaction that holds the write lock from its start.
	async fn write(&self) -> eyre::Result<Transaction<'static, Sqlite>> {
		self.pool.begin_with("BEGIN IMMEDIATE").await.wrap_err("starting a write")
	}

	/// Adds a target.
	pub async fn add_target(&self, t: &InsertTarget, now: Timestamp) -> eyre::Result<TargetId> {
		let id: i64 = sqlx::query_scalar(
			"INSERT INTO targets (label, kind, place_id, gbp_account, gbp_location, lang, interval_secs, enabled, created_at)
			 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?) RETURNING id",
		)
		.bind(&t.label)
		.bind(t.kind.as_ref())
		.bind(&t.place_id)
		.bind(t.gbp.as_ref().map(|g| g.account.as_str()))
		.bind(t.gbp.as_ref().map(|g| g.location.as_str()))
		.bind(&t.lang)
		.bind(i64::try_from(t.interval.duration().as_secs()).map_err(|_| Rejected::invalid("the interval is too long"))?)
		.bind(t.enabled)
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

	/// One target; [`Rejected::NotFound`] when there is none.
	pub async fn target(&self, id: TargetId) -> eyre::Result<Target> {
		let row: Option<TargetRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!("SELECT {TARGET_COLUMNS} FROM targets WHERE id = ?")))
			.bind(id.0)
			.fetch_optional(&self.pool)
			.await
			.wrap_err_with(|| format!("loading target {id}"))?;
		row.ok_or_else(|| no_target(id))?.try_into()
	}

	/// Changes what the patch sets.
	pub async fn update_target(&self, id: TargetId, patch: &TargetPatch) -> eyre::Result<()> {
		if let Some(lang) = &patch.lang {
			check_lang(lang)?;
		}
		let interval = patch.interval.as_deref().map(parse_interval).transpose()?;
		let done = sqlx::query(
			"UPDATE targets SET label = COALESCE(?, label), lang = COALESCE(?, lang), interval_secs = COALESCE(?, interval_secs),
			                    enabled = COALESCE(?, enabled)
			 WHERE id = ?",
		)
		.bind(patch.label.as_deref())
		.bind(patch.lang.as_deref())
		// parse_interval keeps it within i64
		.bind(interval.map(|i| i.duration().as_secs() as i64))
		.bind(patch.enabled)
		.bind(id.0)
		.execute(&self.pool)
		.await
		.wrap_err_with(|| format!("updating target {id}"))?;
		if done.rows_affected() != 1 {
			return Err(no_target(id).into());
		}
		Ok(())
	}

	/// The target on this place and language, read from this GBP location (`None`: from
	/// Maps), if any: where ad-hoc captures and members' tracks go.
	pub async fn find_target(&self, place_id: &str, lang: &str, gbp: Option<&GbpLocation>) -> eyre::Result<Option<Target>> {
		let row: Option<TargetRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
			"SELECT {TARGET_COLUMNS} FROM targets
			 WHERE place_id = ?1 AND lang = ?2 AND kind = IIF(?3 IS NULL, 'maps', 'gbp') AND gbp_account IS ?3 AND gbp_location IS ?4
			 ORDER BY id LIMIT 1"
		)))
		.bind(place_id)
		.bind(lang)
		.bind(gbp.map(|g| g.account.as_str()))
		.bind(gbp.map(|g| g.location.as_str()))
		.fetch_optional(&self.pool)
		.await
		.wrap_err("looking up a target by place")?;
		row.map(Target::try_from).transpose()
	}

	/// What the archive holds for a target, for reconciling and for where to walk.
	pub async fn known(&self, target: TargetId) -> eyre::Result<Known> {
		let (cut_after, initial): (Option<String>, bool) = sqlx::query_as(
			"SELECT cut_after, NOT EXISTS (SELECT 1 FROM runs WHERE target_id = ?1 AND ad_hoc = 0 AND status IN ('ok', 'partial'))
			 FROM targets WHERE id = ?1",
		)
		.bind(target.0)
		.fetch_optional(&self.pool)
		.await
		.wrap_err("loading a target's scan state")?
		.ok_or_else(|| no_target(target))?;
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
			let review = KnownReview {
				id: ReviewId(r.id),
				content_hash: r.content_hash,
				capture_pending: r.capture_pending,
				gone: r.gone_at.is_some(),
				published_est: r.published_est.as_deref().map(parse_ts).transpose()?,
				published_raw: r.published_raw,
				author: r.author,
				rating: r.rating.and_then(|r| u8::try_from(r).ok()),
				text: r.text,
			};
			reviews.insert(r.source_review_id, review);
		}
		Ok(Known { reviews, initial, cut_after })
	}

	/// Records that a run began. An ad-hoc run (a capture) does not count for the schedule.
	pub async fn start_run(&self, target: TargetId, ad_hoc: bool, now: Timestamp) -> eyre::Result<RunId> {
		let id: i64 = sqlx::query_scalar("INSERT INTO runs (target_id, started_at, ad_hoc) VALUES (?, ?, ?) RETURNING id")
			.bind(target.0)
			.bind(fmt_ts(now))
			.bind(ad_hoc)
			.fetch_one(&self.pool)
			.await
			.wrap_err("recording run start")?;
		Ok(RunId(id))
	}

	/// Records a run that stored nothing (and its job): `run.failed` goes out.
	pub async fn fail_run(&self, end: RunEnd<'_>, now: Timestamp) -> eyre::Result<()> {
		let mut tx = self.write().await?;
		finish(&mut tx, end, Counts::default(), &[], now).await?;
		events::Emitter::load(&mut tx, now).await?.run_failed(&mut tx, end.run).await?;
		tx.commit().await.wrap_err("committing a failed run")
	}

	/// The last finished scheduled run and how many failed in a row up to it. Ad-hoc
	/// captures are not the target's schedule and do not count.
	pub async fn last_run(&self, target: TargetId) -> eyre::Result<Option<LastRun>> {
		let rows: Vec<(String, String)> = sqlx::query_as("SELECT finished_at, status FROM runs WHERE target_id = ? AND ad_hoc = 0 AND finished_at IS NOT NULL ORDER BY id DESC LIMIT 64")
			.bind(target.0)
			.fetch_all(&self.pool)
			.await
			.wrap_err("loading run history")?;
		let Some((finished_at, _)) = rows.first() else {
			return Ok(None);
		};
		Ok(Some(LastRun {
			finished_at: parse_ts(finished_at)?,
			consecutive_failures: sat_u32(rows.iter().take_while(|(_, s)| s == RunStatus::Failed.as_ref()).count()),
		}))
	}

	/// The pause on Maps Google's block put in force, if one is.
	pub async fn breaker(&self) -> eyre::Result<Option<Breaker>> {
		let row: Option<(String, String, i64, String)> = sqlx::query_as("SELECT tripped_at, reason_code, trips, probe_after FROM maps_breaker")
			.fetch_optional(&self.pool)
			.await
			.wrap_err("loading the maps breaker")?;
		row.map(|(tripped_at, reason, trips, probe_after)| {
			Ok(Breaker {
				tripped_at: parse_ts(&tripped_at)?,
				reason,
				trips: u32::try_from(trips).wrap_err("the breaker's trips")?,
				probe_after: parse_ts(&probe_after)?,
			})
		})
		.transpose()
	}

	/// Pauses Maps, or pauses it for longer after a failed probe.
	pub async fn trip_breaker(&self, reason: &str, now: Timestamp, schedule: &Schedule) -> eyre::Result<Breaker> {
		let mut tx = self.write().await?;
		let prev: Option<(String, i64)> = sqlx::query_as("SELECT tripped_at, trips FROM maps_breaker")
			.fetch_optional(&mut *tx)
			.await
			.wrap_err("loading the maps breaker")?;
		let prev = prev
			.map(|(tripped_at, trips)| {
				eyre::Ok(Breaker {
					tripped_at: parse_ts(&tripped_at)?,
					reason: String::new(),
					trips: u32::try_from(trips).wrap_err("the breaker's trips")?,
					probe_after: now,
				})
			})
			.transpose()?;
		let b = Breaker::trip(prev, reason, now, schedule);
		sqlx::query("INSERT OR REPLACE INTO maps_breaker (id, tripped_at, reason_code, trips, probe_after) VALUES (1, ?, ?, ?, ?)")
			.bind(fmt_ts(b.tripped_at))
			.bind(&b.reason)
			.bind(i64::from(b.trips))
			.bind(fmt_ts(b.probe_after))
			.execute(&mut *tx)
			.await
			.wrap_err("tripping the maps breaker")?;
		tx.commit().await.wrap_err("committing the maps breaker")?;
		Ok(b)
	}

	/// Lets Maps go again; whether it was paused.
	pub async fn reset_breaker(&self) -> eyre::Result<bool> {
		let done = sqlx::query("DELETE FROM maps_breaker").execute(&self.pool).await.wrap_err("resetting the maps breaker")?;
		Ok(done.rows_affected() > 0)
	}

	/// Applies a reconciled scan in one transaction: the reviews, the webhook events for
	/// what changed, where the walk was cut short, and how the run (and its job) ended.
	pub async fn apply(&self, target: TargetId, scan: ScanWrite<'_>, end: RunEnd<'_>, now: Timestamp) -> eyre::Result<Applied> {
		let plan = scan.plan;
		let now_s = fmt_ts(now);
		let mut tx = self.write().await?;
		let mut emitter = events::Emitter::load(&mut tx, now).await?;
		let mut seen: Vec<(usize, ReviewId)> = Vec::with_capacity(plan.seen());

		for obs in &plan.new {
			let id = insert_review(&mut tx, target, obs, &now_s).await?;
			record_capture(&mut tx, id, scan.captures.get(&obs.source_review_id), scan.scanner_version).await?;
			emitter.review(&mut tx, Event::ReviewNew, id).await?;
			seen.push((plan.position(&obs.source_review_id), id));
		}
		let listed = plan
			.changed
			.iter()
			.map(|&(id, obs)| (id, obs, true))
			.chain(plan.unchanged.iter().map(|&(id, obs)| (id, obs, false)));
		for (id, obs, changed) in listed {
			touch_review(&mut tx, id, obs, changed, &now_s).await?;
			if changed {
				insert_version(&mut tx, id, &now_s, obs).await?;
			}
			record_capture(&mut tx, id, scan.captures.get(&obs.source_review_id), scan.scanner_version).await?;
			if changed {
				emitter.review(&mut tx, Event::ReviewChanged, id).await?;
			}
			seen.push((plan.position(&obs.source_review_id), id));
		}
		for &id in &plan.reappeared {
			sqlx::query("UPDATE reinstatements SET reinstated_at = ? WHERE review_id = ? AND withdrawn_at IS NULL AND reinstated_at IS NULL")
				.bind(&now_s)
				.bind(id.0)
				.execute(&mut *tx)
				.await
				.wrap_err("closing the appeals of a reappeared review")?;
			emitter.review(&mut tx, Event::ReviewReappeared, id).await?;
		}
		for &id in &plan.gone {
			sqlx::query("UPDATE reviews SET gone_at = ? WHERE id = ? AND gone_at IS NULL")
				.bind(&now_s)
				.bind(id.0)
				.execute(&mut *tx)
				.await
				.wrap_err("marking review gone")?;
			emitter.review(&mut tx, Event::ReviewGone, id).await?;
		}
		// an ad-hoc capture may leave a gap of its own, but only a scan walks down to close one
		sqlx::query("UPDATE targets SET cut_after = CASE WHEN ?2 THEN COALESCE(cut_after, ?1) ELSE ?1 END WHERE id = ?3")
			.bind(scan.cut_after)
			.bind(scan.ad_hoc)
			.bind(target.0)
			.execute(&mut *tx)
			.await
			.wrap_err("recording where the walk stopped")?;
		if let Some(listed) = scan.listed {
			sqlx::query("UPDATE targets SET listed = ? WHERE id = ?")
				.bind(i64::try_from(listed).expect("Google lists fewer than 2^63 reviews"))
				.bind(target.0)
				.execute(&mut *tx)
				.await
				.wrap_err("recording how many reviews the source lists")?;
		}

		seen.sort_unstable();
		let seen: Vec<ReviewId> = seen.into_iter().map(|(_, id)| id).collect();
		let counts = Counts {
			seen: sat_u32(plan.seen()),
			new: sat_u32(plan.new.len()),
			changed: sat_u32(plan.changed.len()),
			gone: sat_u32(plan.gone.len()),
		};
		finish(&mut tx, end, counts, &seen, now).await?;
		tx.commit().await.wrap_err("committing scan")?;
		Ok(Applied { counts, seen })
	}

	/// Fails the jobs and runs a previous process died running; queued jobs stay queued.
	/// Returns the jobs failed.
	pub async fn fail_interrupted(&self, now: Timestamp) -> eyre::Result<u64> {
		const WHY: &str = "interrupted: the service stopped while it ran";
		let mut tx = self.write().await?;
		sqlx::query("UPDATE runs SET status = 'failed', finished_at = ?, error = ? WHERE finished_at IS NULL")
			.bind(fmt_ts(now))
			.bind(WHY)
			.execute(&mut *tx)
			.await
			.wrap_err("failing interrupted runs")?;
		let jobs = sqlx::query("UPDATE jobs SET status = 'failed', finished_at = ?, error = ? WHERE status = 'running'")
			.bind(fmt_ts(now))
			.bind(WHY)
			.execute(&mut *tx)
			.await
			.wrap_err("failing interrupted jobs")?;
		tx.commit().await.wrap_err("committing recovery")?;
		Ok(jobs.rows_affected())
	}

	/// Reviews of a target, newest sighting first. `since` filters on `first_seen`.
	pub async fn reviews(&self, target: TargetId, since: Option<Timestamp>, gone: Option<bool>) -> eyre::Result<Vec<ReviewDto>> {
		let rows: Vec<ReviewRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
			"{REVIEW_SELECT}
			 WHERE r.target_id = ?1
			   AND (?2 IS NULL OR r.first_seen >= ?2)
			   AND (?3 IS NULL OR (r.gone_at IS NOT NULL) = ?3)
			 ORDER BY r.first_seen DESC, r.id DESC"
		)))
		.bind(target.0)
		.bind(since.map(fmt_ts))
		.bind(gone)
		.fetch_all(&self.pool)
		.await
		.wrap_err("listing reviews")?;
		Ok(rows.into_iter().map(ReviewDto::from).collect())
	}

	/// These reviews, in the order given; ids that do not exist are left out.
	pub async fn reviews_by_id(&self, ids: &[ReviewId]) -> eyre::Result<Vec<ReviewDto>> {
		let ids = serde_json::to_string(&ids.iter().map(|r| r.0).collect::<Vec<_>>())?;
		let rows: Vec<ReviewRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!("{REVIEW_SELECT} JOIN json_each(?) j ON j.value = r.id ORDER BY j.key")))
			.bind(ids)
			.fetch_all(&self.pool)
			.await
			.wrap_err("loading reviews by id")?;
		Ok(rows.into_iter().map(ReviewDto::from).collect())
	}

	/// A review with every version and capture; `None` when there is none.
	pub async fn review(&self, id: ReviewId) -> eyre::Result<Option<ReviewDetail>> {
		let Some(review) = review_by_id(&self.pool, id).await? else {
			return Ok(None);
		};
		let captures: Vec<CaptureRow> = sqlx::query_as("SELECT sha256, captured_at, width, height, page_url, scanner_version FROM captures WHERE review_id = ? ORDER BY id")
			.bind(id.0)
			.fetch_all(&self.pool)
			.await
			.wrap_err("loading captures")?;
		Ok(Some(ReviewDetail {
			review,
			versions: self.versions(id).await?,
			captures: captures.into_iter().map(CaptureDto::from).collect(),
		}))
	}

	/// How much is archived for a target.
	pub async fn target_counts(&self, id: TargetId) -> eyre::Result<TargetCounts> {
		sqlx::query_as(
			"SELECT (SELECT COUNT(*) FROM reviews WHERE target_id = ?1) AS reviews,
			        (SELECT COUNT(*) FROM reviews WHERE target_id = ?1 AND gone_at IS NOT NULL) AS gone,
			        (SELECT COUNT(*) FROM captures c JOIN reviews r ON r.id = c.review_id WHERE r.target_id = ?1) AS captures",
		)
		.bind(id.0)
		.fetch_one(&self.pool)
		.await
		.wrap_err("counting a target's archive")
	}

	/// A target's runs, newest first.
	pub async fn runs(&self, target: TargetId, limit: u32) -> eyre::Result<Vec<RunDto>> {
		let rows: Vec<RunRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!("SELECT {RUN_COLUMNS} FROM runs WHERE target_id = ? ORDER BY id DESC LIMIT ?")))
			.bind(target.0)
			.bind(limit)
			.fetch_all(&self.pool)
			.await
			.wrap_err("listing runs")?;
		rows.into_iter().map(RunDto::try_from).collect()
	}

	/// One run.
	pub async fn run(&self, id: RunId) -> eyre::Result<Option<RunDto>> {
		run_by_id(&self.pool, id).await
	}

	/// Whether a capture with this hash was recorded.
	pub async fn capture_exists(&self, sha256: &str) -> eyre::Result<bool> {
		sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM captures WHERE sha256 = ?)")
			.bind(sha256)
			.fetch_one(&self.pool)
			.await
			.wrap_err("looking up capture")
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
		// `gone` is what each run marked gone, on the day it ran: a review that comes back
		// later does not undo the day it went missing.
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
			     SELECT target_id, date(finished_at), 0, 0, n_gone, NULL FROM runs WHERE n_gone > 0
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

	/// The version history of one review, oldest first.
	pub async fn versions(&self, review: ReviewId) -> eyre::Result<Vec<VersionDto>> {
		let rows: Vec<VersionRow> = sqlx::query_as("SELECT seen_at, content_hash, rating, text, reply FROM review_versions WHERE review_id = ? ORDER BY id")
			.bind(review.0)
			.fetch_all(&self.pool)
			.await
			.wrap_err("loading versions")?;
		Ok(rows.into_iter().map(VersionDto::from).collect())
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

async fn review_by_id<'e, E: sqlx::SqliteExecutor<'e>>(db: E, id: ReviewId) -> eyre::Result<Option<ReviewDto>> {
	let row: Option<ReviewRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!("{REVIEW_SELECT} WHERE r.id = ?")))
		.bind(id.0)
		.fetch_optional(db)
		.await
		.wrap_err_with(|| format!("loading review {id}"))?;
	Ok(row.map(ReviewDto::from))
}

async fn run_by_id<'e, E: sqlx::SqliteExecutor<'e>>(db: E, id: RunId) -> eyre::Result<Option<RunDto>> {
	let row: Option<RunRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!("SELECT {RUN_COLUMNS} FROM runs WHERE id = ?")))
		.bind(id.0)
		.fetch_optional(db)
		.await
		.wrap_err_with(|| format!("loading run {}", id.0))?;
	row.map(RunDto::try_from).transpose()
}

/// Ends the run, and its job with it: a failed run fails the job, anything else is done.
async fn finish(tx: &mut SqliteConnection, end: RunEnd<'_>, counts: Counts, seen: &[ReviewId], now: Timestamp) -> eyre::Result<()> {
	sqlx::query("UPDATE runs SET finished_at = ?, status = ?, error = ?, n_seen = ?, n_new = ?, n_changed = ?, n_gone = ? WHERE id = ?")
		.bind(fmt_ts(now))
		.bind(end.status.as_ref())
		.bind(end.error)
		.bind(counts.seen)
		.bind(counts.new)
		.bind(counts.changed)
		.bind(counts.gone)
		.bind(end.run.0)
		.execute(&mut *tx)
		.await
		.wrap_err("recording run end")?;
	if let Some(job) = end.job {
		let failed = end.status == RunStatus::Failed;
		let status = if failed { JobStatus::Failed } else { JobStatus::Done };
		jobs::finish(tx, job, status, Some(end.run), end.error.filter(|_| failed), seen, now).await?;
	}
	Ok(())
}

async fn insert_review(tx: &mut SqliteConnection, target: TargetId, obs: &Observed, now: &str) -> eyre::Result<ReviewId> {
	let id: i64 = sqlx::query_scalar(
		"INSERT INTO reviews (target_id, source_review_id, author, author_url, rating, text, reply, photo_count,
		                      published_raw, published_est, first_seen, last_seen, content_hash, capture_pending)
		 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?11, ?12, 1) RETURNING id",
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
	.bind(now)
	.bind(obs.content_hash())
	.fetch_one(&mut *tx)
	.await
	.wrap_err_with(|| format!("inserting review {}", obs.source_review_id))?;
	insert_version(tx, ReviewId(id), now, obs).await?;
	Ok(ReviewId(id))
}

/// A review listed again: seen now, not gone, and — when `changed` — its new content.
///
/// The date is kept as first estimated, together with the text it was estimated from: the
/// relative date moves on by itself as time passes ("Edited a day ago" after an edit), and
/// the estimate made closest to publication is the best one. A later reading only fills in
/// a date that was never read.
async fn touch_review(tx: &mut SqliteConnection, id: ReviewId, obs: &Observed, changed: bool, now: &str) -> eyre::Result<()> {
	sqlx::query(
		"UPDATE reviews SET last_seen = ?1, gone_at = NULL, photo_count = ?2, author_url = COALESCE(?3, author_url),
		                    published_raw = IIF(published_est IS NULL, ?4, published_raw), published_est = COALESCE(published_est, ?5),
		                    author = IIF(?6, ?7, author), rating = IIF(?6, ?8, rating), text = IIF(?6, ?9, text),
		                    reply = IIF(?6, ?10, reply), content_hash = IIF(?6, ?11, content_hash)
		 WHERE id = ?12",
	)
	.bind(now)
	.bind(obs.photo_count)
	.bind(&obs.author_url)
	.bind(&obs.published_raw)
	.bind(obs.published_est.map(fmt_ts))
	.bind(changed)
	.bind(&obs.author)
	.bind(obs.rating)
	.bind(&obs.text)
	.bind(&obs.reply)
	.bind(obs.content_hash())
	.bind(id.0)
	.execute(&mut *tx)
	.await
	.wrap_err("updating a listed review")?;
	Ok(())
}

async fn insert_version(tx: &mut SqliteConnection, review: ReviewId, seen_at: &str, obs: &Observed) -> eyre::Result<()> {
	sqlx::query("INSERT INTO review_versions (review_id, seen_at, content_hash, rating, text, reply) VALUES (?, ?, ?, ?, ?, ?)")
		.bind(review.0)
		.bind(seen_at)
		.bind(obs.content_hash())
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
