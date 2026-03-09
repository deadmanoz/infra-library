use anyhow::{Context, Result};
use axum::{extract::State, http::StatusCode, routing::post, Json, Router};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, env, net::SocketAddr, sync::Arc};
use tokio::{fs::OpenOptions, io::AsyncWriteExt, process::Command};
use tracing::{error, info, warn};

#[derive(Clone)]
struct AppState {
    prometheus_url: String,
    grafana_url: String,
    grafana_api_key: String,
    claude_bin: String,
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

// Prometheus range query response types.
#[derive(Debug, Deserialize)]
struct PromResponse {
    data: Option<PromData>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PromData {
    result: Vec<PromResult>,
}

#[derive(Debug, Deserialize)]
struct PromResult {
    metric: HashMap<String, String>,
    values: Option<Vec<(f64, String)>>,
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
        prometheus_url: env::var("ANNOTATION_AGENT_PROMETHEUS_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:9090".to_string()),
        grafana_url: env::var("ANNOTATION_AGENT_GRAFANA_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:3000".to_string()),
        grafana_api_key: env::var("ANNOTATION_AGENT_GRAFANA_API_KEY")
            .context("ANNOTATION_AGENT_GRAFANA_API_KEY must be set")?,
        // Path to the claude binary. Defaults to "claude" (must be on PATH).
        claude_bin: env::var("ANNOTATION_AGENT_CLAUDE_BIN")
            .unwrap_or_else(|_| "claude".to_string()),
        // Optional path to append a plain-text annotation log.
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
    let metric_summary = query_prometheus(state, alert, alertname).await?;
    let explanation = call_claude(state, alert, alertname, &metric_summary).await?;
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

async fn query_prometheus(state: &AppState, alert: &Alert, alertname: &str) -> Result<String> {
    let start = alert.starts_at - chrono::Duration::minutes(30);
    let end = alert.starts_at + chrono::Duration::minutes(30);

    let queries = vec![
        format!(r#"ALERTS{{alertname="{alertname}"}}"#),
        format!(r#"peerobserver_anomaly:level{{anomaly_name=~".*{alertname}.*"}}"#),
        format!(r#"peerobserver_anomaly:upper_band{{anomaly_name=~".*{alertname}.*"}}"#),
    ];

    let mut summary_parts: Vec<String> = Vec::new();

    for query in &queries {
        match prom_range_query(state, query, start, end).await {
            Ok(results) => {
                for r in &results {
                    let metric_name = r.metric.get("__name__").cloned().unwrap_or_default();
                    if let Some(values) = &r.values {
                        if values.is_empty() {
                            continue;
                        }
                        let nums: Vec<f64> = values
                            .iter()
                            .filter_map(|(_, v)| v.parse::<f64>().ok())
                            .collect();
                        if nums.is_empty() {
                            continue;
                        }
                        let min = nums.iter().cloned().fold(f64::INFINITY, f64::min);
                        let max = nums.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
                        let avg = nums.iter().sum::<f64>() / nums.len() as f64;
                        let labels: String = r
                            .metric
                            .iter()
                            .filter(|(k, _)| *k != "__name__")
                            .map(|(k, v)| format!("{k}={v}"))
                            .collect::<Vec<_>>()
                            .join(", ");
                        summary_parts.push(format!(
                            "{metric_name}{{{labels}}}: min={min:.4}, max={max:.4}, avg={avg:.4} ({} samples)",
                            nums.len()
                        ));
                    }
                }
            }
            Err(e) => {
                warn!(query, "prometheus query failed: {e:#}");
            }
        }
    }

    if summary_parts.is_empty() {
        Ok("No metric data available for this alert window.".to_string())
    } else {
        Ok(summary_parts.join("\n"))
    }
}

async fn prom_range_query(
    state: &AppState,
    query: &str,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> Result<Vec<PromResult>> {
    let resp = state
        .http
        .get(format!("{}/api/v1/query_range", state.prometheus_url))
        .query(&[
            ("query", query),
            ("start", &start.timestamp().to_string()),
            ("end", &end.timestamp().to_string()),
            ("step", &"60".to_string()),
        ])
        .send()
        .await
        .context("prometheus request failed")?
        .json::<PromResponse>()
        .await
        .context("prometheus response parse failed")?;

    Ok(resp.data.map(|d| d.result).unwrap_or_default())
}

/// Call the Claude Code CLI to generate an annotation.
/// Uses the subscription credentials stored in the service user's ~/.claude/ directory.
async fn call_claude(
    state: &AppState,
    alert: &Alert,
    alertname: &str,
    metric_summary: &str,
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

    let system = "You annotate Grafana dashboards for a Bitcoin P2P network monitoring system \
                  (peer-observer). Given an alert and its metric data, write a concise 2-3 sentence \
                  annotation explaining what happened, the likely cause, and whether operator action \
                  is needed. Be specific about the metric values. Do not use markdown formatting.";

    let prompt = format!(
        "{system}\n\n\
         Alert: {alertname}\n\
         Host: {host}\n\
         Severity: {severity}\n\
         Category: {category}\n\
         Started: {}\n\
         Description: {description}\n\n\
         Prometheus metric data (±30 min window):\n{metric_summary}",
        alert.starts_at
    );

    let output = Command::new(&state.claude_bin)
        .args(["--dangerously-skip-permissions", "-p", &prompt])
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
