//! The facade: one object that owns the browser, the API clients and (with a data dir)
//! the store, and does what the CLI and the HTTP API do. It takes the API's DTOs as they
//! come and answers with [`Rejected`](review_archive_core::Rejected) for the caller's
//! mistakes, so whatever serves it only translates.

use std::sync::Arc;
#[cfg(all(feature = "store", feature = "maps"))]
use std::sync::OnceLock;
#[cfg(feature = "store")]
use std::time::Duration;

#[cfg(any(feature = "maps", feature = "store"))]
use jiff::Timestamp;
#[cfg(any(feature = "maps", feature = "store"))]
use review_archive_core::check_lang;
use review_archive_core::dto::CaptureLimits;
#[cfg(feature = "store")]
use review_archive_core::{
	GbpLocation, Rejected, ReviewId, Target, TargetId, TargetKind,
	dto::{self, DayStats, ExportQuery, JobDto, JobKind, NewTarget, NewWebhook, ReviewDetail, ReviewDto, ReviewsQuery, RunDto, RunsQuery, StatsQuery, TargetDetail, TargetPatch, WebhookDto},
	fmt_ts, parse_date, parse_interval, parse_since,
	schedule::{self, LastRun},
};

#[cfg(feature = "maps")]
use crate::browser::Browser;
#[cfg(feature = "store")]
use crate::config::Secrets;
use crate::config::{Config, Defaults};
#[cfg(feature = "store")]
use crate::{
	places::Resolved,
	record::{Recorded, Recorder},
	sources::ReviewSource,
	store::{
		InsertTarget, Store,
		blobs::BlobStore,
		export::{self, Destination, Exported},
	},
	webhooks::{Deliverer, DeliveryReport},
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
	#[cfg(feature = "store")]
	webhooks: Deliverer,
	#[cfg(feature = "maps")]
	browser: Browser,
	#[cfg(all(feature = "store", feature = "maps"))]
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
	/// How much to read; the archive's per-scan limit when `max_reviews` is `None`.
	pub limits: CaptureLimits,
}

impl CaptureRequest {
	/// Every review of the place, newest first, down to the limit.
	pub fn new(place_id: impl Into<String>) -> Self {
		Self {
			place_id: place_id.into(),
			lang: None,
			limits: CaptureLimits::default(),
		}
	}

	/// Sets [`Self::lang`].
	pub fn lang(mut self, lang: impl Into<String>) -> Self {
		self.lang = Some(lang.into());
		self
	}

