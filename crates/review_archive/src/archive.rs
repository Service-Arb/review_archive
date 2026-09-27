//! The facade: one object that owns the browser, the API clients and (with a data dir)
//! the store, and does what the CLI and the HTTP API do.

use std::sync::Arc;
#[cfg(all(feature = "store", feature = "gbp"))]
use std::sync::OnceLock;
#[cfg(feature = "store")]
use std::{path::Path, time::Duration};

#[cfg(any(feature = "maps", feature = "store"))]
use jiff::Timestamp;
#[cfg(feature = "store")]
use review_archive_core::{
	GbpLocation, Rejected, ReviewId, Target, TargetId, TargetKind,
	dto::{self, DayStats, JobDto, JobKind, NewWebhook, ReviewDetail, ReviewDto, RunDto, TargetDetail, TargetPatch, WebhookDto},
	fmt_ts, schedule,
};

#[cfg(feature = "maps")]
use crate::browser::Browser;
#[cfg(feature = "store")]
use crate::config::Secrets;
use crate::config::{Config, Defaults};
#[cfg(feature = "store")]
use crate::{
	places::Resolved,
	record::{Recorded, Recorder, SystemClock},
	sources::ReviewSource,
	store::{
		JobParams, NewTarget, Store,
		blobs::BlobStore,
		export::{self, Exported},
	},
};

/// A review archive: scans places, keeps what it saw, answers questions about it.
///
/// Cheap to clone; clones share the browser, the clients and the store. Everything that
/// needs the store is under the `store` feature and fails with a clear error on an
/// archive opened without a data dir.
///
/// ```no_run
/// # async fn demo() -> eyre::Result<()> {
/// use review_archive::{Archive, CaptureRequest, config::Config};
///
/// // no data dir: nothing is stored, the PNGs come back in memory
/// let mut config = Config::default();
/// config.browser.profile_dir = Some("/tmp/review-archive-profile".into());
/// let archive = Archive::open(config).await?;
/// let got = archive.capture_place(&CaptureRequest::new("ChIJLU7jZClu5kcR4PcOOO6p3I0").max_reviews(5)).await?;
/// for review in &got.scan.reviews {
///     println!("{} {:?} {} bytes", review.author, review.rating, review.capture.as_ref().map_or(0, |c| c.png.len()));
/// }
/// archive.close().await;
/// # Ok(()) }
/// ```
#[derive(Clone)]
pub struct Archive {
	inner: Arc<Inner>,
}

struct Inner {
	defaults: Defaults,
	// Everything that uses the secrets and the HTTP client (resolving places, the gbp
	// client) comes with a store.
	#[cfg(feature = "store")]
	secrets: Secrets,
	#[cfg(feature = "store")]
	http: reqwest::Client,
	#[cfg(feature = "maps")]
	browser: Browser,
	#[cfg(all(feature = "store", feature = "gbp"))]
	gbp: OnceLock<crate::sources::gbp::Client>,
	#[cfg(feature = "store")]
	store: Option<Stored>,
}

#[cfg(feature = "store")]
struct Stored {
	store: Store,
	blobs: BlobStore,
}

impl std::fmt::Debug for Archive {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("Archive").field("defaults", &self.inner.defaults).finish_non_exhaustive()
	}
}

/// A one-off capture of a place.
#[derive(Clone, Debug)]
pub struct CaptureRequest {
	/// The Google place id.
	pub place_id: String,
	/// UI language of the Maps page; the archive's default when `None`.
	pub lang: Option<String>,
	/// Cards read at most; the archive's per-scan limit when `None`.
	pub max_reviews: Option<usize>,
	/// Only these reviews (by Google's review id); the walk stops once all are found.
	pub review_ids: Option<Vec<String>>,
}

impl CaptureRequest {
	/// Every review of the place, newest first, down to the limit.
	pub fn new(place_id: impl Into<String>) -> Self {
		Self {
			place_id: place_id.into(),
			lang: None,
			max_reviews: None,
			review_ids: None,
		}
	}

