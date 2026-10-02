//! What is a member's own: their managing gmails, what each tracks, their appeals and
//! their Telegram channels. Every query here is scoped by the member's email; the
//! archive under it (targets, reviews, captures) stays shared.

use eyre::WrapErr;
use jiff::{SignedDuration, Timestamp};
use review_archive_core::{
	Rejected, ReviewId, Target, TargetId,
	dto::{Board, BoardCard, GmailDto, GmailOverview, LocationSummary, NewTgChannel, ReinstatementDto, ReviewDto, TgChannelDto},
	fmt_ts,
};
use sqlx::FromRow;

use super::{REVIEW_SELECT, ReviewRow, Store, TargetRow, webhooks::parse_events};

#[derive(FromRow)]
struct GmailRow {
	id: i64,
	gmail: String,
	enabled: bool,
	created_at: String,
}

impl From<GmailRow> for GmailDto {
	fn from(r: GmailRow) -> Self {
		Self {
			id: r.id,
			gmail: r.gmail,
			enabled: r.enabled,
			created_at: r.created_at,
		}
	}
}

#[derive(FromRow)]
struct LocationRow {
	gmail_id: i64,
	#[sqlx(flatten)]
	target: TargetRow,
	track_enabled: bool,
	new_7d: i64,
	snapshots_7d: i64,
	snapshots_30d: i64,
	live: i64,
	responded: i64,
	listed: Option<i64>,
	gone: i64,
	reinstating: i64,
	last_run_at: Option<String>,
	last_run_status: Option<String>,
}

#[derive(FromRow)]
struct CardRow {
	#[sqlx(flatten)]
	review: ReviewRow,
	requested_at: Option<String>,
	reinstated_at: Option<String>,
}

#[derive(FromRow)]
struct TgChannelRow {
	id: i64,
	destination: String,
	managing_gmail_id: Option<i64>,
	events: String,
	created_at: String,
}

impl TryFrom<TgChannelRow> for TgChannelDto {
	type Error = eyre::Report;

	fn try_from(r: TgChannelRow) -> eyre::Result<Self> {
		Ok(Self {
			events: parse_events(r.id, &r.events)?,
			id: r.id,
			destination: r.destination,
			gmail_id: r.managing_gmail_id,
			created_at: r.created_at,
		})
	}
}

fn no_gmail(id: i64) -> Rejected {
	Rejected::not_found(format!("no gmail {id}"))
}

impl Store {
	/// Adds a managing gmail to a member.
	pub async fn add_gmail(&self, member: &str, gmail: &str, now: Timestamp) -> eyre::Result<GmailDto> {
		let row: Option<GmailRow> =
			sqlx::query_as("INSERT INTO managing_gmails (member_email, gmail, created_at) VALUES (?, ?, ?) ON CONFLICT DO NOTHING RETURNING id, gmail, enabled, created_at")
				.bind(member)
				.bind(gmail)
				.bind(fmt_ts(now))
				.fetch_optional(&self.pool)
				.await
				.wrap_err("adding a gmail")?;
		Ok(row.ok_or_else(|| Rejected::invalid(format!("{gmail} is already one of your gmails")))?.into())
	}

	/// Removes a gmail with its tracks and the Telegram channels scoped to it. One with
	/// appeals is kept: they are the history of what was asked of Google.
	pub async fn delete_gmail(&self, member: &str, id: i64) -> eyre::Result<()> {
		let mut tx = self.write().await?;
		let appeals: Option<i64> =
			sqlx::query_scalar("SELECT (SELECT COUNT(*) FROM reinstatements WHERE managing_gmail_id = g.id) FROM managing_gmails g WHERE g.id = ? AND g.member_email = ?")
				.bind(id)
				.bind(member)
				.fetch_optional(&mut *tx)
				.await
				.wrap_err("loading a gmail")?;
		match appeals.ok_or_else(|| no_gmail(id))? {
			0 => {}
			n => return Err(Rejected::invalid(format!("gmail {id} has {n} reinstatement requests on record; it is kept with them")).into()),
		}
		sqlx::query("DELETE FROM managing_gmails WHERE id = ?")
			.bind(id)
			.execute(&mut *tx)
			.await
			.wrap_err("deleting a gmail")?;
		tx.commit().await.wrap_err("committing a gmail's removal")
	}

