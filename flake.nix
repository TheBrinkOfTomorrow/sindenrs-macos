{
  description = "A Rust application built with Nix flakes using Crane";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

    systems.url = "github:nix-systems/default";

    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };

    crane.url = "github:ipetkov/crane";

    # nix-direnv for the development shell
    nix-direnv = {
      url = "github:nix-community/nix-direnv";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, systems, rust-overlay, crane, nix-direnv, ... }:
    let
      eachSystem = nixpkgs.lib.genAttrs (import systems);

      # The overlay window (winit + softbuffer) loads these at runtime with dlopen, so they
      # never appear as DT_NEEDED and cannot be found by rpath patching; the binary and the
      # dev shell both get them on LD_LIBRARY_PATH instead.
      # Helper to get pkgs for a system with rust-overlay applied
      pkgsFor = system: import nixpkgs {
        inherit system;
        overlays = [ rust-overlay.overlays.default ];
      };

      # Rust toolchain - pinned via rust-toolchain.toml (single source of truth).
      # rust-overlay (locked in flake.lock) supplies the exact build, so this stays
      # reproducible; rustup users outside Nix get the same version from the file.
      rustToolchainFor = system:
        let pkgs = pkgsFor system;
        in pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;

      # Create crane lib for each system
      cranelibFor = system:
        let
          pkgs = pkgsFor system;
          rustToolchain = rustToolchainFor system;
        in
        (crane.mkLib pkgs).overrideToolchain rustToolchain;

      # Common arguments for all crane builds
      commonArgsFor = system:
        let
          pkgs = pkgsFor system;
          craneLib = cranelibFor system;
        in
        {
          src = craneLib.cleanCargoSource ./.;
          strictDeps = true;

          buildInputs = [
            # Add additional build inputs here
          ] ++ pkgs.lib.optionals pkgs.stdenv.isDarwin [
            pkgs.libiconv
            pkgs.darwin.apple_sdk.frameworks.Security
          ];

          nativeBuildInputs = [
            # Add additional native build inputs here
          ];
        };

      # Build only dependencies (for caching)
      cargoArtifactsFor = system:
        let craneLib = cranelibFor system;
        in craneLib.buildDepsOnly (commonArgsFor system);

      # --- Windows cross-compilation (x86_64-pc-windows-msvc) ---
      #
      # Two entry points, same MSVC target:
      #   - Dev shell: `cargo xwin build --release --target x86_64-pc-windows-msvc`
      #     (cargo-xwin downloads the CRT/SDK to ~/.cache/cargo-xwin on first use)
      #   - Pure Nix:  `nix build .#windows`
      #     (the CRT/SDK comes from the fixed-output derivation below, since
      #     cargo-xwin's downloader can't run inside the Nix build sandbox)
      windowsTarget = "x86_64-pc-windows-msvc";

      # MSVC CRT + Windows SDK, splatted by xwin (the tool cargo-xwin wraps) as a
      # fixed-output derivation. Versions are pinned so the output stays stable;
      # if Microsoft retires these versions or repacks the files, bump the pins
      # and refresh `outputHash` (build with `outputHash = pkgs.lib.fakeHash` and
      # copy the hash from the mismatch error).
      xwinSdkFor = system:
        let pkgs = pkgsFor system;
        in pkgs.stdenvNoCC.mkDerivation {
          pname = "xwin-msvc-sdk";
          version = "crt-14.44.17.14-sdk-10.0.26100";

          nativeBuildInputs = [ pkgs.xwin pkgs.cacert ];

          dontUnpack = true;
          dontFixup = true;

          buildPhase = ''
            xwin \
              --accept-license \
              --cache-dir "$TMPDIR/xwin-cache" \
              --manifest-version 17 \
              --crt-version 14.44.17.14 \
              --sdk-version 10.0.26100 \
              --arch x86_64 \
              splat --copy --output "$out"
          '';

          outputHashMode = "recursive";
          outputHashAlgo = "sha256";
          outputHash = "sha256-UFQjsFVBwcF/9e9tVFoG0Z1JySxyTnFqoaRwr/tUWzA=";
        };

      # Crane args for the Windows build. Linking uses lld-link against the xwin
      # SDK; clang-cl/llvm-lib are wired up so crates with C dependencies (via
      # the `cc` crate) also work.
      windowsArgsFor = system:
        let
          pkgs = pkgsFor system;
          xwinSdk = xwinSdkFor system;
          commonArgs = commonArgsFor system;
        in
        commonArgs // {
          pnameSuffix = "-windows";

          nativeBuildInputs = commonArgs.nativeBuildInputs ++ [
            pkgs.llvmPackages.lld
          ];

          CARGO_BUILD_TARGET = windowsTarget;
          CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER = "lld-link";
          CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS = builtins.concatStringsSep " " [
            "-Lnative=${xwinSdk}/crt/lib/x86_64"
            "-Lnative=${xwinSdk}/sdk/lib/um/x86_64"
            "-Lnative=${xwinSdk}/sdk/lib/ucrt/x86_64"
          ];

          # For crates that compile C code for the Windows target (cc crate)
          CC_x86_64_pc_windows_msvc = "${pkgs.llvmPackages.clang-unwrapped}/bin/clang-cl";
          AR_x86_64_pc_windows_msvc = "${pkgs.llvmPackages.llvm}/bin/llvm-lib";
          CFLAGS_x86_64_pc_windows_msvc = builtins.concatStringsSep " " [
            "-imsvc${xwinSdk}/crt/include"
            "-imsvc${xwinSdk}/sdk/include/ucrt"
            "-imsvc${xwinSdk}/sdk/include/um"
            "-imsvc${xwinSdk}/sdk/include/shared"
          ];

          # Windows binaries can't run on the build host
          doCheck = false;
        };

      windowsCargoArtifactsFor = system:
        let craneLib = cranelibFor system;
        in craneLib.buildDepsOnly (windowsArgsFor system);

    in
    {
      # The main package output
      packages = eachSystem (system:
        let
          pkgs = pkgsFor system;
          craneLib = cranelibFor system;
          commonArgs = commonArgsFor system;
          cargoArtifacts = cargoArtifactsFor system;
        in
        {
          default = craneLib.buildPackage (commonArgs // {
            inherit cargoArtifacts;
            # Only run tests during the check phase, not during build
            doCheck = false;

          });

          sindenrs = self.packages.${system}.default;

          # Cross-compiled Windows binary: `nix build .#windows`
          # Output: ./result/bin/sindenrs.exe
          windows = craneLib.buildPackage ((windowsArgsFor system) // {
            cargoArtifacts = windowsCargoArtifactsFor system;
          });
        });

      # Checks run by `nix flake check`
      checks = eachSystem (system:
        let
          pkgs = pkgsFor system;
          craneLib = cranelibFor system;
          commonArgs = commonArgsFor system;
          cargoArtifacts = cargoArtifactsFor system;
        in
        {
          # Build the crate as part of checks
          build = self.packages.${system}.default;

          # Run clippy
          clippy = craneLib.cargoClippy (commonArgs // {
            inherit cargoArtifacts;
            cargoClippyExtraArgs = "--all-targets -- --deny warnings";
          });

          # Check formatting
          fmt = craneLib.cargoFmt {
            src = commonArgs.src;
          };

          # Run tests
          test = craneLib.cargoNextest (commonArgs // {
            inherit cargoArtifacts;
            partitions = 1;
            partitionType = "count";
          });

          # Run tests with coverage
          coverage = craneLib.cargoLlvmCov (commonArgs // {
            inherit cargoArtifacts;
          });
        });

      # Development shell
      devShells = eachSystem (system:
        let
          pkgs = pkgsFor system;
          rustToolchain = rustToolchainFor system;
        in
        {
          default = pkgs.mkShell {
            inputsFrom = [ self.packages.${system}.default ];

            nativeBuildInputs = [
              # Rust toolchain (includes rust-analyzer, rustfmt, clippy)
              rustToolchain

              # Fast test runner
              pkgs.cargo-nextest

              # Code coverage
              pkgs.cargo-llvm-cov

              # Watch mode for rapid development
              pkgs.bacon

              # Dependency management
              pkgs.cargo-edit

              # Security auditing
              pkgs.cargo-audit

              # Macro expansion (debugging)
              pkgs.cargo-expand

              # Windows cross-compilation:
              #   cargo xwin build --release --target x86_64-pc-windows-msvc
              pkgs.cargo-xwin

              # nix-direnv for this flake's shell
              nix-direnv.packages.${system}.default
            ];

            # Environment variables for development
            RUST_BACKTRACE = "1";
            RUST_LOG = "debug";
          };
        });


      # NixOS module: device access (udev rules), the package, and optionally the driver as
      # a user service. Import as `sindenrs.nixosModules.default` and set
      # `services.sindenrs.enable = true;`.
      nixosModules.default = { config, lib, pkgs, ... }:
        let
          cfg = config.services.sindenrs;
          tomlFormat = pkgs.formats.toml { };
          # The rules files live in udev/ so non-Nix users can copy them; the RUN line
          # needs absolute paths here.
          accessRules = builtins.replaceStrings
            [ "/bin/sh" "chgrp" "chmod" ]
            [ pkgs.runtimeShell "${pkgs.coreutils}/bin/chgrp" "${pkgs.coreutils}/bin/chmod" ]
            (builtins.readFile ./udev/70-sinden-lightgun.rules);
          inputRules = builtins.replaceStrings [ "# @HIDE@" ] [ (if cfg.hideFromDesktop then "" else "# ") ]
            (builtins.readFile ./udev/71-sinden-lightgun-input.rules);
          udevRules = pkgs.runCommand "sinden-lightgun-udev-rules" { } ''
            mkdir -p $out/lib/udev/rules.d
            cp ${pkgs.writeText "70-sinden-lightgun.rules" accessRules} $out/lib/udev/rules.d/70-sinden-lightgun.rules
            ${lib.optionalString cfg.tagAsGun ''
              cp ${pkgs.writeText "71-sinden-lightgun-input.rules" inputRules} $out/lib/udev/rules.d/71-sinden-lightgun-input.rules
            ''}
          '';
          configFile = tomlFormat.generate "sindenrs-config.toml" cfg.settings;
          configArgs = lib.optionals (cfg.settings != { }) [ "--config" "${configFile}" ];
          runCommand = lib.escapeShellArgs ([ "${cfg.package}/bin/sindenrs" ] ++ configArgs ++ [ "run" ] ++ cfg.session.extraArgs);
        in
        {
          options.services.sindenrs = {
            enable = lib.mkEnableOption "Sinden Lightgun support (udev rules, device access, the sindenrs package)";
            package = lib.mkOption {
              type = lib.types.package;
              default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
              defaultText = lib.literalExpression "sindenrs.packages.\${system}.default";
              description = "The sindenrs package to install.";
            };
            users = lib.mkOption {
              type = lib.types.listOf lib.types.str;
              default = [ ];
              example = [ "alice" ];
              description = ''
                Users added to the dialout, video and input groups. Whoever is logged in on
                the seat can already open the gun and camera (udev uaccess), but recovering a
                gun through its hub's sysfs attribute needs dialout, and a user that is not on
                a seat (autologin on a console, SSH) needs all three. List your user here.
              '';
            };
            tagAsGun = lib.mkOption {
              type = lib.types.bool;
              default = true;
              description = "Tag the gun's input devices ID_INPUT_GUN=1, which MAME's udev lightgun provider looks for.";
            };
            hideFromDesktop = lib.mkOption {
              type = lib.types.bool;
              default = false;
              description = ''
                Hide the gun's HID mouse and keyboard from libinput (and so from the desktop
                and X server), so the gun cannot move the desktop pointer or type. Games that
                read evdev directly (MAME with the udev provider) still see it. Turn this on
                for a cabinet; leave it off if you want the gun to work as a mouse.
              '';
            };
            settings = lib.mkOption {
              type = tomlFormat.type;
              default = { };
              example = lib.literalExpression ''
                {
                  display = { aspect = 1.3333; border_thickness = 3.0; overlay = false; };
                  gun.recoil.enabled = true;
                  guns."2146665221".name = "player1";
                }
              '';
              description = ''
                The driver's configuration, written to a file in the Nix store and passed to
                the session service with `--config`. Empty means the driver reads
                `~/.config/sindenrs/config.toml` instead. The file is read-only, which is
                fine: `sindenrs calibrate` saves its result into the gun, not the config.
              '';
            };
            session = {
              enable = lib.mkEnableOption "running `sindenrs run` as a user service in the graphical session";
              extraArgs = lib.mkOption {
                type = lib.types.listOf lib.types.str;
                default = [ ];
                example = [ "--profile" "crt" ];
                description = "Extra command-line arguments for `sindenrs run`.";
              };
              command = lib.mkOption {
                type = lib.types.str;
                readOnly = true;
                description = ''
                  The full `sindenrs run` command line the service runs, for sessions that
                  are not managed by systemd (a bare `startx` session): run it in the
                  background from the session script before starting the frontend.
                '';
              };
            };
          };

          config = lib.mkIf cfg.enable {
            environment.systemPackages = [ cfg.package ];
            services.udev.packages = [ udevRules ];
            users.users = lib.genAttrs cfg.users (_: { extraGroups = [ "dialout" "video" "input" ]; });
            services.sindenrs.session.command = runCommand;

            # A user service, because the border is a window on the user's display: a system
            # service has no DISPLAY / WAYLAND_DISPLAY and no seat ACLs. Sessions started by a
            # display manager or a Wayland compositor reach graphical-session.target and
            # import the display variables; a bare startx session does neither, so it runs
            # `services.sindenrs.session.command` itself instead.
            systemd.user.services.sindenrs = lib.mkIf cfg.session.enable {
              description = "Sinden Lightgun driver";
              wantedBy = [ "graphical-session.target" ];
              partOf = [ "graphical-session.target" ];
              after = [ "graphical-session.target" ];
              serviceConfig = {
                ExecStart = runCommand;
                # `run` exits when no gun is attached; keep trying in case one is plugged in.
                Restart = "always";
                RestartSec = 5;
              };
            };
          };
        };

      # Expose nix-direnv for .envrc to use
      lib = {
        inherit nix-direnv;
      };
    };
}
