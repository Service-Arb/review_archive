# Prod config, evaluated to TOML at build time and baked into the image
# (`flake.nix: prodConfig`). Secret-free — REVIEW_ARCHIVE_TOKEN, GOOGLE_MAPS_KEY,
# the GBP_* OAuth triple, SSO_PUBLIC_KEY + SSO_REFRESH_URL, TELEGRAM_BOT_TOKEN,
# SENTRY_DSN, ALERT_WEBHOOK_{ERROR,WARN} and APP_ENV arrive
# from the container environment (`review_archive --print-required-vars` lists the required ones).
#
# Passed as `--config` explicitly: without it the binary boots on dev defaults,
# a 127.0.0.1 bind that fails the readiness probe and a data dir that is not the
# mounted volume.
{ port, chromium, mfe }:
{
  data_dir = "/data";
  # the dashboard bundle, served under /mfe/, its page at /
  mfe_dir = mfe;
  bind = "0.0.0.0:${toString port}";
  browser.executable = chromium;
}
