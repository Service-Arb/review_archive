//! The `serve` loop: one target at a time, each when its interval says so.

use std::time::Duration;

use jiff::Timestamp;
use rand::RngExt;
use tokio::sync::watch;

use crate::{
	domain::{
		Target,
		schedule::{self, LastRun},
	},
	runner::Runner,
};

/// The longest sleep between looks at the target list, so a target added or re-enabled
/// from the CLI is picked up without a restart.
const MAX_IDLE: Duration = Duration::from_secs(60);

/// Runs until `shutdown` turns true. A scan in flight when it does is abandoned; its
/// run stays unfinished and does not count towards scheduling.
pub async fn run(runner: &Runner, mut shutdown: watch::Receiver<bool>) -> eyre::Result<()> {
	//LOOP: the service's main loop, ends on shutdown
	loop {
		if *shutdown.borrow() {
			return Ok(());
		}
		let wait = tokio::select! {
			r = pass(runner, shutdown.clone()) => match r {
				Ok(wait) => wait,
				Err(e) => {
					// the archive itself failed (database); try again later rather than spin
					tracing::error!(error = %format!("{e:#}"), "scheduler pass failed");
					MAX_IDLE
				}
			},
			_ = shutdown.changed() => return Ok(()),
		};
		runner.end_pass().await;
		tokio::select! {
			() = tokio::time::sleep(wait) => {}
			_ = shutdown.changed() => return Ok(()),
		}
	}
}

/// Scans everything due now, one after another with a pause between. Returns how long
/// to sleep until the next target is due.
async fn pass(runner: &Runner, mut shutdown: watch::Receiver<bool>) -> eyre::Result<Duration> {
	let due = due_now(runner, Timestamp::now()).await?;
	for (i, target) in due.iter().enumerate() {
		if i > 0 {
			let pause = schedule::pause(rand::rng().random::<f64>());
			tokio::select! {
				() = tokio::time::sleep(pause) => {}
				_ = shutdown.changed() => return Ok(Duration::ZERO),
			}
		}
		let summary = runner.scan(target).await?;
		tracing::info!("{summary}");
	}
	next_wait(runner, Timestamp::now()).await
}

async fn due_times(runner: &Runner) -> eyre::Result<Vec<(Target, Option<Timestamp>)>> {
	let mut out = Vec::new();
	for t in runner.store.targets().await? {
		if !t.enabled {
			continue;
		}
		let last: Option<LastRun> = runner.store.last_run(t.id).await?;
		let due = schedule::due_at(t.id, t.interval, last);
		out.push((t, due));
	}
	Ok(out)
}

async fn due_now(runner: &Runner, now: Timestamp) -> eyre::Result<Vec<Target>> {
	let mut due: Vec<(Target, Option<Timestamp>)> = due_times(runner).await?.into_iter().filter(|(_, d)| d.is_none_or(|d| d <= now)).collect();
	due.sort_by_key(|(t, d)| (*d, t.id));
	Ok(due.into_iter().map(|(t, _)| t).collect())
}

async fn next_wait(runner: &Runner, now: Timestamp) -> eyre::Result<Duration> {
	let next = due_times(runner).await?.into_iter().map(|(_, d)| d.unwrap_or(now)).min();
	Ok(match next {
		Some(next) if next > now => Duration::try_from(next - now).unwrap_or(MAX_IDLE).min(MAX_IDLE),
		Some(_) => Duration::ZERO,
		None => MAX_IDLE,
	})
}
