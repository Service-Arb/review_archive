//! The job queue: on-demand work for the one browser, kept in the database so a restart
//! neither loses what was queued nor leaves a job "running" forever.

use eyre::WrapErr;
use jiff::Timestamp;
use review_archive_core::{
	ReviewId, TargetId,
	dto::{JobDto, JobKind, JobStatus},
	fmt_ts,
};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;

use super::{RunId, Store};

/// What an ad-hoc capture is limited to.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct JobParams {
	/// Cards read at most.
	pub max_reviews: Option<usize>,
	/// Only these review ids.
	pub review_ids: Option<Vec<String>>,
}

/// A job the caller now owns and must finish.
#[derive(Clone, Debug)]
pub struct ClaimedJob {
	/// Its id.
	pub id: i64,
	/// What it does.
	pub kind: JobKind,
	/// Its target.
	pub target: TargetId,
	/// Capture limits.
	pub params: JobParams,
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

fn kind_of(s: &str) -> eyre::Result<JobKind> {
	match s {
		"scan" => Ok(JobKind::Scan),
		"capture" => Ok(JobKind::Capture),
		other => eyre::bail!("unknown job kind {other:?}"),
	}
}

fn status_of(s: &str) -> eyre::Result<JobStatus> {
	match s {
		"queued" => Ok(JobStatus::Queued),
		"running" => Ok(JobStatus::Running),
		"done" => Ok(JobStatus::Done),
		"failed" => Ok(JobStatus::Failed),
		other => eyre::bail!("unknown job status {other:?}"),
	}
}

impl Store {
	/// Queues a job; the worker takes queued jobs oldest first, ahead of scheduled scans.
	pub async fn enqueue_job(&self, kind: JobKind, target: TargetId, params: Option<&JobParams>, now: Timestamp) -> eyre::Result<i64> {
		let params = params.map(serde_json::to_string).transpose()?;
		sqlx::query_scalar("INSERT INTO jobs (kind, target_id, params, status, created_at) VALUES (?, ?, ?, 'queued', ?) RETURNING id")
			.bind(kind.as_ref())
			.bind(target.0)
			.bind(params)
			.bind(fmt_ts(now))
			.fetch_one(&self.pool)
			.await
			.wrap_err("queueing a job")
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
			kind: kind_of(&r.kind)?,
			target: TargetId(r.target_id),
			params: r.params.as_deref().map(serde_json::from_str).transpose().wrap_err("reading job params")?.unwrap_or_default(),
		}))
	}

	/// Records how a job ended: its run, and the reviews that run listed.
	pub async fn finish_job(&self, id: i64, status: JobStatus, run: Option<RunId>, error: Option<&str>, reviews: &[ReviewId], now: Timestamp) -> eyre::Result<()> {
		let ids: Vec<i64> = reviews.iter().map(|r| r.0).collect();
		sqlx::query("UPDATE jobs SET status = ?, finished_at = ?, run_id = ?, error = ?, review_ids = ? WHERE id = ?")
			.bind(status.as_ref())
			.bind(fmt_ts(now))
			.bind(run.map(|r| r.0))
			.bind(error)
			.bind(serde_json::to_string(&ids)?)
			.bind(id)
			.execute(&self.pool)
			.await
			.wrap_err_with(|| format!("finishing job {id}"))?;
		Ok(())
	}

	/// Fails the jobs a previous process was running when it died; queued ones stay queued.
	pub async fn fail_interrupted_jobs(&self, now: Timestamp) -> eyre::Result<u64> {
		let done = sqlx::query("UPDATE jobs SET status = 'failed', finished_at = ?, error = 'interrupted: the service stopped while it ran' WHERE status = 'running'")
			.bind(fmt_ts(now))
			.execute(&self.pool)
			.await
			.wrap_err("failing interrupted jobs")?;
		Ok(done.rows_affected())
	}

	/// A job, with its run and (once done) the reviews it saw.
	pub async fn job(&self, id: i64) -> eyre::Result<Option<JobDto>> {
		let row: Option<JobRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!("SELECT {JOB_COLUMNS} FROM jobs WHERE id = ?")))
			.bind(id)
			.fetch_optional(&self.pool)
			.await
			.wrap_err_with(|| format!("loading job {id}"))?;
		let Some(r) = row else { return Ok(None) };
		let status = status_of(&r.status)?;
		let run = match r.run_id {
			Some(run) => self.run(RunId(run)).await?,
			None => None,
		};
		let reviews = match (status, r.review_ids.as_deref()) {
			(JobStatus::Done, Some(ids)) => {
				let ids: Vec<i64> = serde_json::from_str(ids).wrap_err("reading a job's reviews")?;
				Some(self.reviews_by_id(&ids.into_iter().map(ReviewId).collect::<Vec<_>>()).await?)
			}
			_ => None,
		};
		Ok(Some(JobDto {
			id: r.id,
			kind: kind_of(&r.kind)?,
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
