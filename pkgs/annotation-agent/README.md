# annotation-agent

A small Rust HTTP service that automatically annotates Grafana dashboards with AI-generated explanations whenever a Prometheus/Alertmanager alert fires.

## How it works

1. Alertmanager sends a webhook to `POST /webhook`
2. For each firing alert, the agent queries Prometheus for metric data around the alert window (±30 min)
3. Sends the alert context + metric data to Claude (claude-3-5-haiku) for a concise explanation
4. Posts the explanation as a Grafana annotation with tags `[ai-annotation, alertname, host]`

## Configuration

Set these environment variables:

| Variable | Default | Description |
|----------|---------|-------------|
| `ANNOTATION_AGENT_LISTEN_ADDR` | `127.0.0.1:9099` | HTTP listen address |
| `ANNOTATION_AGENT_PROMETHEUS_URL` | `http://127.0.0.1:9090` | Prometheus base URL |
| `ANNOTATION_AGENT_GRAFANA_URL` | `http://127.0.0.1:3000` | Grafana base URL |
| `ANNOTATION_AGENT_GRAFANA_API_KEY` | (required) | Grafana service account token |
| `ANNOTATION_AGENT_ANTHROPIC_API_KEY` | (required) | Anthropic API key |

## Building

```bash
cargo build --release
```

## NixOS deployment

Use the `modules/web/annotation-agent.nix` NixOS module. See the module options for configuration details. Secrets (Grafana API key, Anthropic API key) are managed via agenix.

## Alertmanager receiver

Configure Alertmanager to send webhooks to `http://127.0.0.1:9099/webhook`. The NixOS module can optionally add a receiver to the existing Alertmanager configuration when `peer-observer.web.annotationAgent.enable` is set.
