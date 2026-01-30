{
  description = "rustbb - Gobusybox-style multi-call binary builder for Rust";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
      rust-overlay,
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs {
          inherit system overlays;
        };

        # Use stable Rust with necessary components
        rustToolchain = pkgs.rust-bin.stable.latest.default.override {
          extensions = [
            "rust-src"
            "rust-analyzer"
          ];
        };

        # Common native build inputs for Rust crates with C dependencies
        nativeBuildInputs = with pkgs; [
          # Rust toolchain
          rustToolchain
          cargo

          # Build tools
          pkg-config
          cmake
          gnumake

          # Linker (using lld for faster linking)
          llvmPackages.bintools
          clang
        ];

        # Libraries commonly needed by Rust crates
        buildInputs =
          with pkgs;
          [
            # OpenSSL (for git2, reqwest, etc.)
            openssl
            openssl.dev

            # libgit2 (for git2 crate)
            libgit2

            # zlib (commonly needed)
            zlib

            # libssh2 (for git2 SSH support)
            libssh2

            # curl (for some networking crates)
            curl

            # Other common native deps
            libiconv
          ]
          ++ pkgs.lib.optionals pkgs.stdenv.isDarwin [
            # macOS-specific
            pkgs.darwin.apple_sdk.frameworks.Security
            pkgs.darwin.apple_sdk.frameworks.SystemConfiguration
          ];

      in
      {
        devShells.default = pkgs.mkShell {
          inherit nativeBuildInputs buildInputs;

          shellHook = ''
            # Set up environment for native crate compilation
            export PKG_CONFIG_PATH="${pkgs.openssl.dev}/lib/pkgconfig:${pkgs.libgit2}/lib/pkgconfig:${pkgs.zlib.dev}/lib/pkgconfig:$PKG_CONFIG_PATH"
            export OPENSSL_DIR="${pkgs.openssl.dev}"
            export OPENSSL_LIB_DIR="${pkgs.openssl.out}/lib"
            export OPENSSL_INCLUDE_DIR="${pkgs.openssl.dev}/include"
            export LIBGIT2_SYS_USE_PKG_CONFIG=1
            export LIBSSH2_SYS_USE_PKG_CONFIG=1

            # Use lld for faster linking
            export RUSTFLAGS="-C linker=clang -C link-arg=-fuse-ld=lld"

            echo "rustbb development shell"
            echo "  Rust: $(rustc --version)"
            echo "  OpenSSL: ${pkgs.openssl.version}"
            echo "  libgit2: ${pkgs.libgit2.version}"
          '';
        };

        packages.default = pkgs.rustPlatform.buildRustPackage {
          pname = "rustbb";
          version = "0.1.0";
          src = ./.;

          cargoLock = {
            lockFile = ./Cargo.lock;
          };

          inherit nativeBuildInputs buildInputs;

          # Skip tests for now during package build
          doCheck = false;
        };
      }
    );
}
