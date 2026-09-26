# Architecture

What the service does and why is in [SPEC.md](SPEC.md). This is where things live.

A cargo workspace of four crates; dependencies point inwards only.

```text
crates/review_archive_core/     no I/O: no browser, database, network or clock
  src/lib.rs                    targets, observations, Known, Scan, Coverage, content hashes
  src/reconcile.rs              a scan against what is stored → new / changed / unchanged / gone / reappeared
  src/schedule.rs               when a target is next due: interval, jitter, backoff
  src/relative_date.rs          "il y a 3 semaines" → an estimated timestamp, and its earliest bound
  src/maps/selectors.rs         every assumption about Google's markup, and the in-page scripts
  src/maps/parse.rs             cards out of HTML (tested on tests/fixtures/, insta snapshots)
  src/maps/mod.rs               walk policies, and what a walk may conclude (coverage)
  src/gbp.rs                    the Business Profile API's JSON; matching API reviews to Maps cards
  src/place.rs                  a place id out of what a person pastes
  src/dto.rs                    the JSON of the HTTP API, shared with the client
crates/review_archive/          the engine (features: maps, gbp, store)
  src/archive.rs                the `Archive` facade
  src/browser/                  the `Browser` handle; the CDP session: consent, sorting, the walk, screenshots
  src/sources/                  the `ReviewSource` port; the maps and gbp adapters
  src/store/                    SQLite (runtime sqlx queries, embedded migrations/), PNG blobs, export
  src/store/jobs.rs             the job queue (on-demand scans and ad-hoc captures)
  src/store/events.rs           webhook events into the outbox, in the scan's own transaction
  src/record.rs                 one scan of one target into the store: run row, source, blobs, reconcile, write
  src/webhooks.rs               delivering the outbox: HMAC-SHA256 signature, retries with backoff
  src/places.rs                 Places API search for URLs without an id
  src/rejected.rs               the caller's errors (not found / invalid), for 404 and 400
crates/review_archive_server/   the `review_archive` binary: CLI, HTTP, background loops; thin over `Archive`
  src/http.rs                   the API and its OpenAPI document (utoipa, `GET /openapi.json`)
  src/worker.rs                 the browser's worker (queued jobs, then due targets) and the deliverer
  src/settings.rs               the environment (ev_lib `settings!`): secrets, APP_ENV
  src/config.rs                 the TOML config: data dir, bind, browser, defaults
crates/review_archive_client/   typed async client of the HTTP API, on the core's DTOs
```

## Using the library

Embed the engine instead of calling a running archive:

```toml
review_archive = { version = "0.1", default-features = false, features = ["maps"] }
```

```rust
use review_archive::{Archive, CaptureRequest, config::Config};

// No data dir: nothing stored, the reviews and PNGs come back in memory.
let mut config = Config::default();
config.browser.profile_dir = Some("/var/lib/my-service/chromium".into());
let archive = Archive::open(config).await?;
let got = archive.capture_place(&CaptureRequest::new("ChIJLU7jZClu5kcR4PcOOO6p3I0").lang("fr").max_reviews(20)).await?;
for r in &got.scan.reviews {
    // r.capture: Some(PNG with provenance in tEXt chunks) for each card screenshotted
}
archive.close().await;
```

With `store` and `Config::data_dir` set, the same `Archive` also does `add_target`,
`scan_target`, `stats`, `export`, `reviews`, and gives the schedule (`due`, `next_due`).
A caller that owns a Chromium already passes it with `Archive::open_with_browser`; one
with its own platform implements `sources::ReviewSource` and records through
`Archive::record`. With only HTML in hand, `review_archive_core` parses
(`maps::parse::cards`) and reconciles (`reconcile::plan`) without any of it.

## Invariants

- **Nothing is ever written to Google.** Sources read. There is no stealth, no fingerprint
  masking, no proxy or account rotation, no CAPTCHA solving. When Google does not serve the
  full page — the "unusual traffic" page, or its "limited view" without reviews — the run
  fails and the error says which.
- **History is append-only.** Reviews and captures are never deleted; `review_versions` gets a
  row per distinct content, the first sighting included. `gone_at` is set and cleared, never a
  deletion.
- **`gone` is only concluded where the scan looked.** `gbp` lists everything, so absent means
  gone — unless the API returned fewer reviews than its own `totalReviewCount`. A `maps` walk is
  newest first and stops early; it only judges reviews whose *earliest* possible date (the
  estimate less one unit of its phrase: "a month ago" spans a month) is no earlier than the
  latest possible date of the last card it read. That needs the list to have visibly re-sorted
  to newest and its dates to run newest first; otherwise the run judges nothing. A walk judges
  everything only when it read as many cards as the list's histogram counts — an idle feed
  alone may be a stalled lazy load. A "complete" scan that lists nothing, against an archive
  with live reviews, is taken as a broken response: nothing is marked gone, the run is
  `partial`.
- **One process per browser profile.** A lock file in the profile says so; holding it means
  any Chromium `Singleton*` files there are stale (a crash, a pod with a new hostname) and are
  removed. A second `scan` while `serve` holds the profile fails with that reason.
- **Markup knowledge lives in `selectors.rs`.** A Maps change is fixed there, against fixtures
  refreshed with `scan --dump-html`, and checked by `cargo insta review`.
- **The core has no I/O.** Time comes in as an argument or through `record::Clock`; jitter is
  derived from the target and its last run, so asking twice gives one answer.
- **One browser, one queue.** A single worker uses the browser: queued jobs first
  (`POST /targets/{id}/scan`, `POST /captures`), oldest first, then the most overdue target,
  with a 5–15 s pause between any two. Jobs live in SQLite; a restart keeps the queued ones
  and fails the one that was running. An ad-hoc capture is stored under the place's `maps`
  target for its language, or a new disabled one: nothing captured is lost, nothing extra
  gets scheduled.
- **Events are an outbox.** `review.new/changed/gone/reappeared` and `run.failed` are written
  to `webhook_deliveries` in the same transaction as what they report, and delivered from
  there: signed (`X-Signature: sha256=<HMAC-SHA256 of the body>`), retried with backoff
  (30 s doubling, cap 6 h, 12 tries). Delivery is at least once; `X-Delivery-Id` lets a
  receiver drop repeats.
- **Secrets come from the environment only**, through `ev_lib::settings` in the server; the
  library takes them as `config::Secrets` and never reads the environment. Each is required
  only by what uses it and a missing one fails that with its name — except
  `REVIEW_ARCHIVE_TOKEN`, required at boot when `APP_ENV=production`
  (`review_archive --print-required-vars` lists what a profile needs).
- **A capture is the review as it first appeared.** Cards are screenshotted when new (or while
  `capture_pending`), after "More" is expanded. The PNG carries its provenance in `tEXt` chunks
  and is stored under its SHA-256.
