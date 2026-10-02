{
  nixConfig = {
    extra-substituters = [ "https://valeratrades.cachix.org" ];
    extra-trusted-public-keys = [ "valeratrades.cachix.org-1:gXVwhzO5YB+BaiEJYT48qZgzdaErGQew6xtZcz4Fo1Q=" ];
  };

  inputs = {
    v_flakes.url = "github:valeratrades/v_flakes?ref=v1.6";
    browser_manipulation = {
      url = "github:valeratrades/browser_manipulation?ref=v0.2.1";
      inputs.v_flakes.follows = "v_flakes";
    };
  };

  outputs = { self, v_flakes, browser_manipulation }:
    let
      inherit (v_flakes) flake-utils pre-commit-hooks;
      manifest = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).workspace.package;
      # The binary (crates/review_archive_server, `[[bin]] name`), the image and the
      # release all go by this name; the workspace root has no package of its own.
      pname = "review_archive";
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

        # the Playwright driver browser_manipulation speaks to
        playwrightEnv = {
          PLAYWRIGHT_CLI_JS = "${browser_manipulation.packages.${system}.patchright}/package/cli.js";
          PLAYWRIGHT_NODE_EXE = "${pkgs.nodejs}/bin/node";
          PLAYWRIGHT_SKIP_DRIVER_DOWNLOAD = "1";
        };

        pre-commit-check = pre-commit-hooks.lib.${system}.run (v_flakes.files.preCommit { inherit pkgs; stripClaudeSignature = true; });
        rs = v_flakes.rs {
          inherit pkgs rust;
          build.workspace = {
            "./crates/review_archive" = [ "git_version" ];
            "./crates/review_archive_server" = [ "git_version" "log_directives" ];
          };
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

        build_rust = v_flakes.rs.build_nightly system;
        rustPlatform = pkgs.makeRustPlatform { rustc = build_rust; cargo = build_rust; inherit stdenv; };
        # `.cargo` holds dev-only accelerators (sccache rustc-wrapper, cranelift,
        # mold) the hermetic sandbox lacks — drop it so the pure build uses nix's
        # own toolchain instead of failing on a missing `sccache` on PATH.
        pureSrc = lib.cleanSourceWith {
          src = lib.cleanSource ./.;
          filter = path: _type: baseNameOf path != ".cargo";
        };

        cargoLock = {
          lockFile = ./Cargo.lock;
          outputHashes."browser_manipulation-0.2.1" = "sha256-3Je1LiU6dYg0+Vpw2N+rHN/K47oKMfEf12JAncHc5eM=";
          outputHashes."ev_lib_classes-0.11.0" = "sha256-rwSUyYzP8pWGn7BMBOwsF2XcAB8Y/1POZC0OBkyvI2I=";
        };
        bin = rustPlatform.buildRustPackage {
          inherit pname;
          version = manifest.version;
          src = pureSrc;
          inherit cargoLock;
          cargoBuildFlags = [ "-p" "review_archive_server" ];
          nativeBuildInputs = with pkgs; [ pkg-config ];
          # ev_lib's `sentry` turns on reqwest's native-tls, which is OpenSSL on Linux
          # (Security.framework on Darwin, which needs nothing here).
          buildInputs = lib.optionals pkgs.stdenv.isLinux [ pkgs.openssl ];
          # the parser tests are `cargo test`'s job in the devShell and CI; the
          # live one needs a browser and the network, which the sandbox has neither of
          doCheck = false;
          auditable = false; # cargo-auditable doesn't support edition 2024
        };

        # Pinned to the `wasm-bindgen` crate (`=0.2.129`, review_archive_web): a skew
        # between the two is a hard error at bindgen time.
        wasm-bindgen-cli =
          let
            src = pkgs.fetchCrate {
              pname = "wasm-bindgen-cli";
              version = "0.2.129";
              hash = "sha256-pcecKQd7E8Opw6bkFoE569epUi7gh5qpQF1e5PJY6V8=";
            };
          in
          pkgs.buildWasmBindgenCli {
            inherit src;
            cargoDeps = pkgs.rustPlatform.fetchCargoVendor {
              inherit src;
              inherit (src) pname version;
              hash = "sha256-vmUrWVU7kPJJxO5qIVeAkwQyWDELO1Z4Z5gitz2kco8=";
            };
          };
        wasmFlags = ''--cfg=web_sys_unstable_apis --cfg=getrandom_backend="wasm_js"'';

        # The dashboard bundle the binary serves under /mfe/ (`mfe_dir`). Built with the
        # dev toolchain, which carries the wasm32 target.
        mfe = (pkgs.makeRustPlatform { rustc = rust; cargo = rust; inherit stdenv; }).buildRustPackage {
          pname = "${pname}-mfe";
          version = manifest.version;
          src = pureSrc;
          inherit cargoLock;
          nativeBuildInputs = [ wasm-bindgen-cli pkgs.tailwindcss_4 ];
          buildPhase = ''
            runHook preBuild
            RUSTFLAGS='${wasmFlags}' cargo build -p review_archive_web --target wasm32-unknown-unknown --release --offline
            runHook postBuild
          '';
          installPhase = ''
            runHook preInstall
            bash crates/review_archive_web/package.sh target/wasm32-unknown-unknown/release/review_archive_web.wasm "$out"
            runHook postInstall
          '';
          doCheck = false;
          auditable = false;
        };

        # `nix run .#dev-mfe [-- <member email>]`: the dashboard built (debug) into tmp/mfe-dev and
        # served by a local `serve` at `/`, on the repo's `data/`, signed in as that member
        # (`--dev-member`: no valeratrades.com needed). Scans run in a Chromium window: a
        # headless one gets Maps' limited view (#9).
        devMfe = pkgs.writeShellApplication {
          name = "dev-mfe";
          runtimeInputs = [ pkgs.git ];
          text = ''
            member="''${1:-test@valeratrades.com}"
            repo="$(git rev-parse --show-toplevel)"
            cd "$repo"
            out="$repo/tmp/mfe-dev"
            RUSTFLAGS='${wasmFlags}' nix develop "$repo" --command bash -euc "
              cargo build -p review_archive_web --target wasm32-unknown-unknown
              bash crates/review_archive_web/package.sh target/wasm32-unknown-unknown/debug/review_archive_web.wasm '$out'
            "
            cat >"$out/config.toml" <<EOF
            data_dir = "$repo/data"
            mfe_dir = "$out"
            [browser]
            executable = "${pkgs.chromium}/bin/chromium"
            headful = true
            EOF
            # serve wants an operator token; the dashboard never uses it
            REVIEW_ARCHIVE_TOKEN="$(head -c 24 /dev/urandom | base64)"
            export REVIEW_ARCHIVE_TOKEN
            exec nix develop "$repo" --command cargo r -p review_archive_server -- --config "$out/config.toml" serve --dev-member "$member"
          '';
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
          mfe = "${mfe}";
        });

        containerStd = v_flakes.container.implement {
          inherit pkgs pname;
          containers."" = {
            inherit port;
            mounts = [ "/data" ];
            sqlite = [ "/data/review_archive.db" ];
            healthPath = "/health";
            # an archive that misses a scan catches up on the next one
            criticality = "normal";
            entrypoint = [ "${bin}/bin/${pname}" "--config" "${prodConfig}" "serve" ];
            workingDir = "/data";
            contents = [ pkgs.chromium pkgs.nodejs ];
            imageEnv = lib.mapAttrsToList (k: v: "${k}=${v}") playwrightEnv ++ [
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
            nix build .#mfe                   the dashboard bundle (served under /mfe/)
            nix run .#dev-mfe [-- <email>]    the dashboard, built and served locally at / as that member
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
          dev-mfe = { type = "app"; program = lib.getExe devMfe; };
        };

        packages = {
          default = bin;
          inherit bin mfe;
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
              openssl # sentry's native-tls
              rust
              sqlite # inspecting the archive
              cargo-insta
              wasm-bindgen-cli # the dashboard; pinned with the crate
              tailwindcss_4
            ] ++ chromium ++ pre-commit-check.enabledPackages ++ combined.enabledPackages;

            env = {
              RUST_BACKTRACE = 1;
              RUST_LIB_BACKTRACE = 0;
            } // playwrightEnv // lib.optionalAttrs pkgs.stdenv.isLinux {
              REVIEW_ARCHIVE_CHROME = "${pkgs.chromium}/bin/chromium";
            };
          };
      }
    );
}