	/// Sets [`Self::lang`].
	pub fn lang(mut self, lang: impl Into<String>) -> Self {
		self.lang = Some(lang.into());
		self
	}

	/// Sets [`Self::max_reviews`].
	pub fn max_reviews(mut self, n: usize) -> Self {
		self.max_reviews = Some(n);
		self
	}
}

/// What a one-off capture saw.
#[derive(Clone, Debug)]
pub struct Captured {
	/// The page the reviews were read on.
	pub page_url: String,
	/// The reviews, newest first, each with its PNG (provenance written in) when one was
	/// taken; how much of the list they span; what went wrong along the way.
	pub scan: review_archive_core::Scan,
}

/// A target to add; see [`Archive::add_target`].
#[cfg(feature = "store")]
#[derive(Clone, Debug, Default)]
pub struct AddTarget {
	/// A place id, or a Google Maps URL (resolved with the Places API when it has no id).
	pub place: String,
	/// The place's name, or the id, when `None`.
	pub label: Option<String>,
	/// The archive's default when `None`.
	pub lang: Option<String>,
	/// The archive's default when `None`; at least an hour.
	pub interval: Option<Duration>,
	/// Makes it a `gbp` target.
	pub gbp: Option<GbpLocation>,
	/// Added disabled: kept, not scheduled (ad-hoc captures).
	pub disabled: bool,
}

/// A target just added, and the search that found its place, if one was needed.
#[cfg(feature = "store")]
#[derive(Clone, Debug)]
pub struct Added {
	/// The target.
	pub target: Target,
	/// Set when the input was a URL without a place id.
	pub resolved: Option<Resolved>,
}

impl Archive {
	/// Opens the archive: the store under `config.data_dir` (created and migrated), and a
	/// browser on the profile dir, started on first use.
	pub async fn open(config: Config) -> eyre::Result<Self> {
		#[cfg(feature = "maps")]
		let browser = {
			let profile = config
				.profile_dir()
				.ok_or_else(|| eyre::eyre!("the browser needs a profile dir: set a data dir or browser.profile_dir"))?;
			Browser::new(config.browser.clone(), profile)
		};
		Self::open_inner(
			config,
			#[cfg(feature = "maps")]
			browser,
		)
		.await
	}

	/// Like [`Self::open`], on a browser the caller owns and may share.
	#[cfg(feature = "maps")]
	pub async fn open_with_browser(config: Config, browser: Browser) -> eyre::Result<Self> {
		Self::open_inner(config, browser).await
	}

	async fn open_inner(config: Config, #[cfg(feature = "maps")] browser: Browser) -> eyre::Result<Self> {
		#[cfg(feature = "store")]
		let store = match (config.db_path(), config.blob_dir()) {
			(Some(db), Some(blobs)) => Some(Stored {
				store: Store::open(&db).await?,
				blobs: BlobStore::new(blobs),
			}),
			_ => None,
		};
		Ok(Self {
			inner: Arc::new(Inner {
				defaults: config.defaults,
				#[cfg(feature = "store")]
				secrets: config.secrets,
				#[cfg(feature = "store")]
				http: reqwest::Client::new(),
				#[cfg(feature = "maps")]
				browser,
				#[cfg(all(feature = "store", feature = "gbp"))]
				gbp: OnceLock::new(),
				#[cfg(feature = "store")]
				store,
			}),
		})
	}

	/// Scan limits and target defaults it was opened with.
	pub fn defaults(&self) -> &Defaults {
		&self.inner.defaults
	}

	/// The browser it scans in.
	#[cfg(feature = "maps")]
	pub fn browser(&self) -> &Browser {
		&self.inner.browser
	}

	/// Stops the browser until the next scan needs it.
	pub async fn close(&self) {
		#[cfg(feature = "maps")]
		self.inner.browser.close().await;
	}

