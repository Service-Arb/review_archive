//! Google's block pauses Maps for every target, survives a restart, and ends on one probe.

use std::path::Path;

use jiff::{SignedDuration, Timestamp};
use review_archive::{
	Archive, Rejected, SessionError,
	config::Config,
	core::{Coverage, Known, Scan, Target, TargetId, dto::NewTarget},
	sources::ReviewSource,
};

/// Google's "unusual traffic" page.
struct Blocked;

impl ReviewSource for Blocked {
	async fn scan(&self, _: &Target, _: &Known) -> eyre::Result<Scan> {
		Err(SessionError::new_blocked("https://www.google.com/sorry/index".to_owned()).into())
	}
}

/// A walk that got through.
struct Served;

impl ReviewSource for Served {
	async fn scan(&self, _: &Target, _: &Known) -> eyre::Result<Scan> {
		Ok(Scan {
			reviews: vec![],
			coverage: Coverage::Complete,
			warnings: vec![],
			cut_after: None,
			listed: None,
		})
	}
}

async fn open(dir: &Path) -> Archive {
	Archive::open(Config {
		data_dir: Some(dir.to_owned()),
		..Config::default()
	})
	.await
	.unwrap()
}

async fn due(archive: &Archive, at: Timestamp) -> Vec<TargetId> {
	let mut due: Vec<TargetId> = archive.due(at).await.unwrap().iter().map(|t| t.id).collect();
	due.sort();
	due
}

#[tokio::test]
async fn a_block_pauses_every_maps_target_until_one_probe() {
	let dir = tempfile::tempdir().unwrap();
	let archive = open(dir.path()).await;
	let mut targets = Vec::new();
	for i in 0..5 {
		let t = NewTarget {
			place: Some(format!("ChIJbreakertest{i:0>12}")),
			..Default::default()
		};
		targets.push(archive.add_target(&t).await.unwrap().target);
	}
	let gbp = NewTarget {
		place: Some("ChIJbreakertestgbp000000".into()),
		gbp: Some("1/2".into()),
		..Default::default()
	};
	let gbp = archive.add_target(&gbp).await.unwrap().target;
	let blocked = &targets[0];

	let failed = archive.record(&Blocked, blocked).await.unwrap();
	assert!(failed.failure.is_some());
	let now = Timestamp::now();
	assert_eq!(due(&archive, now).await, [gbp.id]);
	let refused = archive.enqueue_scan(targets[1].id).await.unwrap_err();
	assert!(matches!(refused.downcast_ref::<Rejected>(), Some(Rejected::Busy(_))), "{refused:#}");

	let archive = open(dir.path()).await;
	assert_eq!(due(&archive, now).await, [gbp.id]);

	// the pause is an hour; then one maps target probes, whichever waited longest
	let at_probe = due(&archive, now + SignedDuration::from_mins(61)).await;
	assert_eq!((at_probe.len(), at_probe.contains(&gbp.id)), (2, true), "{at_probe:?}");
	let probe = targets.iter().find(|t| at_probe.contains(&t.id)).unwrap();

	archive.record(&Served, probe).await.unwrap();
	let mut open_again: Vec<TargetId> = targets.iter().map(|t| t.id).filter(|id| ![blocked.id, probe.id].contains(id)).collect();
	open_again.push(gbp.id);
	assert_eq!(due(&archive, now).await, open_again);
}
