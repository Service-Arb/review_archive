//! A typed async client for a running review_archive, reached through the Service-Arb panel,
//! which vouches for the caller. Requests and responses are the DTOs of
//! `review_archive_core::dto`, the same types the server serializes.
//!
//! ```no_run
//! # async fn demo() -> Result<(), review_archive_client::Error> {
//! use review_archive_client::{Client, dto::{CaptureLimits, CaptureRequest}};
//!
//! let client = Client::ambient(reqwest::Client::new(), "https://sa.evinvest.ltd/api/review_archive")?;
//! let req = CaptureRequest {
//!     place: Some("ChIJLU7jZClu5kcR4PcOOO6p3I0".into()),
//!     limits: CaptureLimits { max_reviews: Some(10), ..Default::default() },
//!     ..Default::default()
//! };
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

use reqwest::{Method, RequestBuilder, StatusCode, Url};
pub use review_archive_core::dto;
use review_archive_core::dto::{
	Board, CaptureRequest, DayStats, ErrorBody, ExportQuery, GmailDto, GmailOverview, JobAccepted, JobDto, LedgerEntry, MEMBER_HEADER, Me, MemberDto, NewGmail, NewTarget, NewTgChannel,
	NewTrack, NewWebhook, ReinstatementDto, ReviewDetail, ReviewDto, ReviewsQuery, RunDto, RunsQuery, StatsQuery, Switch, TargetDetail, TargetDto, TargetPatch, TgChannelDto, TokensChange,
	TokensDto, Usage, WaitQuery, WebhookDto,
};
use serde::de::DeserializeOwned;

/// What went wrong with a call.
#[derive(Debug, thiserror::Error)]
pub enum Error {
	/// The request did not complete, or its answer did not decode.
	#[error("review_archive request failed: {0}")]
	Http(#[from] reqwest::Error),
	/// The base URL is unusable, or a path leads off it.
	#[error("review_archive URL: {0}")]
	Url(String),
	/// The archive answered with an error status.
	#[error("review_archive answered {status}: {message}")]
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

/// What `POST /captures` answered.
#[derive(Clone, Debug, PartialEq)]
pub enum Captured {
	/// It finished within the wait: the job, with its reviews when done.
	Done(Box<JobDto>),
	/// Still queued or running: poll [`Client::job`] with this id.
	Queued(i64),
}

/// A running archive.
#[derive(Clone)]
pub struct Client {
	http: reqwest::Client,
	base: Url,
	/// `/me` routes act as this person.
	member: Option<i64>,
}

impl fmt::Debug for Client {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("Client").field("base", &self.base.as_str()).finish_non_exhaustive()
	}
}

impl Client {
	/// A client whose HTTP client carries what authenticates: in a browser, the panel's
	/// session cookie and CSRF header.
	pub fn ambient(http: reqwest::Client, base: &str) -> Result<Self, Error> {
		let mut base: Url = base.parse().map_err(|e| Error::Url(format!("{base:?}: {e}")))?;
		if !base.path().ends_with('/') {
			let path = format!("{}/", base.path());
			base.set_path(&path);
		}
		Ok(Self { http, base, member: None })
	}

	/// The same caller, acting as person `member` on `/me` routes: what one holding
	/// `sa:review_archive:members:act_as` sees and does on their behalf. Anyone else is refused (403).
	pub fn as_member(self, member: i64) -> Self {
		Self { member: Some(member), ..self }
	}

	/// A path that resolves to another origin (an absolute URL, `//host/…`) is refused.
	fn request(&self, method: Method, path: &str) -> Result<RequestBuilder, Error> {
		let url = self.base.join(path.trim_start_matches('/')).map_err(|e| Error::Url(format!("{path:?}: {e}")))?;
		if url.origin() != self.base.origin() {
			return Err(Error::Url(format!("{path:?} is not on {}", self.base)));
		}
		let mut req = self.http.request(method, url);
		if let Some(m) = self.member {
			req = req.header(MEMBER_HEADER, m.to_string());
		}
		Ok(req)
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
		let q = ReviewsQuery {
			since: since.map(str::to_owned),
			gone,
		};
		Self::json(self.request(Method::GET, &format!("targets/{target}/reviews"))?.query(&q)).await
	}

