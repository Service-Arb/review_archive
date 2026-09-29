//! The engine's typed errors, what a failure says, and who it waits for.
#![allow(missing_docs)] // v_utils' `wrap_err` generates constructors and trace fields without docs

use std::fmt::Write as _;

use v_utils::macros::wrap_err;

use crate::Rejected;

/// Who a failure waits for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Remedy {
	/// Passes by itself; the target's backoff retries it.
	Retry,
	/// Google flagged us: every Maps walk pauses, and one probes later.
	Pause,
	/// Retrying cannot fix it; a person has to.
	Human,
	/// The process cannot do its work.
	Fatal,
}

/// Who `e` waits for.
pub fn remedy(e: &eyre::Report) -> Remedy {
	#[cfg(feature = "maps")]
	if let Some(e) = e.downcast_ref::<SessionError>() {
		return match e {
			SessionError::Blocked { .. } | SessionError::LimitedView { .. } => Remedy::Pause,
			SessionError::MarkupChanged { .. } | SessionError::Consent { .. } => Remedy::Human,
			SessionError::Launch { .. } => Remedy::Fatal,
			SessionError::Browser(e) => match *e.kind {
				browser_manipulation::ErrorKind::Launch(_) | browser_manipulation::ErrorKind::DriverMismatch { .. } | browser_manipulation::ErrorKind::ProfileInUse(_) => Remedy::Fatal,
				_ => Remedy::Retry,
			},
			SessionError::PlaceNotFound { .. } | SessionError::Other(_) => Remedy::Retry,
		};
	}
	#[cfg(feature = "maps")]
	if let Some(GbpError::InvalidGrant { .. } | GbpError::Unauthorized { .. }) = e.downcast_ref::<GbpError>() {
		return Remedy::Human;
	}
	Remedy::Retry
}

/// `e` for a text sink (the log, a run's `error`, a webhook): the chain on the first line,
/// then the diagnostic's help and code. `[<path>]`s in it are what the alerts attach.
pub fn describe(e: &eyre::Report) -> String {
	let mut out = format!("{e:#}");
	let diagnostic: Option<&dyn miette::Diagnostic> = None
		.or_else(|| e.downcast_ref::<Rejected>().map(|d| d as _))
		.or_else(|| e.downcast_ref::<PlacesError>().map(|d| d as _));
	#[cfg(feature = "maps")]
	let diagnostic = diagnostic
		.or_else(|| e.downcast_ref::<SessionError>().map(|d| d as _))
		.or_else(|| e.downcast_ref::<GbpError>().map(|d| d as _));
	if let Some(d) = diagnostic {
		if let Some(help) = d.help() {
			write!(out, "\nhelp: {help}").expect("writing to a String");
		}
		if let Some(code) = d.code() {
			write!(out, "\ncode: {code}").expect("writing to a String");
		}
	}
	out
}

#[cfg(feature = "maps")]
/// A Maps page that did not go where the walk needed it to. The page it was on is saved
/// by the walk, which says where around this error.
#[wrap_err]
#[derive(Debug, miette::Diagnostic, thiserror::Error)]
pub enum SessionError {
	#[leaf]
	#[error("blocked by Google (\"unusual traffic\" page at {url})")]
	#[diagnostic(code(review_archive::google::blocked), help("Google flagged the address we scan from: every Maps walk pauses, and one probes later"))]
	Blocked { url: String },
	#[leaf]
	#[error("Google served its \"limited view\" of Maps, which has no reviews")]
	#[diagnostic(
		code(review_archive::google::limited_view),
		help("this browser session is not trusted with the full page: every Maps walk pauses, and one probes later")
	)]
	LimitedView,
	#[leaf]
	#[error("{step}: none of {selectors:?} on the page")]
	#[diagnostic(
		code(review_archive::maps::markup_changed),
		help("the Maps markup changed: fix selectors.rs against fixtures refreshed with `scan --dump-html`; Maps stays halted until a restart")
	)]
	MarkupChanged { step: &'static str, selectors: Vec<String> },
	#[leaf]
	#[error("Google shows no place for {place_id}")]
	#[diagnostic(
		code(review_archive::maps::place_not_found),
		help("the place was removed or merged into another: check the target's place id, or disable it")
	)]
	PlaceNotFound { place_id: String },
	#[leaf]
	#[error("consent page: {why}")]
	#[diagnostic(
		code(review_archive::maps::consent),
		help("the consent page changed: fix CONSENT_* in selectors.rs; Maps stays halted until a restart")
	)]
	Consent { why: &'static str },
	#[leaf]
	#[error("Chromium did not start: {reason}")]
	#[diagnostic(code(review_archive::browser::launch), help("check `browser.executable` and the memory the process has"))]
	Launch { reason: String },
	#[error(transparent)]
	#[diagnostic(transparent)]
	Browser(#[from] browser_manipulation::Error),
	#[error(transparent)]
	Other(#[from] eyre::Report),
}

#[cfg(feature = "maps")]
/// The Business Profile API failing us.
#[wrap_err]
#[derive(Debug, miette::Diagnostic, thiserror::Error)]
pub enum GbpError {
	/// Google no longer takes the refresh token, or the OAuth client.
	#[leaf]
	#[error("refreshing the GBP access token: {status}: {body}")]
	#[diagnostic(
		code(review_archive::gbp::invalid_grant),
		help("re-issue GBP_REFRESH_TOKEN (revoked, expired, or issued to another GBP_CLIENT_ID); gbp scans stay halted until a restart")
	)]
	InvalidGrant {
		/// The token endpoint's status.
		status: u16,
		/// What it said.
		body: String,
	},
	/// A fresh access token is refused too.
	#[leaf]
	#[error("GET {url}: still unauthorized after refreshing the access token")]
	#[diagnostic(
		code(review_archive::gbp::unauthorized),
		help("the token lacks the business.manage scope, or its account lost the location; gbp scans stay halted until a restart")
	)]
	Unauthorized {
		/// What was asked for.
		url: String,
	},
	/// Any other refusal.
	#[leaf]
	#[error("{what}: {status}: {body}")]
	#[diagnostic(code(review_archive::gbp::api))]
	Api {
		/// The call.
		what: String,
		/// Its status.
		status: u16,
		/// What it said.
		body: String,
	},
	/// The request itself failed, or its answer did not decode.
	#[foreign]
	#[diagnostic(code(review_archive::gbp::http))]
	Http(reqwest::Error),
	/// Anything else.
	#[error(transparent)]
	Other(#[from] eyre::Report),
}

/// The Places API failing us.
#[wrap_err]
#[derive(Debug, miette::Diagnostic, thiserror::Error)]
pub enum PlacesError {
	/// It refused the search.
	#[leaf]
	#[error("Places text search for {query:?}: {status}: {body}")]
	#[diagnostic(code(review_archive::places::api), help("check GOOGLE_MAPS_KEY, and that the Places API (New) is enabled for its project"))]
	Api {
		/// What was searched for.
		query: String,
		/// Its status.
		status: u16,
		/// What it said.
		body: String,
	},
	/// The request itself failed, or its answer did not decode.
	#[foreign]
	#[diagnostic(code(review_archive::places::http))]
	Http(reqwest::Error),
}
