//! Delivering the outbox: each due delivery is POSTed, signed, and retried with backoff
//! until it is acknowledged or its tries run out. The outbox is in the database, so a
//! restart picks up where the last process stopped.

use std::time::Duration;

use jiff::{SignedDuration, Timestamp};
use review_archive_core::hex;
use sha2::{Digest, Sha256};

use crate::store::{Delivery, Store};

/// Tries before a delivery is given up on. With the backoff below that is about a day.
pub const MAX_ATTEMPTS: i64 = 12;
const FIRST_RETRY: Duration = Duration::from_secs(30);
const RETRY_CAP: Duration = Duration::from_secs(6 * 3600);
/// Deliveries taken per pass.
const BATCH: u32 = 50;
const TIMEOUT: Duration = Duration::from_secs(10);

/// HMAC-SHA256 (RFC 2104) — the construction is a dozen lines over `sha2`, which the
/// crate already has.
pub fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
	const BLOCK: usize = 64;
	let mut k = [0u8; BLOCK];
	if key.len() > BLOCK {
		k[..32].copy_from_slice(&Sha256::digest(key));
	} else {
		k[..key.len()].copy_from_slice(key);
	}
	let ipad: Vec<u8> = k.iter().map(|b| b ^ 0x36).collect();
	let opad: Vec<u8> = k.iter().map(|b| b ^ 0x5c).collect();
	let inner = Sha256::new().chain_update(&ipad).chain_update(msg).finalize();
	Sha256::new().chain_update(&opad).chain_update(inner).finalize().into()
}

/// The `X-Signature` header of a body: `sha256=<hex of the HMAC>`. A receiver recomputes
/// it over the raw body with the hook's secret and compares.
pub fn signature(secret: &str, body: &[u8]) -> String {
	format!("sha256={}", hex(&hmac_sha256(secret.as_bytes(), body)))
}

/// How long after the `n`th failed try (1-based) the next one is: 30 s, doubling, at most
/// six hours.
pub fn retry_delay(n: i64) -> Duration {
	let doublings = u32::try_from(n.saturating_sub(1).clamp(0, 30)).unwrap_or(30);
	FIRST_RETRY.saturating_mul(2u32.saturating_pow(doublings)).min(RETRY_CAP)
}

/// What one pass over the outbox did.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DeliveryReport {
	/// Acknowledged.
	pub delivered: usize,
	/// Failed, to be tried again.
	pub retrying: usize,
	/// Failed for the last time.
	pub gave_up: usize,
}

/// Sends every delivery due at `now` once. `Err` is the store failing; a receiver failing
/// is a retry.
pub async fn deliver_due(store: &Store, http: &reqwest::Client, now: Timestamp) -> eyre::Result<DeliveryReport> {
	let mut report = DeliveryReport::default();
	for d in store.due_deliveries(now, BATCH).await? {
		match send(http, &d).await {
			Ok(()) => {
				store.delivered(d.id, Timestamp::now()).await?;
				report.delivered += 1;
			}
			Err(e) => {
				let tries = d.attempts + 1;
				let error = format!("{e:#}");
				let retry_at = (tries < MAX_ATTEMPTS).then(|| {
					now.checked_add(SignedDuration::try_from(retry_delay(tries)).unwrap_or(SignedDuration::MAX))
						.unwrap_or(Timestamp::MAX)
				});
				tracing::warn!(delivery = d.id, webhook = d.webhook_id, tries, error, "webhook delivery failed");
				store.delivery_failed(d.id, &error, retry_at, Timestamp::now()).await?;
				if retry_at.is_some() {
					report.retrying += 1;
				} else {
					report.gave_up += 1;
				}
			}
		}
	}
	Ok(report)
}

async fn send(http: &reqwest::Client, d: &Delivery) -> eyre::Result<()> {
	let resp = http
		.post(&d.url)
		.timeout(TIMEOUT)
		.header(reqwest::header::CONTENT_TYPE, "application/json")
		.header("X-Signature", signature(&d.secret, d.payload.as_bytes()))
		.header("X-Event", &d.event)
		.header("X-Delivery-Id", d.id.to_string())
		.body(d.payload.clone())
		.send()
		.await?;
	let status = resp.status();
	eyre::ensure!(status.is_success(), "{} answered {status}", d.url);
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;

	/// RFC 4231, test cases 1, 2 and 6 (a key longer than the block).
	#[test]
	fn hmac_matches_rfc_4231() {
		assert_eq!(hex(&hmac_sha256(&[0x0b; 20], b"Hi There")), "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7");
		assert_eq!(
			hex(&hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
			"5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
		);
		assert_eq!(
			hex(&hmac_sha256(&[0xaa; 131], b"Test Using Larger Than Block-Size Key - Hash Key First")),
			"60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
		);
	}

	#[test]
	fn backoff_doubles_and_caps() {
		assert_eq!(retry_delay(1), Duration::from_secs(30));
		assert_eq!(retry_delay(2), Duration::from_secs(60));
		assert_eq!(retry_delay(5), Duration::from_secs(480));
		assert_eq!(retry_delay(40), RETRY_CAP);
		let total: Duration = (1..MAX_ATTEMPTS).map(retry_delay).sum();
		assert!(total > Duration::from_secs(12 * 3600) && total < Duration::from_secs(48 * 3600), "{total:?}");
	}
}
