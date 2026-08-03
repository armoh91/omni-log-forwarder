# `omni-log-forwarder` Policy

> A vendor-neutral OTLP log-forwarding policy for MuleSoft Omni Gateway (Flex
> Gateway). Works with any OpenTelemetry-compatible logs backend.


A custom Flex Gateway policy, built with the [Policy Development Kit
(PDK)](https://docs.mulesoft.com/pdk/latest/policies-pdk-overview), that behaves
**exactly like the out-of-the-box (OOTB) MuleSoft "Message Logging" policy** —
a repeatable list of DataWeave-driven log configurations — with **one addition**:
as well as writing each rendered line to the local API instance message logs
(as OOTB Message Logging does, on by default), it **also forwards the line to
any third-party observability platform over OTLP/HTTP**, asynchronously and
without adding latency to the request path. Local logging can be turned off
per configuration (**Also Log to Message Logs**) to forward to OTLP only.

- Works with **any OTLP/HTTP logs backend** that accepts `POST /v1/logs` — an
  OpenTelemetry Collector, or a managed observability platform's OTLP ingest
  endpoint.
- If your platform requires an ingest key/token, the policy sends it on the
  `api-key` request header.

---

## 1. What the policy does

For each request that flows through the gateway, the policy evaluates every
configured **Logging Configuration** — just like OOTB Message Logging:

1. **Before Calling API** (request phase) and/or **After Calling API** (response
   phase), per the checkboxes on each configuration.
2. Renders that configuration's **Message** (a DataWeave expression) — e.g.
   `#[payload]`, `#[attributes.headers['id']]`,
   `#[attributes.method ++ ' ' ++ attributes.requestUri]`.
3. If a **Conditional** DataWeave expression is present, the line is only
   emitted when it evaluates to boolean `true`.
4. Unless **Also Log to Message Logs** is unchecked, the rendered line is written
   to the local API instance message logs at the configured **Level** — exactly
   like OOTB Message Logging.
5. The rendered line is turned into an OTLP `LogRecord` and **enqueued** onto an
   in-memory, per-worker queue.
6. A **background exporter** drains that queue on a timer and issues a real
   `POST <otlp_endpoint>/v1/logs` (OTLP/HTTP + JSON), carrying the ingest
   key/token on the **`api-key`** header.

Logging is **transparent**: the policy never rejects, blocks, or mutates live
traffic. If the endpoint is slow or unreachable, the batch is re-queued and
retried (fail-open) — the client always gets its normal response.

---

## 2. Architecture — asynchronous, non-blocking by design

```
                    REQUEST/RESPONSE PATH (synchronous, hot — zero network I/O)
  client ──▶ [on_request]   for each "Before Calling API" config:
                  │             evaluate Conditional, render Message  ─▶ enqueue()
                  │  Flow::Continue(RequestContext{method, path})
                  ▼
             upstream API
                  │
  client ◀── [on_response]  for each "After Calling API" config:
                  │             evaluate Conditional, render Message  ─▶ enqueue()
                  ▼                                                          │ O(1) push
                                             ┌───────────────────────────────┐
                                             │  thread_local! LOG_QUEUE       │
                                             │  (VecDeque, bounded 10 000)    │
                                             └───────────────────────────────┘
                                                                     ▲  drain_batch()
                    BACKGROUND PATH (asynchronous, off the hot path)  │
             export_loop:  every export_timeout_ms ── timer tick ─────┘
                  │  drain up to batch_max_size records
                  ▼
             OTLP/HTTP  POST  <otlp_endpoint>/v1/logs      (header: api-key)
                  │  (fail-open: requeue batch on non-2xx / transport error)
                  ▼
             OTLP/HTTP observability backend
```

### Memory safety — the payload is read lazily

Reading a request/response **body** can be expensive (large uploads). The policy
therefore binds DataWeave inputs in cost order — `attributes` first, then
`authentication` — and **only reads the body if a Message or Conditional
expression actually references `payload`**. Transactions whose expressions never
touch the body pay zero buffering cost. The export queue is also hard-capped
(10 000 records) so a slow endpoint can never make the gateway leak memory.

---

## 3. Performance & gateway impact

**Forwarding logs off the gateway does not add strain to the request path.** All
the work that could be expensive — serializing records and sending them over the
network — runs in a background task, never in line with client traffic. The only
synchronous, per-request cost is evaluating the DataWeave expressions, which is
the same cost the built-in "Message Logging" policy already incurs.

### Why it's performance-friendly

- **The request/response path does no network I/O.** For each transaction the
  policy only renders the expression and pushes a small record onto an in-memory
  queue — an O(1) operation. The HTTP `POST` to the observability backend happens
  entirely in a separate background task, so the backend's latency, TLS cost, or
  an outage add **zero** latency to the proxied request.
- **Lock-free.** The queue is per-worker (Flex workers are single-threaded), so
  there is no cross-thread locking or contention on the hot path.
- **Request bodies are read lazily.** Buffering a body is the costly operation.
  The policy only reads/buffers the body **if a Message or Conditional expression
  actually references `payload`**. Configurations that touch only headers, method,
  or status never buffer the body, regardless of upload size.
- **Memory is hard-capped.** The buffer holds at most **10,000 records**; if the
  backend is slow or unreachable the oldest record is dropped rather than letting
  memory grow. A broken backend can never OOM the gateway.
- **Exports are batched.** Up to `batch_max_size` records (default **512**) go out
  in a single `POST`, so HTTP/TLS/serialization overhead is amortized across many
  transactions instead of paid per log line.
- **Fail-open.** On any backend error the batch is re-queued and retried later;
  live traffic always gets its normal response and is never blocked or failed.
- **Idle-cheap.** The exporter is timer-driven (it sleeps between flushes, no
  busy-loop), and the compiled WebAssembly module is size-optimized.

### Honest caveats

So the picture isn't overstated — there is some cost, and it lives in known places:

- **DataWeave evaluation is synchronous**, on the request path, once per active
  configuration. It's cheap in-memory CPU work (expressions are compiled once at
  policy load), but at very high request rates with many complex expressions it is
  measurable. This is the same class of cost as the OOTB Message Logging policy —
  forwarding to OTLP adds nothing here.
