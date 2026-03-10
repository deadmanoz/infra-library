# NixOS module for annotation-agent: AI-powered Grafana annotation service.
# Options are defined in web.nix under peer-observer.web.annotationAgent.
# Uses Claude Code CLI (claude) with a Prometheus MCP server for autonomous
# alert investigation. Claude queries Prometheus directly via MCP tools to
# drill into per-peer data, identify root causes, and write specific annotations.
#
# The peer-observer-agent package comes from the peer-observer-agents flake input,
# passed via specialArgs in lib.nix.
{ config, lib, pkgs, peer-observer-agent-pkg, ... }:

let
  cfg = config.peer-observer.web.annotationAgent;
  CONSTANTS = import ../constants.nix;

  # MCP config for Claude CLI — gives it access to Prometheus via MCP tools.
  # Uses uvx to run the prometheus-mcp-server Python package on demand.
  mcpConfig = pkgs.writeText "annotation-agent-mcp.json" (builtins.toJSON {
    mcpServers = {
      prometheus = {
        command = "${pkgs.uv}/bin/uvx";
        args = [ "prometheus-mcp-server@1.6.0" ];
        env = {
          PROMETHEUS_URL = "http://127.0.0.1:${toString config.services.prometheus.port}";
        };
      };
    };
  });

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
        startScript = pkgs.writeShellScript "annotation-agent-start" ''
          set -euo pipefail
          export ANNOTATION_AGENT_GRAFANA_API_KEY="$(cat ${grafanaSecretPath})"
          exec ${peer-observer-agent-pkg}/bin/peer-observer-agent
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
          "ANNOTATION_AGENT_GRAFANA_URL=http://127.0.0.1:${toString CONSTANTS.GRAFANA_PORT}"
          "ANNOTATION_AGENT_CLAUDE_BIN=/etc/profiles/per-user/${cfg.serviceUser}/bin/claude"
          "ANNOTATION_AGENT_MCP_CONFIG=${mcpConfig}"
          "ANNOTATION_AGENT_LOG_FILE=${CONSTANTS.ANNOTATION_LOG_FILE}"
          "HOME=/home/${cfg.serviceUser}"
        ];
      };
    };
  };
}
