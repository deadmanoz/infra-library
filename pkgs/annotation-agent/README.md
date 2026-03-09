# annotation-agent

A Rust HTTP service that automatically annotates Grafana dashboards with AI-generated investigatory explanations whenever a Prometheus/Alertmanager alert fires.

## How it works

1. Alertmanager sends a webhook to `POST /webhook`
2. For each firing alert, the agent calls Claude Code CLI with a Prometheus MCP server
3. Claude autonomously queries Prometheus via MCP tools — drilling into per-peer data, correlating across hosts, and identifying root causes
4. Posts the investigation findings as a Grafana annotation with tags `[ai-annotation, alertname, host]`

## Architecture

```
Alertmanager webhook
        │
        ▼
  annotation-agent (Rust)
        │
        ├──▶ Claude CLI (--mcp-config)
        │         │
        │         └──▶ prometheus-mcp-server (via uvx)
        │                    │
        │                    └──▶ Prometheus API
        │
        └──▶ Grafana Annotations API
```

Claude has access to Prometheus via MCP and can:
- Query any PromQL expression (instant or range)
- List available metrics and metadata
- Drill into per-peer connection/message data
- Compare across hosts
- Identify specific peers causing issues

## Configuration

| Variable | Default | Description |
|----------|---------|-------------|
| `ANNOTATION_AGENT_LISTEN_ADDR` | `127.0.0.1:9099` | HTTP listen address |
| `ANNOTATION_AGENT_GRAFANA_URL` | `http://127.0.0.1:3000` | Grafana base URL |
| `ANNOTATION_AGENT_GRAFANA_API_KEY` | (required) | Grafana service account token |
| `ANNOTATION_AGENT_CLAUDE_BIN` | `claude` | Path to Claude CLI binary |
| `ANNOTATION_AGENT_MCP_CONFIG` | (required) | Path to MCP config JSON for Prometheus |
| `ANNOTATION_AGENT_LOG_FILE` | (optional) | Path to append plain-text annotation log |

## NixOS deployment

Use the `modules/web/annotation-agent.nix` NixOS module. The module automatically generates the MCP config pointing to the local Prometheus instance and uses `uvx` to run the `prometheus-mcp-server` Python package.

## Alertmanager receiver

Configure Alertmanager to send webhooks to `http://127.0.0.1:9099/webhook`. The NixOS module adds a receiver to the Alertmanager configuration when `peer-observer.web.annotationAgent.enable` is set.
