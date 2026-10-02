//! One scan of one target into the store, start to finish: the run row, the source, the
//! blobs, the reconciliation and what gets written. [`Recorder`] is the low-level entry
//! for a caller with its own [`ReviewSource`]; [`Archive`](crate::Archive) uses it too.

use std::collections::HashMap;

use jiff::Timestamp;
use review_archive_core::{
	Capture, Coverage, Known, ReviewId, Scan, Target, TargetKind,
	dto::{Counts, RunStatus, RunSummary},
	reconcile,
	schedule::Schedule,
	tokens::{Meter, Tokens},
};

use crate::{
	SCANNER_VERSION, png_meta,
	sources::ReviewSource,
	store::{RunEnd, RunId, ScanWrite, Store, StoredCapture, blobs::BlobStore, sat_u32},
};

/// A recorded run: the summary, its row, the reviews it listed, and why it failed.
#[derive(Debug)]
pub struct Recorded {
	/// As reported.
	pub summary: RunSummary,
	/// Its row in `runs`.
	pub run: RunId,
	/// Every review the scan listed, in its order; empty for a failed run.
	pub seen: Vec<ReviewId>,
	/// The source's error, for a failed run; the summary has it as text.
	pub failure: Option<eyre::Report>,
}

/// Records scans of a source into a store.
#[derive(Debug)]
pub struct Recorder<'a> {
	/// Where the rows go.
	pub store: &'a Store,
	/// Where the PNGs go.
	pub blobs: &'a BlobStore,
	/// What dates the run: [`Timestamp::now`], or a pinned clock in tests (time is I/O).
	pub now: fn() -> Timestamp,
	/// How long a tripped Maps breaker waits.
	pub schedule: &'a Schedule,
	/// What members' balances and the account's hour allow.
	pub tokens: &'a Tokens,
}

impl Recorder<'_> {
	/// Scans one target and records the run, whatever its outcome. `Err` is only for the
	/// archive itself failing (the database); a failing source is a `failed` run.
	pub async fn run<S: ReviewSource>(&self, source: &S, target: &Target) -> eyre::Result<RunSummary> {
		Ok(self.record(source, target, None).await?.summary)
	}

	/// [`Self::run`], with the run's id and the reviews it listed. With `job`, that job ends
	/// with the run, in the same transaction. The members tracking the target pay for its
	/// walk, in that transaction too — but not for a job or an ad-hoc look, which are the operator's.
	pub async fn record<S: ReviewSource>(&self, source: &S, target: &Target, job: Option<i64>) -> eyre::Result<Recorded> {
		let known = self.store.known(target.id).await?;
		let bill = self.store.bill(target.id, job.is_some() || source.ad_hoc(), (self.now)(), self.tokens).await?;
		let mut meter = Meter::new(bill.allowance);
		let run = self.store.start_run(target.id, source.ad_hoc(), (self.now)()).await?;
		let mut summary = RunSummary {
			target: target.id.0,
			label: target.label.clone(),
			status: RunStatus::Failed,
			counts: Counts::default(),
			captured: 0,
			complete: false,
			error: None,
		};
		let mut failure = None;
		let seen = match source.scan(target, &known, &mut meter).await {
			Err(e) => {
				let error = crate::describe(&e);
				let end = RunEnd {
					run,
					status: RunStatus::Failed,
					error: Some(&error),
					job,
					tokens: meter.spent(),
					payers: &bill.payers,
				};
				self.store.fail_run(end, (self.now)()).await?;
				#[cfg(feature = "maps")]
				if crate::remedy(&e) == crate::Remedy::Pause {
					let reason = e
						.downcast_ref::<crate::SessionError>()
						.and_then(miette::Diagnostic::code)
						.expect("a pause is a SessionError, which has codes");
					let b = self.store.trip_breaker(&reason.to_string(), (self.now)(), self.schedule).await?;
					tracing::error!(error, trips = b.trips, probe_after = %b.probe_after, "Google flagged us: every Maps walk pauses until the probe");
				}
				summary.error = Some(error);
				failure = Some(e);
				Vec::new()
			}
			Ok(mut scan) => {
				let mut warnings = std::mem::take(&mut scan.warnings);
				let captures = self.store_captures(target, &known, &scan, &mut warnings).await;
				let plan = reconcile::plan(&known, &scan);
				warnings.extend(plan.warnings.iter().cloned());
				summary.status = if warnings.is_empty() { RunStatus::Ok } else { RunStatus::Partial };
				summary.error = (!warnings.is_empty()).then(|| warnings.join("; "));
				summary.captured = sat_u32(captures.len());
				summary.complete = scan.coverage == Coverage::Complete;
				let write = ScanWrite {
					plan: &plan,
					captures: &captures,
					cut_after: scan.cut_after.as_deref(),
					listed: scan.listed,
					post: scan.post.as_ref(),
					ad_hoc: source.ad_hoc(),
					scanner_version: SCANNER_VERSION,
				};
				let end = RunEnd {
					run,
					status: summary.status,
					error: summary.error.as_deref(),
					job,
					tokens: meter.spent(),
					payers: &bill.payers,
				};
				let applied = match self.store.apply(target.id, write, end, (self.now)()).await {
					Ok(applied) => applied,
					Err(e) => {
						// An open run is invisible to the schedule: the target would stay the most
						// overdue and be scanned again at once, forever. Failed, it backs off.
						let failed = RunEnd {
							status: RunStatus::Failed,
							error: Some("internal error: the scan could not be stored"),
							..end
						};
						if let Err(also) = self.store.fail_run(failed, (self.now)()).await {
							tracing::warn!(run = run.0, error = %format!("{also:#}"), "could not fail the run either");
						}
						return Err(e);
					}
				};
				summary.counts = applied.counts;
				if target.kind == TargetKind::Maps && self.store.reset_breaker().await? {
					tracing::info!(target = %target.id, "maps unpaused: the probe got through");
				}
				applied.seen
			}
		};
		Ok(Recorded { summary, run, seen, failure })
	}

	/// Writes the screenshots the archive wants to the blob store; one that fails is a
	/// warning and stays pending.
	async fn store_captures(&self, target: &Target, known: &Known, scan: &Scan, warnings: &mut Vec<String>) -> HashMap<String, StoredCapture> {
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
		captures
	}

	async fn store_capture(&self, target: &Target, source_review_id: &str, c: &Capture) -> eyre::Result<StoredCapture> {
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
