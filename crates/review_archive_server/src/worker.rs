//! The background loops of `serve`: the one browser's worker (queued jobs first, then
//! whatever the schedule says is due) and the webhook deliverer.

use std::time::Duration;

use jiff::Timestamp;
use rand::RngExt;
use review_archive::{Archive, Remedy, describe, remedy};
use review_archive_core::{dto::JobStatus, schedule::Schedule};
use serde::{Deserialize, Serialize};
use smart_default::SmartDefault;
use tokio::{
	sync::{Notify, watch},
	time::Instant,
};
use v_utils::{Timeframe, macros::SettingsNested};

use crate::report;

/// How the worker and the deliverer take turns.
#[derive(Clone, Debug, Deserialize, Serialize, SettingsNested, SmartDefault, schemars::JsonSchema)]
#[settings(prefix = "worker")]
#[serde(default, deny_unknown_fields)]
pub struct WorkerConfig {
	/// The longest sleep between looks at the target list, so a target added or re-enabled
	/// from the CLI is picked up without a restart.
	#[default(Timeframe::from("1m"))]
	pub max_idle: Timeframe,
	/// How often the outbox is looked at.
	#[default(Timeframe::from("5s"))]
	pub delivery_tick: Timeframe,
	/// Queued jobs run back to back at most before an overdue target gets its scan: a busy
	/// queue delays the schedule, it does not stop it.
	#[default(3)]
	pub jobs_in_a_row: u32,
}

/// How the HTTP side and the worker nudge each other.
#[derive(Debug)]
pub struct Signals {
	/// A job was queued: the worker wakes.
	pub job_queued: Notify,
	/// Bumped each time a job ends: `?wait=` requests re-check.
	pub job_finished: watch::Sender<u64>,
}

impl Default for Signals {
	fn default() -> Self {
		Self {
			job_queued: Notify::new(),
			job_finished: watch::Sender::new(0),
		}
	}
}

/// The browser's worker, until `shutdown` turns true. One scan at a time: a queued job
/// goes first (but not more than a few in a row while a target is overdue), then the most
/// overdue target; between two, a polite pause. A scan in flight at shutdown is
/// abandoned; the next start fails its run and its job.
pub async fn run(archive: &Archive, signals: &Signals, cfg: &WorkerConfig, schedule: &Schedule, mut shutdown: watch::Receiver<bool>) -> eyre::Result<()> {
	let interrupted = archive.recover().await?;
	if interrupted > 0 {
		tracing::warn!(interrupted, "failed the jobs the previous process died running");
	}
	let mut last_scan: Option<Instant> = None;
	let mut jobs_in_a_row = 0;
	//LOOP: the service's main loop, ends on shutdown
	loop {
		if *shutdown.borrow() {
			return Ok(());
		}
		if let Some(at) = last_scan {
			let pause = schedule.pause(rand::rng().random::<f64>());
			tokio::select! {
				() = tokio::time::sleep_until(at + pause) => {}
				_ = shutdown.changed() => return Ok(()),
			}
		}
		let step = tokio::select! {
			r = step(archive, signals, cfg, &mut jobs_in_a_row) => r,
			_ = shutdown.changed() => return Ok(()),
		};
		let wait = match step {
			Ok(Step::Fatal(e)) => return Err(e),
			Ok(Step::Scanned) => {
				last_scan = Some(Instant::now());
				continue;
			}
			Ok(Step::Idle(wait)) => wait,
			Err(e) => {
				// the archive itself failed (database); try again later rather than spin
				report(&e, "worker step failed");
				cfg.max_idle.duration()
			}
		};
		last_scan = None;
		archive.close().await;
		tokio::select! {
			() = tokio::time::sleep(wait) => {}
			() = signals.job_queued.notified() => {}
			_ = shutdown.changed() => return Ok(()),
		}
	}
}

enum Step {
	Scanned,
	/// The worker cannot go on; the process exits with this.
	Fatal(eyre::Report),
	/// Nothing to do for this long.
	Idle(Duration),
}

async fn step(archive: &Archive, signals: &Signals, cfg: &WorkerConfig, jobs_in_a_row: &mut u32) -> eyre::Result<Step> {
	if *jobs_in_a_row < cfg.jobs_in_a_row && run_job(archive, signals).await? {
		*jobs_in_a_row += 1;
		return Ok(Step::Scanned);
	}
	*jobs_in_a_row = 0;
	if let Some(target) = archive.due(Timestamp::now()).await?.into_iter().next() {
		let recorded = archive.scan(&target).await?;
		tracing::info!("{}", recorded.summary);
		if let Some(e) = recorded.failure {
			let e = e.wrap_err(format!("scan of target {} ({})", target.id, target.label));
			if remedy(&e) == Remedy::Fatal {
				tracing::error!(error = describe(&e), "the worker cannot go on");
				return Ok(Step::Fatal(e));
			}
			report(&e, "scan failed");
		}
		return Ok(Step::Scanned);
	}
	if run_job(archive, signals).await? {
		*jobs_in_a_row = 1;
		return Ok(Step::Scanned);
	}
	let now = Timestamp::now();
	let max_idle = cfg.max_idle.duration();
	Ok(Step::Idle(match archive.next_due(now).await? {
		Some(next) if next > now => Duration::try_from(next.duration_since(now)).expect("positive: next > now").min(max_idle),
		Some(_) => Duration::ZERO,
		None => max_idle,
	}))
}

/// Runs the oldest queued job, if any; whether there was one.
async fn run_job(archive: &Archive, signals: &Signals) -> eyre::Result<bool> {
	let job = archive.run_next_job().await;
	if !matches!(job, Ok(None)) {
		signals.job_finished.send_modify(|n| *n += 1);
	}
	let Some(job) = job? else { return Ok(false) };
	tracing::info!(job = job.id, kind = %job.kind, status = %job.status, "job finished");
	if job.status == JobStatus::Failed {
		let why = job.error.as_deref().unwrap_or("no reason given");
		report(&eyre::eyre!("job {} ({}) failed: {why}", job.id, job.kind), "job failed");
	}
	Ok(true)
}

/// Delivers the webhook outbox every few seconds, until `shutdown`.
pub async fn deliver(archive: &Archive, cfg: &WorkerConfig, mut shutdown: watch::Receiver<bool>) -> eyre::Result<()> {
	//LOOP: runs for the life of `serve`
	loop {
		match archive.deliver_webhooks().await {
			Ok(r) if r.delivered + r.retrying + r.gave_up > 0 => tracing::info!(delivered = r.delivered, retrying = r.retrying, gave_up = r.gave_up, "webhooks"),
			Ok(_) => {}
			Err(e) => report(&e, "webhook delivery pass failed"),
		}
		tokio::select! {
			() = tokio::time::sleep(cfg.delivery_tick.duration()) => {}
			_ = shutdown.changed() => return Ok(()),
		}
	}
}
