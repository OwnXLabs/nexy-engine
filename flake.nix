{
  description = "Nexy Engine: a small, sandboxed agent engine for local models";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" "aarch64-darwin" "x86_64-darwin" ];
      forAll = f: nixpkgs.lib.genAttrs systems (s: f nixpkgs.legacyPackages.${s});
    in
    {
      packages = forAll (pkgs: rec {
        nexy-engine = pkgs.rustPlatform.buildRustPackage {
          pname = "nexy-engine";
          version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).package.version;
          src = pkgs.lib.cleanSource ./.;
          cargoLock.lockFile = ./Cargo.lock;
          nativeBuildInputs = [ pkgs.pkg-config ];
          buildInputs = [ pkgs.sqlite ];
          # tests start servers and child processes; they run in CI and before each push
          doCheck = false;
          postInstall = ''
            mkdir -p $out/share/nexy-engine
            cp -r packs $out/share/nexy-engine/packs
          '';
          meta = {
            description = "A small, sandboxed agent engine for local models";
            license = pkgs.lib.licenses.mit;
            mainProgram = "nexy-engine";
          };
        };
        default = nexy-engine;
      });

      nixosModules.default = { config, lib, pkgs, ... }:
        let
          cfg = config.services.nexy-engine;
          inherit (lib) mkOption mkEnableOption mkIf types;
        in
        {
          options.services.nexy-engine = {
            enable = mkEnableOption "Nexy Engine";
            package = mkOption { type = types.package; default = self.packages.${pkgs.stdenv.hostPlatform.system}.default; };
            host = mkOption { type = types.str; default = "127.0.0.1"; description = "Listen address; keep it on loopback unless a key is set."; };
            port = mkOption { type = types.port; default = 7300; };
            modelUrl = mkOption { type = types.str; example = "http://127.0.0.1:11434"; description = "OpenAI-compatible model server."; };
            model = mkOption { type = types.str; example = "qwen3.6:35b-a3b"; };
            keyFile = mkOption { type = types.nullOr types.path; default = null; description = "File holding the API key clients must send (NEXY_ENGINE_KEY). Required off loopback."; };
            modelKeyFile = mkOption { type = types.nullOr types.path; default = null; description = "File holding the model server's API key, if it needs one."; };
            searxngUrl = mkOption { type = types.nullOr types.str; default = null; };
            slots = mkOption { type = types.nullOr types.int; default = null; };
            memoryMax = mkOption { type = types.str; default = "2G"; description = "systemd MemoryMax for the engine and the commands it runs."; };
            extraEnvironment = mkOption { type = types.attrsOf types.str; default = { }; };
          };

          config = mkIf cfg.enable {
            # a static user: DynamicUser breaks StateDirectory in Own1 containers
            users.users.nexy-engine = { isSystemUser = true; group = "nexy-engine"; home = "/var/lib/nexy-engine"; };
            users.groups.nexy-engine = { };
            systemd.services.nexy-engine = {
              description = "Nexy Engine";
              wantedBy = [ "multi-user.target" ];
              after = [ "network.target" ];
              environment = {
                NEXY_ENGINE_HOST = cfg.host;
                NEXY_ENGINE_PORT = toString cfg.port;
                NEXY_ENGINE_MODEL_URL = cfg.modelUrl;
                NEXY_ENGINE_MODEL = cfg.model;
                NEXY_ENGINE_DATA = "/var/lib/nexy-engine";
                NEXY_ENGINE_PACKS = "${cfg.package}/share/nexy-engine/packs";
              } // lib.optionalAttrs (cfg.searxngUrl != null) { NEXY_ENGINE_SEARXNG_URL = cfg.searxngUrl; }
                // lib.optionalAttrs (cfg.slots != null) { NEXY_ENGINE_SLOTS = toString cfg.slots; }
                // cfg.extraEnvironment;
              # keys arrive as systemd credentials, never in the unit or the store
              script = ''
                ${lib.optionalString (cfg.keyFile != null) ''export NEXY_ENGINE_KEY="$(< "$CREDENTIALS_DIRECTORY/key")"''}
                ${lib.optionalString (cfg.modelKeyFile != null) ''export NEXY_ENGINE_MODEL_KEY="$(< "$CREDENTIALS_DIRECTORY/model-key")"''}
                exec ${lib.getExe cfg.package} serve
              '';
              serviceConfig = {
                User = "nexy-engine";
                Group = "nexy-engine";
                StateDirectory = "nexy-engine";
                StateDirectoryMode = "0700";
                MemoryMax = cfg.memoryMax;
                LoadCredential = lib.optional (cfg.keyFile != null) "key:${cfg.keyFile}"
                  ++ lib.optional (cfg.modelKeyFile != null) "model-key:${cfg.modelKeyFile}";
                Restart = "on-failure";
                NoNewPrivileges = true;
                ProtectSystem = "strict";
                ProtectHome = true;
                PrivateTmp = true;
                PrivateDevices = true;
                ProtectKernelTunables = true;
                ProtectKernelModules = true;
                ProtectControlGroups = true;
                RestrictNamespaces = false; # the run tool's sandbox uses user namespaces and landlock
                LockPersonality = true;
                RestrictRealtime = true;
                MemoryDenyWriteExecute = false; # Lightpanda/V8 when enabled
              };
            };
          };
        };
    };
}
