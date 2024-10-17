{
  inputs = {
    nixpkgs.url = "nixpkgs";
    devenv.url = "github:cachix/devenv";
  };
  outputs =
    inputs@{
      self,
      nixpkgs,
      devenv,
      ...
    }:
    let
      system = "x86_64-linux";
      pkgs = import nixpkgs { inherit system; };
    in
    {
      devShells.${system}.default = devenv.lib.mkShell {
        inherit inputs pkgs;
        modules = [
          (
            { pkgs, config, ... }:
            {
              name = "Flake";

              packages = with pkgs; [
                rust-analyzer

                openssl
                sqlite
              ];

              languages = {
                nix.enable = true;
                rust.enable = true;
              };
            }
          )
        ];
      };
    };
}