	/// Tracks a target under a gmail; tracking it twice is tracking it. `member: None` is
	/// the operator, who may assign any gmail.
	pub async fn track(&self, member: Option<&str>, gmail: i64, target: TargetId, now: Timestamp) -> eyre::Result<()> {
		self.check_gmail(member, gmail).await?;
		self.target(target).await?;
		sqlx::query("INSERT INTO tracks (managing_gmail_id, target_id, created_at) VALUES (?, ?, ?) ON CONFLICT DO NOTHING")
			.bind(gmail)
			.bind(target.0)
			.bind(fmt_ts(now))
			.execute(&self.pool)
			.await
			.wrap_err("tracking a target")?;
		Ok(())
	}

	/// Stops tracking; the target and its archive stay.
	pub async fn untrack(&self, member: &str, gmail: i64, target: TargetId) -> eyre::Result<()> {
		let done = sqlx::query("DELETE FROM tracks WHERE managing_gmail_id = (SELECT id FROM managing_gmails WHERE id = ? AND member_email = ?) AND target_id = ?")
			.bind(gmail)
			.bind(member)
			.bind(target.0)
			.execute(&self.pool)
			.await
			.wrap_err("untracking a target")?;
		if done.rows_affected() != 1 {
			return Err(Rejected::not_found(format!("gmail {gmail} does not track target {target}")).into());
		}
		Ok(())
	}

	/// Switches a member's gmail on or off.
	pub async fn set_gmail_enabled(&self, member: &str, gmail: i64, on: bool) -> eyre::Result<()> {
		let done = sqlx::query("UPDATE managing_gmails SET enabled = ? WHERE id = ? AND member_email = ?")
			.bind(on)
			.bind(gmail)
			.bind(member)
			.execute(&self.pool)
			.await
			.wrap_err("switching a gmail")?;
		if done.rows_affected() != 1 {
			return Err(no_gmail(gmail).into());
		}
		Ok(())
	}

	/// Switches a member's track on or off.
	pub async fn set_track_enabled(&self, member: &str, gmail: i64, target: TargetId, on: bool) -> eyre::Result<()> {
		let done = sqlx::query("UPDATE tracks SET enabled = ? WHERE managing_gmail_id = (SELECT id FROM managing_gmails WHERE id = ? AND member_email = ?) AND target_id = ?")
			.bind(on)
			.bind(gmail)
			.bind(member)
			.bind(target.0)
			.execute(&self.pool)
			.await
			.wrap_err("switching a track")?;
		if done.rows_affected() != 1 {
			return Err(Rejected::not_found(format!("gmail {gmail} does not track target {target}")).into());
		}
		Ok(())
	}

