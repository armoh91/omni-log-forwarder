// Copyright 2026 Salesforce, Inc. All rights reserved.

//! Flex Gateway custom policy: behaves like the OOTB "Message Logging" policy
//! (a repeatable list of DataWeave-driven logging configurations) and also
//! forwards every rendered line to an OTLP/HTTP endpoint via the `api-key`
//! header. DataWeave runs inline; the OTLP POST happens off the request path.

mod generated;
mod otel;

#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::time::{Duration, UNIX_EPOCH};

use anyhow::{anyhow, Result};
use futures::join;

use pdk::hl::*;
use pdk::hl::timer::{Clock, Timer};
use pdk::authentication::{Authentication, AuthenticationData, AuthenticationHandler};
use pdk::logger;
use pdk::metadata::Metadata;
use pdk::script::{AttributesBinding, Script, Value};

use crate::generated::config::{Config, LoggingConfigurations0Config};
use crate::otel::LogEntry;

const DEFAULT_BATCH_MAX_SIZE: usize = 512;
const DEFAULT_EXPORT_TIMEOUT_MS: u64 = 1000;

/// Request method/path carried to the response phase (response headers lack them).
#[derive(Clone, Debug)]
struct RequestContext {
    method: String,
    path: String,
}

// Owned snapshot of the attributes the DataWeave `attributes` object needs, so
// we can bind them after transitioning into the body state (where the live
// headers handler is gone). Pseudo-headers (`:method`/`:path`/`:status`) back
// the trait's method()/path()/statusCode accessors and are hidden from
// `attributes.headers`.
struct OwnedAttributes {
    headers: Vec<(String, String)>,
}

impl OwnedAttributes {
    fn new(
        mut headers: Vec<(String, String)>,
        method: Option<&str>,
        path: Option<&str>,
        status: Option<u32>,
    ) -> Self {
        headers.retain(|(k, _)| {
            let kl = k.to_ascii_lowercase();
            !(method.is_some() && kl == ":method")
                && !(path.is_some() && kl == ":path")
                && !(status.is_some() && kl == ":status")
        });
        if let Some(m) = method {
            headers.push((":method".to_string(), m.to_string()));
        }
        if let Some(p) = path {
            headers.push((":path".to_string(), p.to_string()));
        }
        if let Some(s) = status {
            headers.push((":status".to_string(), s.to_string()));
        }
        Self { headers }
    }
}

impl AttributesBinding for OwnedAttributes {
    fn extract_headers(&self) -> HashMap<String, String> {
        self.headers
            .iter()
            .filter(|(k, _)| !k.starts_with(':'))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    fn extract_header(&self, key: &str) -> Option<String> {
        let kl = key.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(k, _)| k.to_ascii_lowercase() == kl)
            .map(|(_, v)| v.clone())
    }
}

/// Evaluate a script, binding inputs in cost order and only reading the body
/// (last arg) if the expression still isn't ready after attributes + auth.
fn evaluate(
    script: &Script,
    attrs: &dyn AttributesBinding,
    auth: &Option<AuthenticationData>,
    body: Option<&[u8]>,
) -> Option<Value> {
    let mut ev = script.evaluator();
    ev.bind_attributes(attrs);
    if !ev.is_ready() {
        ev.bind_authentication(auth);
    }
    if !ev.is_ready() {
        if let Some(b) = body {
            ev.bind_payload(&b);
        }
    }
    ev.eval().ok()
}

/// True if the script still needs the payload after attributes + auth are bound.
fn needs_payload(
    script: &Script,
    attrs: &dyn AttributesBinding,
    auth: &Option<AuthenticationData>,
) -> bool {
    let mut ev = script.evaluator();
    ev.bind_attributes(attrs);
    if !ev.is_ready() {
        ev.bind_authentication(auth);
    }
    !ev.is_ready()
}

fn log_locally(level: &str, line: &str) {
    match level.to_ascii_uppercase().as_str() {
        "DEBUG" => logger::debug!("{}", line),
        "WARN" | "WARNING" => logger::warn!("{}", line),
        "ERROR" => logger::error!("{}", line),
        _ => logger::info!("{}", line),
    }
}

/// Strings pass through; other values are JSON-encoded; null becomes empty.
fn value_to_string(value: Value) -> String {
    match value {
        Value::String(s) => s,
        Value::Null => String::new(),
        other => serde_json::Value::from(other).to_string(),
    }
}

