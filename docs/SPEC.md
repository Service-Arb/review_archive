# review_archive — spec

Archive of public place reviews: an AVIF screenshot of every review as it first
appears, plus structured data for statistics. Google first; the source layer is a
trait so other platforms slot in later.

Replaces a person manually checking places and saving screenshots.

## Scope

In:

- Watch a list of **targets** (places) on an interval (hours, not seconds).
- On each scan: detect reviews not seen before, store their data, take an AVIF
  screenshot of the review card, keep edit history, note reviews that are no
  longer listed.
- Statistics and export over what was stored.

Out — do not build, even as an option:

- Posting, editing or replying to anything; any write call to Google.
- Browser fingerprint masking, "stealth" plugins, proxy or account rotation,
  CAPTCHA solving. The scanner is a plain headless Chromium at a polite rate; if
  Google blocks it, the run fails and is reported.
- Automatic submission of anything to Google (appeals, reports).

## Sources

`trait ReviewSource { async fn scan(&self, target: &Target, known: &Known) -> Result<Scan> }`

### `maps` — any public place (headless browser)

- Chromium through `browser_manipulation` (Playwright protocol, patchright driver). One
  browser process, targets scanned sequentially.
- URL: `https://www.google.com/maps/place/?q=place_id:<PLACE_ID>&hl=<lang>`.
- EU consent interstitial (`consent.google.com`): click the reject-all button;
  persist the resulting cookies in the profile dir so it is not shown again.
- The browser profile is signed in to a Google account: signed out, Maps shows a place's
  first few reviews only and will not sort them, so the walk fails (`signed_out`) and Maps
  halts until a restart.
- On the place's overview, read the owner's latest post under "From the owner" (text,
  date as printed); it is kept once per distinct text (`posts`), with when it was first
  and last seen. Only the latest shows there: two posts between scans keep the newer.
- Open the Reviews tab. When the place's review count is the one the last scan read
  (`targets.listed`; ad-hoc captures do not set it), and no gap or pending screenshot is
  owed, stop there: the run lists nothing and judges nothing. Otherwise sort by *Newest*,
  scroll the feed. Stop when a full screen of
  cards is already known (cards still without a screenshot do not count, and the walk
  goes on until it has read or passed every one of those), or at `max_reviews_per_scan`
  (default 200).
  First scan of a target walks the whole list (bounded by
  `max_reviews_initial`, default 2000), whatever ad-hoc captures stored before it.
- A walk cut short before reaching archived cards — by its limit, or by the page failing
  under it — records its last card (`targets.cut_after`). The next scan reads past that
  card before a screen of known cards may end it, with `max_reviews_initial` as its
  limit, so what lay below is not skipped for good. A failure after cards were read
  keeps them (the run is `partial`); a block by Google still fails the run.
- Per card (`[data-review-id]`): click *More* to expand, extract review id,
  author name, author profile URL, star rating (from `aria-label`), relative
  date text, text, owner reply, photo count; element screenshot → PNG, kept as AVIF.
- Selectors live in one module with HTML fixtures under `tests/fixtures/` and
  parser tests on them — the page changes, and the fix must be one file.

### `gbp` — profiles we manage (official API)

- Google Business Profile API, `GET https://mybusiness.googleapis.com/v4/accounts/{account}/locations/{location}/reviews`
  (`pageSize=50`, paginate with `nextPageToken`; a token seen twice, or more than 1000
  pages, fails the run). `account` and `location` are numbers. OAuth2 refresh-token flow;
  `GBP_CLIENT_ID`, `GBP_CLIENT_SECRET`, `GBP_REFRESH_TOKEN` from env.
- The API gives the authoritative, complete list (id, reviewer, starRating,
  comment, createTime, updateTime, reply), so `gone` detection is exact here.
- Screenshots still come from the public Maps page: for each new API review, run
  the `maps` capture on the target's `place_id` and match the card by
  (author name, rating, normalised text prefix). Unmatched → no capture row,
  `capture_pending` stays set, retried on the next scan (Maps lags the API).
- A `gbp` target therefore needs both `gbp_account`/`gbp_location` and
  `place_id`.

### Place id resolution

`target add` accepts a place id or a Google Maps URL. From a URL, parse the id if
present; otherwise resolve via Places API (New) `places:searchText`
(`GOOGLE_MAPS_KEY`, field mask `places.id,places.displayName,places.formattedAddress`),
same as `gmaps_optimal_placement_sources/src/poi.rs`.

