{
  nixConfig = {
    extra-substituters = [ "https://valeratrades.cachix.org" ];
    extra-trusted-public-keys = [ "valeratrades.cachix.org-1:gXVwhzO5YB+BaiEJYT48qZgzdaErGQew6xtZcz4Fo1Q=" ];
  };

  inputs = {
    v_flakes.url = "github:valeratrades/v_flakes?ref=v1.6";
  };

  outputs = { self, v_flakes }:
    let
      inherit (v_flakes) flake-utils pre-commit-hooks;
      manifest = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).package;
      pname = manifest.name;
    in
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import v_flakes.default_nixpkgs { inherit system; };
        lib = pkgs.lib;
        rust = v_flakes.rs.default_nightly system;
        # mold is Linux-only — the adapter throws at *eval* time on Darwin, which
        # would break every target on a mac, not just the ones that link.
        stdenv = if pkgs.stdenv.isDarwin then pkgs.stdenv else pkgs.stdenvAdapters.useMoldLinker pkgs.stdenv;

        # Single source for the container's exposed port and the prod config's
        # bind. The binary's own default (`config::DEFAULT_BIND`) is the same
        # port on 127.0.0.1.
        port = 59110;

        # nixpkgs builds Chromium for Linux only. On a mac the scanner is pointed
        # at a local Chrome through `browser.executable` in the config instead.
        chromium = lib.optional pkgs.stdenv.isLinux pkgs.chromium;

        pre-commit-check = pre-commit-hooks.lib.${system}.run (v_flakes.files.preCommit { inherit pkgs; stripClaudeSignature = true; });
        rs = v_flakes.rs {
          inherit pkgs rust;
          build.workspace."./" = [ "git_version" "log_directives" ];
        };
        github = v_flakes.github {
          inherit pkgs pname rs;
          enable = true;
          lastSupportedVersion = "nightly-2026-09-03";
          containerRelease = { registry = "ghcr.io/service-arb"; };
          jobs.default = true;
          lfs = false;
          gitignore.extra = ''
            # the data dir of a local `serve` / `scan`: the database, the PNGs and the browser profile
            /data/
          '';
        };
        readme = v_flakes.readme-fw {
          inherit pkgs pname;
          defaults = true;
          lastSupportedVersion = "nightly-1.100";
          rootDir = ./.;
          badges = [ "msrv" "loc" "ci" ];
        };
        combined = v_flakes.utils.combine { inherit rust; modules = [ rs github readme ]; };

        rustPlatform = pkgs.makeRustPlatform { rustc = rust; cargo = rust; inherit stdenv; };
        # `.cargo` holds dev-only accelerators (sccache rustc-wrapper, cranelift,
        # mold) the hermetic sandbox lacks — drop it so the pure build uses nix's
        # own toolchain instead of failing on a missing `sccache` on PATH.
        pureSrc = lib.cleanSourceWith {
          src = lib.cleanSource ./.;
          filter = path: _type: baseNameOf path != ".cargo";
        };

        bin = rustPlatform.buildRustPackage {
          inherit pname;
          version = manifest.version;
          src = pureSrc;
          cargoLock.lockFile = ./Cargo.lock;
          nativeBuildInputs = with pkgs; [ pkg-config ];
          # the parser tests are `cargo test`'s job in the devShell and CI; the
          # live one needs a browser and the network, which the sandbox has neither of
          doCheck = false;
          auditable = false; # cargo-auditable doesn't support edition 2024
        };

        # A headless Chromium renders review text with whatever fonts fontconfig
        # finds; an image without any turns every screenshot into tofu boxes.
        fontsConf = pkgs.makeFontsConf {
          fontDirectories = with pkgs; [ dejavu_fonts liberation_ttf noto-fonts-color-emoji ];
        };

        # Secret-free prod config baked into the image; the container has no `nix`,
        # so it is evaluated to TOML here. Secrets arrive from the k8s Secret.
        prodConfig = (pkgs.formats.toml { }).generate "config.toml" (import ./deploy/config.nix {
          inherit port;
          chromium = "${pkgs.chromium}/bin/chromium";
        });

        containerStd = v_flakes.container.implement {
          inherit pkgs pname;
          containers."" = {
            inherit port;
            mounts = [ "/data" ];
            healthPath = "/health";
            # an archive that misses a scan catches up on the next one
            criticality = "normal";
            entrypoint = [ "${bin}/bin/${pname}" "--config" "${prodConfig}" "serve" ];
            workingDir = "/data";
            contents = [ pkgs.chromium ];
            imageEnv = [
              "HOME=/data"
              # the image has no /tmp, and Chromium's shared memory goes to TMPDIR
              # (--disable-dev-shm-usage); the binary creates it on start, as the
              # volume is empty on first boot
              "TMPDIR=/data/tmp"
              "FONTCONFIG_FILE=${fontsConf}"
            ];
          };
        };

        help = pkgs.writeShellApplication {
          name = "help";
          text = ''
            cat <<'EOF'
            nix develop                       toolchain, sqlite, cargo-insta (and chromium on Linux)
            nix build                         the review_archive binary
            nix build .#${pname}-container    OCI image with chromium (Linux only)
            nix run .#help                    this
            cargo test                        parser snapshots, repository, scheduler, gbp stub
            cargo test -- --ignored live      one real scan; needs a browser and the network
            the CLI itself: review_archive --help
            EOF
          '';
        };
      in
      {
        apps = {
          help = { type = "app"; program = lib.getExe help; };
        };

        packages = {
          default = bin;
          inherit bin;
        } // lib.optionalAttrs pkgs.stdenv.isLinux containerStd.packages;

        # Linux-only: the image carries Chromium, which nixpkgs does not build for Darwin.
        containers = lib.optionalAttrs pkgs.stdenv.isLinux containerStd.containers;

        devShells.default =
          with pkgs;
          mkShell {
            inherit stdenv;
            shellHook =
              pre-commit-check.shellHook
              + combined.shellHook
              + ''
                cp -f ${(v_flakes.files.treefmt) { inherit pkgs; }} ./.treefmt.toml
                cp -f ${(v_flakes.files.gitattributes) { inherit pkgs; lfs = false; }} ./.gitattributes
              '';

            packages = [
              mold
              pkg-config
              rust
              sqlite # inspecting the archive
              cargo-insta
            ] ++ chromium ++ pre-commit-check.enabledPackages ++ combined.enabledPackages;

            env.RUST_BACKTRACE = 1;
            env.RUST_LIB_BACKTRACE = 0;
          };
      }
    );
}
