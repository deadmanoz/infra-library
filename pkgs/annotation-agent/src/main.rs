use anyhow::{Context, Result};
use axum::{extract::State, http::StatusCode, routing::post, Json, Router};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, env, net::SocketAddr, sync::Arc};
use tokio::{fs::OpenOptions, io::AsyncWriteExt, process::Command};
use tracing::{error, info, warn};

#[derive(Clone)]
struct AppState {
    grafana_url: String,
    grafana_api_key: String,
    claude_bin: String,
    mcp_config: String,
    log_file: Option<String>,
    http: reqwest::Client,
}

// Alertmanager webhook payload types.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AlertmanagerPayload {
    alerts: Vec<Alert>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Alert {
    status: String,
    labels: HashMap<String, String>,
    annotations: Option<HashMap<String, String>>,
    starts_at: DateTime<Utc>,
    ends_at: Option<DateTime<Utc>>,
}

// Grafana annotation payload.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GrafanaAnnotation {
    time: i64,
    time_end: i64,
    tags: Vec<String>,
    text: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "annotation_agent=info".into()),
        )
        .init();

    let listen_addr: SocketAddr = env::var("ANNOTATION_AGENT_LISTEN_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:9099".to_string())
        .parse()
        .context("invalid listen address")?;

    let state = Arc::new(AppState {
        grafana_url: env::var("ANNOTATION_AGENT_GRAFANA_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:3000".to_string()),
        grafana_api_key: env::var("ANNOTATION_AGENT_GRAFANA_API_KEY")
            .context("ANNOTATION_AGENT_GRAFANA_API_KEY must be set")?,
        claude_bin: env::var("ANNOTATION_AGENT_CLAUDE_BIN")
            .unwrap_or_else(|_| "claude".to_string()),
        mcp_config: env::var("ANNOTATION_AGENT_MCP_CONFIG")
            .context("ANNOTATION_AGENT_MCP_CONFIG must be set")?,
        log_file: env::var("ANNOTATION_AGENT_LOG_FILE").ok(),
        http: reqwest::Client::new(),
    });

    let app = Router::new()
        .route("/webhook", post(handle_webhook))
        .with_state(state);

    info!("annotation-agent listening on {listen_addr}");
    let listener = tokio::net::TcpListener::bind(listen_addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn handle_webhook(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<AlertmanagerPayload>,
) -> StatusCode {
    let firing: Vec<&Alert> = payload.alerts.iter().filter(|a| a.status == "firing").collect();
    info!("received webhook with {} alerts ({} firing)", payload.alerts.len(), firing.len());

    for alert in firing {
        let alertname = alert.labels.get("alertname").cloned().unwrap_or_default();
        if let Err(e) = process_alert(&state, alert, &alertname).await {
            error!(alertname, "failed to process alert: {e:#}");
        }
    }

    StatusCode::OK
}

async fn process_alert(state: &AppState, alert: &Alert, alertname: &str) -> Result<()> {
    let explanation = call_claude(state, alert, alertname).await?;
    post_grafana_annotation(state, alert, alertname, &explanation).await?;
    append_log(state, alert, alertname, &explanation).await;
    info!(alertname, "annotation posted successfully");
    Ok(())
}

async fn append_log(state: &AppState, alert: &Alert, alertname: &str, explanation: &str) {
    let Some(ref path) = state.log_file else { return };
    let host = alert.labels.get("host").cloned().unwrap_or_else(|| "unknown".to_string());
    let line = format!(
        "[{}] {} on {} — {}\n",
        alert.starts_at.format("%Y-%m-%d %H:%M:%S UTC"),
        alertname,
        host,
        explanation
    );
    match OpenOptions::new().create(true).append(true).open(path).await {
        Ok(mut f) => { let _ = f.write_all(line.as_bytes()).await; }
        Err(e) => warn!(path, "failed to write annotation log: {e}"),
    }
}

/// Call the Claude Code CLI with Prometheus MCP tools to investigate the alert.
///
/// Claude has access to Prometheus via MCP and can autonomously query metrics,
/// drill into per-peer data, and correlate across hosts to determine root cause.
async fn call_claude(
    state: &AppState,
    alert: &Alert,
    alertname: &str,
) -> Result<String> {
    let host = alert.labels.get("host").cloned().unwrap_or_else(|| "unknown".to_string());
    let severity = alert.labels.get("severity").cloned().unwrap_or_else(|| "unknown".to_string());
    let category = alert.labels.get("category").cloned().unwrap_or_else(|| "unknown".to_string());
    let description = alert
        .annotations
        .as_ref()
        .and_then(|a| a.get("description"))
        .cloned()
        .unwrap_or_else(|| "No description provided.".to_string());
    let dashboard = alert
        .annotations
        .as_ref()
        .and_then(|a| a.get("dashboard"))
        .cloned()
        .unwrap_or_default();
    let runbook = alert
        .annotations
        .as_ref()
        .and_then(|a| a.get("runbook"))
        .cloned()
        .unwrap_or_default();

    let prompt = format!(
        r#"You are an investigator for a Bitcoin P2P network monitoring system (peer-observer).
You have access to Prometheus via MCP tools. Use them to investigate this alert.

## Alert Details
- Alert: {alertname}
- Host: {host}
- Severity: {severity}
- Category: {category}
- Started: {started}
- Description: {description}
{dashboard_line}{runbook_line}
## Investigation Instructions

1. QUERY the alert's triggering metric to confirm current values and see the trend.
2. DRILL DOWN into related metrics to identify the specific cause:
   - For connection alerts: check per-peer connection data, network types, peer ages
   - For P2P message alerts: check per-peer message rates, identify top senders
   - For queue alerts: check per-peer INV queue depths, identify stalled peers
   - For mempool alerts: check fee rates, transaction counts, memory usage trends
   - For resource alerts: check per-process resource usage
   - For chain alerts: check block heights, IBD status, verification progress
3. COMPARE across hosts if relevant (are other nodes seeing the same thing?).
4. FORM a specific conclusion: what happened, which peer/component caused it, and whether action is needed.

## Available Metric Prefixes
- peerobserver_conn_* (connection metrics, per-peer data)
- peerobserver_msg_* (P2P message counts by type and peer)
- peerobserver_anomaly:* (anomaly detection bands and levels)
- peerobserver_rpc_* (Bitcoin Core RPC data: peer_info, mempoolinfo, blockchaininfo, networkinfo)
- peerobserver_validation_* (block validation events)
- node_* (system metrics: CPU, memory, disk, network)

Use execute_query for current values and execute_range_query for trends (use the ±30 min window around {started}).

## Output Format
Write a concise 3-5 sentence annotation. Be SPECIFIC: name the peer IP if you find one, quote exact metric values, state the likely cause with evidence. Do not use markdown formatting. End with whether operator action is needed and what action if so."#,
        started = alert.starts_at,
        dashboard_line = if dashboard.is_empty() { String::new() } else { format!("- Dashboard: {dashboard}\n") },
        runbook_line = if runbook.is_empty() { String::new() } else { format!("- Runbook: {runbook}\n") },
    );

    info!(alertname, host, "calling claude with MCP prometheus tools");

    let output = Command::new(&state.claude_bin)
        .args([
            "--dangerously-skip-permissions",
            "--mcp-config", &state.mcp_config,
            "-p", &prompt,
            "--model", "claude-sonnet-4-20250514",
            "--max-turns", "10",
        ])
        .output()
        .await
        .with_context(|| format!("failed to spawn claude process at '{}'", state.claude_bin))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("claude process exited with {}: {stderr}", output.status);
    }

    let text = String::from_utf8(output.stdout)
        .context("claude output is not valid UTF-8")?
        .trim()
        .to_string();

    if text.is_empty() {
        anyhow::bail!("claude returned empty output");
    }

    Ok(text)
}

async fn post_grafana_annotation(
    state: &AppState,
    alert: &Alert,
    alertname: &str,
    text: &str,
) -> Result<()> {
    let time_ms = alert.starts_at.timestamp_millis();
    // Alertmanager uses "0001-01-01T00:00:00Z" as a sentinel for still-firing alerts.
    // Ignore any endsAt that precedes the Unix epoch — treat the annotation as a point in time.
    let time_end_ms = alert
        .ends_at
        .filter(|t| t.timestamp() > 0)
        .map(|t| t.timestamp_millis())
        .unwrap_or(time_ms);

    let host = alert.labels.get("host").cloned().unwrap_or_else(|| "unknown".to_string());

    let annotation = GrafanaAnnotation {
        time: time_ms,
        time_end: time_end_ms,
        tags: vec!["ai-annotation".to_string(), alertname.to_string(), host],
        text: text.to_string(),
    };

    let resp = state
        .http
        .post(format!("{}/api/annotations", state.grafana_url))
        .header("Authorization", format!("Bearer {}", state.grafana_api_key))
        .header("Content-Type", "application/json")
        .json(&annotation)
        .send()
        .await
        .context("grafana annotation request failed")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        anyhow::bail!("grafana API returned {status}: {text}");
    }

    Ok(())
}
