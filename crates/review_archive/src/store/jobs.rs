//! The job queue: on-demand work for the one browser, kept in the database so a restart
//! neither loses what was queued nor leaves a job "running" forever.

use eyre::WrapErr;
use jiff::Timestamp;
use review_archive_core::{
	Rejected, ReviewId, TargetId,
	dto::{CaptureLimits, JobDto, JobKind, JobStatus},
	fmt_ts,
};
use sqlx::{FromRow, SqliteConnection};

use super::{RunId, Store};

/// A job the caller now owns and must finish.
#[derive(Clone, Debug)]
pub struct ClaimedJob {
	/// Its id.
	pub id: i64,
	/// What it does.
	pub kind: JobKind,
	/// Its target.
	pub target: TargetId,
	/// What a capture reads.
	pub limits: CaptureLimits,
}

#[derive(FromRow)]
struct JobRow {
	id: i64,
	kind: String,
	target_id: i64,
	params: Option<String>,
	status: String,
	created_at: String,
	started_at: Option<String>,
	finished_at: Option<String>,
	run_id: Option<i64>,
	error: Option<String>,
	review_ids: Option<String>,
}

const JOB_COLUMNS: &str = "id, kind, target_id, params, status, created_at, started_at, finished_at, run_id, error, review_ids";

impl JobRow {
	fn kind(&self) -> eyre::Result<JobKind> {
		self.kind.parse().wrap_err_with(|| format!("job {} has an unknown kind", self.id))
	}

	fn limits(&self) -> eyre::Result<CaptureLimits> {
		Ok(self.params.as_deref().map(serde_json::from_str).transpose().wrap_err("reading job params")?.unwrap_or_default())
	}
}

impl Store {
	/// Queues a job; the worker takes queued jobs oldest first, ahead of scheduled scans. The
	/// same job already queued is not queued twice: its id comes back. With `max_queued`
	/// jobs waiting, [`Rejected::Busy`].
	pub async fn enqueue_job(&self, kind: JobKind, target: TargetId, limits: Option<&CaptureLimits>, max_queued: usize, now: Timestamp) -> eyre::Result<i64> {
		let params = limits.map(serde_json::to_string).transpose()?;
		let mut tx = self.write().await?;
		let same: Option<i64> = sqlx::query_scalar("SELECT id FROM jobs WHERE status = 'queued' AND kind = ? AND target_id = ? AND params IS ? ORDER BY id LIMIT 1")
			.bind(kind.as_ref())
			.bind(target.0)
			.bind(&params)
			.fetch_optional(&mut *tx)
			.await
			.wrap_err("looking for the same job")?;
		if let Some(id) = same {
			return Ok(id);
		}
		let queued: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM jobs WHERE status = 'queued'")
			.fetch_one(&mut *tx)
			.await
			.wrap_err("counting queued jobs")?;
		if usize::try_from(queued).is_ok_and(|q| q >= max_queued) {
			return Err(Rejected::Busy(format!("{queued} jobs are already waiting for the browser; try again later")).into());
		}
		let id = sqlx::query_scalar("INSERT INTO jobs (kind, target_id, params, status, created_at) VALUES (?, ?, ?, 'queued', ?) RETURNING id")
			.bind(kind.as_ref())
			.bind(target.0)
			.bind(params)
			.bind(fmt_ts(now))
			.fetch_one(&mut *tx)
			.await
			.wrap_err("queueing a job")?;
		tx.commit().await.wrap_err("committing a queued job")?;
		Ok(id)
	}

	/// Takes the oldest queued job and marks it running.
	pub async fn claim_job(&self, now: Timestamp) -> eyre::Result<Option<ClaimedJob>> {
		let row: Option<JobRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
			"UPDATE jobs SET status = 'running', started_at = ?
			 WHERE id = (SELECT id FROM jobs WHERE status = 'queued' ORDER BY id LIMIT 1)
			 RETURNING {JOB_COLUMNS}"
		)))
		.bind(fmt_ts(now))
		.fetch_optional(&self.pool)
		.await
		.wrap_err("claiming a job")?;
		let Some(r) = row else { return Ok(None) };
		Ok(Some(ClaimedJob {
			id: r.id,
			kind: r.kind()?,
			target: TargetId(r.target_id),
			limits: r.limits()?,
		}))
	}

	/// Records how a job ended: its run, and the reviews that run listed.
	pub async fn finish_job(&self, id: i64, status: JobStatus, run: Option<RunId>, error: Option<&str>, reviews: &[ReviewId], now: Timestamp) -> eyre::Result<()> {
		finish(&mut *self.pool.acquire().await?, id, status, run, error, reviews, now).await
	}

	/// A job, with its run and (once done) the reviews it saw — for a capture of named
	/// reviews, those of them it saw.
	pub async fn job(&self, id: i64) -> eyre::Result<Option<JobDto>> {
		let row: Option<JobRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!("SELECT {JOB_COLUMNS} FROM jobs WHERE id = ?")))
			.bind(id)
			.fetch_optional(&self.pool)
			.await
			.wrap_err_with(|| format!("loading job {id}"))?;
		let Some(r) = row else { return Ok(None) };
		let status: JobStatus = r.status.parse().wrap_err_with(|| format!("job {id} has an unknown status"))?;
		let run = match r.run_id {
			Some(run) => self.run(RunId(run)).await?,
			None => None,
		};
		let reviews = match (status, r.review_ids.as_deref()) {
			(JobStatus::Done, Some(ids)) => {
				let ids: Vec<ReviewId> = serde_json::from_str::<Vec<i64>>(ids).wrap_err("reading a job's reviews")?.into_iter().map(ReviewId).collect();
				let mut reviews = self.reviews_by_id(&ids).await?;
				if let Some(wanted) = r.limits()?.review_ids {
					reviews.retain(|r| wanted.contains(&r.source_review_id));
				}
				Some(reviews)
			}
			_ => None,
		};
		Ok(Some(JobDto {
			kind: r.kind()?,
			id: r.id,
			status,
			target_id: r.target_id,
			created_at: r.created_at,
			started_at: r.started_at,
			finished_at: r.finished_at,
			error: r.error,
			run,
			reviews,
		}))
	}
}

pub(super) async fn finish(db: &mut SqliteConnection, id: i64, status: JobStatus, run: Option<RunId>, error: Option<&str>, reviews: &[ReviewId], now: Timestamp) -> eyre::Result<()> {
	let ids: Vec<i64> = reviews.iter().map(|r| r.0).collect();
	sqlx::query("UPDATE jobs SET status = ?, finished_at = ?, run_id = ?, error = ?, review_ids = ? WHERE id = ?")
		.bind(status.as_ref())
		.bind(fmt_ts(now))
		.bind(run.map(|r| r.0))
		.bind(error)
		.bind(serde_json::to_string(&ids)?)
		.bind(id)
		.execute(db)
		.await
		.wrap_err_with(|| format!("finishing job {id}"))?;
	Ok(())
}
