//! The review_archive service: the HTTP API and the background loops, over the library's
//! `Archive`. The binary (`review_archive`) adds the CLI and the process bootstrap.
//!
//! A library target too so the client's tests can serve the real router in-process.

pub mod http;
pub mod worker;

/// Reports an error to Sentry and logs it — as a warning: the tracing layer would send an
/// error-level event to Sentry a second time. What stops a source is logged at error level
/// by the archive, once, when it stops it.
pub fn report(e: &eyre::Report, what: &str) {
	ev_lib::error_monitoring::report(&**e);
	tracing::warn!(error = review_archive::describe(e), "{what}");
}