	/// `GET /targets/{id}/runs`, newest first.
	pub async fn runs(&self, target: i64, limit: Option<u32>) -> Result<Vec<RunDto>, Error> {
		Self::json(self.request(Method::GET, &format!("targets/{target}/runs"))?.query(&RunsQuery { limit })).await
	}

	/// `POST /targets/{id}/scan`: scan now. Returns the job id.
	pub async fn scan(&self, target: i64) -> Result<i64, Error> {
		let accepted: JobAccepted = Self::json(self.request(Method::POST, &format!("targets/{target}/scan"))?).await?;
		Ok(accepted.job_id)
	}

	/// `POST /captures`: capture a place without registering it, waiting up to `wait`
	/// seconds (at most 120) for the result.
	pub async fn capture(&self, req: &CaptureRequest, wait: Option<u64>) -> Result<Captured, Error> {
		let resp = Self::send(self.request(Method::POST, "captures")?.json(req).query(&WaitQuery { wait })).await?;
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

	/// A capture's WebP, by the `capture_url` (or `url`) the archive gave for it.
	pub async fn capture_webp(&self, capture_url: &str) -> Result<Vec<u8>, Error> {
		Ok(Self::send(self.request(Method::GET, capture_url)?).await?.bytes().await?.to_vec())
	}

	/// `GET /targets/{id}/export.zip`: `manifest.json` and the first capture of each review.
	pub async fn export_zip(&self, target: i64, since: Option<&str>) -> Result<Vec<u8>, Error> {
		let q = ExportQuery { since: since.map(str::to_owned) };
		Ok(Self::send(self.request(Method::GET, &format!("targets/{target}/export.zip"))?.query(&q))
			.await?
			.bytes()
			.await?
			.to_vec())
	}

	/// `GET /stats`.
	pub async fn stats(&self, target: Option<i64>, from: Option<&str>, to: Option<&str>) -> Result<Vec<DayStats>, Error> {
		let q = StatsQuery {
			target,
			from: from.map(str::to_owned),
			to: to.map(str::to_owned),
		};
		Self::json(self.request(Method::GET, "stats")?.query(&q)).await
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

	/// `GET /me`: who is signed in.
	pub async fn me(&self) -> Result<Me, Error> {
		Self::json(self.request(Method::GET, "me")?).await
	}

	/// `GET /members`: everyone, with their balances.
	pub async fn members(&self) -> Result<Vec<MemberDto>, Error> {
		Self::json(self.request(Method::GET, "members")?).await
	}

	/// `POST /members/{id}/tokens`: sets a member's balance or adds to it.
	pub async fn change_tokens(&self, member: i64, change: &TokensChange) -> Result<TokensDto, Error> {
		Self::json(self.request(Method::POST, &format!("members/{member}/tokens"))?.json(change)).await
	}

	/// `GET /me/tokens`: the member's token ledger, newest first.
	pub async fn ledger(&self) -> Result<Vec<LedgerEntry>, Error> {
		Self::json(self.request(Method::GET, "me/tokens")?).await
	}

	/// `GET /me/usage`: the member's charges by day, and what they track.
	pub async fn usage(&self) -> Result<Usage, Error> {
		Self::json(self.request(Method::GET, "me/usage")?).await
	}

	/// `GET /me/overview`: the member's gmails, each with its places.
	pub async fn overview(&self) -> Result<Vec<GmailOverview>, Error> {
		Self::json(self.request(Method::GET, "me/overview")?).await
	}

	/// `POST /me/gmails`.
	pub async fn add_gmail(&self, gmail: &str) -> Result<GmailDto, Error> {
		Self::json(self.request(Method::POST, "me/gmails")?.json(&NewGmail { gmail: gmail.to_owned() })).await
	}

	/// `DELETE /me/gmails/{gmail}`.
	pub async fn delete_gmail(&self, gmail: i64) -> Result<(), Error> {
		Self::send(self.request(Method::DELETE, &format!("me/gmails/{gmail}"))?).await?;
		Ok(())
	}

	/// `PATCH /me/gmails/{gmail}`: on or off.
	pub async fn set_gmail_enabled(&self, gmail: i64, enabled: bool) -> Result<(), Error> {
		Self::send(self.request(Method::PATCH, &format!("me/gmails/{gmail}"))?.json(&Switch { enabled })).await?;
		Ok(())
	}

	/// `POST /me/gmails/{gmail}/tracks`: the place's target, shared or new.
	pub async fn track(&self, gmail: i64, req: &NewTrack) -> Result<TargetDto, Error> {
		Self::json(self.request(Method::POST, &format!("me/gmails/{gmail}/tracks"))?.json(req)).await
	}

	/// `DELETE /me/gmails/{gmail}/tracks/{target}`.
	pub async fn untrack(&self, gmail: i64, target: i64) -> Result<(), Error> {
		Self::send(self.request(Method::DELETE, &format!("me/gmails/{gmail}/tracks/{target}"))?).await?;
		Ok(())
	}

	/// `PATCH /me/gmails/{gmail}/tracks/{target}`: on or off.
	pub async fn set_track_enabled(&self, gmail: i64, target: i64, enabled: bool) -> Result<(), Error> {
		Self::send(self.request(Method::PATCH, &format!("me/gmails/{gmail}/tracks/{target}"))?.json(&Switch { enabled })).await?;
		Ok(())
	}

	/// `GET /me/gmails/{gmail}/locations/{target}/board`.
	pub async fn board(&self, gmail: i64, target: i64) -> Result<Board, Error> {
		Self::json(self.request(Method::GET, &format!("me/gmails/{gmail}/locations/{target}/board"))?).await
	}

	/// `PUT /me/gmails/{gmail}/reinstatements/{review}`: reinstatement asked of Google, now.
	pub async fn reinstate(&self, gmail: i64, review: i64) -> Result<ReinstatementDto, Error> {
		Self::json(self.request(Method::PUT, &format!("me/gmails/{gmail}/reinstatements/{review}"))?).await
	}

	/// `DELETE /me/gmails/{gmail}/reinstatements/{review}`: withdrawn, kept on record.
	pub async fn withdraw_reinstatement(&self, gmail: i64, review: i64) -> Result<(), Error> {
		Self::send(self.request(Method::DELETE, &format!("me/gmails/{gmail}/reinstatements/{review}"))?).await?;
		Ok(())
	}

	/// `GET /me/tg-channels`.
	pub async fn tg_channels(&self) -> Result<Vec<TgChannelDto>, Error> {
		Self::json(self.request(Method::GET, "me/tg-channels")?).await
	}

	/// `POST /me/tg-channels`.
	pub async fn add_tg_channel(&self, ch: &NewTgChannel) -> Result<TgChannelDto, Error> {
		Self::json(self.request(Method::POST, "me/tg-channels")?.json(ch)).await
	}

	/// `DELETE /me/tg-channels/{id}`.
	pub async fn delete_tg_channel(&self, id: i64) -> Result<(), Error> {
		Self::send(self.request(Method::DELETE, &format!("me/tg-channels/{id}"))?).await?;
		Ok(())
	}

	/// `POST /me/tg-channels/{id}/test`: a line posted now; Telegram's refusal as a 400.
	pub async fn test_tg_channel(&self, id: i64) -> Result<(), Error> {
		Self::send(self.request(Method::POST, &format!("me/tg-channels/{id}/test"))?).await?;
		Ok(())
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn requests_stay_on_the_archive() {
		let c = Client::ambient(reqwest::Client::new(), "http://archive:59110/api").unwrap();
		assert!(c.request(Method::GET, "/captures/x.webp").is_ok());
		for elsewhere in ["https://evil.example/x.webp", "http://archive:59111/x"] {
			assert!(matches!(c.request(Method::GET, elsewhere), Err(Error::Url(_))), "{elsewhere}");
		}
	}
}
