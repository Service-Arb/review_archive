//! Delivering the outbox: each due delivery is POSTed, signed, and retried with backoff
//! until it is acknowledged or its tries run out. The outbox is in the database, so a
//! restart picks up where the last process stopped.
//!
//! A webhook URL comes from whoever holds the API token, and the archive POSTs to it from
//! inside its network: it may not point at loopback, private or link-local addresses —
//! checked on the URL when the hook is added and on every address it resolves to when a
//! delivery is sent — unless `allowed_hosts` names it. Redirects are not followed.

use std::{
	collections::BTreeMap,
	net::{IpAddr, SocketAddr},
	sync::Arc,
	time::Duration,
};

use futures::{StreamExt, TryStreamExt};
use hmac::{Hmac, KeyInit, Mac};
use jiff::{SignedDuration, Timestamp};
use reqwest::{
	StatusCode, Url,
	dns::{Addrs, Name, Resolve, Resolving},
};
use review_archive_core::{Rejected, hex, schedule};
use sha2::Sha256;

use crate::{
	config::WebhookConfig,
	store::{Delivery, Store},
};

/// Tries before a delivery is given up on. With the backoff below that is about a day.
pub const MAX_ATTEMPTS: i64 = 12;
const FIRST_RETRY: Duration = Duration::from_secs(30);
const RETRY_CAP: Duration = Duration::from_secs(6 * 3600);
/// Deliveries taken per pass.
const BATCH: u32 = 50;
/// Hooks delivered to at once: a slow one does not hold up the others.
const PARALLEL_HOOKS: usize = 8;

/// HMAC-SHA256 (RFC 2104).
pub fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
	let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC takes a key of any length");
	mac.update(msg);
	mac.finalize().into_bytes().into()
}

/// The `X-Signature` header of a body: `sha256=<hex of the HMAC>`. A receiver recomputes
/// it over the raw body with the hook's secret and compares.
pub fn signature(secret: &str, body: &[u8]) -> String {
	format!("sha256={}", hex(&hmac_sha256(secret.as_bytes(), body)))
}