- **Referencing `payload` opts into buffering the body** on the hot path — an
  unavoidable cost shared by any body-logging policy. Keep `payload` out of
  high-throughput or large-body configurations if you want it free.
- **Background work shares the instance.** Serialization, the outbound `POST`, and
  the (bounded) queue use the same worker's CPU and memory as everything else.
  Under a sustained backend outage you pay that bounded memory and drop the oldest
  logs.

### Tuning knobs

- **`batch_max_size`** and **`export_timeout_ms`** trade export frequency against
  batch size — larger batches / longer intervals mean fewer, bigger POSTs.
- Scope high-cost expressions (especially `payload`) to the configurations and
  phases that truly need them.

---

## 4. Configuration

Configured exactly like OOTB Message Logging, plus the OTLP sink settings.

### Logging Configuration (repeatable list — `loggingConfigurations`)

| Field | Required | Description |
|---|---|---|
| **Configuration Name** (`configurationName`) | ✅ | Friendly name; emitted as the `log.configuration_name` attribute. |
| **Message** (`message`) | ✅ | DataWeave expression producing the content to log. |
| **Conditional** (`conditional`) | | DataWeave expression; the line is logged only when it is `true`. Leave empty to always log. |
| **Category** (`category`) | | Prefix included in the line (`[CATEGORY] message`) and emitted as the `category` attribute. |
| **Level** (`level`) | | `INFO` (default) / `WARN` / `ERROR` / `DEBUG`. Mapped to OTLP severity (DEBUG=5, INFO=9, WARN=13, ERROR=17). |
| **Before Calling API** (`beforeCallingApi`) | | Evaluate in the request phase. Default `false`. |
| **After Calling API** (`afterCallingApi`) | | Evaluate in the response phase (status code available). Default `true`. |
| **Also Log to Message Logs** (`alsoLogToMessageLogs`) | | Also write the rendered line to the local API instance message logs at **Level**, like OOTB Message Logging. Default `true`; uncheck to forward to OTLP only. |

### OTLP ingestion sink

| Field | Required | Description |
|---|---|---|
| **OTLP Ingestion Endpoint** (`otlp_endpoint`) | ✅ | Base URL of the observability platform's OTLP/HTTP receiver. Lines are POSTed to `<endpoint>/v1/logs`. |
| **Ingest Key / Token** (`otlp_api_key`) | ✅ | Ingest credential for the platform, sent verbatim on the `api-key` header. Stored/displayed as a **secret**. |
| **Batch Max Size** (`batch_max_size`) | | Max records per export batch. Default `512`. |
| **Export Timeout (ms)** (`export_timeout_ms`) | | Background flush interval. Default `1000`. |