	/// Reads a place's reviews and screenshots them, storing nothing: the reviews and the
	/// PNGs come back in memory. Works on an archive without a data dir.
	#[cfg(feature = "maps")]
	pub async fn capture_place(&self, req: &CaptureRequest) -> eyre::Result<Captured> {
		use review_archive_core::maps::{CaptureAll, scan_to_limit};

		let lang = req.lang.as_deref().unwrap_or(&self.inner.defaults.lang);
		review_archive_core::check_lang(lang)?;
		let max = req.max_reviews.unwrap_or(self.inner.defaults.max_reviews_per_scan);
		let mut policy = match &req.review_ids {
			Some(ids) => CaptureAll::only(ids.iter().cloned()),
			None => CaptureAll::every(),
		};
		let walked = self.inner.browser.walk(&req.place_id, lang, &mut policy, max).await?;
		let page_url = walked.page_url.clone();
		let mut scan = scan_to_limit(walked, Timestamp::now());
		for r in &mut scan.reviews {
			if let Some(c) = &mut r.capture {
				c.png = crate::png_meta::provenance(c, &req.place_id, &r.source_review_id)?;
			}
		}
		Ok(Captured { page_url, scan })
	}
}

#[cfg(feature = "store")]
impl Archive {
	/// The store, for what the facade does not cover. An error on an archive opened
	/// without a data dir.
	pub fn store(&self) -> eyre::Result<&Store> {
		Ok(&self.stored()?.store)
	}

	/// The PNG store.
	pub fn blobs(&self) -> eyre::Result<&BlobStore> {
		Ok(&self.stored()?.blobs)
	}

	fn stored(&self) -> eyre::Result<&Stored> {
		self.inner
			.store
			.as_ref()
			.ok_or_else(|| eyre::eyre!("this archive was opened without a data dir, so it stores nothing"))
	}

	/// Adds a target. A Maps URL without a place id is resolved with the Places API, which
	/// needs `google_maps_key`.
	pub async fn add_target(&self, req: AddTarget) -> eyre::Result<Added> {
		let store = self.store()?;
		let interval = req.interval.unwrap_or(self.inner.defaults.interval);
		if interval < schedule::MIN_INTERVAL {
			return Err(Rejected::invalid("the interval must be at least 1h").into());
		}
		let lang = req.lang.unwrap_or_else(|| self.inner.defaults.lang.clone());
		review_archive_core::check_lang(&lang)?;
		let (place_id, resolved) = crate::places::resolve(&self.inner.http, self.inner.secrets.google_maps_key.as_deref(), &req.place).await?;
		let kind = if req.gbp.is_some() { TargetKind::Gbp } else { TargetKind::Maps };
		let label = req.label.or_else(|| resolved.as_ref().and_then(|r| r.name.clone())).unwrap_or_else(|| place_id.clone());
		let id = store
			.add_target(
				&NewTarget {
					label,
					kind,
					place_id,
					gbp: req.gbp,
					lang,
					interval,
				},
				Timestamp::now(),
			)
			.await?;
		if req.disabled {
			store.set_enabled(id, false).await?;
		}
		Ok(Added {
			target: store.target(id).await?,
			resolved,
		})
	}

	/// Every target.
	pub async fn targets(&self) -> eyre::Result<Vec<Target>> {
		self.store()?.targets().await
	}

	/// One target.
	pub async fn target(&self, id: TargetId) -> eyre::Result<Target> {
		self.store()?.target(id).await
	}

	/// Enables or disables a target. Nothing archived is ever deleted.
	pub async fn set_enabled(&self, id: TargetId, enabled: bool) -> eyre::Result<()> {
		self.store()?.set_enabled(id, enabled).await
	}

	/// Scans one target now and records the run. A failing source is a `failed` run, not
	/// an `Err`; `Err` is the archive itself failing.
	#[cfg(feature = "maps")]
	pub async fn scan_target(&self, id: TargetId) -> eyre::Result<dto::RunSummary> {
		let target = self.target(id).await?;
		self.scan(&target).await
	}

	/// [`Self::scan_target`], for a target already loaded.
	#[cfg(feature = "maps")]
	pub async fn scan(&self, target: &Target) -> eyre::Result<dto::RunSummary> {
		Ok(self.scan_recorded(target).await?.summary)
	}

