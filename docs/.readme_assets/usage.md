```sh
# Everything lives under one data dir: `review_archive.db`, `blobs/` with the PNGs, and the
# browser profile that remembers the consent answer. Set it, the bind address and the defaults in
# a TOML file passed as `--config`; secrets only ever come from the environment.
review_archive --config config.toml target add 'https://www.google.com/maps/place/?q=place_id:ChIJ...' --label cafe --lang fr

# A Maps URL without a place id is resolved through the Places API (needs GOOGLE_MAPS_KEY).
review_archive target add 'https://www.google.com/maps/place/Le+Procope/@48.853,2.338,17z' --interval 12h

# A profile we manage: the review list comes from the Business Profile API
# (GBP_CLIENT_ID, GBP_CLIENT_SECRET, GBP_REFRESH_TOKEN), the screenshots from Maps.
review_archive target add ChIJ... --gbp 1234567890/9876543210

review_archive target list
review_archive scan 1          # one pass now, prints a summary
review_archive scan --all

# Scheduler + HTTP on 127.0.0.1:59110. The API wants `Authorization: Bearer
# $REVIEW_ARCHIVE_TOKEN`, or a browser's valeratrades.com sign-in (docs/SPEC.md, Auth).
review_archive serve

# PNGs + manifest.json, to a directory or a .zip
review_archive export --target 1 --since 2026-01-01 --out cafe.zip
```

```toml
# config.toml — every key is optional, but a scan needs `browser.executable`
data_dir = "./data"
bind = "127.0.0.1:59110"

[browser]
executable = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"

[defaults]
lang = "en"
interval = "1d"
max_reviews_per_scan = 200
max_reviews_initial = 2000
```
