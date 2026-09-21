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
      graphicsLibs = pkgs: with pkgs; [
        wayland
        libxkbcommon
        libx11
        libxcursor
        libxrandr
        libxi
      ];

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
          ] ++ pkgs.lib.optionals pkgs.stdenv.isLinux (graphicsLibs pkgs)
          ++ pkgs.lib.optionals pkgs.stdenv.isDarwin [
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

            nativeBuildInputs = commonArgs.nativeBuildInputs
              ++ pkgs.lib.optionals pkgs.stdenv.isLinux [ pkgs.makeWrapper ];

            postInstall = pkgs.lib.optionalString pkgs.stdenv.isLinux ''
              wrapProgram $out/bin/sindenrs \
                --prefix LD_LIBRARY_PATH : ${pkgs.lib.makeLibraryPath (graphicsLibs pkgs)}
            '';
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

            # winit/softbuffer dlopen these, so `cargo run` needs them on the library path.
            LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath (graphicsLibs pkgs);
          };
        });


      # NixOS module: udev rules so the gun, its camera and its internal hub are usable
      # without root, ModemManager kept away from the gun's serial port, and an optional
      # system service. Import as `sindenrs.nixosModules.default` and set
      # `services.sindenrs.enable = true;`.
      nixosModules.default = { config, lib, pkgs, ... }:
        let
          cfg = config.services.sindenrs;
          udevRules = pkgs.writeTextFile {
            name = "sinden-lightgun-udev-rules";
            destination = "/lib/udev/rules.d/70-sinden-lightgun.rules";
            # Numbered below 73-seat-late.rules so TAG+="uaccess" takes effect (the logged-in
            # seat user gets an ACL on each node); the GROUP fallbacks cover headless use.
            text = ''
              # Sinden Lightgun: ATmega32U4 with CDC-ACM serial + HID. All firmware variants.
              SUBSYSTEM=="tty", ATTRS{idVendor}=="16c0", ATTRS{idProduct}=="0f01|0f02|0f38|0f39", MODE="0660", GROUP="dialout", TAG+="uaccess", ENV{ID_MM_DEVICE_IGNORE}="1"
              SUBSYSTEM=="usb", ATTR{idVendor}=="16c0", ATTR{idProduct}=="0f01|0f02|0f38|0f39", ENV{ID_MM_DEVICE_IGNORE}="1"
              # The gun's Caterina bootloader (Arduino Leonardo), used for resets and firmware updates.
              SUBSYSTEM=="tty", ATTRS{idVendor}=="2341", ATTRS{idProduct}=="0036", MODE="0660", GROUP="dialout", TAG+="uaccess", ENV{ID_MM_DEVICE_IGNORE}="1"
              SUBSYSTEM=="usb", ATTR{idVendor}=="2341", ATTR{idProduct}=="0036", ENV{ID_MM_DEVICE_IGNORE}="1"
              # Sinden camera boards (video capture + metadata nodes).
              SUBSYSTEM=="video4linux", ATTRS{idVendor}=="05a3|32e4|16d0", ATTRS{idProduct}=="9210|0109", MODE="0660", GROUP="video", TAG+="uaccess"
              # The hub inside the gun (Microchip USB2512, manufacturer string "KSB"). Access to its
              # ports' sysfs `disable` attribute (Linux >= 6.0) lets the driver power-cycle a gun
              # whose firmware has stopped answering; the usbfs node is the fallback for old kernels.
              SUBSYSTEM=="usb", ATTR{idVendor}=="0424", ATTR{idProduct}=="2512", ATTR{manufacturer}=="KSB", ATTR{bDeviceClass}=="09", MODE="0660", GROUP="dialout", TAG+="uaccess"
              SUBSYSTEM=="usb", DRIVER=="hub", ATTRS{idVendor}=="0424", ATTRS{idProduct}=="2512", ATTRS{manufacturer}=="KSB", RUN+="${pkgs.runtimeShell} -c '${pkgs.coreutils}/bin/chgrp -f dialout $sys$devpath/*-port*/disable; ${pkgs.coreutils}/bin/chmod -f 660 $sys$devpath/*-port*/disable'"
            '';
          };
        in
        {
          options.services.sindenrs = {
            enable = lib.mkEnableOption "Sinden Lightgun support (udev rules, device access)";
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
                Users added to the dialout and video groups. Seat users already get the
                device nodes via uaccess, but power-cycling a wedged gun goes through a sysfs
                attribute that only the dialout group can write, so list your desktop user
                here too if you want `sindenrs` to recover the gun without root.
              '';
            };
            daemon = {
              enable = lib.mkEnableOption "the sindenrs system service (not yet functional; the tracker is in progress)";
              extraArgs = lib.mkOption {
                type = lib.types.listOf lib.types.str;
                default = [ ];
                description = "Extra command-line arguments for the service.";
              };
            };
          };

          config = lib.mkIf cfg.enable {
            environment.systemPackages = [ cfg.package ];
            services.udev.packages = [ udevRules ];
            users.users = lib.genAttrs cfg.users (_: { extraGroups = [ "dialout" "video" ]; });

            systemd.services.sindenrs = lib.mkIf cfg.daemon.enable {
              description = "Sinden Lightgun driver";
              wantedBy = [ "multi-user.target" ];
              after = [ "systemd-udev-settle.service" ];
              serviceConfig = {
                ExecStart = "${cfg.package}/bin/sindenrs run ${lib.escapeShellArgs cfg.daemon.extraArgs}";
                Restart = "on-failure";
                RestartSec = 2;
                DynamicUser = true;
                SupplementaryGroups = [ "dialout" "video" ];
                DeviceAllow = [ "char-ttyACM rw" "char-video4linux rw" "char-usb_device rw" ];
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
