//! Where reviews come from. [`ReviewSource`] is the port; `maps` and `gbp` are the two
//! Google adapters, and other platforms plug in by implementing it.

#[cfg(feature = "maps")]
pub mod gbp;
#[cfg(feature = "maps")]
pub mod maps;

use std::future::Future;

use review_archive_core::{Known, Scan, Target};

/// A source of reviews for a target. Read-only by contract: no implementation may post,
/// edit, reply to or report anything, and none may disguise what it is.
///
/// `known` is what the archive already holds for the target, so a source can stop early
/// and screenshot only what is new or still pending ([`Known::wants_capture`]). The
/// [`Scan`]'s coverage decides what the archive may conclude is gone; claim only what the
/// source actually saw.
///
/// ```
/// use review_archive::{core::{Coverage, Known, Scan, Target}, sources::ReviewSource};
///
/// /// A platform whose API lists every review.
/// struct Listed(Vec<review_archive::core::Observed>);
///
/// impl ReviewSource for Listed {
///     async fn scan(&self, _: &Target, _: &Known) -> eyre::Result<Scan> {
///         Ok(Scan { reviews: self.0.clone(), coverage: Coverage::Complete, warnings: vec![], cut_after: None, listed: None, post: None })
///     }
/// }
/// ```
pub trait ReviewSource: Sync {
	/// One pass over the target.
	fn scan(&self, target: &Target, known: &Known) -> impl Future<Output = eyre::Result<Scan>> + Send;

	/// An ad-hoc look (a capture) rather than the target's scan: its run neither moves the
	/// target's schedule nor counts as the target's first full walk.
	fn ad_hoc(&self) -> bool {
		false
	}
}