	#[cfg(feature = "maps")]
	async fn scan_recorded(&self, target: &Target) -> eyre::Result<Recorded> {
		match target.kind {
			TargetKind::Maps => {
				let source = crate::sources::maps::MapsSource {
					browser: &self.inner.browser,
					defaults: &self.inner.defaults,
				};
				self.record(&source, target).await
			}
			#[cfg(not(feature = "gbp"))]
			TargetKind::Gbp => self.record(&Unavailable(eyre::eyre!("built without the gbp feature")), target).await,
			#[cfg(feature = "gbp")]
			TargetKind::Gbp => match self.gbp_client() {
				Ok(client) => {
					let source = crate::sources::gbp::GbpSource {
						client,
						browser: &self.inner.browser,
						defaults: &self.inner.defaults,
					};
					self.record(&source, target).await
				}
				// Missing credentials fail the run like any source error, so the target
				// backs off instead of spinning.
				Err(e) => self.record(&Unavailable(e), target).await,
			},
		}
	}

	/// Records a scan of `target` by any source: another platform's reviews go into the
	/// same archive this way.
	pub async fn record<S: ReviewSource>(&self, source: &S, target: &Target) -> eyre::Result<Recorded> {
		let stored = self.stored()?;
		Recorder {
			store: &stored.store,
			blobs: &stored.blobs,
			clock: &SystemClock,
		}
		.record(source, target)
		.await
	}

	#[cfg(feature = "gbp")]
	fn gbp_client(&self) -> eyre::Result<&crate::sources::gbp::Client> {
		if let Some(c) = self.inner.gbp.get() {
			return Ok(c);
		}
		let creds = self
			.inner
			.secrets
			.gbp
			.clone()
			.ok_or_else(|| eyre::eyre!("GBP_CLIENT_ID, GBP_CLIENT_SECRET and GBP_REFRESH_TOKEN are not all set (needed for gbp targets)"))?;
		Ok(self.inner.gbp.get_or_init(|| crate::sources::gbp::Client::new(self.inner.http.clone(), creds)))
	}

	/// Changes a target's label, language, interval or enabled flag.
	pub async fn update_target(&self, id: TargetId, patch: &TargetPatch) -> eyre::Result<Target> {
		let store = self.store()?;
		store.update_target(id, patch).await?;
		store.target(id).await
	}

	/// A target with its latest run, what is archived, and when it is scanned next.
	pub async fn target_detail(&self, id: TargetId) -> eyre::Result<TargetDetail> {
		let store = self.store()?;
		let target = store.target(id).await?;
		let last_run = store.runs(id, 8).await?.into_iter().find(|r| r.finished_at.is_some());
		let counts = store.target_counts(id).await?;
		let next_scan_at = if target.enabled {
			Some(fmt_ts(self.due_at(&target).await?.unwrap_or_else(Timestamp::now)))
		} else {
			None
		};
		Ok(TargetDetail {
			target: target.into(),
			last_run,
			reviews: counts.reviews,
			gone: counts.gone,
			captures: counts.captures,
			next_scan_at,
		})
	}

	/// A target's runs, newest first.
	pub async fn runs(&self, id: TargetId, limit: u32) -> eyre::Result<Vec<RunDto>> {
		let store = self.store()?;
		store.target(id).await?;
		store.runs(id, limit).await
	}

	/// A review with every version and capture.
	pub async fn review(&self, id: ReviewId) -> eyre::Result<Option<ReviewDetail>> {
		self.store()?.review(id).await
	}

	/// Queues a scan of a target now, ahead of the scheduled ones. Returns the job id.
	pub async fn enqueue_scan(&self, id: TargetId) -> eyre::Result<i64> {
		let store = self.store()?;
		store.target(id).await?;
		store.enqueue_job(JobKind::Scan, id, None, Timestamp::now()).await
	}

