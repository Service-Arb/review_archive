//! A typed async client for a running review_archive, for services that call it over
//! HTTP instead of embedding the library. Requests and responses are the DTOs of
//! `review_archive_core::dto`, the same types the server serializes.
//!
//! ```no_run
//! # async fn demo() -> Result<(), review_archive_client::Error> {
//! use review_archive_client::{Client, dto::CaptureRequest};
//!
//! let client = Client::new("http://review-archive:59110", "the-bearer-token")?;
//! let req = CaptureRequest { place: Some("ChIJLU7jZClu5kcR4PcOOO6p3I0".into()), max_reviews: Some(10), ..Default::default() };
//! match client.capture(&req, Some(60)).await? {
//!     review_archive_client::Captured::Done(job) => {
//!         for r in job.reviews.unwrap_or_default() {
//!             println!("{} {:?}", r.author, r.capture_url);
//!         }
//!     }
//!     review_archive_client::Captured::Queued(id) => println!("still running: GET /jobs/{id}"),
//! }
//! # Ok(()) }
//! ```

#![warn(missing_docs)]

use std::fmt;

use reqwest::{Method, RequestBuilder, StatusCode};
pub use review_archive_core::dto;
use review_archive_core::dto::{
	CaptureRequest, DayStats, ErrorBody, JobAccepted, JobDto, NewTarget, NewWebhook, ReviewDetail, ReviewDto, RunDto, TargetDetail, TargetDto, TargetPatch, WebhookDto,
};
use serde::de::DeserializeOwned;

/// What went wrong with a call.
#[derive(Debug)]
pub enum Error {
	/// The request did not complete, or its answer did not decode.
	Http(reqwest::Error),
	/// The base URL is unusable.
	Url(String),
	/// The archive answered with an error status.
	Api {
		/// The status.
		status: StatusCode,
		/// Its `error` message.
		message: String,
	},
}

impl Error {
	/// The status, for an answer from the archive.
	pub fn status(&self) -> Option<StatusCode> {
		match self {
			Self::Api { status, .. } => Some(*status),
			Self::Http(e) => e.status(),
			Self::Url(_) => None,
		}
	}
}

impl fmt::Display for Error {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::Http(e) => write!(f, "review_archive request failed: {e}"),
			Self::Url(m) => write!(f, "review_archive base URL: {m}"),
			Self::Api { status, message } => write!(f, "review_archive answered {status}: {message}"),
		}
	}
}

impl std::error::Error for Error {
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		match self {
			Self::Http(e) => Some(e),
			Self::Url(_) | Self::Api { .. } => None,
		}
	}
}

impl From<reqwest::Error> for Error {
	fn from(e: reqwest::Error) -> Self {
		Self::Http(e)
	}
}

/// What `POST /captures` answered.
#[derive(Clone, Debug, PartialEq)]
pub enum Captured {
	/// It finished within the wait: the job, with its reviews when done.
	Done(Box<JobDto>),
	/// Still queued or running: poll [`Client::job`] with this id.
	Queued(i64),
}

/// A running archive.
#[derive(Clone, Debug)]
pub struct Client {
	http: reqwest::Client,
	base: url::Url,
	token: String,
}

impl Client {
	/// A client for the archive at `base` (e.g. `http://review-archive:59110`), with its
	/// bearer token.
	pub fn new(base: &str, token: impl Into<String>) -> Result<Self, Error> {
		Self::with_http(reqwest::Client::new(), base, token)
	}

	/// The same, on an HTTP client the caller configured (timeouts, proxies of its own).
	pub fn with_http(http: reqwest::Client, base: &str, token: impl Into<String>) -> Result<Self, Error> {
		let mut base: url::Url = base.parse().map_err(|e| Error::Url(format!("{base:?}: {e}")))?;
		if !base.path().ends_with('/') {
			let path = format!("{}/", base.path());
			base.set_path(&path);
		}
		Ok(Self { http, base, token: token.into() })
	}

	fn request(&self, method: Method, path: &str) -> Result<RequestBuilder, Error> {
		let url = self.base.join(path.trim_start_matches('/')).map_err(|e| Error::Url(format!("{path:?}: {e}")))?;
		Ok(self.http.request(method, url).bearer_auth(&self.token))
	}

	async fn send(req: RequestBuilder) -> Result<reqwest::Response, Error> {
		let resp = req.send().await?;
		let status = resp.status();
		if status.is_success() {
			return Ok(resp);
		}
		let body = resp.text().await.unwrap_or_default();
		let message = serde_json::from_str::<ErrorBody>(&body).map(|b| b.error).unwrap_or(body);
		Err(Error::Api { status, message })
	}

	async fn json<T: DeserializeOwned>(req: RequestBuilder) -> Result<T, Error> {
		Ok(Self::send(req).await?.json().await?)
	}

	/// `GET /targets`.
	pub async fn targets(&self) -> Result<Vec<TargetDto>, Error> {
		Self::json(self.request(Method::GET, "targets")?).await
	}

	/// `POST /targets`: watch a place.
	pub async fn add_target(&self, req: &NewTarget) -> Result<TargetDto, Error> {
		Self::json(self.request(Method::POST, "targets")?.json(req)).await
	}