### DataWeave bindings available to `message` / `conditional`

- `attributes` — `method`, `requestUri`, `requestPath`, `queryParams`,
  `statusCode`, `headers['name']`, `scheme`, `queryString`.
- `payload` — the request/response body (JSON / XML / plain text). Reading it
  triggers the lazy body read described above.
- `authentication` — `clientId`, `clientName`, `principal`, `properties`.

> **Note on `attributes.path` vs `attributes.requestUri`:** `requestUri` is the
> raw request target (the `:path` pseudo-header, e.g. `/orders?q=1`).
> `attributes.path` strips the query string via a URL round-trip; prefer
> `requestUri` for the raw value.

### Example configuration

```yaml
config:
  loggingConfigurations:
    - configurationName: access-log
      message: "#[attributes.method ++ ' ' ++ attributes.requestUri ++ ' -> ' ++ attributes.statusCode]"
      level: INFO
      category: ACCESS
      afterCallingApi: true
    - configurationName: inbound-audit
      message: "#['inbound ' ++ attributes.method]"
      conditional: "#[attributes.headers['x-audit'] == 'true']"
      level: DEBUG
      beforeCallingApi: true
      afterCallingApi: false
  otlp_endpoint: https://otlp.your-observability-platform.example/
  otlp_api_key: YOUR_OTLP_INGEST_KEY_HERE
  batch_max_size: 512
  export_timeout_ms: 1000
```

---

## 5. OTLP payload shape

Each rendered line becomes one OTLP `logRecord`. The rendered Message is the
record `body`; context rides along as attributes:

```jsonc
{ "resourceLogs": [ {
    "resource": { "attributes": [
      { "key": "service.name",        "value": { "stringValue": "<api name>" } },
      { "key": "service.instance.id", "value": { "stringValue": "<flex name>" } }
    ] },
    "scopeLogs": [ {
      "scope": { "name": "omni-log-forwarder", "version": "1.0.0" },
      "logRecords": [ {
        "timeUnixNano": "…",
        "severityNumber": 9, "severityText": "INFO",
        "body": { "stringValue": "GET /orders/42 -> 200" },
        "attributes": [
          { "key": "log.configuration_name",   "value": { "stringValue": "access-log" } },
          { "key": "log.phase",                 "value": { "stringValue": "response" } },
          { "key": "category",                  "value": { "stringValue": "ACCESS" } },
          { "key": "http.request.method",       "value": { "stringValue": "GET" } },
          { "key": "url.path",                  "value": { "stringValue": "/orders/42" } },
          { "key": "http.response.status_code", "value": { "intValue": 200 } }
        ]
      } ]
    } ]
} ] }
```

- `service.name` is the API instance name (falls back to the policy name).
- `service.instance.id` is the Flex Gateway instance name.

---

## 6. Building

```bash
make setup             # one-time: install pinned cargo-anypoint toolchain
make build-asset-files # regenerate src/generated/config.rs from definition/gcl.yaml
make build             # compile the wasm32-wasip1 module
```

Do **not** hand-edit `src/generated/config.rs` — it is regenerated from
`definition/gcl.yaml` on every `build-asset-files` run.

---

## 7. Testing

### Unit tests (no Docker)

```bash
cargo test --lib
```

`src/tests.rs` drives the full request → response → **background export**
lifecycle in-process with `pdk-unit`, mocks the OTLP endpoint with a
`TraceBackend`, and asserts the export carries the `api-key` header and the
rendered DataWeave line. DataWeave config fields are supplied via
`pdk_unit::dw2pel(...)`, which compiles a `#[...]` expression to the PEL wire
format the generated deserializer expects.

### Integration tests (Docker + local-mode registration)

`tests/requests.rs` composes a **real Flex Gateway on port 8081**, an httpmock
upstream, and an httpmock OTLP endpoint, then asserts the endpoint received a
`POST /v1/logs` with the `api-key` header. In the integration test the DataWeave
fields are plain `#[...]` strings — the real gateway compiles them to PEL.

