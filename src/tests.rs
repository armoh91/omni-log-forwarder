// Copyright 2026 Salesforce, Inc. All rights reserved.

//! pdk-unit tests: drive a transaction through the policy, advance the clock to
//! fire the exporter, and assert on the OTLP POST the mock endpoint receives.
//! DataWeave config fields are `format: dataweave`, so `dw2pel` converts the
//! expression into the PEL wire format the deserializer expects.

mod tests {
    use std::rc::Rc;
    use std::time::Duration;

    use pdk_unit::{
        dw2pel, TraceBackend, UnitHttpMessage, UnitHttpRequest, UnitHttpResponse, UnitTestBuilder,
    };
    use serde_json::{json, Value};

    const ENDPOINT: &str = "http://collector";
    const ENDPOINT_AUTHORITY: &str = "collector";
    const LICENSE_KEY: &str = "test-ingest-license-key-abc123";

    fn config_with(logging_configurations: Value) -> String {
        json!({
            "otlp_endpoint": ENDPOINT,
            "otlp_api_key": LICENSE_KEY,
            "loggingConfigurations": logging_configurations,
            "batch_max_size": 100,
            "export_timeout_ms": 100
        })
        .to_string()
    }

    fn tester_with(config: String) -> (pdk_unit::UnitTest, Rc<TraceBackend<UnitHttpResponse>>) {
        let endpoint = Rc::new(TraceBackend::new(UnitHttpResponse::new(200)));
        let tester = UnitTestBuilder::default()
            .with_config(&config)
            .with_backend(UnitHttpResponse::new(200))
            .with_http_upstream_from_authority(ENDPOINT_AUTHORITY, Rc::clone(&endpoint))
            .with_entrypoint(crate::configure);
        (tester, endpoint)
    }

    fn flatten_attrs(attrs: &Value) -> std::collections::HashMap<String, String> {
        let mut out = std::collections::HashMap::new();
        if let Some(arr) = attrs.as_array() {
            for a in arr {
                let key = a["key"].as_str().unwrap_or_default().to_string();
                let v = &a["value"];
                let val = if let Some(s) = v["stringValue"].as_str() {
                    s.to_string()
                } else if let Some(i) = v["intValue"].as_i64() {
                    i.to_string()
                } else {
                    v.to_string()
                };
                out.insert(key, val);
            }
        }
        out
    }

    fn only_record(otlp: &UnitHttpRequest) -> Value {
        let doc: Value = serde_json::from_slice(otlp.body()).expect("valid OTLP JSON");
        doc["resourceLogs"][0]["scopeLogs"][0]["logRecords"][0].clone()
    }

    #[test]
    fn forwards_rendered_message_with_api_key() {
        // `attributes.requestUri` is the raw request target (":path"); `attributes.path`
        // routes through a URL round-trip the in-process harness leaves null.
        let config = config_with(json!([{
            "configurationName": "access-log",
            "message": dw2pel("attributes.method ++ ' ' ++ attributes.requestUri"),
            "level": "INFO",
            "afterCallingApi": true
        }]));
        let (mut tester, endpoint) = tester_with(config);

        let response = tester.request(UnitHttpRequest::get().with_path("/orders/echo"));
        assert_eq!(response.status_code(), 200);

        tester.sleep(Duration::from_millis(500));

        let otlp = endpoint
            .next()
            .expect("endpoint should have received an OTLP export");

        assert_eq!(
            otlp.header("api-key").as_deref(),
            Some(LICENSE_KEY),
            "api-key header must carry the configured ingest license key"
        );
        assert_eq!(otlp.header(":path").as_deref(), Some("/v1/logs"));
        assert_eq!(otlp.header(":method").as_deref(), Some("POST"));
        assert_eq!(otlp.header("content-type").as_deref(), Some("application/json"));

        let record = only_record(&otlp);
        assert_eq!(
            record["body"]["stringValue"].as_str(),
            Some("GET /orders/echo"),
            "OTLP record body must be the evaluated DataWeave message"
        );

        let body = String::from_utf8_lossy(otlp.body());
        assert!(body.contains("resourceLogs"));
        assert!(body.contains("logRecords"));
        let attrs = flatten_attrs(&record["attributes"]);
        assert_eq!(attrs.get("log.configuration_name").map(String::as_str), Some("access-log"));
        assert_eq!(attrs.get("log.phase").map(String::as_str), Some("response"));
        assert_eq!(attrs.get("http.response.status_code").map(String::as_str), Some("200"));
    }

    #[test]
    fn conditional_true_forwards_the_message() {
        let config = config_with(json!([{
            "configurationName": "only-gets",
            "message": dw2pel("attributes.requestUri"),
            "conditional": dw2pel("attributes.method == 'GET'"),
            "afterCallingApi": true
        }]));
        let (mut tester, endpoint) = tester_with(config);

        tester.request(UnitHttpRequest::get().with_path("/widgets"));
        tester.sleep(Duration::from_millis(500));

        let otlp = endpoint
            .next()
            .expect("GET must pass the conditional and be exported");
        let record = only_record(&otlp);
        assert_eq!(record["body"]["stringValue"].as_str(), Some("/widgets"));
    }