## Storage

SQLite (sqlx, runtime `sqlx::query`, embedded migrations) + content-addressed
blob dir. Everything under one data dir (`/data` in the container).

- `targets(id, label, kind[maps|gbp], place_id, gbp_account, gbp_location, lang, interval_secs, enabled, created_at, cut_after)`
  — `lang` is a language tag (`fr`, `pt-BR`); anything else is refused.
- `reviews(id, target_id, source_review_id, author, author_url, rating, text, reply, published_raw, published_est, first_seen, last_seen, gone_at, content_hash, capture_pending)`
  — unique `(target_id, source_review_id)`.
  — `published_raw` is the date text `published_est` was estimated from; a later
  reading ("Edited a day ago") does not replace it.
- `review_versions(review_id, seen_at, content_hash, rating, text, reply)` — a
  row whenever `content_hash` changes; history is never overwritten.
- `posts(id, target_id, content_hash, text, published_raw, published_est, first_seen, last_seen)`
- `captures(id, review_id, captured_at, sha256, width, height, page_url, scanner_version)`
- `runs(id, target_id, started_at, finished_at, status[ok|partial|failed], error, n_seen, n_new, n_changed, n_gone, ad_hoc, tokens)`
  — `ad_hoc` marks the runs of ad-hoc captures; `tokens` is what its walk cost (see Tokens), a failed one's too. A run a stopped process left open is
  failed ("interrupted") on the next start of `serve` — a hand-run `scan` still going at
  that moment included; its run is marked ended again, with its real outcome, when it
  finishes.
- Blobs: `<data>/blobs/<sha256[0..2]>/<sha256>.avif`, lossy. Exif carries the provenance:
  capture time (`DateTimeOriginal`, `OffsetTimeOriginal` `+00:00`), page URL (`DocumentName`),
  target label (`ImageDescription`), source review id (`ImageUniqueID`), scanner version
  (`Software`); text as UTF-8.

`gone`: set when a review is absent from a scan that covered its position
(`gbp`: always complete; `maps`: only if the scan walked past its
`published_est`). Cleared if it reappears. Reviews and captures are never
deleted.

## Scheduling

- Per-target `interval_secs`, default `defaults.interval` (1 d), minimum `schedule.min_interval`
  (1 h), ±`schedule.jitter` (10 %).
- One scan at a time; a random pause between targets, `schedule.pause_{min,max}` (5–15 s).
- Failure → `runs.status=failed` with the error, exponential backoff for that
  target (`schedule.backoff_base` doubling to `schedule.backoff_cap`: 1 h → 24 h), other targets continue.
- A tracked target whose members hold fewer tokens together than a walk's first screen
  costs is held, not scanned, until a balance renews or is topped up. All walks together
  spend `tokens.per_hour` at most in any hour; past it Maps closes until older runs leave
  the window, like the breaker.
- Ad-hoc captures run on the same browser but are not the target's schedule: their
  runs neither delay the next scan nor count as failures.
- A scan whose browser profile another process holds (a `scan` beside a running
  `serve`) fails with that reason and records no run.

## Interfaces

CLI (`clap`):

- `review_archive target add <place-id|maps-url> [--label] [--lang fr] [--interval 6h] [--gbp <account>/<location>]`
- `review_archive target list | disable <id> | enable <id>`
- `review_archive scan <target-id|--all>` — one pass now, prints a summary.
- `review_archive serve` — scheduler + HTTP.
- `review_archive export --target <id> [--since <date>] --out <dir|file.zip>` — captures (AVIF) + `manifest.json`.

HTTP (axum), behind the Service-Arb panel's assertion (see Auth), binds `127.0.0.1` unless
configured:

- `GET /health` (no auth)
- `GET /targets`
- `GET /targets/{id}/reviews?since=&gone=`
- `GET /captures/{sha256}.avif`
- `GET /stats?target=&from=&to=` — per target and per day: new, changed, gone,
  mean rating, rating histogram. `Accept: text/csv` → CSV. `gone` counts what the runs
  of that day marked gone, whether or not it came back later.

