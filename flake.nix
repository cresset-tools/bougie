{
  description = "bougie dev environment";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
      in {
        # `nix develop` drops you into a shell with the tools the
        # fixture-generation scripts need. The Rust toolchain itself is
        # not provided here — rustup / your host toolchain own that;
        # this shell only bridges the gap for contributors who don't
        # have PHP (or a specific composer version) on the host.
        devShells.default = pkgs.mkShell {
          packages = [
            pkgs.php       # `scripts/generate-autoload-fixtures.sh`
            pkgs.curl      # `scripts/generate-autoload-fixtures.sh` fetch hint
            pkgs.bash      # the scripts use `set -euo pipefail`
            pkgs.coreutils # `mktemp -d`, etc., consistent across macOS/Linux
          ];
        };
      });
}
