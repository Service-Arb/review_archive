//! A review archive: a PNG of every review of a place as it first appears, plus the data
//! behind it — new, edited, gone — for statistics.
//!
//! The pure parts (parsing Maps HTML, deciding what is new or gone, relative dates, the
//! JSON types) are in [`core`]; this crate adds the I/O:
//!
//! - [`Archive`] — the facade: `capture_place`, `scan_target`, `add_target`, `stats`,
//!   `export`, the schedule. The server is a thin layer over it.
//! - [`browser::Browser`] (feature `maps`) — one headless Chromium, which callers can own
//!   and share.
//! - [`sources::ReviewSource`] — the port every platform implements; `maps` and `gbp` are
//!   the two Google ones.
//! - [`store::Store`] (feature `store`) — SQLite plus content-addressed PNGs.
//!
//! Features: `maps` (the browser, and both Google sources), `store`; both on by default.
//!
//! Scanning never writes to Google and never disguises itself: a plain headless Chromium
//! at a polite rate, and a block fails the run.
//!
//! ```no_run
//! # async fn demo() -> eyre::Result<()> {
//! use review_archive::{Archive, config::Config, core::dto::NewTarget};
//!
//! let mut config = Config::default();
//! config.data_dir = Some("/var/lib/review_archive".into());
//! let archive = Archive::open(config).await?;
//! let added = archive.add_target(&NewTarget { place: Some("ChIJLU7jZClu5kcR4PcOOO6p3I0".into()), ..Default::default() }).await?;
//! let run = archive.scan_target(added.target.id).await?;
//! println!("{run}");
//! archive.close().await;
//! # Ok(()) }
//! ```

#![feature(error_generic_member_access)]
#![warn(missing_docs)]

mod archive;
#[cfg(feature = "maps")]
pub mod browser;
pub mod config;
mod failure;
pub mod places;
pub mod png_meta;
#[cfg(feature = "store")]
pub mod record;
pub mod sources;
#[cfg(feature = "store")]
pub mod store;
#[cfg(feature = "store")]
pub mod webhooks;

#[cfg(feature = "store")]
pub use archive::Added;
pub use archive::{Archive, CaptureRequest, Captured};
#[cfg(feature = "maps")]
pub use failure::{GbpError, SessionError};
pub use failure::{PlacesError, Remedy, describe, remedy};
pub use review_archive_core as core;
pub use review_archive_core::Rejected;

/// What wrote a capture: this crate's version and commit, recorded with every PNG.
pub const SCANNER_VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), "+", env!("GIT_HASH"));
