# NixOS module for annotation-agent: AI-powered Grafana annotation service.
# Options are defined in web.nix under peer-observer.web.annotationAgent.
# Uses Claude Code CLI (claude) for AI generation — no Anthropic API key needed.
# The service runs as `cfg.serviceUser` to access that user's ~/.claude/ credentials.
{ config, lib, pkgs, ... }:

let
  cfg = config.peer-observer.web.annotationAgent;
  CONSTANTS = import ../constants.nix;

  annotation-agent = pkgs.rustPlatform.buildRustPackage {
    pname = "annotation-agent";
    version = "0.1.0";
    src = ../../pkgs/annotation-agent;
    cargoLock.lockFile = ../../pkgs/annotation-agent/Cargo.lock;

    nativeBuildInputs = [ pkgs.pkg-config ];
    buildInputs = [ pkgs.openssl ];
  };

in
{
  config = lib.mkIf (config.peer-observer.web.enable && cfg.enable) {

    age.secrets."annotation-agent-grafana-api-key-${config.peer-observer.base.name}" = {
      file = cfg.grafanaApiKeyFile;
      owner = cfg.serviceUser;
    };

    systemd.services.annotation-agent = {
      description = "AI annotation agent for Grafana dashboards";
      wantedBy = [ "multi-user.target" ];
      after = [ "network.target" "prometheus.service" "grafana.service" ];

      serviceConfig = let
        grafanaSecretPath = config.age.secrets."annotation-agent-grafana-api-key-${config.peer-observer.base.name}".path;
        # Wrapper script that reads the agenix secret at runtime and passes it
        # as an env var. Runs as cfg.serviceUser (no root needed — secret is
        # owned by that user via the age.secrets.owner setting above).
        startScript = pkgs.writeShellScript "annotation-agent-start" ''
          set -euo pipefail
          export ANNOTATION_AGENT_GRAFANA_API_KEY="$(cat ${grafanaSecretPath})"
          exec ${annotation-agent}/bin/annotation-agent
        '';
      in {
        ExecStart = "${startScript}";
        Restart = "on-failure";
        RestartSec = "10s";

        # Run as the configured user so it has access to ~/.claude/ credentials.
        User = cfg.serviceUser;
        StateDirectory = "annotation-agent";
        StateDirectoryMode = "0755";

        Environment = [
          "ANNOTATION_AGENT_LISTEN_ADDR=${cfg.listenAddr}"
          "ANNOTATION_AGENT_PROMETHEUS_URL=http://127.0.0.1:${toString config.services.prometheus.port}"
          "ANNOTATION_AGENT_GRAFANA_URL=http://127.0.0.1:${toString CONSTANTS.GRAFANA_PORT}"
          "ANNOTATION_AGENT_CLAUDE_BIN=/etc/profiles/per-user/${cfg.serviceUser}/bin/claude"
          "ANNOTATION_AGENT_LOG_FILE=${CONSTANTS.ANNOTATION_LOG_FILE}"
          "HOME=/home/${cfg.serviceUser}"
        ];
      };
    };
  };
}