Prerequisite: a local-mode `registration.yaml` in `tests/config/`. If you have a
Flex instance registered in Local Mode, copy its `registration.yaml` there;
otherwise generate one from Runtime Manager → Flex Gateway → Add Gateway (Docker,
`--connected=false`). This file is `.gitignore`d and device-specific.

```bash
make test
```

---

## 8. Running locally (playground)

`playground/config/api.yaml` is pre-wired with two example logging
configurations. Point `otlp_endpoint` at your OTLP/HTTP endpoint and replace
the `otlp_api_key` placeholder with a real ingest key/token to send live data,
provide a local-mode `registration.yaml` in `playground/config/`, then:

```bash
make run
# gateway on http://localhost:8081
curl -i http://localhost:8081/anything/echo/
```

The rendered lines appear in your observability platform's **Logs** view,
filtered by `service.name` (the API instance name).

---

## 9. Installing and deploying to your Anypoint organization

This policy is a **custom Flex Gateway policy**. Deploying it to an org is a
two-part job: (1) get the policy asset into your **Exchange**, then (2) **apply**
it to an API instance in **API Manager** and point your Flex Gateway at it.

There are two ways to get the asset into Exchange. Pick **Option A** if you were
handed the policy already published (you only need to consume it); pick
**Option B** if you have this source project and want to publish it into your own
org yourself.

### Prerequisites

- A **Flex Gateway** registered in your Anypoint organization and running
  (Connected Mode, so it can sync policies from the control plane). Minimum Flex
  runtime version **1.6.1**.
- An API instance in **API Manager** fronting your upstream service (the API you
  want to log).