Errors are JSON (`{"error": …}`) with 400 for input the archive cannot use (a body or a
query it cannot read included), 404 for what does not exist, 429 when the job queue is
full, and a bare `internal error` for a 5xx. API requests take `http.request_timeout` at most (an export as long as it
needs) and `http.max_concurrent` are served at once; `/health` and `/openapi.json` are outside both limits.

Config: v_utils `Settings` (`--config`, else `$XDG_CONFIG_HOME/review_archive.{toml,nix,…}`;
each key also a flag, e.g. `--schedule-min-interval`) for data dir, bind address, defaults,
pacing (`[schedule]`, `[worker]`, `[http]`, `[webhooks]`, `[browser]`, `[tokens]`); every key has a default,
`review_archive config write-defaults` lists them. Secrets only from env.

### HTTP API for other services

Everything the CLI can do is reachable over HTTP, so other services drive the
archive without shelling into its container. JSON in and out; DTOs live in the
library (below) and are shared with the client crate. OpenAPI document at
`GET /openapi.json` (utoipa, as in concierge).

Targets:

- `POST /targets` `{place | maps_url, label?, lang?, interval?, gbp?}` → `201`
  with the target. Same resolution as `target add`.
- `PATCH /targets/{id}` `{label?, lang?, interval?, enabled?}`
- `DELETE /targets/{id}` — disables; archived data is never deleted.
- `GET /targets/{id}` — target with last run, counts, next scheduled scan.

Jobs (one browser, one queue; on-demand jobs go ahead of scheduled scans):

- `POST /targets/{id}/scan` → `202 {job_id}` — scan now.
- `POST /captures` `{place | maps_url, lang?, max_reviews?, review_ids?}` →
  `202 {job_id}` — ad-hoc capture of a place **without** registering a target;
  results are stored under an implicit, disabled target so nothing is lost.
  `?wait=<secs>` (cap 120) blocks and returns the result directly if done.
  `max_reviews` and the number of `review_ids` are at most `max_reviews_initial`.
- The queue holds `max_queued_jobs` (default 20) at most: past that, `429`. Asking again
  for a job already queued (same target, kind and limits) returns its id. After three
  jobs in a row, an overdue scheduled scan goes first.
- `GET /jobs/{id}` → `queued | running | done | failed`, and on `done` the
  reviews with their capture URLs — for a capture of `review_ids`, those of them it
  found.
- `GET /targets/{id}/runs?limit=`

Reviews and captures:

- `GET /reviews/{id}` — review with versions and captures.
- `GET /targets/{id}/export.zip?since=` — same archive as `export`.

Events:

- `POST /webhooks` `{url, events: [review.new, review.changed, review.gone, review.reappeared, run.failed], secret}`,
    `GET /webhooks`, `DELETE /webhooks/{id}`. Deliveries are signed
  (`X-Signature: sha256=<hmac of body>`), retried with backoff, recorded in an
  outbox table so a restart does not drop them.
- A webhook URL is `http(s)`; with `[webhooks] allowed_hosts` empty it may not name a
  loopback, private, link-local, carrier-grade-NAT or otherwise local address, and a
  host name is refused at delivery when it resolves only to such addresses. A non-empty
  `allowed_hosts` is the whole list of hosts webhooks may go to (a service in the same
  cluster, say). Redirects are not followed; a delivery gets 3 s to connect and 10 s in
  all, and a receiver that cannot be reached is not held against the other hooks.

## Members

Several people track their places here, grouped the way they manage them: by
**managing gmail**, the Google manager account a small group of GBPs is attached to.

- A **member** is a person: a concierge account the Service-Arb panel vouches for, kept in
  `people(id, sub UNIQUE, email, name, first_seen)` and found or made by `sub` on every
  request. Anyone signed in to the panel is one; what they may do beyond their own `/me` is
  their `sa:review_archive:*` permissions, granted in concierge. Rows from before people had
  ids were keyed by email: each address is a person without a `sub`, claimed by the first
  sign-in with it — only when concierge says the address is verified and no other account
  already holds it; otherwise the request is a 403 and an error is logged for an admin.
- `managing_gmails(id, person_id, gmail, created_at)`, unique per member — a grouping,
  not a credential: an address, or any alias for one without spaces (`tg:@owner`), lowercased. GBP reads keep the service's one grant: a client adds the service's
  Google account as a manager of their GBP.
