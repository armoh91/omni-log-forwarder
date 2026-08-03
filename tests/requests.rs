// Copyright 2026 Salesforce, Inc. All rights reserved.

//! Docker-based integration test for the NR Direct Log Forwarder.
//!
//! Requires Docker AND a local-mode `registration.yaml` in `tests/config/` (see
//! README "Running the integration tests"). It composes a real Flex Gateway
//! (bound to **port 8081**, hardcoded — no dynamic/random ports), an httpmock
//! backend standing in for the upstream API, and a second httpmock standing in
//! for the OTLP ingestion endpoint.
//!
//! It drives a request through the gateway on 8081 and asserts the OTLP endpoint
//! received a `POST /v1/logs` carrying the `api-key` auth header — proving the
//! policy forwards the rendered DataWeave log line to the configured sink.
//!
//! The DataWeave `message`/`conditional` fields are supplied here as ordinary
//! `#[...]` expressions (NOT the `dw2pel` PEL wire format used by the unit
//! tests): the real Flex Gateway compiles `format: dataweaveExpression` fields
//! to PEL before the policy receives them.
//!
//! Run with: `make test` (which builds the wasm first).

mod common;

use httpmock::MockServer;
use pdk_test::port::Port;
use pdk_test::services::flex::{ApiConfig, Flex, FlexConfig, PolicyConfig};
use pdk_test::services::httpmock::{HttpMock, HttpMockConfig};
use pdk_test::{pdk_test, TestComposite};

use common::*;

// ZERO-TOLERANCE PORT ENFORCEMENT:
// The local Flex listener is hardcoded to 8081. This is the ONLY port the test
// harness binds and the ONLY port requests target. No dynamic ports, no Docker
// random port mapping (`-P`).
const FLEX_PORT: Port = 8081;

// Hostname of the mock OTLP endpoint on the test network. Must match the
// authority the policy is configured to call (`otlp_endpoint`).
const ENDPOINT_HOSTNAME: &str = "otlp-endpoint";

// A stand-in ingest key. The test asserts this exact value rides on `api-key`.
const TEST_API_KEY: &str = "test-ingest-license-key-xyz";

#[pdk_test]
async fn forwards_rendered_log_to_otlp_endpoint() -> anyhow::Result<()> {
    // --- Upstream API backend (what the client's request proxies to) ---------
    let backend_config = HttpMockConfig::builder()
        .hostname("backend")
        .port(80)
        .version("latest")
        .build();

    // --- Mock OTLP ingestion endpoint (receives the forwarded OTLP logs) -----
    let endpoint_config = HttpMockConfig::builder()
        .hostname(ENDPOINT_HOSTNAME)
        .port(80)
        .version("latest")
        .build();

    // --- Policy configuration -------------------------------------------------
    // otlp_endpoint points at the in-network mock endpoint. The policy POSTs
    // to `<endpoint>/v1/logs` with the `api-key` header. Small export_timeout_ms
    // so the background exporter flushes quickly within the test. The message is
    // a DataWeave expression, exactly as an operator would author it.
    let policy_config = PolicyConfig::builder()
        .name(POLICY_NAME)
        .configuration(serde_json::json!({
            "loggingConfigurations": [{
                "configurationName": "access-log",
                "message": "#[attributes.method ++ ' ' ++ attributes.requestUri]",
                "level": "INFO",
                "afterCallingApi": true
            }],
            "otlp_endpoint": format!("http://{ENDPOINT_HOSTNAME}"),
            "otlp_api_key": TEST_API_KEY,
            "batch_max_size": 100,
            "export_timeout_ms": 500
        }))
        .build();

    // --- API bound to PORT 8081 ----------------------------------------------
    let api_config = ApiConfig::builder()
        .name("ingress-http")
        .upstream(&backend_config)
        .path("/anything/echo/")
        .port(FLEX_PORT) // hardcoded 8081
        .policies([policy_config])
        .build();

    // --- Flex Gateway, listening on 8081 -------------------------------------
    let flex_config = FlexConfig::builder()
        .version("1.10.0")
        .hostname("local-flex")
        .with_api(api_config)
        .config_mounts([(POLICY_DIR, "policy"), (COMMON_CONFIG_DIR, "common")])
        .build();

    // Compose gateway + upstream backend + mock OTLP endpoint.
    let composite = TestComposite::builder()
        .with_service(flex_config)
        .with_service(backend_config)
        .with_service(endpoint_config)
        .build()
        .await?;

    // Program the mock OTLP endpoint to accept POST /v1/logs carrying the
    // api-key header, replying 200 (as OTLP endpoints do on success).
    let endpoint: HttpMock = composite.service()?;
    let endpoint_server = MockServer::connect_async(endpoint.socket()).await;
    let otlp_mock = endpoint_server
        .mock_async(|when, then| {
            when.method("POST")
                .path("/v1/logs")
                .header("api-key", TEST_API_KEY);
            then.status(200).body("{}");
        })
        .await;

    // Program the upstream backend to answer the proxied request.
    let backend: HttpMock = composite.service()?;
    let backend_server = MockServer::connect_async(backend.socket()).await;
    backend_server
        .mock_async(|when, then| {
            when.path_contains("/echo");
            then.status(200).body("ok");
        })
        .await;

    // Drive a request through the gateway on PORT 8081.
    let flex: Flex = composite.service()?;
    let flex_url = flex.external_url(FLEX_PORT).unwrap();
    let response = reqwest::Client::new()
        .get(format!("{flex_url}/anything/echo/"))
        .send()
        .await?;

    // Logging is transparent — the client still gets the upstream 200.
    assert_eq!(response.status(), 200);

    // Give the async background exporter time to flush to the endpoint.
    // (export_timeout_ms is 500ms; allow a comfortable margin.)
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;

    // The endpoint must have received at least one OTLP export with the api-key.
    otlp_mock.assert_async().await;

    Ok(())
}
