//! The `serve` loop: one target at a time, each when its interval says so. The schedule
//! itself is the library's (`Archive::due`, `Archive::next_due`); this only waits.

use std::time::Duration;

use jiff::Timestamp;
use rand::RngExt;
use review_archive::Archive;
use review_archive_core::{dto::RunStatus, schedule};
use tokio::sync::watch;

/// The longest sleep between looks at the target list, so a target added or re-enabled
/// from the CLI is picked up without a restart.
const MAX_IDLE: Duration = Duration::from_secs(60);

/// Runs until `shutdown` turns true. A scan in flight when it does is abandoned; its
/// run stays unfinished and does not count towards scheduling.
pub async fn run(archive: &Archive, mut shutdown: watch::Receiver<bool>) -> eyre::Result<()> {
	//LOOP: the service's main loop, ends on shutdown
	loop {
		if *shutdown.borrow() {
			return Ok(());
		}
		let wait = tokio::select! {
			r = pass(archive, shutdown.clone()) => match r {
				Ok(wait) => wait,
				Err(e) => {
					// the archive itself failed (database); try again later rather than spin
					ev_lib::error_monitoring::report(&*e);
					tracing::warn!(error = %format!("{e:#}"), "scheduler pass failed");
					MAX_IDLE
				}
			},
			_ = shutdown.changed() => return Ok(()),
		};
		archive.close().await;
		tokio::select! {
			() = tokio::time::sleep(wait) => {}
			_ = shutdown.changed() => return Ok(()),
		}
	}
}

/// Scans everything due now, one after another with a pause between. Returns how long
/// to sleep until the next target is due.
async fn pass(archive: &Archive, mut shutdown: watch::Receiver<bool>) -> eyre::Result<Duration> {
	let due = archive.due(Timestamp::now()).await?;
	for (i, target) in due.iter().enumerate() {
		if i > 0 {
			let pause = schedule::pause(rand::rng().random::<f64>());
			tokio::select! {
				() = tokio::time::sleep(pause) => {}
				_ = shutdown.changed() => return Ok(Duration::ZERO),
			}
		}
		let summary = archive.scan(target).await?;
		tracing::info!("{summary}");
		if summary.status == RunStatus::Failed {
			let e = eyre::eyre!(
				"scan of target {} ({}) failed: {}",
				summary.target,
				summary.label,
				summary.error.as_deref().unwrap_or("no reason given")
			);
			ev_lib::error_monitoring::report(&*e);
		}
	}
	let now = Timestamp::now();
	Ok(match archive.next_due(now).await? {
		Some(next) if next > now => Duration::try_from(next - now).unwrap_or(MAX_IDLE).min(MAX_IDLE),
		Some(_) => Duration::ZERO,
		None => MAX_IDLE,
	})
}
