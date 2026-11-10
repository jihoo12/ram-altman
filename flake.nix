{
  description = "Track RAM usage and, at most once a day, tell Sam Altman when this PC has RAM to spare";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      # The program reads /proc/meminfo, so it only runs on Linux.
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];

      forAllSystems =
        f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});

      mkRamAltman =
        pkgs:
        pkgs.rustPlatform.buildRustPackage {
          pname = "ram-altman";
          version = "0.1.0";

          src = ./.;

          cargoLock.lockFile = ./Cargo.lock;

          meta = with pkgs.lib; {
            description = "Track RAM usage and tell Sam Altman when this PC has RAM to spare";
            homepage = "https://ram-altman.lol";
            license = licenses.unlicense;
            mainProgram = "ram-altman";
            platforms = platforms.linux;
          };
        };
    in
    {
      packages = forAllSystems (
        pkgs:
        let
          ram-altman = mkRamAltman pkgs;
        in
        {
          inherit ram-altman;
          default = ram-altman;
        }
      );

      apps = forAllSystems (pkgs: {
        default = {
          type = "app";
          program = "${mkRamAltman pkgs}/bin/ram-altman";
          meta.description = "Track RAM usage and tell Sam Altman when this PC has RAM to spare";
        };
        ram-altman = {
          type = "app";
          program = "${mkRamAltman pkgs}/bin/ram-altman";
          meta.description = "Track RAM usage and tell Sam Altman when this PC has RAM to spare";
        };
      });

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          packages = with pkgs; [
            cargo
            rustc
            clippy
            rustfmt
            rust-analyzer
          ];

          env.RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
        };
      });

      checks = forAllSystems (pkgs: {
        # Building the package also runs `cargo test` against simulated memory
        # and a fake sendmail, so the checks need no inbox and no network.
        ram-altman = mkRamAltman pkgs;
      });
    };
}