/// True if any active config references `payload`, so the body must be read.
fn phase_needs_body(
    active: &[&LoggingConfigurations0Config],
    attrs: &OwnedAttributes,
    auth: &Option<AuthenticationData>,
) -> bool {
    active.iter().any(|cfg| {
        needs_payload(&cfg.message, attrs, auth)
            || cfg
                .conditional
                .as_ref()
                .map(|c| needs_payload(c, attrs, auth))
                .unwrap_or(false)
    })
}

#[allow(clippy::too_many_arguments)]
fn emit_entry(
    cfg: &LoggingConfigurations0Config,
    attrs: &dyn AttributesBinding,
    auth: &Option<AuthenticationData>,
    body: Option<&[u8]>,
    phase: &str,
    status: u32,
    ctx: &RequestContext,
    now_unix_nano: u128,
) {
    if let Some(cond) = &cfg.conditional {
        let pass = evaluate(cond, attrs, auth, body)
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if !pass {
            return;
        }
    }

    let rendered = match evaluate(&cfg.message, attrs, auth, body) {
        Some(v) => value_to_string(v),
        None => {
            logger::warn!(
                "omni-log-forwarder: '{}' message expression failed to evaluate; skipping",
                cfg.configuration_name
            );
            return;
        }
    };

    let level = cfg.level.clone().unwrap_or_else(|| "INFO".to_string());
    let category = cfg.category.clone().unwrap_or_default();
    let line = if category.is_empty() {
        rendered
    } else {
        format!("[{category}] {rendered}")
    };

    if cfg.also_log_to_message_logs.unwrap_or(true) {
        log_locally(&level, &line);
    }

    let entry = LogEntry {
        time_unix_nano: now_unix_nano,
        body: line,
        level,
        configuration_name: cfg.configuration_name.clone(),
        category,
        phase: phase.to_string(),
        http_method: ctx.method.clone(),
        http_path: ctx.path.clone(),
        http_status_code: status,
    };

    if otel::enqueue(entry) {
        logger::warn!(
            "omni-log-forwarder: export buffer full, dropped oldest log record (endpoint slow?)"
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn run_phase(
    active: &[&LoggingConfigurations0Config],
    attrs: &OwnedAttributes,
    auth: &Option<AuthenticationData>,
    body: Option<&[u8]>,
    phase: &str,
    status: u32,
    ctx: &RequestContext,
    now_unix_nano: u128,
) {
    for cfg in active {
        emit_entry(cfg, attrs, auth, body, phase, status, ctx, now_unix_nano);
    }
}

async fn request_filter(
    request_state: RequestState,
    config: &Config,
    auth: Authentication,
    now_unix_nano: u128,
) -> Flow<RequestContext> {
    let headers_state = request_state.into_headers_state().await;
    let method = headers_state.method();
    let path = headers_state.path();
    let ctx = RequestContext {
        method: method.clone(),
        path: path.clone(),
    };

    let active: Vec<&LoggingConfigurations0Config> = config
        .logging_configurations
        .iter()
        .filter(|c| c.before_calling_api.unwrap_or(false))
        .collect();

    if active.is_empty() {
        return Flow::Continue(ctx);
    }

    let attrs = OwnedAttributes::new(
        headers_state.handler().headers(),
        Some(&method),
        Some(&path),
        None,
    );
    let auth_data = auth.authentication();

    let body: Option<Vec<u8>> =
        if headers_state.contains_body() && phase_needs_body(&active, &attrs, &auth_data) {
            let body_state = headers_state.into_body_state().await;
            Some(body_state.handler().body())
        } else {
            None
        };

    run_phase(
        &active,
        &attrs,
        &auth_data,
        body.as_deref(),
        "request",
        0,
        &ctx,
        now_unix_nano,
    );

    Flow::Continue(ctx)
}

async fn response_filter(
    response_state: ResponseState,
    request_data: RequestData<RequestContext>,
    config: &Config,
    auth: Authentication,
    now_unix_nano: u128,
) {
    let ctx = match request_data {
        RequestData::Continue(c) => c,
        _ => RequestContext {
            method: "-".to_string(),
            path: "-".to_string(),
        },
    };

    let headers_state = response_state.into_headers_state().await;
    let status = headers_state.status_code();

    let active: Vec<&LoggingConfigurations0Config> = config
        .logging_configurations
        .iter()
        .filter(|c| c.after_calling_api.unwrap_or(true))
        .collect();

    if active.is_empty() {
        return;
    }

    let attrs = OwnedAttributes::new(
        headers_state.handler().headers(),
        Some(&ctx.method),
        Some(&ctx.path),
        Some(status),
    );
    let auth_data = auth.authentication();

    let body: Option<Vec<u8>> =
        if headers_state.contains_body() && phase_needs_body(&active, &attrs, &auth_data) {
            let body_state = headers_state.into_body_state().await;
            Some(body_state.handler().body())
        } else {
            None
        };

    run_phase(
        &active,
        &attrs,
        &auth_data,
        body.as_deref(),
        "response",
        status,
        &ctx,
        now_unix_nano,
    );
}

/// Background exporter: drain a batch each tick and POST it to `<endpoint>/v1/logs`,
/// re-queueing on failure so a transient outage doesn't lose logs.
async fn export_loop(
    timer: &Timer,
    config: &Config,
    client: &HttpClient,
    service_name: &str,
    service_instance_id: &str,
) {
    let batch_max_size = normalize_usize(config.batch_max_size, DEFAULT_BATCH_MAX_SIZE);
    let auth_header = config
        .otlp_auth_header
        .clone()
        .unwrap_or_else(|| "api-key".to_string());

    while timer.next_tick().await {
        while otel::queue_len() > 0 {
            let batch = otel::drain_batch(batch_max_size);
            if batch.is_empty() {
                break;
            }

            let body = otel::to_otlp_body(&batch, service_name, service_instance_id);
            let count = batch.len();

            let result = client
                .request(&config.otlp_endpoint)
                .path("/v1/logs")
                .headers(vec![
                    ("content-type", "application/json"),
                    (auth_header.as_str(), config.otlp_api_key.as_str()),
                ])
                .body(&body)
                .post()
                .await;

            match result {
                Ok(resp) if (200..300).contains(&resp.status_code()) => {
                    logger::debug!(
                        "omni-log-forwarder: exported {count} log record(s) (status {})",
                        resp.status_code()
                    );
                }
                Ok(resp) => {
                    logger::warn!(
                        "omni-log-forwarder: endpoint returned status {} for {count} record(s); requeueing",
                        resp.status_code()
                    );
                    otel::requeue_front(batch);
                    break;
                }
                Err(e) => {
                    logger::warn!(
                        "omni-log-forwarder: failed to reach OTLP endpoint ({e:?}); requeueing {count} record(s)"
                    );
                    otel::requeue_front(batch);
                    break;
                }
            }
        }
    }
}

fn normalize_usize(value: Option<i64>, default: usize) -> usize {
    match value {
        Some(v) if v > 0 => v as usize,
        _ => default,
    }
}

fn normalize_millis(value: Option<i64>, default: u64) -> u64 {
    match value {
        Some(v) if v > 0 => v as u64,
        _ => default,
    }
}

#[entrypoint]
async fn configure(
    launcher: Launcher,
    Configuration(bytes): Configuration,
    metadata: Metadata,
    clock: Clock,
    client: HttpClient,
) -> Result<()> {
    let config: Config = serde_json::from_slice(&bytes).map_err(|err| {
        anyhow!(
            "Failed to parse configuration '{}'. Cause: {}",
            String::from_utf8_lossy(&bytes),
            err
        )
    })?;

    let service_instance_id = metadata.flex_metadata.flex_name.clone();
    let service_name = metadata
        .api_metadata
        .name
        .clone()
        .unwrap_or_else(|| metadata.policy_metadata.policy_name.clone());

    let export_period = Duration::from_millis(normalize_millis(
        config.export_timeout_ms,
        DEFAULT_EXPORT_TIMEOUT_MS,
    ));

    logger::info!(
        "omni-log-forwarder starting: service.name={service_name}, \
         service.instance.id={service_instance_id}, configurations={}, export_period={:?}",
        config.logging_configurations.len(),
        export_period
    );

    let timer = clock.period(export_period);

    let filter = on_request(|rs, auth| {
        let now = timer
            .now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        request_filter(rs, &config, auth, now)
    })
    .on_response(|rs, request_data, auth| {
        let now = timer
            .now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        response_filter(rs, request_data, &config, auth, now)
    });

    let launched = launcher.launch(filter);
    let exporting = export_loop(&timer, &config, &client, &service_name, &service_instance_id);
    let (launched, _) = join!(launched, exporting);
    launched?;

    Ok(())
}