	/// Queues an ad-hoc capture of a place. Nothing is registered: the results are kept
	/// under the place's existing `maps` target for that language if there is one, else
	/// under a new, disabled one, so nothing captured is lost and nothing gets scheduled.
	pub async fn enqueue_capture(&self, req: &dto::CaptureRequest) -> eyre::Result<i64> {
		let store = self.store()?;
		let place = req
			.place
			.as_deref()
			.or(req.maps_url.as_deref())
			.ok_or_else(|| Rejected::invalid("name the place: `place` or `maps_url`"))?;
		let lang = req.lang.clone().unwrap_or_else(|| self.inner.defaults.lang.clone());
		review_archive_core::check_lang(&lang)?;
		let (place_id, resolved) = crate::places::resolve(&self.inner.http, self.inner.secrets.google_maps_key.as_deref(), place).await?;
		let target = match store.find_target(&place_id, &lang).await? {
			Some(t) => t,
			None => {
				let name = resolved.and_then(|r| r.name).unwrap_or_else(|| place_id.clone());
				self.add_target(AddTarget {
					place: place_id.clone(),
					label: Some(format!("ad hoc: {name}")),
					lang: Some(lang),
					disabled: true,
					..Default::default()
				})
				.await?
				.target
			}
		};
		let params = JobParams {
			max_reviews: req.max_reviews,
			review_ids: req.review_ids.clone(),
		};
		store.enqueue_job(JobKind::Capture, target.id, Some(&params), Timestamp::now()).await
	}

	/// A job, and once done what it saw.
	pub async fn job(&self, id: i64) -> eyre::Result<Option<JobDto>> {
		self.store()?.job(id).await
	}

	/// Fails the jobs a previous process died running. Call once on start, before
	/// [`Self::run_next_job`].
	pub async fn recover_jobs(&self) -> eyre::Result<u64> {
		self.store()?.fail_interrupted_jobs(Timestamp::now()).await
	}

	/// Runs the oldest queued job to its end, if there is one, and returns it as it ended.
	/// A failing scan is a `failed` job; `Err` is the archive itself failing.
	#[cfg(feature = "maps")]
	pub async fn run_next_job(&self) -> eyre::Result<Option<JobDto>> {
		let store = self.store()?;
		let Some(job) = store.claim_job(Timestamp::now()).await? else {
			return Ok(None);
		};
		let outcome = async {
			let target = store.target(job.target).await?;
			match job.kind {
				JobKind::Scan => self.scan_recorded(&target).await,
				JobKind::Capture => {
					let source = crate::sources::maps::RequestedSource {
						browser: &self.inner.browser,
						max: job.params.max_reviews.unwrap_or(self.inner.defaults.max_reviews_per_scan),
						review_ids: job.params.review_ids.clone(),
					};
					self.record(&source, &target).await
				}
			}
		}
		.await;
		match outcome {
			Ok(rec) => {
				let (status, error) = match rec.summary.status {
					dto::RunStatus::Failed => (dto::JobStatus::Failed, rec.summary.error.clone()),
					dto::RunStatus::Ok | dto::RunStatus::Partial => (dto::JobStatus::Done, None),
				};
				store.finish_job(job.id, status, Some(rec.run), error.as_deref(), &rec.seen, Timestamp::now()).await?;
			}
			Err(e) => {
				store.finish_job(job.id, dto::JobStatus::Failed, None, Some(&format!("{e:#}")), &[], Timestamp::now()).await?;
				return Err(e);
			}
		}
		store.job(job.id).await
	}

	/// Adds a webhook. Its URL must be http(s) and its secret 16+ characters.
	pub async fn add_webhook(&self, hook: &NewWebhook) -> eyre::Result<WebhookDto> {
		let invalid = |m: String| eyre::Report::new(Rejected::Invalid(m));
		let url: reqwest::Url = hook.url.parse().map_err(|e| invalid(format!("webhook url {:?}: {e}", hook.url)))?;
		if !matches!(url.scheme(), "http" | "https") {
			return Err(invalid(format!("webhook url must be http or https, not {}", url.scheme())));
		}
		if hook.events.is_empty() {
			return Err(invalid("subscribe the webhook to at least one event".into()));
		}
		if hook.secret.len() < 16 {
			return Err(invalid("the webhook secret is too short to be one (16+ characters)".into()));
		}
		self.store()?.add_webhook(hook, Timestamp::now()).await
	}

