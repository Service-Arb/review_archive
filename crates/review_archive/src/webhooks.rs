//! Delivering the outbox: each due delivery is POSTed, signed, to its hook — or sent to a
//! member's Telegram channel by the archive's bot — and retried with backoff until it is
//! acknowledged or its tries run out. The outbox is in the database, so a restart picks up
//! where the last process stopped.
//!
//! A webhook URL comes from whoever holds the API token, and the archive POSTs to it from
//! inside its network: it may not point at loopback, private or link-local addresses —
//! checked on the URL when the hook is added and on every address it resolves to when a
//! delivery is sent — unless `allowed_hosts` names it. Redirects are not followed.

use std::{
	collections::BTreeMap,
	net::{IpAddr, Ipv4Addr, SocketAddr},
	sync::Arc,
};

use futures::{StreamExt, TryStreamExt};
use hmac::{Hmac, KeyInit, Mac};
use jiff::Timestamp;
use reqwest::{
	StatusCode, Url,
	dns::{Addrs, Name, Resolve, Resolving},
};
use review_archive_core::{
	Rejected, TargetId,
	dto::{Event, EventPayload},
	hex, schedule,
};
use sha2::Sha256;
use tg_types::TelegramDestination;

use crate::{
	config::WebhookConfig,
	store::{Delivery, Recipient, Store, blobs::BlobStore},
};

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

/// The archive's Telegram bot, which posts to members' channels.
#[derive(Clone)]
pub struct Telegram {
	/// The Bot API's root, `https://api.telegram.org` but for tests.
	pub api: Url,
	/// `TELEGRAM_BOT_TOKEN`.
	pub token: String,
	/// Where a removed review's screenshot is read from.
	pub blobs: BlobStore,
}

impl std::fmt::Debug for Telegram {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("Telegram").field("api", &self.api.as_str()).finish_non_exhaustive()
	}
}

/// Sends webhooks where they are allowed to go, and nowhere else; and Telegram messages
/// through the bot, when there is one.
#[derive(Clone, Debug)]
pub struct Deliverer {
	http: reqwest::Client,
	cfg: WebhookConfig,
	/// Lowercase; empty means any host with only public addresses.
	allowed: Arc<[String]>,
	/// The Bot API is the operator's to name, so it goes through a client of its own,
	/// without the resolver that keeps hooks off private addresses.
	telegram: Option<(reqwest::Client, Telegram)>,
}

impl Deliverer {
	/// A deliverer with its own HTTP client: short timeouts, no redirects, no proxy, and a
	/// resolver that refuses addresses webhooks may not reach.
	pub fn new(cfg: &WebhookConfig, telegram: Option<Telegram>) -> eyre::Result<Self> {
		let allowed: Arc<[String]> = cfg.allowed_hosts.iter().map(|h| h.to_ascii_lowercase()).collect();
		let http = reqwest::Client::builder()
			.redirect(reqwest::redirect::Policy::none())
			.connect_timeout(cfg.connect_timeout.duration())
			.timeout(cfg.timeout.duration())
			.no_proxy()
			.dns_resolver(Arc::new(PublicOnly { allowed: allowed.clone() }))
			.build()?;
		let telegram = telegram
			.map(|t| {
				let client = reqwest::Client::builder()
					.connect_timeout(cfg.connect_timeout.duration())
					.timeout(cfg.telegram_timeout.duration())
					.build()?;
				eyre::Ok((client, t))
			})
			.transpose()?;
		Ok(Self {
			http,
			cfg: cfg.clone(),
			allowed,
			telegram,
		})
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
		let mut by_recipient: BTreeMap<(bool, i64), Vec<Delivery>> = BTreeMap::new();
		for d in store.due_deliveries(now, self.cfg.batch).await? {
			let key = match &d.to {
				Recipient::Webhook { id, .. } => (false, *id),
				Recipient::Telegram { id, .. } => (true, *id),
			};
			by_recipient.entry(key).or_default().push(d);
		}
		futures::stream::iter(by_recipient.into_values())
			.map(|deliveries| self.deliver_to(store, deliveries, now))
			.buffer_unordered(self.cfg.parallel)
			.try_fold(DeliveryReport::default(), |mut sum, r| async move {
				sum += r;
				Ok(sum)
			})
			.await
	}

