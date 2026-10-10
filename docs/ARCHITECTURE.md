# Architecture

What the service does and why is in [SPEC.md](SPEC.md). This is where things live.

A cargo workspace of five crates; dependencies point inwards only.

```text
crates/review_archive_core/     no I/O: no browser, database, network or clock
    src/lib.rs                    targets, observations, Known, Scan, Coverage, content hashes;
                                what callers type in (intervals, langs, dates) and Rejected
  src/reconcile.rs              a scan against what is stored → new / changed / unchanged / gone / reappeared
  src/schedule.rs               when a target is next due: interval, jitter, backoff
  src/tokens.rs                 risk tokens: daily renewal, splitting a charge, the walk's meter
  src/relative_date.rs          "il y a 3 semaines" → an estimated timestamp, and its earliest bound
  src/maps/selectors.rs         every assumption about Google's markup, and the in-page scripts
  src/maps/parse.rs             cards out of HTML (tested on tests/fixtures/, insta snapshots)
    src/maps/mod.rs               walk policies, and what a walk may conclude (coverage, gaps)
  src/gbp.rs                    the Business Profile API's JSON; matching API reviews to Maps cards
  src/place.rs                  a place id out of what a person pastes
    src/dto.rs                    the JSON of the HTTP API (bodies and queries), shared with the client
crates/review_archive/          the engine (features: maps, store)
  src/archive.rs                the `Archive` facade
  src/archive/members.rs        what a member does, scoped to them
  src/browser/                  the `Browser` handle over `browser_manipulation`: consent, sorting, the walk, screenshots
  src/sources/                  the `ReviewSource` port; the maps and gbp adapters
  src/store/                    SQLite (runtime sqlx queries, embedded migrations/), WebP blobs, export
  src/store/jobs.rs             the job queue (on-demand scans and ad-hoc captures)
  src/store/events.rs           events into the outbox (hooks, members' Telegram channels), in the scan's own transaction
  src/store/members.rs          per-member state: gmails, tracks, reinstatements, Telegram channels
  src/store/people.rs           people by concierge `sub`; claiming an address's rows from before
  src/store/tokens.rs           the token ledger: balances, who pays for a scan, the account's hourly limit
  src/record.rs                 one scan of one target into the store: run row, source, blobs, reconcile, write
  src/failure.rs                the typed errors (miette codes and help), `describe`, and who a failure waits for
    src/webhooks.rs               where a hook may point; delivering the outbox: signature, retries, the Telegram bot
  src/places.rs                 Places API search for URLs without an id
crates/review_archive_server/   the `review_archive` binary: CLI, HTTP, background loops; thin over `Archive`
  src/http.rs                   the API and its OpenAPI document (utoipa, `GET /openapi.json`), `/mfe/`, the page at `/`
  src/auth.rs                   who is calling: the panel's assertion (`sa_auth`), a person by `sub`; the
                                permission guards; whose `/me` it is (`X-Member`)
  src/worker.rs                 the browser's worker (queued jobs, then due targets) and the deliverer
  src/settings.rs               the environment (ev_lib `settings!`): secrets, APP_ENV
  src/config.rs                 the config (v_utils `Settings`): data dir, bind, and every library and server section
crates/review_archive_client/   typed async client of the HTTP API through the panel, on the core's DTOs; native and wasm
crates/review_archive_web/      the dashboard MFE (dioxus, wasm), over the client; `package.sh` lays out /mfe/, `index.html` is `/`
```

## Using the library

Embed the engine instead of calling a running archive:

```toml
review_archive = { version = "0.1", default-features = false, features = ["maps"] }
```

```rust
use review_archive::{Archive, CaptureRequest, config::Config};

// No data dir: nothing stored, the reviews and WebPs come back in memory.
let mut config = Config::default();
config.browser.profile_dir = Some("/var/lib/my-service/chromium".into());
config.browser.executable = Some("/usr/bin/chromium".into());
let archive = Archive::open(config).await?;
let got = archive.capture_place(&CaptureRequest::new("ChIJLU7jZClu5kcR4PcOOO6p3I0").lang("fr").max_reviews(20)).await?;
for r in &got.scan.reviews {
    // got.webps[&r.source_review_id]: the WebP, provenance in Exif, for each card screenshotted
}
archive.close().await;
```