- `tracks(managing_gmail_id, target_id, created_at)`. Tracking a place finds the target on
  that place, language and source, or makes one (enabling a disabled one): targets,
  reviews, captures and blobs stay shared, so two members on one place cost one scan and
  one capture. Scheduling stays per target. The operator assigns existing targets with
  `gmail add <member> <gmail>` and `track <gmail-id> <target-id>`.
- Gmails and tracks are switched on or off by their member (`enabled`, on when added;
  `PATCH /me/gmails/{id}`, `PATCH /me/gmails/{id}/tracks/{target}`). A target someone
  tracks is scanned while one of its tracks is on under a gmail that is on; one nobody
  tracks keeps to its own `enabled`. Switching never touches the shared target.
- `reinstatements(managing_gmail_id, review_id, requested_at, withdrawn_at, reinstated_at)`
  record that a member asked Google to reinstate a removed review (the archive asks
  nothing of Google). Withdrawing sets `withdrawn_at`; nothing is deleted. A scan that lists
  the review again sets `reinstated_at` in its transaction, and it returns to the
  Snapshotted column with a "reinstated after N d" badge. A gmail with appeals on record
  cannot be removed.
- A member's board of a tracked place has three columns, each by its own time:
  Snapshotted (listed, by first sighting), Removed (gone, no open appeal, by `gone_at`),
  Reinstating (gone, open appeal, by `requested_at`).

### Tokens

Every Maps walk runs on the operator's signed-in Google account, and each action is
exposure for it. Tokens price that, and the members whose places are scanned pay.

- A walk is metered in `maps::cost`: opening the place 7, the Reviews tab 1, sorting 7
  (each retried click 2), each scroll of the feed 1 — the data requests each sends Google, a
  scroll's ~9 being the unit. "More" and screenshots send none. An unchanged place costs 8;
  a rescan 15 and 1 per screen of cards.
- `token_ledger(id, person_id, at, delta, kind[accrual|grant|purchase|set|charge], run_id, by_email, note)`:
  append-only; a balance is the sum of its rows. Reading a balance renews it first, a whole
  day at a time: `tokens.daily` (15) per day while under `tokens.cap` (300); granted or bought
  tokens above the cap stay. A member seen for the first time starts with a day's worth.
  `set` writes the difference to the balance asked for.
- A scheduled scan of a target is paid by the members tracking it (a track on, under a gmail
  that is on). Its walk may spend their balances together, within what the account has left
  this hour; the next scroll past that ends the walk (`WalkEnd::Budget`), recording a cut as
  a limit does — a first scan too — so the next scan goes on from there. Whatever the walk
  spent is charged with the run's end, in its transaction: equal shares, none past its
  payer's balance, the rest falling on the others. Places nobody tracks, the operator's jobs
  and ad-hoc captures are paid by no one and limited by the hour only.
- `purchase` is recorded by an admin (a payment reference in `note`); nothing is sold here.
- `GET /me` carries the signed-in person's `tokens {balance, daily, cap}`; `GET /me/tokens` is
  the member's ledger, each charge with its run and place; `GET /me/usage` its charges summed
  per UTC day over the last 30 (`{day, walks, tokens}`, every day present) and the places it
  tracks now. `GET /members` lists every person
  with their balance; `POST /members/{id}/tokens` `{set | grant | purchase: n, note?}` sets or
  adds to one. Both need `sa:review_archive:tokens:grant`.
  A place held for tokens shows "out of tokens" on its card (`held`).

### Auth

Served behind the Service-Arb panel at `sa.evinvest.ltd`: its `/api/review_archive/*` reaches
the API (the prefix stripped), its `/review_archive/mfe/*` the bundle. Nothing else reaches
the service.

- Every authenticated request carries the panel's assertion in `x-sa-assertion` (`sa_auth`):
  an Ed25519 JWS `{aud: review_archive, sub, email, email_verified, name, permissions, method,
  path, exp}`, verified with the panel's public keys (`PANEL_ASSERTION_KEYS`). It names this
  one request — its method and path — and lives 60 s; anything else is a 401. `permissions`
  is the caller's `sa:review_archive:*` slice.
- `sa:review_archive:archive:operate` opens the archive's own routes: targets, scans,
  captures, jobs, reviews, stats, webhooks, export, and any capture's AVIF.
  `sa:review_archive:tokens:grant` opens `GET /members` and `POST /members/{id}/tokens`.
