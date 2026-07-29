{
  description = "squeezed - serve a raw PCM audio stream to Squeezelite/Squeezebox clients over SlimProto";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

    # Current crane doesn't expose a `nixpkgs` input, so we don't follow it.
    crane.url = "github:ipetkov/crane";

    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
      inputs.rust-analyzer-src.follows = "";
    };

    flake-utils.url = "github:numtide/flake-utils";

    advisory-db = {
      url = "github:rustsec/advisory-db";
      flake = false;
    };
  };

  outputs = { self, nixpkgs, crane, fenix, flake-utils, advisory-db, ... }:
    # nixpkgs-unstable has dropped x86_64-darwin (EOL Intel macOS), so we list
    # the currently-supported systems explicitly rather than eachDefaultSystem.
    flake-utils.lib.eachSystem [
      "x86_64-linux"
      "aarch64-linux"
      "aarch64-darwin"
    ] (system:
      let
        pkgs = import nixpkgs {
          inherit system;
        };

        inherit (pkgs) lib;

        craneLib = crane.mkLib pkgs;

        src = craneLib.cleanCargoSource ./.;

        # squeezed is pure Rust — no C dependencies, no openssl, no pkg-config.
        # The default stdenv toolchain (for linking) is all that's needed, so
        # there are no extra native/build inputs.
        commonArgs = {
          inherit src;

          pname = "squeezed";
          version = "0.1.0";
          strictDeps = true;

          # Single-package crate with one bin target — build just that.
          cargoExtraArgs = "--locked --bin squeezed";
        };

        craneLibLLvmTools = craneLib.overrideToolchain
          (fenix.packages.${system}.complete.withComponents [
            "cargo"
            "llvm-tools"
            "rustc"
          ]);

        # Cache the dependency graph separately from the crate source.
        cargoArtifacts = craneLib.buildDepsOnly commonArgs;

        squeezed = craneLib.buildPackage (commonArgs // {
          inherit cargoArtifacts;
          doCheck = false;

          meta = {
            description = "Serve a raw PCM audio stream to Squeezelite/Squeezebox clients over SlimProto";
            homepage = "https://github.com/tsirysndr/squeezed";
            license = lib.licenses.mit;
            mainProgram = "squeezed";
            platforms = lib.platforms.unix;
          };
        });

      in
      {
        checks = {
          inherit squeezed;

          squeezed-clippy = craneLib.cargoClippy (commonArgs // {
            inherit cargoArtifacts;
            cargoClippyExtraArgs = "--all-targets -- --deny warnings";
          });

          squeezed-doc = craneLib.cargoDoc (commonArgs // {
            inherit cargoArtifacts;
          });

          squeezed-fmt = craneLib.cargoFmt {
            inherit src;
          };

          squeezed-audit = craneLib.cargoAudit {
            inherit src advisory-db;
          };

          squeezed-nextest = craneLib.cargoNextest (commonArgs // {
            inherit cargoArtifacts;
            partitions = 1;
            partitionType = "count";
          });
        } // lib.optionalAttrs (system == "x86_64-linux") {
          squeezed-coverage = craneLib.cargoTarpaulin (commonArgs // {
            inherit cargoArtifacts;
          });
        };

        packages = {
          default = squeezed;
          squeezed = squeezed;

          squeezed-llvm-coverage = craneLibLLvmTools.cargoLlvmCov (commonArgs // {
            inherit cargoArtifacts;
          });
        };

        apps.default = flake-utils.lib.mkApp {
          drv = squeezed;
          name = "squeezed";
        };

        devShells.default = pkgs.mkShell {
          inputsFrom = builtins.attrValues self.checks.${system};

          nativeBuildInputs = with pkgs; [
            cargo
            rustc
            rustfmt
            clippy
            rust-analyzer
          ];

          shellHook = ''
            echo "🔊 squeezed dev shell — $(cargo --version)"
          '';
        };
      });
}