	/// Sets the most cards read.
	pub fn max_reviews(mut self, n: usize) -> Self {
		self.limits.max_reviews = Some(n);
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
			let cfg = crate::config::BrowserConfig {
				diagnostics_dir: config.diagnostics_dir(),
				..config.browser.clone()
			};
			Browser::new(cfg, profile)
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
				// Places and the Business Profile API answer in seconds; a hung call must not
				// hold the one worker forever.
				#[cfg(feature = "store")]
				http: reqwest::Client::builder().timeout(Duration::from_secs(15)).build()?,
				#[cfg(feature = "store")]
				webhooks: Deliverer::new(&config.webhooks)?,
				#[cfg(feature = "maps")]
				browser,
				#[cfg(all(feature = "store", feature = "maps"))]
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

	/// Stops the browser until the next scan needs it, and lets go of its profile.
	pub async fn close(&self) {
		#[cfg(feature = "maps")]
		self.inner.browser.close().await;
	}

	/// Reads a place's reviews and screenshots them, storing nothing: the reviews and the
	/// PNGs come back in memory. Works on an archive without a data dir.
	#[cfg(feature = "maps")]
	pub async fn capture_place(&self, req: &CaptureRequest) -> eyre::Result<Captured> {
		use review_archive_core::{Known, maps::Requested};

		let lang = req.lang.as_deref().unwrap_or(&self.inner.defaults.lang);
		check_lang(lang)?;
		let max = req.limits.max_reviews.unwrap_or(self.inner.defaults.max_reviews_per_scan);
		// nothing archived: every card is wanted, and stopping at the limit leaves no gap
		let nothing = Known { initial: true, ..Known::default() };
		let mut policy = Requested::new(&nothing, req.limits.review_ids.clone());
		let walked = self.inner.browser.walk(&req.place_id, lang, &mut policy, max).await?;
		let page_url = walked.page_url.clone();
		let mut scan = policy.conclude(walked, Timestamp::now());
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

	fn stored(&self) -> eyre::Result<&Stored> {
		self.inner
			.store
			.as_ref()
			.ok_or_else(|| eyre::eyre!("this archive was opened without a data dir, so it stores nothing"))
	}

	/// Watches a place: a place id, or a Google Maps URL (resolved with the Places API when
	/// it has no id, which needs `google_maps_key`). A `gbp` location makes it a `gbp`
	/// target.
	pub async fn add_target(&self, req: &NewTarget) -> eyre::Result<Added> {
		let place = req
			.place
			.as_deref()
			.or(req.maps_url.as_deref())
			.ok_or_else(|| Rejected::invalid("name the place: `place` or `maps_url`"))?;
		let interval = req.interval.as_deref().map(parse_interval).transpose()?;
		let gbp = req.gbp.as_deref().map(str::parse::<GbpLocation>).transpose()?;
		self.insert_target(place, req.label.clone(), req.lang.clone(), interval, gbp, true).await
	}

	async fn insert_target(&self, place: &str, label: Option<String>, lang: Option<String>, interval: Option<Duration>, gbp: Option<GbpLocation>, enabled: bool) -> eyre::Result<Added> {
		let store = self.store()?;
		let interval = interval.unwrap_or(self.inner.defaults.interval);
		if interval < schedule::MIN_INTERVAL {
			return Err(Rejected::invalid("the interval must be at least 1h").into());
		}
		let lang = lang.unwrap_or_else(|| self.inner.defaults.lang.clone());
		check_lang(&lang)?;
		let (place_id, resolved) = crate::places::resolve(&self.inner.http, self.inner.secrets.google_maps_key.as_deref(), place).await?;
		let target = InsertTarget {
			label: label.or_else(|| resolved.as_ref().and_then(|r| r.name.clone())).unwrap_or_else(|| place_id.clone()),
			kind: if gbp.is_some() { TargetKind::Gbp } else { TargetKind::Maps },
			place_id,
			gbp,
			lang,
			interval,
			enabled,
		};
		let id = store.add_target(&target, Timestamp::now()).await?;
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

	/// Changes a target's label, language, interval or enabled flag. Nothing archived is
	/// ever deleted.
	pub async fn update_target(&self, id: TargetId, patch: &TargetPatch) -> eyre::Result<Target> {
		let store = self.store()?;
		store.update_target(id, patch).await?;
		store.target(id).await
	}

	/// The target's last scheduled run and the failures in a row up to it.
	pub async fn last_run(&self, id: TargetId) -> eyre::Result<Option<LastRun>> {
		self.store()?.last_run(id).await
	}

	/// Scans one target now and records the run. A failing source is a `failed` run, not
	/// an `Err`; `Err` is the archive itself failing — or the browser profile held by
	/// another process, which is no run at all.
	#[cfg(feature = "maps")]
	pub async fn scan_target(&self, id: TargetId) -> eyre::Result<dto::RunSummary> {
		let target = self.target(id).await?;
		self.scan(&target).await
	}

	/// [`Self::scan_target`], for a target already loaded.
	#[cfg(feature = "maps")]
	pub async fn scan(&self, target: &Target) -> eyre::Result<dto::RunSummary> {
		Ok(self.scan_recorded(target, None).await?.summary)
	}

	#[cfg(feature = "maps")]
	async fn scan_recorded(&self, target: &Target, job: Option<i64>) -> eyre::Result<Recorded> {
		self.inner.browser.claim().await?;
		match target.kind {
			TargetKind::Maps => {
				let source = crate::sources::maps::MapsSource {
					browser: &self.inner.browser,
					defaults: &self.inner.defaults,
				};
				self.record_job(&source, target, job).await
			}
			TargetKind::Gbp => match self.gbp_client() {
				Ok(client) => {
					let source = crate::sources::gbp::GbpSource {
						client,
						browser: &self.inner.browser,
						defaults: &self.inner.defaults,
					};
					self.record_job(&source, target, job).await
				}
				// Missing credentials fail the run like any source error, so the target
				// backs off instead of spinning.
				Err(e) => self.record_job(&Unavailable(e), target, job).await,
			},
		}
	}

	/// Records a scan of `target` by any source: another platform's reviews go into the
	/// same archive this way.
	pub async fn record<S: ReviewSource>(&self, source: &S, target: &Target) -> eyre::Result<Recorded> {
		self.record_job(source, target, None).await
	}

	async fn record_job<S: ReviewSource>(&self, source: &S, target: &Target, job: Option<i64>) -> eyre::Result<Recorded> {
		let stored = self.stored()?;
		Recorder {
			store: &stored.store,
			blobs: &stored.blobs,
			now: Timestamp::now,
		}
		.record(source, target, job)
		.await
	}

	#[cfg(feature = "maps")]
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

	/// A target's runs, newest first: 20 unless asked, 500 at most.
	pub async fn runs(&self, id: TargetId, q: &RunsQuery) -> eyre::Result<Vec<RunDto>> {
		let store = self.store()?;
		store.target(id).await?;
		store.runs(id, q.limit.unwrap_or(20).min(500)).await
	}

	/// A review with every version and capture.
	pub async fn review(&self, id: ReviewId) -> eyre::Result<ReviewDetail> {
		Ok(self.store()?.review(id).await?.ok_or_else(|| Rejected::not_found(format!("no review {id}")))?)
	}

	/// Reviews of a target: first seen at or after `since`, gone or not.
	pub async fn reviews(&self, target: TargetId, q: &ReviewsQuery) -> eyre::Result<Vec<ReviewDto>> {
		let since = q.since.as_deref().map(parse_since).transpose()?;
		let store = self.store()?;
		store.target(target).await?;
		store.reviews(target, since, q.gone).await
	}

	/// Queues a scan of a target now, ahead of the scheduled ones. Returns the job id.
	pub async fn enqueue_scan(&self, id: TargetId) -> eyre::Result<i64> {
		let store = self.store()?;
		store.target(id).await?;
		store.enqueue_job(JobKind::Scan, id, None, self.inner.defaults.max_queued_jobs, Timestamp::now()).await
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
		let most = self.inner.defaults.max_reviews_initial;
		if req.limits.max_reviews.is_some_and(|n| n > most) || req.limits.review_ids.as_ref().is_some_and(|ids| ids.len() > most) {
			return Err(Rejected::invalid(format!("a capture reads {most} reviews at most")).into());
		}
		let lang = req.lang.clone().unwrap_or_else(|| self.inner.defaults.lang.clone());
		check_lang(&lang)?;
		let (place_id, resolved) = crate::places::resolve(&self.inner.http, self.inner.secrets.google_maps_key.as_deref(), place).await?;
		let target = match store.find_target(&place_id, &lang).await? {
			Some(t) => t,
			None => {
				let name = resolved.and_then(|r| r.name).unwrap_or_else(|| place_id.clone());
				self.insert_target(&place_id, Some(format!("ad hoc: {name}")), Some(lang), None, None, false).await?.target
			}
		};
		store
			.enqueue_job(JobKind::Capture, target.id, Some(&req.limits), self.inner.defaults.max_queued_jobs, Timestamp::now())
			.await
	}

	/// A job, and once done what it saw.
	pub async fn job(&self, id: i64) -> eyre::Result<JobDto> {
		Ok(self.store()?.job(id).await?.ok_or_else(|| Rejected::not_found(format!("no job {id}")))?)
	}

	/// Fails the jobs and runs a previous process died running. Call once on start, before
	/// [`Self::run_next_job`]. Returns the jobs failed.
	pub async fn recover(&self) -> eyre::Result<u64> {
		self.store()?.fail_interrupted(Timestamp::now()).await
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
				JobKind::Scan => self.scan_recorded(&target, Some(job.id)).await,
				JobKind::Capture => {
					self.inner.browser.claim().await?;
					let d = &self.inner.defaults;
					let source = crate::sources::maps::RequestedSource {
						browser: &self.inner.browser,
						max: job.limits.max_reviews.unwrap_or(d.max_reviews_per_scan).min(d.max_reviews_initial),
						limits: &job.limits,
					};
					self.record_job(&source, &target, Some(job.id)).await
				}
			}
		}
		.await;
		if let Err(e) = outcome {
			// what the caller did wrong (the target is gone) is theirs to read; the rest stays in the log
			let error = e.downcast_ref::<Rejected>().map_or_else(|| "internal error".to_owned(), ToString::to_string);
			tracing::warn!(job = job.id, error = %format!("{e:#}"), "job failed");
			store.finish_job(job.id, dto::JobStatus::Failed, None, Some(&error), &[], Timestamp::now()).await?;
			return Err(e);
		}
		store.job(job.id).await
	}

	/// Adds a webhook: its URL http(s) to a host webhooks may go to, its secret 16+
	/// characters.
	pub async fn add_webhook(&self, hook: &NewWebhook) -> eyre::Result<WebhookDto> {
		self.inner.webhooks.check_url(&hook.url)?;
		if hook.events.is_empty() {
			return Err(Rejected::invalid("subscribe the webhook to at least one event").into());
		}
		if hook.secret.len() < 16 {
			return Err(Rejected::invalid("the webhook secret is too short to be one (16+ characters)").into());
		}
		self.store()?.add_webhook(hook, Timestamp::now()).await
	}

	/// Every webhook, without secrets.
	pub async fn webhooks(&self) -> eyre::Result<Vec<WebhookDto>> {
		self.store()?.webhooks().await
	}

	/// Removes a webhook, and what it was still owed.
	pub async fn delete_webhook(&self, id: i64) -> eyre::Result<()> {
		if !self.store()?.delete_webhook(id).await? {
			return Err(Rejected::not_found(format!("no webhook {id}")).into());
		}
		Ok(())
	}

	/// Sends what the outbox has due, once.
	pub async fn deliver_webhooks(&self) -> eyre::Result<DeliveryReport> {
		self.inner.webhooks.deliver_due(self.store()?, Timestamp::now()).await
	}

	/// The PNG of a recorded capture. [`Rejected::NotFound`] for a hash the archive never
	/// recorded, even if a file by that name exists.
	pub async fn capture_png(&self, sha256: &str) -> eyre::Result<Vec<u8>> {
		let stored = self.stored()?;
		let not_found = || Rejected::not_found("no such capture");
		let path = stored.blobs.path_of(sha256).ok_or_else(not_found)?;
		if !stored.store.capture_exists(sha256).await? {
			return Err(not_found().into());
		}
		match tokio::fs::read(&path).await {
			Ok(b) => Ok(b),
			Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(not_found().into()),
			Err(e) => Err(eyre::Report::new(e).wrap_err(format!("reading {}", path.display()))),
		}
	}

	/// Per target and UTC day over `[from, to]`: new, changed, gone, mean rating, histogram.
	pub async fn stats(&self, q: &StatsQuery) -> eyre::Result<Vec<DayStats>> {
		let from = q.from.as_deref().map(parse_date).transpose()?;
		let to = q.to.as_deref().map(parse_date).transpose()?;
		self.store()?.stats(q.target.map(TargetId), from, to).await
	}

	/// Writes a target's reviews (first seen at or after `since`) and first screenshots to
	/// `out`.
	pub async fn export(&self, target: TargetId, q: &ExportQuery, out: Destination) -> eyre::Result<Exported> {
		let since = q.since.as_deref().map(parse_since).transpose()?;
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
		for t in self.targets().await?.into_iter().filter(|t| t.enabled) {
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