/// How long after the `n`th failed try (1-based) the next one is: 30 s, doubling, at most
/// six hours.
pub fn retry_delay(n: i64) -> Duration {
	schedule::backoff(FIRST_RETRY, RETRY_CAP, u32::try_from(n).unwrap_or(u32::MAX))
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

impl std::ops::AddAssign for DeliveryReport {
	fn add_assign(&mut self, o: Self) {
		self.delivered += o.delivered;
		self.retrying += o.retrying;
		self.gave_up += o.gave_up;
	}
}

/// Sends webhooks where they are allowed to go, and nowhere else.
#[derive(Clone, Debug)]
pub struct Deliverer {
	http: reqwest::Client,
	/// Lowercase; empty means any host with only public addresses.
	allowed: Arc<[String]>,
}

impl Deliverer {
	/// A deliverer with its own HTTP client: short timeouts, no redirects, no proxy, and a
	/// resolver that refuses addresses webhooks may not reach.
	pub fn new(cfg: &WebhookConfig) -> eyre::Result<Self> {
		let allowed: Arc<[String]> = cfg.allowed_hosts.iter().map(|h| h.to_ascii_lowercase()).collect();
		let http = reqwest::Client::builder()
			.redirect(reqwest::redirect::Policy::none())
			.connect_timeout(Duration::from_secs(3))
			.timeout(Duration::from_secs(10))
			.no_proxy()
			.dns_resolver(Arc::new(PublicOnly { allowed: allowed.clone() }))
			.build()?;
		Ok(Self { http, allowed })
	}

	/// Whether a webhook may be sent to `url`: `http` or `https`, to a host `allowed_hosts`
	/// names — or, with none named, to anything but a local or private address. A name's
	/// addresses are checked again on every delivery.
	pub fn check_url(&self, url: &str) -> Result<Url, Rejected> {
		let invalid = |why: String| Rejected::invalid(format!("webhook url {url:?}: {why}"));
		let parsed: Url = url.parse().map_err(|e| invalid(format!("{e}")))?;
		if !matches!(parsed.scheme(), "http" | "https") {
			return Err(invalid(format!("must be http or https, not {}", parsed.scheme())));
		}
		let host = parsed.host_str().ok_or_else(|| invalid("no host".into()))?.to_ascii_lowercase();
		if !self.allowed.is_empty() {
			return if self.allowed.contains(&host) {
				Ok(parsed)
			} else {
				Err(invalid(format!("{host} is not one of the hosts webhooks may go to")))
			};
		}
		let local_name = host == "localhost" || host.ends_with(".localhost");
		let local_ip = host.trim_matches(['[', ']']).parse::<IpAddr>().is_ok_and(|ip| !is_public(ip));
		if local_name || local_ip {
			return Err(invalid(format!("{host} is not a public address")));
		}
		Ok(parsed)
	}

	/// Sends every delivery due at `now` once. `Err` is the store failing; a receiver
	/// failing is a retry.
	pub async fn deliver_due(&self, store: &Store, now: Timestamp) -> eyre::Result<DeliveryReport> {
		let mut by_hook: BTreeMap<i64, Vec<Delivery>> = BTreeMap::new();
		for d in store.due_deliveries(now, BATCH).await? {
			by_hook.entry(d.webhook_id).or_default().push(d);
		}
		futures::stream::iter(by_hook.into_values())
			.map(|deliveries| self.deliver_to_hook(store, deliveries, now))
			.buffer_unordered(PARALLEL_HOOKS)
			.try_fold(DeliveryReport::default(), |mut sum, r| async move {
				sum += r;
				Ok(sum)
			})
			.await
	}

	/// One hook's deliveries, oldest first. A receiver that cannot be reached at all gets
	/// the rest on a later pass, rather than a timeout per delivery on this one.
	async fn deliver_to_hook(&self, store: &Store, deliveries: Vec<Delivery>, now: Timestamp) -> eyre::Result<DeliveryReport> {
		let mut report = DeliveryReport::default();
		for d in deliveries {
			let sent = self.send(&d).await;
			let error = match &sent {
				Ok(status) if status.is_success() => None,
				Ok(status) => Some(format!("{} answered {status}", d.url)),
				Err(e) => Some(format!("{e:#}")),
			};
			let Some(error) = error else {
				store.delivered(d.id, Timestamp::now()).await?;
				report.delivered += 1;
				continue;
			};
			let tries = d.attempts + 1;
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
			if sent.is_err() {
				break;
			}
		}
		Ok(report)
	}

	/// The receiver's status, or why there is none.
	async fn send(&self, d: &Delivery) -> eyre::Result<StatusCode> {
		let url = self.check_url(&d.url)?;
		let resp = self
			.http
			.post(url)
			.header(reqwest::header::CONTENT_TYPE, "application/json")
			.header("X-Signature", signature(&d.secret, d.payload.as_bytes()))
			.header("X-Event", &d.event)
			.header("X-Delivery-Id", d.id.to_string())
			.body(d.payload.clone())
			.send()
			.await?;
		Ok(resp.status())
	}
}

/// Resolves names, keeping only the addresses webhooks may reach: every one for a host
/// `allowed_hosts` names, public ones otherwise.
struct PublicOnly {
	allowed: Arc<[String]>,
}

impl Resolve for PublicOnly {
	fn resolve(&self, name: Name) -> Resolving {
		let host = name.as_str().to_ascii_lowercase();
		let trusted = self.allowed.contains(&host);
		Box::pin(async move {
			let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0)).await?.filter(|a| trusted || is_public(a.ip())).collect();
			if addrs.is_empty() {
				return Err(format!("{host} has no public address").into());
			}
			Ok(Box::new(addrs.into_iter()) as Addrs)
		})
	}
}

/// Not loopback, private, link-local, carrier-grade NAT, unspecified, broadcast,
/// documentation or multicast.
fn is_public(ip: IpAddr) -> bool {
	match ip {
		IpAddr::V4(v4) => {
			let [a, b, ..] = v4.octets();
			let shared = a == 100 && b & 0xc0 == 64;
			!(v4.is_private() || v4.is_loopback() || v4.is_link_local() || v4.is_unspecified() || v4.is_broadcast() || v4.is_documentation() || v4.is_multicast() || a == 0 || shared)
		}
		IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
			Some(v4) => is_public(v4.into()),
			None => !(v6.is_loopback() || v6.is_unspecified() || v6.is_multicast() || v6.is_unique_local() || v6.is_unicast_link_local()),
		},
	}
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

	#[test]
	fn hooks_go_only_where_they_may() {
		let open = Deliverer::new(&WebhookConfig::default()).unwrap();
		assert!(open.check_url("https://hooks.example.com/x").is_ok());
		for local in [
			"http://127.0.0.1:9/x",
			"http://localhost/x",
			"http://10.1.2.3/x",
			"http://169.254.169.254/latest/meta-data",
			"http://100.77.201.111/x",
			"http://[::1]/x",
			"http://[fd00::1]/x",
			"http://[::ffff:192.168.0.1]/x",
			"http://0.0.0.0/x",
			"ftp://example.com/x",
		] {
			assert!(open.check_url(local).is_err(), "{local}");
		}
		let listed = Deliverer::new(&WebhookConfig {
			allowed_hosts: vec!["Concierge".into()],
		})
		.unwrap();
		assert!(listed.check_url("http://concierge:8080/hook").is_ok());
		assert!(listed.check_url("https://hooks.example.com/x").is_err(), "only the listed hosts");
	}
}
