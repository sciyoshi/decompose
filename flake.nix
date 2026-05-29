{
  description = "decompose - fast local process orchestration for coding workflows";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs = { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];
      forAllSystems = f:
        nixpkgs.lib.genAttrs systems (system:
          f (import nixpkgs { inherit system; }));
      cargoToml = builtins.fromTOML (builtins.readFile ./Cargo.toml);
    in {
      packages = forAllSystems (pkgs: {
        default = pkgs.rustPlatform.buildRustPackage {
          pname = cargoToml.package.name;
          version = cargoToml.package.version;
          src = ./.;
          cargoDeps = pkgs.stdenvNoCC.mkDerivation {
            name = "${cargoToml.package.name}-${cargoToml.package.version}-vendor";
            src = ./.;
            nativeBuildInputs = [ pkgs.cargo pkgs.cacert ];
            outputHash = "sha256-e+cuFjGL7mKQ/4d3ifmNgOdOQFo5Tm3OAEv2DantMK8=";
            outputHashAlgo = "sha256";
            outputHashMode = "recursive";
            dontConfigure = true;
            dontFixup = true;
            buildPhase = ''
              runHook preBuild
              export CARGO_HOME="$TMPDIR/cargo-home"
              export SSL_CERT_FILE="${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt"
              cargo vendor --locked --versioned-dirs vendor >/dev/null
              runHook postBuild
            '';
            installPhase = ''
              runHook preInstall
              mkdir -p "$out/.cargo"
              cp -R vendor/. "$out/"
              cp Cargo.lock "$out/Cargo.lock"
              cat > "$out/.cargo/config.toml" <<'EOF'
              [source.crates-io]
              replace-with = "vendored-sources"

              [source.vendored-sources]
              directory = "@vendor@"
              EOF
              runHook postInstall
            '';
          };
          nativeBuildInputs = [ pkgs.pkg-config pkgs.installShellFiles ];
          nativeCheckInputs = [ pkgs.python3 ];
          postInstall = ''
            installShellCompletion --cmd decompose \
              --bash <($out/bin/decompose completion bash) \
              --fish <($out/bin/decompose completion fish) \
              --zsh  <($out/bin/decompose completion zsh)
          '';
          meta = {
            description = cargoToml.package.description;
            homepage = cargoToml.package.homepage;
            license = [ nixpkgs.lib.licenses.mit nixpkgs.lib.licenses.asl20 ];
            mainProgram = "decompose";
          };
        };
      });

      apps = forAllSystems (pkgs: {
        default = {
          type = "app";
          program = "${self.packages.${pkgs.system}.default}/bin/decompose";
        };
      });

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          packages = with pkgs; [
            cargo
            rustc
            rustfmt
            clippy
            pkg-config
            vhs
          ];
        };
      });
    };
}
