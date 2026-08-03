// Copyright 2026 Salesforce, Inc. All rights reserved.

//! OTLP/HTTP JSON payload construction and the per-worker export queue.

use std::cell::RefCell;
use std::collections::VecDeque;

use serde_json::{json, Value};

#[derive(Clone, Debug, Default)]
pub struct LogEntry {
    pub time_unix_nano: u128,
    pub body: String,
    pub level: String,
    pub configuration_name: String,
    pub category: String,
    pub phase: String,
    pub http_method: String,
    pub http_path: String,
    pub http_status_code: u32,
}

fn severity(level: &str) -> (i64, &'static str) {
    match level.to_ascii_uppercase().as_str() {
        "DEBUG" => (5, "DEBUG"),
        "WARN" | "WARNING" => (13, "WARN"),
        "ERROR" => (17, "ERROR"),
        _ => (9, "INFO"),
    }
}

fn str_attr(key: &str, value: &str) -> Value {
    json!({ "key": key, "value": { "stringValue": value } })
}

fn int_attr(key: &str, value: i64) -> Value {
    json!({ "key": key, "value": { "intValue": value } })
}

impl LogEntry {
    fn to_log_record(&self) -> Value {
        let (sev_num, sev_text) = severity(&self.level);

        let mut attrs = vec![
            str_attr("log.configuration_name", &self.configuration_name),
            str_attr("log.phase", &self.phase),
        ];
        if !self.category.is_empty() {
            attrs.push(str_attr("category", &self.category));
        }
        if !self.http_method.is_empty() {
            attrs.push(str_attr("http.request.method", &self.http_method));
        }
        if !self.http_path.is_empty() {
            attrs.push(str_attr("url.path", &self.http_path));
        }
        if self.http_status_code > 0 {
            attrs.push(int_attr(
                "http.response.status_code",
                self.http_status_code as i64,
            ));
        }

        json!({
            "timeUnixNano": self.time_unix_nano.to_string(),
            "observedTimeUnixNano": self.time_unix_nano.to_string(),
            "severityNumber": sev_num,
            "severityText": sev_text,
            "body": { "stringValue": self.body },
            "attributes": attrs
        })
    }
}

pub fn to_otlp_body(entries: &[LogEntry], service_name: &str, service_instance_id: &str) -> Vec<u8> {
    let records: Vec<Value> = entries.iter().map(LogEntry::to_log_record).collect();

    let doc = json!({
        "resourceLogs": [ {
            "resource": {
                "attributes": [
                    str_attr("service.name", service_name),
                    str_attr("service.instance.id", service_instance_id),
                ]
            },
            "scopeLogs": [ {
                "scope": { "name": "omni-log-forwarder", "version": "1.0.0" },
                "logRecords": records
            } ]
        } ]
    });

    serde_json::to_vec(&doc).unwrap_or_default()
}

thread_local! {
    static LOG_QUEUE: RefCell<VecDeque<LogEntry>> = RefCell::new(VecDeque::new());
}

const MAX_BUFFERED_RECORDS: usize = 10_000;

/// Enqueue a record; returns true if the buffer was full and the oldest was dropped.
pub fn enqueue(entry: LogEntry) -> bool {
    LOG_QUEUE.with(|q| {
        let mut q = q.borrow_mut();
        let mut dropped = false;
        if q.len() >= MAX_BUFFERED_RECORDS {
            q.pop_front();
            dropped = true;
        }
        q.push_back(entry);
        dropped
    })
}

pub fn drain_batch(max: usize) -> Vec<LogEntry> {
    LOG_QUEUE.with(|q| {
        let mut q = q.borrow_mut();
        let take = max.min(q.len());
        q.drain(..take).collect()
    })
}

/// Re-queue a failed batch at the front, preserving order.
pub fn requeue_front(mut batch: Vec<LogEntry>) {
    LOG_QUEUE.with(|q| {
        let mut q = q.borrow_mut();
        while let Some(entry) = batch.pop() {
            if q.len() >= MAX_BUFFERED_RECORDS {
                break;
            }
            q.push_front(entry);
        }
    })
}

pub fn queue_len() -> usize {
    LOG_QUEUE.with(|q| q.borrow().len())
}

#[cfg(test)]
mod otel_tests {
    use super::*;

    fn sample(level: &str) -> LogEntry {
        LogEntry {
            time_unix_nano: 1_700_000_000_000_000_000,
            body: "hello from dataweave".to_string(),
            level: level.to_string(),
            configuration_name: "log-everything".to_string(),
            category: "AUDIT".to_string(),
            phase: "response".to_string(),
            http_method: "GET".to_string(),
            http_path: "/orders/42".to_string(),
            http_status_code: 200,
        }
    }

    #[test]
    fn otlp_body_carries_rendered_message_and_context() {
        let body = to_otlp_body(&[sample("INFO")], "omni-gateway", "flex-abc");
        let text = String::from_utf8(body).unwrap();
        assert!(text.contains("resourceLogs"));
        assert!(text.contains("logRecords"));
        assert!(text.contains("hello from dataweave"));
        assert!(text.contains("log-everything"));
        assert!(text.contains("AUDIT"));
        assert!(text.contains("omni-gateway"));
        assert!(text.contains("flex-abc"));
    }

    #[test]
    fn severity_maps_from_level() {
        let doc: Value =
            serde_json::from_slice(&to_otlp_body(&[sample("ERROR")], "s", "i")).unwrap();
        let rec = &doc["resourceLogs"][0]["scopeLogs"][0]["logRecords"][0];
        assert_eq!(rec["severityText"], "ERROR");
        assert_eq!(rec["severityNumber"], 17);

        let doc: Value =
            serde_json::from_slice(&to_otlp_body(&[sample("DEBUG")], "s", "i")).unwrap();
        let rec = &doc["resourceLogs"][0]["scopeLogs"][0]["logRecords"][0];
        assert_eq!(rec["severityText"], "DEBUG");
        assert_eq!(rec["severityNumber"], 5);
    }

    #[test]
    fn drain_batch_respects_order_and_max() {
        let _ = drain_batch(usize::MAX);
        for i in 0..5 {
            let mut e = sample("INFO");
            e.body = format!("msg-{i}");
            enqueue(e);
        }
        assert_eq!(queue_len(), 5);
        let batch = drain_batch(3);
        assert_eq!(batch.len(), 3);
        assert_eq!(batch[0].body, "msg-0");
        assert_eq!(queue_len(), 2);
        let _ = drain_batch(usize::MAX);
    }
}