The process needs the Playwright driver `browser_manipulation` names (`PLAYWRIGHT_CLI_JS`,
`PLAYWRIGHT_NODE_EXE`, `PLAYWRIGHT_SKIP_DRIVER_DOWNLOAD`; this flake's devShell and image set them).

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
  fails and the error says which. A browser signed out of Google fails every walk
  (`signed_out`): Maps shows it a place's first few reviews only, unsorted, which is no
  archive; Maps halts until a restart, its profile signed in. A failed walk saves the
  page as `<data_dir>/artifacts/browser_captures/<UTC time>-<step>.{png,html}` (kept `browser.artifacts_retention`),
  and its error names them as `[<path>]`s — which is what the server's alerts attach, from
  under `<data_dir>/artifacts/` only.
- **A block pauses Maps, not a target.** Google flags the address, so a `blocked` or
  `limited_view` failure trips `maps_breaker` (SQLite, so a restart keeps it): no Maps walk
  for any target until its probe time (`schedule.backoff_base`, doubling to `schedule.backoff_cap`), then one probes; a walk that
  gets through clears it. `gbp` lists go on, their screenshots wait. What retrying cannot
  fix — changed markup or consent page, a refused GBP grant — halts that source until a
  restart; Chromium that will not start ends `serve`.
- **History is append-only.** Reviews and captures are never deleted; `review_versions` gets a
  row per distinct content, the first sighting included. `gone_at` is set and cleared, never a
  deletion.
- **A walk that stops short leaves no hole.** A scan stops at a screen of archived cards
  only once it is past the last card a previous walk was cut short at
  (`targets.cut_after`), and past every review still waiting for its screenshot; a
  target's first scan reads the whole list. Ad-hoc captures may record a cut, never
  clear one.
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
- **One process per browser profile.** `review_archive.lock` in the profile says so, taken
  before Chromium starts (`browser_manipulation`'s own lock, which clears stale `Singleton*`
  files, is only taken at launch). The profile is claimed before a run is recorded, so a second `scan` while `serve`
  holds it fails with that reason and leaves no run behind. A browser that failed under a walk
  is closed; the next walk starts a new one.
- **Markup knowledge lives in `selectors.rs`.** A Maps change is fixed there, against fixtures
  refreshed with `scan --dump-html`, and checked by `cargo insta review`.
- **The core has no I/O.** Time comes in as an argument or through `Recorder::now` (a plain
  `fn`, pinned in tests); jitter is derived from the target and its last run, so asking twice
  gives one answer.
- **The caller's mistakes are `Rejected`.** The facade takes the API's DTOs as they come and
  answers `NotFound`, `Invalid` or `Busy` for what the caller got wrong; the server only maps
  them to 404, 400 and 429, and anything else to a logged 500.
- **One write lock per transaction.** A write that reads first begins `IMMEDIATE`, so `serve`'s
  worker, its HTTP side and a hand-run `scan` never fail on each other's writes. A scan's
  reviews, events, cut, run end, job end and token charges commit together.
- **One browser, one queue.** A single worker uses the browser: queued jobs first
    (`POST /targets/{id}/scan`, `POST /captures`), oldest first — but no more than `worker.jobs_in_a_row`
  while a target is overdue — then the most overdue target, with a `schedule.pause_{min,max}` pause
  between any two. The queue is bounded (`defaults.max_queued_jobs`) and a job already queued is not
  queued twice. Jobs live in SQLite; a restart keeps the queued ones and fails the job and the runs that were running — a
  hand-run `scan` still going then included, until it records how it really ended. An ad-hoc capture is stored under the place's `maps` target for its
  language, or a new disabled one: nothing captured is lost, nothing extra gets scheduled, and
  its run does not count for the target's schedule.
- **Public facts are shared; per-member state only annotates.** Targets, reviews, versions,
  captures and blobs belong to no one: a place tracked by several members is one target,
  scanned once. What is a member's — gmails, tracks, reinstatements, Telegram channels —
  points at those facts and never alters them (tracking may put a disabled target back on
  the schedule; a target whose every track is switched off is left off it); every member query is scoped by the member's person id in SQL.
- **Tokens are a ledger; a balance is its sum; members pay for the scans of their places.**
  `token_ledger` rows are only added (renewal, an admin's set as a difference, charges). A
  walk is metered by `maps::cost` and stops at what its payers hold and the account's hour
  (`tokens.per_hour`) leaves; it is charged in the run's end transaction, a failed walk
  included — Google saw it all the same. A tracked place whose members are out of tokens is
  not walked.
- **Events are an outbox.** `review.new/changed/gone/reappeared` and `run.failed` are written
  to `webhook_deliveries` in the same transaction as what they report — a row per hook, and
  per Telegram channel of a member tracking the target — and delivered from there, retried
  with backoff (`webhooks.first_retry` doubling, cap `webhooks.retry_cap`, `webhooks.max_attempts` tries), recipients in parallel, at least once. To
  hooks: signed (`X-Signature: sha256=<HMAC-SHA256 of the body>`); `X-Delivery-Id` lets a
  receiver drop repeats. Where a hook may go: with `webhooks.allowed_hosts` empty, public
  addresses only — its URL is checked when added and every address its host resolves to when
  sent; with it set, only the hosts it lists (private addresses allowed), nowhere else.
  Redirects are not followed.
- **Secrets come from the environment only**, through `ev_lib::settings` in the server
  (`PANEL_ASSERTION_KEYS`, `GOOGLE_MAPS_KEY`, `GBP_*`, `TELEGRAM_BOT_TOKEN`, `SENTRY_DSN`,
  `ALERT_WEBHOOK_*`); the
  library takes them as `config::Secrets` and never reads the environment. Each is required
  only by what uses it and a missing one fails that with its name — except
  `PANEL_ASSERTION_KEYS`, required at boot when `APP_ENV=production`
  (`review_archive --print-required-vars` lists what a profile needs).
- **A capture is the review as it first appeared.** Cards are screenshotted when new (or while
  `capture_pending`), after "More" is expanded. It is kept as WebP, its provenance in Exif,
  under its SHA-256.
