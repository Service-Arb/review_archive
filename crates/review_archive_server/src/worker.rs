//! The background loops of `serve`: the one browser's worker (queued jobs first, then
//! whatever the schedule says is due) and the webhook deliverer.

use std::time::Duration;

use jiff::Timestamp;
use rand::RngExt;
use review_archive::Archive;
use review_archive_core::{
	dto::{JobStatus, RunStatus},
	schedule,
};
use tokio::{
	sync::{Notify, watch},
	time::Instant,
};

/// The longest sleep between looks at the target list, so a target added or re-enabled
/// from the CLI is picked up without a restart.
const MAX_IDLE: Duration = Duration::from_secs(60);
/// How often the outbox is looked at.
const DELIVERY_TICK: Duration = Duration::from_secs(5);

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
/// goes first, then the most overdue target; between two, a polite pause. A scan in
/// flight at shutdown is abandoned; its run and job stay unfinished until the next start
/// fails the job.
pub async fn run(archive: &Archive, signals: &Signals, mut shutdown: watch::Receiver<bool>) -> eyre::Result<()> {
	let interrupted = archive.recover_jobs().await?;
	if interrupted > 0 {
		tracing::warn!(interrupted, "failed the jobs the previous process died running");
	}
	let mut last_scan: Option<Instant> = None;
	//LOOP: the service's main loop, ends on shutdown
	loop {
		if *shutdown.borrow() {
			return Ok(());
		}
		if let Some(at) = last_scan {
			let pause = schedule::pause(rand::rng().random::<f64>());
			tokio::select! {
				() = tokio::time::sleep_until(at + pause) => {}
				_ = shutdown.changed() => return Ok(()),
			}
		}
		let step = tokio::select! {
			r = step(archive, signals) => r,
			_ = shutdown.changed() => return Ok(()),
		};
		match step {
			Ok(Step::Scanned) => last_scan = Some(Instant::now()),
			Ok(Step::Idle(wait)) => {
				last_scan = None;
				archive.close().await;
				tokio::select! {
					() = tokio::time::sleep(wait) => {}
					() = signals.job_queued.notified() => {}
					_ = shutdown.changed() => return Ok(()),
				}
			}
			Err(e) => {
				// the archive itself failed (database); try again later rather than spin
				ev_lib::error_monitoring::report(&*e);
				tracing::warn!(error = %format!("{e:#}"), "worker step failed");
				archive.close().await;
				tokio::select! {
					() = tokio::time::sleep(MAX_IDLE) => {}
					_ = shutdown.changed() => return Ok(()),
				}
			}
		}
	}
}

enum Step {
	Scanned,
	/// Nothing to do for this long.
	Idle(Duration),
}

async fn step(archive: &Archive, signals: &Signals) -> eyre::Result<Step> {
	let job = archive.run_next_job().await;
	if !matches!(job, Ok(None)) {
		signals.job_finished.send_modify(|n| *n += 1);
	}
	if let Some(job) = job? {
		tracing::info!(job = job.id, kind = job.kind.as_str(), status = job.status.as_str(), "job finished");
		if job.status == JobStatus::Failed {
			report(&format!("job {} ({}) failed: {}", job.id, job.kind.as_str(), job.error.as_deref().unwrap_or("no reason given")));
		}
		return Ok(Step::Scanned);
	}
	let now = Timestamp::now();
	if let Some(target) = archive.due(now).await?.into_iter().next() {
		let summary = archive.scan(&target).await?;
		tracing::info!("{summary}");
		if summary.status == RunStatus::Failed {
			report(&format!(
				"scan of target {} ({}) failed: {}",
				summary.target,
				summary.label,
				summary.error.as_deref().unwrap_or("no reason given")
			));
		}
		return Ok(Step::Scanned);
	}
	let now = Timestamp::now();
	Ok(Step::Idle(match archive.next_due(now).await? {
		Some(next) if next > now => Duration::try_from(next - now).unwrap_or(MAX_IDLE).min(MAX_IDLE),
		Some(_) => Duration::ZERO,
		None => MAX_IDLE,
	}))
}

fn report(msg: &str) {
	let e = eyre::eyre!("{msg}");
	ev_lib::error_monitoring::report(&*e);
}

/// Delivers the webhook outbox every few seconds, until `shutdown`.
pub async fn deliver(archive: &Archive, mut shutdown: watch::Receiver<bool>) -> eyre::Result<()> {
	//LOOP: runs for the life of `serve`
	loop {
		match archive.deliver_webhooks().await {
			Ok(r) if r.delivered + r.retrying + r.gave_up > 0 => tracing::info!(delivered = r.delivered, retrying = r.retrying, gave_up = r.gave_up, "webhooks"),
			Ok(_) => {}
			Err(e) => {
				ev_lib::error_monitoring::report(&*e);
				tracing::warn!(error = %format!("{e:#}"), "webhook delivery pass failed");
			}
		}
		tokio::select! {
			() = tokio::time::sleep(DELIVERY_TICK) => {}
			_ = shutdown.changed() => return Ok(()),
		}
	}
}
