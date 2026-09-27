//! The worker loop on a temp archive, without a browser: `gbp` targets with no credentials
//! fail their scans before any page is opened, which is enough to see what runs first.

use std::{path::PathBuf, time::Duration};

use jiff::Timestamp;
use review_archive::{
	Archive,
	config::Config,
	core::{
		TargetId,
		dto::{JobKind, JobStatus, NewTarget, TargetPatch},
	},
};
use review_archive_server::worker::{Signals, run};
use tokio::sync::watch;

const PLACE: &str = "ChIJLU7jZClu5kcR4PcOOO6p3I0";

/// A fresh directory under the system temp dir, removed on drop.
struct TempDir(PathBuf);

impl TempDir {
	fn new(name: &str) -> Self {
		let dir = std::env::temp_dir().join(format!("review_archive-{name}-{}-{}", std::process::id(), Timestamp::now().as_nanosecond()));
		std::fs::create_dir_all(&dir).unwrap();
		Self(dir)
	}
}

impl Drop for TempDir {
	fn drop(&mut self) {
		let _ = std::fs::remove_dir_all(&self.0);
	}
}

async fn gbp_target(archive: &Archive, disabled: bool) -> TargetId {
	let req = NewTarget {
		place: Some(PLACE.into()),
		gbp: Some("1/2".into()),
		..Default::default()
	};
	let id = archive.add_target(&req).await.unwrap().target.id;
	if disabled {
		let off = TargetPatch {
			enabled: Some(false),
			..Default::default()
		};
		archive.update_target(id, &off).await.unwrap();
	}
	id
}

/// On start the worker fails the job a dead process left `running`, then takes the queued
/// job before any target the schedule says is due.
#[tokio::test]
async fn a_restart_fails_the_interrupted_job_and_queued_jobs_go_before_due_targets() {
	let dir = TempDir::new("worker");
	let archive = Archive::open(Config {
		data_dir: Some(dir.0.clone()),
		..Config::default()
	})
	.await
	.unwrap();
	let store = archive.store().unwrap();
	// never scanned and enabled: due now
	let due = gbp_target(&archive, false).await;
	// only reachable through the queue
	let queued_for = gbp_target(&archive, true).await;

	// a previous process took this job and died
	let interrupted = store.enqueue_job(JobKind::Scan, queued_for, None, 20, Timestamp::now()).await.unwrap();
	assert_eq!(store.claim_job(Timestamp::now()).await.unwrap().unwrap().id, interrupted);
	let queued = store.enqueue_job(JobKind::Scan, queued_for, None, 20, Timestamp::now()).await.unwrap();

	let signals = Signals::default();
	let (stop, stopped) = watch::channel(false);
	let mut finished = signals.job_finished.subscribe();
	let worker = run(&archive, &signals, stopped);
	let observe = async {
		tokio::time::timeout(Duration::from_secs(20), finished.changed()).await.expect("a job finished").unwrap();
		let job = store.job(queued).await.unwrap().unwrap();
		let due_runs = store.runs(due, 10).await.unwrap();
		stop.send(true).unwrap();
		(job, due_runs)
	};
	let (worked, (job, due_runs)) = tokio::join!(worker, observe);
	worked.unwrap();

	let interrupted = store.job(interrupted).await.unwrap().unwrap();
	assert_eq!(interrupted.status, JobStatus::Failed);
	assert!(interrupted.error.unwrap().contains("interrupted"));

	// the queued job ran (and failed: no GBP credentials here), with a run of its own
	assert_eq!(job.status, JobStatus::Failed, "{job:?}");
	assert!(job.run.is_some(), "{job:?}");
	assert!(due_runs.is_empty(), "the due target was scanned before the queued job: {due_runs:?}");
}