	/// Every webhook, without secrets.
	pub async fn webhooks(&self) -> eyre::Result<Vec<WebhookDto>> {
		self.store()?.webhooks().await
	}

	/// Removes a webhook, and what it was still owed. `false` when there was none.
	pub async fn delete_webhook(&self, id: i64) -> eyre::Result<bool> {
		self.store()?.delete_webhook(id).await
	}

	/// Sends what the outbox has due, once.
	pub async fn deliver_webhooks(&self) -> eyre::Result<crate::webhooks::DeliveryReport> {
		crate::webhooks::deliver_due(self.store()?, &self.inner.http, Timestamp::now()).await
	}

	/// Reviews of a target: first seen at or after `since`, gone or not.
	pub async fn reviews(&self, target: TargetId, since: Option<Timestamp>, gone: Option<bool>) -> eyre::Result<Vec<ReviewDto>> {
		self.store()?.reviews(target, since, gone).await
	}

	/// The PNG of a recorded capture. `None` for a hash the archive never recorded, even if
	/// a file by that name exists.
	pub async fn capture_png(&self, sha256: &str) -> eyre::Result<Option<Vec<u8>>> {
		let stored = self.stored()?;
		let Some(path) = stored.blobs.path_of(sha256) else {
			return Ok(None);
		};
		if !stored.store.capture_exists(sha256).await? {
			return Ok(None);
		}
		match tokio::fs::read(&path).await {
			Ok(b) => Ok(Some(b)),
			Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
			Err(e) => Err(eyre::Report::new(e).wrap_err(format!("reading {}", path.display()))),
		}
	}

	/// Per target and UTC day over `[from, to]`: new, changed, gone, mean rating, histogram.
	pub async fn stats(&self, target: Option<TargetId>, from: Option<jiff::civil::Date>, to: Option<jiff::civil::Date>) -> eyre::Result<Vec<DayStats>> {
		self.store()?.stats(target, from, to).await
	}

	/// Writes a target's reviews and first screenshots to `out`: a directory, or a `.zip`.
	pub async fn export(&self, target: TargetId, since: Option<Timestamp>, out: &Path) -> eyre::Result<Exported> {
		let stored = self.stored()?;
		export::export(&stored.store, &stored.blobs, target, since, out, Timestamp::now()).await
	}

	/// Enabled targets due at `now`, the most overdue first.
	pub async fn due(&self, now: Timestamp) -> eyre::Result<Vec<Target>> {
		let mut due: Vec<(Target, Option<Timestamp>)> = self.due_times().await?.into_iter().filter(|(_, d)| d.is_none_or(|d| d <= now)).collect();
		due.sort_by_key(|(t, d)| (*d, t.id));
		Ok(due.into_iter().map(|(t, _)| t).collect())
	}

	/// When the next enabled target is due; `None` without any. A target never scanned is
	/// due at `now`.
	pub async fn next_due(&self, now: Timestamp) -> eyre::Result<Option<Timestamp>> {
		Ok(self.due_times().await?.into_iter().map(|(_, d)| d.unwrap_or(now)).min())
	}

	/// When a target is next due by its schedule; `None`: never scanned, due now.
	pub async fn due_at(&self, target: &Target) -> eyre::Result<Option<Timestamp>> {
		let last = self.store()?.last_run(target.id).await?;
		Ok(schedule::due_at(target.id, target.interval, last))
	}

	async fn due_times(&self) -> eyre::Result<Vec<(Target, Option<Timestamp>)>> {
		let mut out = Vec::new();
		for t in self.targets().await? {
			if !t.enabled {
				continue;
			}
			let due = self.due_at(&t).await?;
			out.push((t, due));
		}
		Ok(out)
	}
}

/// A source that could not be set up; scanning it fails with why.
#[cfg(all(feature = "store", feature = "maps"))]
struct Unavailable(eyre::Report);

#[cfg(all(feature = "store", feature = "maps"))]
impl ReviewSource for Unavailable {
	async fn scan(&self, _: &Target, _: &review_archive_core::Known) -> eyre::Result<review_archive_core::Scan> {
		Err(eyre::eyre!("{:#}", self.0))
	}
}
