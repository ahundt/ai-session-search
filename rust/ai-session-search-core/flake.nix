# SPDX-FileCopyrightText: 2026 Andrew Hundt
# SPDX-FileCopyrightText: 2026 Guilherme (@guilhermeprokisch)
# SPDX-License-Identifier: Apache-2.0

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

        aise = pkgs.rustPlatform.buildRustPackage {
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
            mainProgram = "aise";
          };
        };
      in
      {
        packages = {
          default = aise;
          aise = aise;
        };

        apps = {
          default = flake-utils.lib.mkApp {
            drv = aise;
            name = "aise";
          };
          aise = flake-utils.lib.mkApp {
            drv = aise;
            name = "aise";
          };
        };

        devShells.default = pkgs.mkShell {
          inputsFrom = [ aise ];
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