- Your OTLP ingestion details for the target observability platform:
  - **Endpoint** — any OTLP/HTTP endpoint that accepts `POST /v1/logs` (an
    OpenTelemetry Collector, or your platform's managed OTLP ingest URL).
  - **Ingest key / token** — if the platform requires one, it is sent on the
    `api-key` header.
- For **Option B only**: the developer toolchain installed on your computer
  (Rust + the `wasm32-wasip1` target, Node.js, Anypoint CLI v4 + PDK plugin,
  `cargo-anypoint`) — see [Toolchain you need locally](#toolchain-you-need-locally-option-b)
  below — and a Connected App with the **Exchange Contributor** scope on the
  target org.

### Option A — Consume a policy already published to Exchange

If the policy has already been released to your org's Exchange (asset
`omni-log-forwarder`), you don't build anything — skip straight to
[applying it in API Manager](#applying-the-policy-in-api-manager). Confirm it's
visible under **Exchange → filter by type “Policy”**. If it lives in a *different*
org than your API, share the Exchange asset (or its business group) with the
target org first.

### Option B — Publish this policy into your org from source

<a name="toolchain-you-need-locally-option-b"></a>
#### Toolchain you need locally

`make release` (and `make publish`) first runs `make build`, which **compiles
the Rust source to a WebAssembly module and then packages and uploads it** with
the Anypoint tooling. So publishing from source is a full local build — you need
all of the following installed on your machine:

| Requirement | What it's for | Install / verify |
|---|---|---|
| **Rust toolchain** (stable, `rustc` + `cargo`) | Compiles the policy source. | Install from <https://rustup.rs>. Verify: `rustc --version && cargo --version`. |
| **`wasm32-wasip1` target** | The WebAssembly compile target Flex runs. | `rustup target add wasm32-wasip1` |
| **Node.js** (LTS) + **npm** | Runtime for the Anypoint CLI. | Install Node 18+; verify `node --version`. |
| **Anypoint CLI v4** + **PDK plugin** | Packages the asset and uploads it to Exchange. | `npm i -g anypoint-cli-v4` then `anypoint-cli-v4 plugins:install anypoint-pdk-plugin`. Verify the plugin list shows `anypoint-pdk-plugin`. |
| **`cargo-anypoint`** (pinned) | Generates config bindings and the implementation GCL. | Installed for you by `make setup` (runs `cargo install cargo-anypoint@1.9.0`). |
| **Connected App** with **Exchange Contributor** on the target org | Authenticates the upload. | See step 2 below. |

> **Docker is _not_ required for `make release`.** You only need Docker for
> `make run` (the local playground) and `make test` (integration tests) —
> publishing to Exchange does not use it.

One-time setup once the above are present:

```bash
make setup   # installs the pinned cargo-anypoint (and coverage tooling); runs `cargo fetch`
```

#### Publishing

Run these from the project root. `make release` reads the target org from the
`[package.metadata.anypoint]` block in `Cargo.toml` (`group_id`,
`definition_asset_id`, `implementation_asset_id`) and authenticates with your
`anypoint-cli-v4` credentials.

1. **Point the asset at your org.** Edit `Cargo.toml` →
   `[package.metadata.anypoint]` and set `group_id` to **your** organization (or
   business group) ID. The `definition_asset_id` /`implementation_asset_id` can
   stay as-is.
2. **Log in** to the target org (Connected App with Exchange Contributor):
   ```bash
   anypoint-cli-v4 conf client_id   <CONNECTED_APP_CLIENT_ID>
   anypoint-cli-v4 conf client_secret <CONNECTED_APP_SECRET>
   ```
3. **Publish.** For evaluation use a dev build; for a real handoff cut a release:
   ```bash
   make publish   # dev: assetId gets '-dev', version gets a timestamp — re-runnable
   make release   # production: immutable, version from Cargo.toml (e.g. 1.0.0)
   ```
   A custom policy publishes as **two Exchange assets** — the *definition*
   (`omni-log-forwarder`, the config schema) and the *implementation*
   (`omni-log-forwarder-flex`, the wasm). `make release` uploads both.
   Released versions are **immutable**: bump `version` in `Cargo.toml` before
   re-releasing.

<a name="applying-the-policy-in-api-manager"></a>
### Applying the policy in API Manager

Once the asset is in Exchange, apply it to the API you want to log:

1. **API Manager → your API instance → Policies → Add policy.**
2. In the policy picker, choose **`omni-log-forwarder`** (under *Custom*)
   and select the version you published (e.g. `1.0.0`).
3. Fill in the configuration:
   - **Logging Configuration** — add one entry per log line. Set
     **Configuration Name**, the DataWeave **Message** (e.g.
     `#[attributes.method ++ ' ' ++ attributes.requestUri ++ ' -> ' ++ attributes.statusCode]`),
     optionally a **Conditional**, **Category**, **Level**, and the
     **Before/After Calling API** checkboxes. See
     [§4 Configuration](#4-configuration) for the full field reference and
     [§4 DataWeave bindings](#dataweave-bindings-available-to-message--conditional)
     for what you can reference.
   - **OTLP Ingestion Endpoint** — your platform's OTLP/HTTP receiver URL
     (lines are POSTed to `<endpoint>/v1/logs`).
   - **Ingest Key / Token** — your platform's ingest credential. It's masked in
     the UI (`security:sensitive`).
   - **Batch Max Size** / **Export Timeout (ms)** — leave the defaults (512 /
     1000) unless you have a reason to tune them.
4. **Apply / Save.** The Flex Gateway picks up the policy on its next config
   sync (typically seconds). If it doesn't take effect, on the policy row use the
   kebab menu **⋮ → Check for implementation updates** to force Flex to re-fetch
   the wasm, and confirm in Runtime Manager that the gateway is connected.

You can also apply it as an **automated policy** (org-wide, across all APIs on
the gateway) from **API Manager → Automated Policies** instead of per-API.

### Verify it's forwarding

Send traffic through the gateway, then look in your observability backend:

```bash
curl -i https://<your-api-host>/<path>
```

In your observability platform's **Logs** view, filter by `service.name` (the
API instance name) — you should see the rendered lines within a few seconds. No
lines usually means: a
`conditional` evaluated false, the wrong phase checkbox (request vs response), a
bad endpoint/key (check the Flex logs for `omni-log-forwarder: endpoint
returned status ...`), or the gateway hasn't synced the policy yet.

*Reference: [Uploading Custom Policies to Exchange](https://docs.mulesoft.com/pdk/latest/policies-pdk-publish-policies)
and [Applying Custom Policies](https://docs.mulesoft.com/gateway/latest/flex-gateway-policies-custom).*

---

## 10. Security notes

- `otlp_api_key` is declared **secret** in `definition/gcl.yaml`
  (`security:sensitive`), so Anypoint/API Manager masks it in the UI and never
  renders it in plaintext.
- Never commit a real ingest key/token. The playground and registration files
  ship with the placeholder `YOUR_OTLP_INGEST_KEY_HERE`.
