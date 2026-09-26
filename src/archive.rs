//! One scan of one target, start to finish: the run row, the source, the blobs,
//! the reconciliation and what gets written.

use std::collections::HashMap;

use jiff::Timestamp;
use serde::Serialize;

use crate::{
	domain::{Coverage, ReviewSource, Scan, Target, reconcile},
	png_meta,
	store::{Counts, RunStatus, Store, StoredCapture, blobs::BlobStore},
};

pub const SCANNER_VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), "+", env!("GIT_HASH"));

/// Time is I/O: it comes through here so tests can pin it.
pub trait Clock: Send + Sync {
	fn now(&self) -> Timestamp;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
	fn now(&self) -> Timestamp {
		Timestamp::now()
	}
}

#[derive(Clone, Debug, Serialize)]
pub struct RunSummary {
	pub target: i64,
	pub label: String,
	pub status: RunStatus,
	#[serde(flatten)]
	pub counts: Counts,
	pub captured: u32,
	pub complete: bool,
	pub error: Option<String>,
}

impl std::fmt::Display for RunSummary {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		let status = match self.status {
			RunStatus::Ok => "ok",
			RunStatus::Partial => "partial",
			RunStatus::Failed => "FAILED",
		};
		write!(
			f,
			"#{} {:<24} {status:<8} seen {:>4}  new {:>4}  changed {:>3}  gone {:>3}  captured {:>4}{}",
			self.target,
			self.label,
			self.counts.seen,
			self.counts.new,
			self.counts.changed,
			self.counts.gone,
			self.captured,
			if self.complete { "  (whole list)" } else { "" }
		)?;
		if let Some(e) = &self.error {
			write!(f, "\n    {e}")?;
		}
		Ok(())
	}
}

pub struct Archive<'a, C: Clock> {
	pub store: &'a Store,
	pub blobs: &'a BlobStore,
	pub clock: &'a C,
}

impl<C: Clock> Archive<'_, C> {
	/// Scans one target and records the run, whatever its outcome. `Err` is only for the
	/// archive itself failing (the database); a failing source is a `failed` run.
	pub async fn run<S: ReviewSource>(&self, source: &S, target: &Target) -> eyre::Result<RunSummary> {
		let run = self.store.start_run(target.id, self.clock.now()).await?;
		let outcome = self.scan_and_apply(source, target).await;
		let now = self.clock.now();
		let summary = match outcome {
			Ok((counts, captured, complete, warnings)) => {
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
		Ok(summary)
	}

	async fn scan_and_apply<S: ReviewSource>(&self, source: &S, target: &Target) -> eyre::Result<(Counts, u32, bool, Vec<String>)> {
		let known = self.store.known(target.id).await?;
		let mut scan: Scan = source.scan(target, &known).await?;
		let mut warnings = std::mem::take(&mut scan.warnings);
		let coverage = scan.coverage;
		let plan = reconcile::plan(&known, &scan);

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

		let counts = self.store.apply(target.id, &plan, &captures, SCANNER_VERSION, self.clock.now()).await?;
		Ok((counts, captured, coverage == Coverage::Complete, warnings))
	}

	async fn store_capture(&self, target: &Target, source_review_id: &str, c: &crate::domain::Capture) -> eyre::Result<StoredCapture> {
		let captured_at = crate::store::fmt_ts(c.captured_at);
		let tagged = png_meta::with_text(
			&c.png,
			&[
				("Creation Time", &captured_at),
				("Source", &c.page_url),
				("Title", &target.label),
				("Review ID", source_review_id),
				("Software", &format!("review_archive {SCANNER_VERSION}")),
			],
		)?;
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