	/// One recipient's deliveries, oldest first. A receiver that cannot be reached at all
	/// gets the rest on a later pass, rather than a timeout per delivery on this one.
	async fn deliver_to(&self, store: &Store, deliveries: Vec<Delivery>, now: Timestamp) -> eyre::Result<DeliveryReport> {
		let mut report = DeliveryReport::default();
		for d in deliveries {
			let sent = match &d.to {
				Recipient::Webhook { url, secret, .. } => self.post(&d, url, secret).await,
				Recipient::Telegram { destination, .. } => self.telegram(store, &d, destination).await,
			};
			let error = match &sent {
				Ok(Ok(())) => None,
				Ok(Err(refused)) => Some(refused.clone()),
				Err(e) => Some(format!("{e:#}")),
			};
			let Some(error) = error else {
				store.delivered(d.id, Timestamp::now()).await?;
				report.delivered += 1;
				continue;
			};
			let tries = d.attempts + 1;
			let retry_at = (tries < i64::from(self.cfg.max_attempts)).then(|| {
				let n = u32::try_from(tries).expect("below max_attempts, a u32");
				now.checked_add(schedule::backoff(self.cfg.first_retry, self.cfg.retry_cap, n)).unwrap_or(Timestamp::MAX)
			});
			tracing::warn!(delivery = d.id, to = ?d.to, tries, error, "delivery failed");
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

	/// `Ok(Err)`: the hook answered, refusing; `Err`: it could not be reached.
	async fn post(&self, d: &Delivery, url: &str, secret: &str) -> eyre::Result<Result<(), String>> {
		let resp = self
			.http
			.post(self.check_url(url)?)
			.header(reqwest::header::CONTENT_TYPE, "application/json")
			.header("X-Signature", signature(secret, d.payload.as_bytes()))
			.header("X-Event", &d.event)
			.header("X-Delivery-Id", d.id.to_string())
			.body(d.payload.clone())
			.send()
			.await?;
		let status: StatusCode = resp.status();
		Ok(if status.is_success() { Ok(()) } else { Err(format!("{url} answered {status}")) })
	}

	/// The event as a message; a removed review's with the screenshot of it as it first
	/// appeared, when there is one.
	async fn telegram(&self, store: &Store, d: &Delivery, destination: &str) -> eyre::Result<Result<(), String>> {
		let (_, tg) = self.bot()?;
		let payload: EventPayload = serde_json::from_str(&d.payload).map_err(|e| eyre::eyre!("delivery {} has an unreadable payload: {e}", d.id))?;
		let label = store.target(TargetId(payload.target_id)).await?.label;
		let text = message(&payload, &label);
		let photo = match payload.review.as_ref().and_then(|r| r.capture_sha256.as_deref()).filter(|_| payload.event == Event::ReviewGone) {
			Some(sha) => {
				let path = tg.blobs.path_of(sha).ok_or_else(|| eyre::eyre!("capture {sha:?} is not a blob name"))?;
				let avif = tokio::fs::read(&path).await.map_err(|e| eyre::eyre!("reading {}: {e}", path.display()))?;
				Some(crate::avif::to_png(&avif)?) // Telegram does not take AVIF photos
			}
			None => None,
		};
		self.send_telegram(destination, text, photo).await
	}

	/// Sends a line to a channel now, so a member sees the bot can post there. `Err` is
	/// what Telegram said, or that it could not be reached.
	pub async fn test_telegram(&self, destination: &str, text: String) -> Result<(), Rejected> {
		match self.send_telegram(destination, text, None).await {
			Ok(Ok(())) => Ok(()),
			Ok(Err(refused)) => Err(Rejected::invalid(refused)),
			Err(e) => Err(Rejected::Busy(format!("{e:#}"))),
		}
	}

	fn bot(&self) -> eyre::Result<&(reqwest::Client, Telegram)> {
		self.telegram.as_ref().ok_or_else(|| eyre::eyre!("TELEGRAM_BOT_TOKEN is not set (needed for Telegram channels)"))
	}

	async fn send_telegram(&self, destination: &str, text: String, photo: Option<Vec<u8>>) -> eyre::Result<Result<(), String>> {
		let (http, tg) = self.bot()?;
		let dest: TelegramDestination = destination.parse().map_err(|e| eyre::eyre!("stored destination {destination:?}: {e}"))?;
		let method = if photo.is_some() { "sendPhoto" } else { "sendMessage" };
		// `./`: a token has a colon, which would make `bot<token>` read as a URL scheme
		let url = tg.api.join(&format!("./bot{}/{method}", tg.token))?;
		let req = http.post(url);
		let req = match photo {
			Some(png) => {
				let mut form = reqwest::multipart::Form::new().text("caption", truncate(&text, CAPTION_MAX));
				for (k, v) in dest.destination_params() {
					form = form.text(k.to_owned(), v);
				}
				req.multipart(form.part("photo", reqwest::multipart::Part::bytes(png).file_name("review.png").mime_str("image/png")?))
			}
			None => {
				let mut form: Vec<(&str, String)> = dest.destination_params();
				form.push(("text", truncate(&text, MESSAGE_MAX)));
				req.form(&form)
			}
		};
		// the URL carries the token: reqwest's error would print it
		let resp = req.send().await.map_err(|e| eyre::eyre!("the Telegram Bot API could not be reached: {}", e.without_url()))?;
		if resp.status().is_success() {
			return Ok(Ok(()));
		}
		let status = resp.status();
		#[derive(serde::Deserialize)]
		struct Answer {
			description: Option<String>,
		}
		let why = resp.json::<Answer>().await.ok().and_then(|a| a.description).unwrap_or_default(); // an unreadable refusal is still a refusal, told by its status
		Ok(Err(format!("Telegram answered {status}: {why}")))
	}
}

/// Telegram's limits, in characters.
const MESSAGE_MAX: usize = 4096;
const CAPTION_MAX: usize = 1024;

fn truncate(s: &str, max: usize) -> String {
	match s.char_indices().nth(max - 1) {
		Some((i, _)) => format!("{}…", &s[..i]),
		None => s.to_owned(),
	}
}

/// What a member reads: what happened, where, and the review as it is now.
fn message(p: &EventPayload, label: &str) -> String {
	let what = match p.event {
		Event::ReviewNew => "New review",
		Event::ReviewChanged => "Review edited",
		Event::ReviewGone => "Review removed",
		Event::ReviewReappeared => "Review back",
		Event::RunFailed => "Scan failed",
	};
	let mut out = format!("{what} · {label}");
	if let Some(r) = &p.review {
		let stars = r.rating.map(|n| "★".repeat(n.clamp(0, 5) as usize)).unwrap_or_default();
		out.push_str(&format!("\n{stars} {}", r.author));
		if let Some(t) = &r.text {
			out.push_str(&format!("\n{t}"));
		}
	}
	if let Some(e) = p.run.as_ref().and_then(|r| r.error.as_deref()) {
		out.push_str(&format!("\n{e}"));
	}
	out
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
/// documentation, benchmarking, reserved or multicast — nor an IPv6 address that carries
/// such an IPv4 one (mapped, compatible, NAT64, 6to4).
fn is_public(ip: IpAddr) -> bool {
	match ip {
		IpAddr::V4(v4) => {
			let [a, b, c, _] = v4.octets();
			let shared = a == 100 && b & 0xc0 == 64;
			let benchmarking = a == 198 && b & 0xfe == 18;
			let protocol = a == 192 && b == 0 && c == 0;
			// 240.0.0.0/4 is reserved, 255.255.255.255 (broadcast) included
			let reserved = a == 0 || a >= 240;
			!(v4.is_private()
				|| v4.is_loopback()
				|| v4.is_link_local()
				|| v4.is_unspecified()
				|| v4.is_documentation()
				|| v4.is_multicast()
				|| shared || benchmarking
				|| protocol || reserved)
		}
		IpAddr::V6(v6) => {
			let s = v6.segments();
			let v4_at = |i: usize| IpAddr::from(Ipv4Addr::from((u32::from(s[i]) << 16) | u32::from(s[i + 1])));
			if let Some(v4) = v6.to_ipv4_mapped() {
				return is_public(v4.into());
			}
			match s {
				// NAT64 (64:ff9b::/96)
				[0x64, 0xff9b, 0, 0, 0, 0, ..] => is_public(v4_at(6)),
				// 6to4 (2002::/16)
				[0x2002, ..] => is_public(v4_at(1)),
				// IPv4-compatible (::a.b.c.d); `::` and `::1` fall through
				[0, 0, 0, 0, 0, 0, hi, _] if hi != 0 => is_public(v4_at(6)),
				_ => !(v6.is_loopback() || v6.is_unspecified() || v6.is_multicast() || v6.is_unique_local() || v6.is_unicast_link_local()),
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use jiff::SignedDuration;

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
	fn default_retries_span_about_a_day() {
		let c = WebhookConfig::default();
		let delay = |n| schedule::backoff(c.first_retry, c.retry_cap, n);
		assert_eq!(delay(1), SignedDuration::from_secs(30));
		assert_eq!(delay(2), SignedDuration::from_secs(60));
		assert_eq!(delay(5), SignedDuration::from_secs(480));
		assert_eq!(delay(40), SignedDuration::from_hours(6));
		let total: SignedDuration = (1..c.max_attempts).map(delay).fold(SignedDuration::ZERO, |a, b| a + b);
		assert!(total > SignedDuration::from_hours(12) && total < SignedDuration::from_hours(48), "{total:?}");
	}

	#[test]
	fn hooks_go_only_where_they_may() {
		let open = Deliverer::new(&WebhookConfig::default(), None).unwrap();
		for public in [
			"https://hooks.example.com/x",
			"http://8.8.8.8/x",
			"http://[64:ff9b::808:808]/x",
			"http://[2002:808:808::]/x",
			"http://[2001:4860::8888]/x",
		] {
			assert!(open.check_url(public).is_ok(), "{public}");
		}
		for local in [
			"http://127.0.0.1:9/x",
			"http://localhost/x",
			"http://10.1.2.3/x",
			"http://169.254.169.254/latest/meta-data",
			"http://100.77.201.111/x",
			"http://[::1]/x",
			"http://[fd00::1]/x",
			"http://[::ffff:192.168.0.1]/x",
			"http://[64:ff9b::a9fe:a9fe]/x",
			"http://[2002:7f00:1::]/x",
			"http://[::10.0.0.1]/x",
			"http://198.18.0.1/x",
			"http://240.0.0.1/x",
			"http://192.0.0.8/x",
			"http://0.0.0.0/x",
			"ftp://example.com/x",
		] {
			assert!(open.check_url(local).is_err(), "{local}");
		}
		let listed = Deliverer::new(
			&WebhookConfig {
				allowed_hosts: vec!["Concierge".into()],
				..WebhookConfig::default()
			},
			None,
		)
		.unwrap();
		assert!(listed.check_url("http://concierge:8080/hook").is_ok());
		assert!(listed.check_url("https://hooks.example.com/x").is_err(), "only the listed hosts");
	}
}
