# TraceGate Plugin Authoring

TraceGate v1 supports one stable WASM hook:

```text
before_request(request: RequestPolicyInput) -> RequestPolicyDecision
```

Plugins are compiled as `wasm32-wasip2` components and loaded by path from `tracegate.toml`.

## Capabilities

A plugin can:

- allow a request
- deny a request with a status and message
- set request headers
- remove request headers
- emit structured policy events

A plugin cannot:

- access the filesystem
- access the network
- read environment variables
- inherit process handles
- bypass timeout, fuel, or memory limits
- receive raw sensitive headers unless explicitly allowlisted in its own config

## Example Build

```powershell
cargo build --manifest-path examples/plugins/api-key-guard/Cargo.toml --target wasm32-wasip2 --release
cargo build --manifest-path examples/plugins/header-normalizer/Cargo.toml --target wasm32-wasip2 --release
```

Inspect compatibility:

```powershell
cargo run -p tracegate -- plugins inspect examples/plugins/api-key-guard/target/wasm32-wasip2/release/tracegate_api_key_guard.wasm --json
```

## Configuration

```toml
[[plugins]]
id = "api-key-guard"
path = "/usr/local/share/tracegate/plugins/api-key-guard.wasm"
hook = "before_request"
routes = ["payments"]
timeout_ms = 50
memory_limit_bytes = 16777216
fuel = 10000000
body_preview_bytes = 0
raw_headers = ["x-api-key"]
config = { header = "x-api-key", expected = "tracegate-demo-key", message = "missing or invalid API key" }
```

Keep plugin config values out of public APIs. The Console exposes config keys, not config values.
