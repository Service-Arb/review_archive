# Prod config, evaluated to TOML at build time and baked into the image
# (`flake.nix: prodConfig`). Secret-free — PANEL_ASSERTION_KEYS, GOOGLE_MAPS_KEY,
# the GBP_* OAuth triple, TELEGRAM_BOT_TOKEN,
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
  # members start at 0 and nothing renews: an admin assigns tokens by hand
  tokens.daily = 0;
}
