{
  description = "Subspace Nix Integration";

  inputs = {
    # Nix utils libraries
    utils.url = "github:numtide/flake-utils";

    # Rust toolchain parsing and build tools
    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };

    crane = {
      #url = "github:ipetkov/crane";
      url = "github:FlorianFranzen/crane";
      inputs.nixpkgs.follows = "nixpkgs";
      inputs.flake-utils.follows = "utils";
    };

    naersk = {
      url = "github:nix-community/naersk";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self,  utils, nixpkgs, fenix, crane, naersk, ... } @ inputs:
    utils.lib.eachSystem [ "x86_64-linux" "aarch64-linux" ] (system:
      let
        pkgs = nixpkgs.legacyPackages.${system};

        version = if self ? rev
                  then (builtins.substring 0 7 self.rev)
                  else self.lastModifiedDate;

        # Import rust version from
        rustToolchain = fenix.packages.${system}.fromToolchainFile {
      	  file = ./rust-toolchain.toml;
          sha256 = "gGuwXw8QUSviadgK6KPT3biJkaUJkwwPu6tkx+RFZCk=";
        };

        rustToolchainAttrs = { cargo = rustToolchain; rustc = rustToolchain; };

        rustVersion = pkgs.lib.getVersion rustToolchain;

        # Initialize nix builders
        rustPlatform = pkgs.makeRustPlatform rustToolchainAttrs;

        craneLib = crane.lib.${system}.overrideToolchain rustToolchain;

        naerskLib = naersk.lib.${system}.override rustToolchainAttrs;

        # Define shared package attrs
        commonPkgArgs = {
          pname = "subspace";

          inherit version;
          SUBSTRATE_CLI_GIT_COMMIT_HASH = version;

          src = self;

          # Additional tooling needed
          nativeBuildInputs = [ pkgs.clang ];

          LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
          PROTOC = "${pkgs.protobuf}/bin/protoc";

          # Subspace uses special profile for production builds
          CARGO_PROFILE = "production";

          # FIXME: It should be possible to not build the runtime
          #SKIP_WASM_BUILD = 1;

          # Currently fails (and needs WASM runtimes)
          doCheck = false;
        };

        # Build subspace with each nix integration
        mkNixpkgsPkg = rustPlatform.buildRustPackage (commonPkgArgs // {

          cargoLock.lockFile = ./Cargo.lock;

          SKIP_WASM_BUILD = 1;
        });

        mkNaerskPkg = bin: naerskLib.buildPackage (commonPkgArgs // {
          pname = "subspace-${bin}";

          # Needed for "odd" deps
          gitSubmodules = true;

          # Pass additional build options
          release = false;
          cargoBuildOptions = def: def ++ [ "--bin" "subspace-${bin}" ];

          # Works when only building bins with naersk
          SKIP_WASM_BUILD = 1;
        });

        mkCranePkg = bin: craneLib.buildPackage (commonPkgArgs // {
          # Filter source to reduce rebuilds
          #src = craneLib.cleanCargoSource self;

          # Support wasm builds with LLVM
          CC_wasm32_unknown_unknown = "${pkgs.clang}/bin/cc"; 
          CFLAGS_wasm32_unknown_unknown = "--target=wasm32";
        } // (pkgs.lib.optionalAttrs (bin != null) {
          pname = "subspace-${bin}";
          cargoExtraArgs = "--package subspace-${bin}";

        }));

        # Add development shell
        shell = pkgs.mkShell {
          name = "subspace-${version}-devshell-${rustVersion}";

          nativeBuildInputs = with pkgs; [
            rustToolchain
            cmake
            clang
          ];

          buildInputs = with pkgs; [
            openssl #?
            pkgconfig #?
            protobuf
            llvmPackages.libclang
          ];

          # Support wasm builds with LLVM
          CC_wasm32_unknown_unknown = "${pkgs.clang}/bin/cc"; 
          CFLAGS_wasm32_unknown_unknown = "--target=wasm32";

          # Provide any other dependencies
          PROTOC          = "${pkgs.protobuf}/bin/protoc";
          LIBCLANG_PATH   = "${pkgs.llvmPackages.libclang}/lib";
          ROCKSDB_LIB_DIR = "${pkgs.rocksdb}/lib";
        };

      in {
        packages = {
          defaultViaCrane = mkCranePkg null;

          nodeViaCrane = mkCranePkg "node";
          farmerViaCrane = mkCranePkg "farmer";

          nodeViaNaersk = mkNaerskPkg "node";
          farmerViaNaersk = mkNaerskPkg "farmer";

          defaultViaNixpkgs = mkNixpkgsPkg;
        };

        devShells = {
          default = shell;
        };

        checks = self.packages.${system};
      }
    );
  }
