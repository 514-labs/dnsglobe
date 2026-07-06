{
  description = "Global DNS propagation checker TUI — watch a DNS record propagate across public resolvers worldwide, on a world map in your terminal";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils, ... }@inputs:
    {
      overlays.default = final: prev: {
        dnsglobe = self.packages.${final.system}.default;
      };
    } // flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
        dnsglobe = pkgs.rustPlatform.buildRustPackage {
          pname = "dnsglobe";
          version = "0.3.0";
          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;
          nativeBuildInputs = [ pkgs.pkg-config ];
          buildInputs = pkgs.lib.optionals pkgs.stdenv.isDarwin [
            pkgs.libiconv
          ];
          meta = {
            description = "Global DNS propagation checker TUI — watch a DNS record propagate across public resolvers worldwide, on a world map in your terminal";
            homepage = "https://github.com/514-labs/dnsglobe";
            license = pkgs.lib.licenses.mit;
            mainProgram = "dnsglobe";
          };
        };
      in
      {
        packages = {
          default = dnsglobe;
          dnsglobe = dnsglobe;
          source = dnsglobe;
        };

        apps = {
          default = {
            type = "app";
            program = "${dnsglobe}/bin/dnsglobe";
          };
        };

        checks = {
          build = dnsglobe;
        };
      }
    );
}