	/// `GET /targets/{id}`.
	pub async fn target(&self, id: i64) -> Result<TargetDetail, Error> {
		Self::json(self.request(Method::GET, &format!("targets/{id}"))?).await
	}

	/// `PATCH /targets/{id}`.
	pub async fn update_target(&self, id: i64, patch: &TargetPatch) -> Result<TargetDto, Error> {
		Self::json(self.request(Method::PATCH, &format!("targets/{id}"))?.json(patch)).await
	}

	/// `DELETE /targets/{id}`: disables it; nothing archived is deleted.
	pub async fn disable_target(&self, id: i64) -> Result<TargetDto, Error> {
		Self::json(self.request(Method::DELETE, &format!("targets/{id}"))?).await
	}

	/// `GET /targets/{id}/reviews`.
	pub async fn reviews(&self, target: i64, since: Option<&str>, gone: Option<bool>) -> Result<Vec<ReviewDto>, Error> {
		let mut req = self.request(Method::GET, &format!("targets/{target}/reviews"))?;
		if let Some(since) = since {
			req = req.query(&[("since", since)]);
		}
		if let Some(gone) = gone {
			req = req.query(&[("gone", gone)]);
		}
		Self::json(req).await
	}

	/// `GET /targets/{id}/runs`, newest first.
	pub async fn runs(&self, target: i64, limit: Option<u32>) -> Result<Vec<RunDto>, Error> {
		let mut req = self.request(Method::GET, &format!("targets/{target}/runs"))?;
		if let Some(limit) = limit {
			req = req.query(&[("limit", limit)]);
		}
		Self::json(req).await
	}

	/// `POST /targets/{id}/scan`: scan now. Returns the job id.
	pub async fn scan(&self, target: i64) -> Result<i64, Error> {
		let accepted: JobAccepted = Self::json(self.request(Method::POST, &format!("targets/{target}/scan"))?).await?;
		Ok(accepted.job_id)
	}

	/// `POST /captures`: capture a place without registering it, waiting up to `wait`
	/// seconds (at most 120) for the result.
	pub async fn capture(&self, req: &CaptureRequest, wait: Option<u64>) -> Result<Captured, Error> {
		let mut r = self.request(Method::POST, "captures")?.json(req);
		if let Some(wait) = wait {
			r = r.query(&[("wait", wait)]);
		}
		let resp = Self::send(r).await?;
		if resp.status() == StatusCode::ACCEPTED {
			let accepted: JobAccepted = resp.json().await?;
			return Ok(Captured::Queued(accepted.job_id));
		}
		Ok(Captured::Done(Box::new(resp.json().await?)))
	}

	/// `GET /jobs/{id}`.
	pub async fn job(&self, id: i64) -> Result<JobDto, Error> {
		Self::json(self.request(Method::GET, &format!("jobs/{id}"))?).await
	}

	/// `GET /reviews/{id}`: with every version and capture.
	pub async fn review(&self, id: i64) -> Result<ReviewDetail, Error> {
		Self::json(self.request(Method::GET, &format!("reviews/{id}"))?).await
	}

	/// A capture's PNG, by the `capture_url` (or `url`) the archive gave for it.
	pub async fn capture_png(&self, capture_url: &str) -> Result<Vec<u8>, Error> {
		Ok(Self::send(self.request(Method::GET, capture_url)?).await?.bytes().await?.to_vec())
	}

	/// `GET /targets/{id}/export.zip`: `manifest.json` and the first capture of each review.
	pub async fn export_zip(&self, target: i64, since: Option<&str>) -> Result<Vec<u8>, Error> {
		let mut req = self.request(Method::GET, &format!("targets/{target}/export.zip"))?;
		if let Some(since) = since {
			req = req.query(&[("since", since)]);
		}
		Ok(Self::send(req).await?.bytes().await?.to_vec())
	}

	/// `GET /stats`.
	pub async fn stats(&self, target: Option<i64>, from: Option<&str>, to: Option<&str>) -> Result<Vec<DayStats>, Error> {
		let mut req = self.request(Method::GET, "stats")?;
		if let Some(t) = target {
			req = req.query(&[("target", t)]);
		}
		if let Some(from) = from {
			req = req.query(&[("from", from)]);
		}
		if let Some(to) = to {
			req = req.query(&[("to", to)]);
		}
		Self::json(req).await
	}

	/// `POST /webhooks`.
	pub async fn add_webhook(&self, hook: &NewWebhook) -> Result<WebhookDto, Error> {
		Self::json(self.request(Method::POST, "webhooks")?.json(hook)).await
	}

	/// `GET /webhooks`.
	pub async fn webhooks(&self) -> Result<Vec<WebhookDto>, Error> {
		Self::json(self.request(Method::GET, "webhooks")?).await
	}

	/// `DELETE /webhooks/{id}`.
	pub async fn delete_webhook(&self, id: i64) -> Result<(), Error> {
		Self::send(self.request(Method::DELETE, &format!("webhooks/{id}"))?).await?;
		Ok(())
	}
}
