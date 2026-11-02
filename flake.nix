{
  description = "codjd - the codj DJ/player daemon";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    utils.url = "github:numtide/flake-utils";
    # upstream rustc with its own LLVM: nixpkgs' rustc (system LLVM) miscompiles rten-gemm's
    # AVX-512 intrinsics ("intrinsic signature mismatch for llvm.x86.avx512.vpdpbusd.512")
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      utils,
      rust-overlay,
    }:
    let
      rustPlatformFor =
        pkgs:
        let
          toolchain = (rust-overlay.lib.mkRustBin { } pkgs).stable.latest.minimal;
        in
        pkgs.makeRustPlatform {
          cargo = toolchain;
          rustc = toolchain;
        };
      codjdFor =
        pkgs:
        (rustPlatformFor pkgs).buildRustPackage {
          pname = "codjd";
          version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).package.version;
          src = pkgs.lib.cleanSource ./.;
          cargoLock.lockFile = ./Cargo.lock;
          nativeBuildInputs = [
            pkgs.pkg-config
            (rustPlatformFor pkgs).bindgenHook # cpal's pipewire backend (libspa-sys)
          ];
          buildInputs = [
            pkgs.alsa-lib
            pkgs.pipewire
          ];
          # tests want a session bus and an audio device
          doCheck = false;
          meta = {
            description = "codjd - the codj DJ/player daemon";
            homepage = "https://github.com/hitchho/codjd";
            license = pkgs.lib.licenses.gpl3Plus;
            mainProgram = "codjd";
            platforms = pkgs.lib.platforms.linux;
          };
        };
    in
    {
      lib = { inherit rustPlatformFor; };
      overlays.default = final: _prev: { codjd = codjdFor final; };
    }
    // utils.lib.eachSystem [ "x86_64-linux" "aarch64-linux" ] (
      system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
        codjd = codjdFor pkgs;
      in
      {
        packages = {
          inherit codjd;
          default = codjd;
        };
        apps.default = utils.lib.mkApp { drv = codjd; };
        devShells.default = pkgs.mkShell {
          inputsFrom = [ codjd ];
          packages = [
            pkgs.cargo
            pkgs.rustc
            pkgs.rustfmt
            pkgs.clippy
            pkgs.rust-analyzer
          ];
        };
      }
    );
}
