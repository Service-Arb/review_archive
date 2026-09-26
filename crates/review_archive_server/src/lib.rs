//! The review_archive service: the HTTP API and the background loops, over the library's
//! `Archive`. The binary (`review_archive`) adds the CLI and the process bootstrap.
//!
//! A library target too so the client's tests can serve the real router in-process.

pub mod http;
pub mod worker;
