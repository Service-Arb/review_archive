//! One scan of one target into the store, start to finish: the run row, the source, the
//! blobs, the reconciliation and what gets written. [`Recorder`] is the low-level entry
//! for a caller with its own [`ReviewSource`]; [`Archive`](crate::Archive) uses it too.

use std::collections::HashMap;

use jiff::Timestamp;
use review_archive_core::{
	Coverage, ReviewId, Scan, Target,
	dto::{Counts, RunStatus, RunSummary},
	reconcile,
};

use crate::{
	SCANNER_VERSION, png_meta,
	sources::ReviewSource,
	store::{Applied, RunId, Store, StoredCapture, blobs::BlobStore},
};

/// A recorded run: the summary, its row, and the reviews it listed.
#[derive(Clone, Debug)]
pub struct Recorded {
	/// As reported.
	pub summary: RunSummary,
	/// Its row in `runs`.
	pub run: RunId,
	/// Every review the scan listed, in its order; empty for a failed run.
	pub seen: Vec<ReviewId>,
}

/// Time is I/O: it comes through here so tests can pin it.
pub trait Clock: Send + Sync {
	/// The current time.
	fn now(&self) -> Timestamp;
}

/// The real clock.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
	fn now(&self) -> Timestamp {
		Timestamp::now()
	}
}

/// Records scans of a source into a store.
#[derive(Debug)]
pub struct Recorder<'a, C: Clock> {
	/// Where the rows go.
	pub store: &'a Store,
	/// Where the PNGs go.
	pub blobs: &'a BlobStore,
	/// What dates the run.
	pub clock: &'a C,
}

impl<C: Clock> Recorder<'_, C> {
	/// Scans one target and records the run, whatever its outcome. `Err` is only for the
	/// archive itself failing (the database); a failing source is a `failed` run.
	pub async fn run<S: ReviewSource>(&self, source: &S, target: &Target) -> eyre::Result<RunSummary> {
		Ok(self.record(source, target).await?.summary)
	}

	/// [`Self::run`], with the run's id and the reviews it listed.
	pub async fn record<S: ReviewSource>(&self, source: &S, target: &Target) -> eyre::Result<Recorded> {
		let run = self.store.start_run(target.id, self.clock.now()).await?;
		let outcome = self.scan_and_apply(source, target).await;
		let now = self.clock.now();
		let mut seen = Vec::new();
		let summary = match outcome {
			Ok((applied, captured, complete, warnings)) => {
				let counts = applied.counts;
				seen = applied.seen;
				let status = if warnings.is_empty() { RunStatus::Ok } else { RunStatus::Partial };
				let error = (!warnings.is_empty()).then(|| warnings.join("; "));
				RunSummary {
					target: target.id.0,
					label: target.label.clone(),
					status,
					counts,
					captured,
					complete,
					error,
				}
			}
			Err(e) => {
				tracing::warn!(target = %target.id, error = %format!("{e:#}"), "scan failed");
				RunSummary {
					target: target.id.0,
					label: target.label.clone(),
					status: RunStatus::Failed,
					counts: Counts::default(),
					captured: 0,
					complete: false,
					error: Some(format!("{e:#}")),
				}
			}
		};
		self.store.finish_run(run, now, summary.status, summary.error.as_deref(), summary.counts).await?;
		Ok(Recorded { summary, run, seen })
	}

	async fn scan_and_apply<S: ReviewSource>(&self, source: &S, target: &Target) -> eyre::Result<(Applied, u32, bool, Vec<String>)> {
		let known = self.store.known(target.id).await?;
		let mut scan: Scan = source.scan(target, &known).await?;
		let mut warnings = std::mem::take(&mut scan.warnings);
		let coverage = scan.coverage;
		let plan = reconcile::plan(&known, &scan);
		warnings.extend(plan.warnings.iter().cloned());

		let mut captures = HashMap::new();
		for obs in &scan.reviews {
			let Some(c) = &obs.capture else { continue };
			if !known.wants_capture(&obs.source_review_id) || captures.contains_key(&obs.source_review_id) {
				continue;
			}
			match self.store_capture(target, &obs.source_review_id, c).await {
				Ok(stored) => {
					captures.insert(obs.source_review_id.clone(), stored);
				}
				Err(e) => warnings.push(format!("capture of {} not stored: {e:#}", obs.source_review_id)),
			}
		}
		let captured = u32::try_from(captures.len()).unwrap_or(u32::MAX);

		let applied = self.store.apply(target.id, &plan, &captures, SCANNER_VERSION, self.clock.now()).await?;
		Ok((applied, captured, coverage == Coverage::Complete, warnings))
	}

	async fn store_capture(&self, target: &Target, source_review_id: &str, c: &review_archive_core::Capture) -> eyre::Result<StoredCapture> {
		let tagged = png_meta::provenance(c, &target.label, source_review_id)?;
		let (width, height) = png_meta::dimensions(&tagged)?;
		let sha256 = self.blobs.put(&tagged).await?;
		Ok(StoredCapture {
			sha256,
			width,
			height,
			captured_at: c.captured_at,
			page_url: c.page_url.clone(),
		})
	}
}
