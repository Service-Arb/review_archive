# Prod config, evaluated to TOML at build time and baked into the image
# (`flake.nix: prodConfig`). Secret-free — REVIEW_ARCHIVE_TOKEN, GOOGLE_MAPS_KEY
# and the GBP_* OAuth triple arrive from the container environment.
#
# Passed as `--config` explicitly: without it the binary boots on dev defaults,
# a 127.0.0.1 bind that fails the readiness probe and a data dir that is not the
# mounted volume.
{ port, chromium }:
{
  data_dir = "/data";
  bind = "0.0.0.0:${toString port}";
  browser = {
    executable = chromium;
    # The image runs as `nobody` without the user namespaces Chromium's sandbox
    # needs; the container boundary is the sandbox here.
    no_sandbox = true;
  };
}
