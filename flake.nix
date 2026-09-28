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
      # Substitute store paths while leaving runtime ${...} and $$ escapes
      # untouched. References in the YAML retain their runtime closures.
      lib.mkFragment = { pkgs, name, src, substitutions ? {} }:
        assert builtins.match "[A-Za-z0-9_-]+" name != null;
        pkgs.runCommand "${name}-decompose-fragment" {} ''
          install -Dm444 ${pkgs.replaceVars src substitutions} \
            "$out/share/decompose/${name}.yaml"
        '';

      checks = forAllSystems (pkgs:
        let
          runner = pkgs.writeShellScriptBin "fragment-check" ''
            echo packaged-fragment-ok
          '';
          fragment = self.lib.mkFragment {
            inherit pkgs;
            name = "check";
            src = ./examples/fragments/check.yaml;
            substitutions = { inherit runner; };
          };
        in {
          fragment = pkgs.runCommand "decompose-fragment-check" {} ''
            file=${fragment}/share/decompose/check.yaml
            grep -F '${runner}/bin/fragment-check' "$file"
            grep -F '${"$"}{DECOMPOSE_PROJECT_DIR}' "$file"
            grep -F '$$RUNTIME_VALUE' "$file"
            test "$(${runner}/bin/fragment-check)" = packaged-fragment-ok
            mkdir -p "$out"
            cp "$file" "$out/check.yaml"
          '';
        });

      packages = forAllSystems (pkgs: {
        default = pkgs.rustPlatform.buildRustPackage {
          pname = cargoToml.package.name;
          version = cargoToml.package.version;
          src = ./.;
          cargoDeps = pkgs.stdenvNoCC.mkDerivation {
            name = "${cargoToml.package.name}-${cargoToml.package.version}-vendor";
            src = ./.;
            nativeBuildInputs = [ pkgs.cargo pkgs.cacert ];
            # Includes Cargo.lock: refresh even for package-only version bumps.
            # See the Nix vendor check in .github/workflows/nix.yml.
            outputHash = "sha256-cUmOmFQAhq+7KE40CS5eQZ+GlpC6oXDTsNweBhMJaA4=";
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
          checkFlags = [
            # The full integration suite is run in CI and before releases.
            # This localhost HTTP-server probe is unreliable in the Nix build
            # sandbox, while the rest of the package checks remain useful.
            "--skip=http_get_readiness_probe_flips_healthy_flag"
          ];
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