	/// The member's gmails and each one's places, by screenshots over 7 days, most first.
	pub async fn overview(&self, member: &str, now: Timestamp) -> eyre::Result<Vec<GmailOverview>> {
		let days_ago = |d: i64| fmt_ts(now - SignedDuration::from_hours(24 * d));
		// one snapshot: every location row's gmail is among the gmails read
		let mut tx = self.pool.begin().await.wrap_err("starting a read")?;
		let gmails: Vec<GmailRow> = sqlx::query_as("SELECT id, gmail, enabled, created_at FROM managing_gmails WHERE member_email = ? ORDER BY gmail")
			.bind(member)
			.fetch_all(&mut *tx)
			.await
			.wrap_err("listing gmails")?;
		let rows: Vec<LocationRow> = sqlx::query_as(
			"SELECT k.managing_gmail_id AS gmail_id,
			        t.id, t.label, t.kind, t.place_id, t.gbp_account, t.gbp_location, t.lang, t.interval_secs, t.enabled, t.created_at,
			        k.enabled AS track_enabled,
			        (SELECT COUNT(*) FROM reviews r WHERE r.target_id = t.id AND r.published_est >= ?2) AS new_7d,
			        (SELECT COUNT(*) FROM captures c JOIN reviews r ON r.id = c.review_id WHERE r.target_id = t.id AND c.captured_at >= ?2) AS snapshots_7d,
			        (SELECT COUNT(*) FROM captures c JOIN reviews r ON r.id = c.review_id WHERE r.target_id = t.id AND c.captured_at >= ?3) AS snapshots_30d,
			        (SELECT COUNT(*) FROM reviews r WHERE r.target_id = t.id AND r.gone_at IS NULL) AS live,
			        (SELECT COUNT(*) FROM reviews r WHERE r.target_id = t.id AND r.gone_at IS NULL AND r.reply IS NOT NULL) AS responded,
			        t.listed,
			        (SELECT COUNT(*) FROM reviews r WHERE r.target_id = t.id AND r.gone_at IS NOT NULL) AS gone,
			        (SELECT COUNT(*) FROM reviews r JOIN reinstatements x ON x.review_id = r.id
			          WHERE r.target_id = t.id AND r.gone_at IS NOT NULL AND x.managing_gmail_id = k.managing_gmail_id
			            AND x.withdrawn_at IS NULL AND x.reinstated_at IS NULL) AS reinstating,
			        (SELECT finished_at FROM runs WHERE target_id = t.id AND finished_at IS NOT NULL ORDER BY id DESC LIMIT 1) AS last_run_at,
			        (SELECT status FROM runs WHERE target_id = t.id AND finished_at IS NOT NULL ORDER BY id DESC LIMIT 1) AS last_run_status
			 FROM tracks k
			 JOIN managing_gmails g ON g.id = k.managing_gmail_id
			 JOIN targets t ON t.id = k.target_id
			 WHERE g.member_email = ?1
			 ORDER BY snapshots_7d DESC, t.id",
		)
		.bind(member)
		.bind(days_ago(7))
		.bind(days_ago(30))
		.fetch_all(&mut *tx)
		.await
		.wrap_err("loading the overview")?;
		tx.commit().await.wrap_err("ending a read")?;
		let mut out: Vec<GmailOverview> = gmails.into_iter().map(|g| GmailOverview { gmail: g.into(), locations: vec![] }).collect();
		for r in rows {
			let status = r
				.last_run_status
				.as_deref()
				.map(str::parse)
				.transpose()
				.wrap_err_with(|| format!("target {}'s last run has an unknown status", r.target.id))?;
			let g = out.iter_mut().find(|g| g.gmail.id == r.gmail_id).expect("read in the same transaction");
			g.locations.push(LocationSummary {
				target: Target::try_from(r.target)?.into(),
				enabled: r.track_enabled,
				new_7d: r.new_7d,
				snapshots_7d: r.snapshots_7d,
				snapshots_30d: r.snapshots_30d,
				live: r.live,
				responded: r.responded,
				listed: r.listed,
				removed: r.gone - r.reinstating,
				reinstating: r.reinstating,
				last_run_at: r.last_run_at,
				last_run_status: status,
			});
		}
		Ok(out)
	}