    #[test]
    fn conditional_false_suppresses_the_message() {
        let config = config_with(json!([{
            "configurationName": "only-posts",
            "message": dw2pel("attributes.requestUri"),
            "conditional": dw2pel("attributes.method == 'POST'"),
            "afterCallingApi": true
        }]));
        let (mut tester, endpoint) = tester_with(config);

        tester.request(UnitHttpRequest::get().with_path("/widgets"));
        tester.sleep(Duration::from_millis(500));

        assert!(
            endpoint.next().is_none(),
            "conditional evaluated false — nothing should have been exported"
        );
    }

    #[test]
    fn before_calling_api_logs_in_request_phase() {
        let config = config_with(json!([{
            "configurationName": "inbound",
            "message": dw2pel("'inbound ' ++ attributes.method"),
            "beforeCallingApi": true,
            "afterCallingApi": false
        }]));
        let (mut tester, endpoint) = tester_with(config);

        tester.request(UnitHttpRequest::post().with_path("/submit"));
        tester.sleep(Duration::from_millis(500));

        let otlp = endpoint.next().expect("request-phase log must be exported");
        let record = only_record(&otlp);
        assert_eq!(record["body"]["stringValue"].as_str(), Some("inbound POST"));
        let attrs = flatten_attrs(&record["attributes"]);
        assert_eq!(attrs.get("log.phase").map(String::as_str), Some("request"));
        assert!(!attrs.contains_key("http.response.status_code"));
    }

    #[test]
    fn payload_message_forwards_request_body() {
        let config = config_with(json!([{
            "configurationName": "echo-body",
            "message": dw2pel("payload"),
            "beforeCallingApi": true,
            "afterCallingApi": false
        }]));
        let (mut tester, endpoint) = tester_with(config);

        tester.request(
            UnitHttpRequest::post()
                .with_path("/ingest")
                .with_header("content-type", "text/plain")
                .with_body("hello-payload"),
        );
        tester.sleep(Duration::from_millis(500));

        let otlp = endpoint.next().expect("payload log must be exported");
        let body = String::from_utf8_lossy(otlp.body());
        assert!(
            body.contains("hello-payload"),
            "rendered payload must appear in the OTLP body; got: {}",
            body
        );
    }

    #[test]
    fn level_maps_to_otlp_severity() {
        let config = config_with(json!([{
            "configurationName": "warn-line",
            "message": dw2pel("attributes.path"),
            "level": "WARN",
            "afterCallingApi": true
        }]));
        let (mut tester, endpoint) = tester_with(config);

        tester.request(UnitHttpRequest::get().with_path("/x"));
        tester.sleep(Duration::from_millis(500));

        let record = only_record(&endpoint.next().expect("expected an export"));
        assert_eq!(record["severityText"].as_str(), Some("WARN"));
        assert_eq!(record["severityNumber"].as_i64(), Some(13));
    }

    #[test]
    fn category_is_prefixed_and_attributed() {
        let config = config_with(json!([{
            "configurationName": "audit-line",
            "message": dw2pel("attributes.method"),
            "category": "AUDIT",
            "afterCallingApi": true
        }]));
        let (mut tester, endpoint) = tester_with(config);

        tester.request(UnitHttpRequest::get().with_path("/x"));
        tester.sleep(Duration::from_millis(500));

        let record = only_record(&endpoint.next().expect("expected an export"));
        assert_eq!(record["body"]["stringValue"].as_str(), Some("[AUDIT] GET"));
        let attrs = flatten_attrs(&record["attributes"]);
        assert_eq!(attrs.get("category").map(String::as_str), Some("AUDIT"));
    }

    // `alsoLogToMessageLogs` controls only the local write; OTLP forwarding
    // happens regardless. Here the flag is off, yet the line is still exported.
    #[test]
    fn also_log_flag_off_still_forwards_to_otlp() {
        let config = config_with(json!([{
            "configurationName": "otlp-only",
            "message": dw2pel("attributes.method"),
            "alsoLogToMessageLogs": false,
            "afterCallingApi": true
        }]));
        let (mut tester, endpoint) = tester_with(config);

        tester.request(UnitHttpRequest::get().with_path("/x"));
        tester.sleep(Duration::from_millis(500));

        let record = only_record(
            &endpoint
                .next()
                .expect("OTLP forwarding must happen even when local logging is off"),
        );
        assert_eq!(record["body"]["stringValue"].as_str(), Some("GET"));
    }

    #[test]
    fn batches_multiple_transactions_into_one_export() {
        let config = config_with(json!([{
            "configurationName": "path-log",
            "message": dw2pel("attributes.requestUri"),
            "afterCallingApi": true
        }]));
        let (mut tester, endpoint) = tester_with(config);

        for i in 0..3 {
            let resp = tester.request(UnitHttpRequest::get().with_path(&format!("/p{i}")));
            assert_eq!(resp.status_code(), 200);
        }
        tester.sleep(Duration::from_millis(500));

        let otlp = endpoint.next().expect("expected a batched OTLP export");
        let body = String::from_utf8_lossy(otlp.body());
        assert!(body.contains("/p0") && body.contains("/p1") && body.contains("/p2"));
    }
}
