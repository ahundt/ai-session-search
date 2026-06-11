{
  description = "Local-first search, inspection, export, and resume for Claude Code, Codex CLI, and Cursor sessions, with an MCP server for agent-driven recall.";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs { inherit system; };

        cargoToml = builtins.fromTOML (builtins.readFile ./Cargo.toml);

        sessiongrep = pkgs.rustPlatform.buildRustPackage {
          pname = cargoToml.package.name;
          version = cargoToml.package.version;

          src = self;

          cargoLock.lockFile = ./Cargo.lock;

          # `rusqlite` is built with the `bundled` feature, which compiles
          # SQLite from C source via the `cc` crate.
          nativeBuildInputs = [ pkgs.pkg-config ];

          meta = {
            inherit (cargoToml.package) description;
            homepage = cargoToml.package.repository;
            license = pkgs.lib.licenses.asl20;
            mainProgram = "sessiongrep";
          };
        };
      in
      {
        packages = {
          default = sessiongrep;
          sessiongrep = sessiongrep;
        };

        apps = {
          default = flake-utils.lib.mkApp {
            drv = sessiongrep;
            name = "sessiongrep";
          };
          sessiongrep = flake-utils.lib.mkApp {
            drv = sessiongrep;
            name = "sessiongrep";
          };
          sessiongrep-mcp = flake-utils.lib.mkApp {
            drv = sessiongrep;
            name = "sessiongrep-mcp";
          };
        };

        devShells.default = pkgs.mkShell {
          inputsFrom = [ sessiongrep ];
          packages = [
            pkgs.cargo
            pkgs.rustc
            pkgs.rustfmt
            pkgs.clippy
            pkgs.rust-analyzer
          ];
          RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
        };

        formatter = pkgs.nixfmt-rfc-style;
      }
    );
}