- `/me` routes are every signed-in person's own, and `GET /captures/{sha}.avif` of the places
  their gmails track. With `sa:review_archive:members:act_as`, `X-Member: <person id>` acts as
  that member: their gmails, boards and channels, their writes; anyone else sending it gets 403.
- `POST /me/tg-channels/{id}/test` posts at most once a minute per person (429).
- CSRF is the panel's: it checks its own header on every write before forwarding.
- `serve --dev-member <alias | permissions | none>` (loopback, outside production): every
  request is one made-up person holding that, without the panel, for a local dashboard
  (`nix run .#dev-mfe`).

### Telegram

- `tg_channels(person_id, managing_gmail_id NULL = all, destination, events)`;
  `destination` is what the member pasted (`@channel`, `-100…`, `<group>/<topic>`),
  parsed as `tg_types::TelegramDestination`.
- Events go through the outbox (`webhook_deliveries`, a row names exactly one hook or
  channel), fanned out in the scan's transaction to the channels whose member tracks the
  target (under the channel's gmail, if it names one). Same retries as hooks.
- One service bot (`TELEGRAM_BOT_TOKEN`) sends through the Bot API (`webhooks.telegram_api`):
  `review.gone` as `sendPhoto` with the review's first capture (as PNG: Telegram does not take AVIF), the rest as text. The
  member adds the bot to the chat; `POST /me/tg-channels/{id}/test` answers with
  Telegram's refusal, if any. Ops alerts stay on Discord.

### Dashboard

`crates/review_archive_web`: a dioxus microfrontend, `<mfe-review-archive-dashboard>`, its
bundle served by the binary under `/mfe/` (`mfe_dir`), which the panel forwards as
`/review_archive/mfe/`; the panel's page at `/review_archive` mounts it under its top bar
and gives it the API base (`/api/review_archive`), its sign-in and its CSRF cookie. A 401
sends the top window to the sign-in with `return_to` = the page, which comes back signed
in. Design: Figma "review_archive / dashboard", on ev_lib's `uikit`. One holding
`members:act_as` gets tabs: their own dashboard, and one per member opened from
`GET /members`, acting as them through `X-Member`. Where it is is its URL — `/gmails/{id}`,
`/gmails/{id}/places/{target}`, `/telegram`, `/tokens` (the ledger), under `/members/{id}`
for a member's tab. Standalone (`--dev-member`), the binary serves the same page at `/`.

## Library

The crate is usable without the server, inside other systems, at the low level:

- `review_archive_core` — no I/O: domain types, DTOs, reconciliation, the Maps
  card parser and selectors, relative dates. Anyone with their own browser or
  their own HTML can parse and reconcile.
- `review_archive` — the engine: `ReviewSource` trait (public, so other
  platforms plug in), `maps` and `gbp` sources, a `Browser` handle that callers
  can own or share, `Store` (SQLite + blobs), and a facade
  `Archive::open(config)` with `capture_place(..)`, `scan_target(..)`,
    `add_target(..)`, `stats(..)`, `export(..)`. Features: `maps` (the browser, and the
  `maps` and `gbp` sources), `store`.
  A caller can also run `capture_place` with no store at all and get the reviews
  and AVIF bytes back in memory.
- `review_archive_server` — binary: CLI, scheduler, HTTP, webhooks. Thin over
  the facade; no logic that the library does not also expose.
- `review_archive_client` — typed async HTTP client over the same DTOs, for
  services that call a running archive instead of embedding it; builds for wasm too.
- `review_archive_web` — the dashboard, over the client.

## Repo conventions

Follow `gmaps_optimal_placement` / `aquafix`: edition 2024, nightly via
`v_flakes` (`github:valeratrades/v_flakes?ref=v1.6`), `eyre` + `color-eyre`, the
same `rustfmt.toml`, `v_flakes.github` generated workflows, container via
`v_flakes.container.implement` (mount `/data`, `healthPath = "/health"`,
`chromium` in the image) and `github.containerRelease = { registry = "ghcr.io/service-arb"; }`.
A `v*` tag is a release — do not tag.

## Tests

- Parser tests on saved Maps HTML fixtures (insta snapshots).
- Repository tests on a temp SQLite: new / changed / gone / reappeared.
- Scheduler: interval, jitter bounds, backoff.
- `gbp` client against a stub HTTP server (pagination, token refresh).
- Live scan of one real place — `#[ignore]`, run by hand.