	/// A tracked target's reviews in the board's three columns, as this gmail sees them.
	// ponytail: every live review in one answer; page `snapshotted` when a place's list outgrows a screen's worth of JSON
	pub async fn board(&self, member: &str, gmail: i64, target: TargetId) -> eyre::Result<Board> {
		self.check_track(member, gmail, target).await?;
		let select = REVIEW_SELECT.replacen("SELECT ", "SELECT x.requested_at, x.reinstated_at, ", 1);
		let rows: Vec<CardRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
			"{select}
			 LEFT JOIN reinstatements x ON x.id = (SELECT MAX(id) FROM reinstatements WHERE review_id = r.id AND managing_gmail_id = ?1 AND withdrawn_at IS NULL)
			 WHERE r.target_id = ?2"
		)))
		.bind(gmail)
		.bind(target.0)
		.fetch_all(&self.pool)
		.await
		.wrap_err("loading a board")?;
		let mut board = Board::default();
		for r in rows {
			let card = BoardCard {
				reinstatement: r.requested_at.map(|requested_at| ReinstatementDto {
					requested_at,
					reinstated_at: r.reinstated_at.clone(),
				}),
				review: ReviewDto::from(r.review),
			};
			let open = card.reinstatement.as_ref().is_some_and(|x| x.reinstated_at.is_none());
			match (card.review.gone_at.is_some(), open) {
				(false, _) => board.snapshotted.push(card),
				(true, false) => board.removed.push(card),
				(true, true) => board.reinstating.push(card),
			}
		}
		board.snapshotted.sort_by(|a, b| (&b.review.first_seen, b.review.id).cmp(&(&a.review.first_seen, a.review.id)));
		board.removed.sort_by(|a, b| (&b.review.gone_at, b.review.id).cmp(&(&a.review.gone_at, a.review.id)));
		let requested = |c: &BoardCard| c.reinstatement.as_ref().map(|x| x.requested_at.clone());
		board.reinstating.sort_by_key(|c| std::cmp::Reverse((requested(c), c.review.id)));
		Ok(board)
	}

	/// Opens an appeal of a gone review under a gmail tracking its place; one already open
	/// stays as it is.
	pub async fn reinstate(&self, member: &str, gmail: i64, review: ReviewId, now: Timestamp) -> eyre::Result<ReinstatementDto> {
		let (target, gone): (i64, bool) = sqlx::query_as("SELECT target_id, gone_at IS NOT NULL FROM reviews WHERE id = ?")
			.bind(review.0)
			.fetch_optional(&self.pool)
			.await
			.wrap_err("loading a review")?
			.ok_or_else(|| Rejected::not_found(format!("no review {review}")))?;
		self.check_track(member, gmail, TargetId(target)).await?;
		if !gone {
			return Err(Rejected::invalid(format!("review {review} is listed; only a removed review can be appealed")).into());
		}
		let mut tx = self.write().await?;
		sqlx::query(
			"INSERT INTO reinstatements (managing_gmail_id, review_id, requested_at)
			 SELECT ?1, ?2, ?3 WHERE NOT EXISTS (SELECT 1 FROM reinstatements WHERE managing_gmail_id = ?1 AND review_id = ?2 AND withdrawn_at IS NULL AND reinstated_at IS NULL)",
		)
		.bind(gmail)
		.bind(review.0)
		.bind(fmt_ts(now))
		.execute(&mut *tx)
		.await
		.wrap_err("opening an appeal")?;
		let (requested_at, reinstated_at): (String, Option<String>) =
			sqlx::query_as("SELECT requested_at, reinstated_at FROM reinstatements WHERE managing_gmail_id = ? AND review_id = ? AND withdrawn_at IS NULL AND reinstated_at IS NULL")
				.bind(gmail)
				.bind(review.0)
				.fetch_one(&mut *tx)
				.await
				.wrap_err("reading the open appeal")?;
		tx.commit().await.wrap_err("committing an appeal")?;
		Ok(ReinstatementDto { requested_at, reinstated_at })
	}

	/// Withdraws the open appeal of a review: kept, marked withdrawn.
	pub async fn withdraw_reinstatement(&self, member: &str, gmail: i64, review: ReviewId, now: Timestamp) -> eyre::Result<()> {
		self.check_gmail(Some(member), gmail).await?;
		let done = sqlx::query("UPDATE reinstatements SET withdrawn_at = ? WHERE managing_gmail_id = ? AND review_id = ? AND withdrawn_at IS NULL AND reinstated_at IS NULL")
			.bind(fmt_ts(now))
			.bind(gmail)
			.bind(review.0)
			.execute(&self.pool)
			.await
			.wrap_err("withdrawing an appeal")?;
		if done.rows_affected() == 0 {
			return Err(Rejected::not_found(format!("no open appeal of review {review} under gmail {gmail}")).into());
		}
		Ok(())
	}

	/// Whether a capture shows a review of a place the member tracks.
	pub async fn member_sees_capture(&self, member: &str, sha256: &str) -> eyre::Result<bool> {
		sqlx::query_scalar(
			"SELECT EXISTS (SELECT 1 FROM captures c JOIN reviews r ON r.id = c.review_id JOIN tracks k ON k.target_id = r.target_id
			                JOIN managing_gmails g ON g.id = k.managing_gmail_id
			                WHERE c.sha256 = ? AND g.member_email = ?)",
		)
		.bind(sha256)
		.bind(member)
		.fetch_one(&self.pool)
		.await
		.wrap_err("checking a capture's owner")
	}

	/// Adds a Telegram channel; `destination` already parsed by the caller.
	pub async fn add_tg_channel(&self, member: &str, ch: &NewTgChannel, now: Timestamp) -> eyre::Result<TgChannelDto> {
		if let Some(g) = ch.gmail_id {
			self.check_gmail(Some(member), g).await?;
		}
		let row: TgChannelRow = sqlx::query_as(
			"INSERT INTO tg_channels (member_email, managing_gmail_id, destination, events, created_at) VALUES (?, ?, ?, ?, ?)
			 RETURNING id, destination, managing_gmail_id, events, created_at",
		)
		.bind(member)
		.bind(ch.gmail_id)
		.bind(ch.destination.trim())
		.bind(serde_json::to_string(&ch.events)?)
		.bind(fmt_ts(now))
		.fetch_one(&self.pool)
		.await
		.wrap_err("adding a Telegram channel")?;
		row.try_into()
	}

	/// A member's Telegram channels.
	pub async fn tg_channels(&self, member: &str) -> eyre::Result<Vec<TgChannelDto>> {
		let rows: Vec<TgChannelRow> = sqlx::query_as("SELECT id, destination, managing_gmail_id, events, created_at FROM tg_channels WHERE member_email = ? ORDER BY id")
			.bind(member)
			.fetch_all(&self.pool)
			.await
			.wrap_err("listing Telegram channels")?;
		rows.into_iter().map(TgChannelDto::try_from).collect()
	}

	/// Removes a channel and what it was still owed. `false` when the member has none by that id.
	pub async fn delete_tg_channel(&self, member: &str, id: i64) -> eyre::Result<bool> {
		let done = sqlx::query("DELETE FROM tg_channels WHERE id = ? AND member_email = ?")
			.bind(id)
			.bind(member)
			.execute(&self.pool)
			.await
			.wrap_err("deleting a Telegram channel")?;
		Ok(done.rows_affected() == 1)
	}

	/// [`Rejected::NotFound`] unless the gmail is the member's (any gmail for the operator).
	pub(crate) async fn check_gmail(&self, member: Option<&str>, gmail: i64) -> eyre::Result<()> {
		let found: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM managing_gmails WHERE id = ?1 AND (?2 IS NULL OR member_email = ?2))")
			.bind(gmail)
			.bind(member)
			.fetch_one(&self.pool)
			.await
			.wrap_err("looking up a gmail")?;
		if !found {
			return Err(no_gmail(gmail).into());
		}
		Ok(())
	}

	/// [`Rejected::NotFound`] unless the member's gmail tracks the target.
	async fn check_track(&self, member: &str, gmail: i64, target: TargetId) -> eyre::Result<()> {
		let found: bool =
			sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM tracks k JOIN managing_gmails g ON g.id = k.managing_gmail_id WHERE g.id = ? AND g.member_email = ? AND k.target_id = ?)")
				.bind(gmail)
				.bind(member)
				.bind(target.0)
				.fetch_one(&self.pool)
				.await
				.wrap_err("looking up a track")?;
		if !found {
			return Err(Rejected::not_found(format!("gmail {gmail} does not track target {target}")).into());
		}
		Ok(())
	}
}
