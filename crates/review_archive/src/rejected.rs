//! Errors that are the caller's, not the archive's: something named that does not exist,
//! or input that cannot be used. They travel inside `eyre::Report` like any other error;
//! a caller that answers requests finds them with `downcast_ref::<Rejected>()` and says
//! 404 or 400 instead of 500.

use std::fmt;

/// A request the archive turns down.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Rejected {
	/// What was named does not exist.
	NotFound(String),
	/// The input cannot be used; the message says why.
	Invalid(String),
}

impl fmt::Display for Rejected {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::NotFound(m) | Self::Invalid(m) => f.write_str(m),
		}
	}
}

impl std::error::Error for Rejected {}

/// A [`Rejected::NotFound`] report.
#[cfg(feature = "store")]
pub(crate) fn not_found(msg: impl Into<String>) -> eyre::Report {
	eyre::Report::new(Rejected::NotFound(msg.into()))
}

/// A [`Rejected::Invalid`] report.
pub(crate) fn invalid(msg: impl Into<String>) -> eyre::Report {
	eyre::Report::new(Rejected::Invalid(msg.into()))
}
