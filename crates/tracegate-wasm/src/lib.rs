use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use http::{HeaderName, HeaderValue, StatusCode};
use serde::Serialize;
use thiserror::Error;
use tokio::time::timeout;
use tracegate_core::{PluginConfig, PluginConfigValue, PluginHook};
use wasmtime::{
    Config, Engine, Store, StoreLimits, StoreLimitsBuilder,
    component::{Component, Linker, ResourceTable},
};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

mod bindings {
    wasmtime::component::bindgen!({
        path: "wit",
        world: "policy-plugin",
    });
}

use bindings::tracegate::policy::types as wit;

#[derive(Debug, Error)]
pub enum PolicyError {
    #[error("failed to initialize Wasmtime engine: {0}")]
    Engine(String),
    #[error("failed to load plugin `{plugin_id}` from `{path}`: {reason}")]
    Load {
        plugin_id: String,
        path: PathBuf,
        reason: String,
    },
    #[error("plugin `{plugin_id}` is not compatible with the TraceGate policy contract: {reason}")]
    Contract { plugin_id: String, reason: String },
    #[error("plugin `{plugin_id}` failed: {reason}")]
    Invocation { plugin_id: String, reason: String },
}

#[derive(Clone)]
pub struct PolicyEngine {
    plugins: Vec<Arc<LoadedPlugin>>,
    route_plugins: HashMap<String, Vec<usize>>,
}

struct LoadedPlugin {
    engine: Engine,
    config: PluginConfig,
    component: Component,
}

struct StoreState {
    limits: StoreLimits,
    wasi: WasiCtx,
    table: ResourceTable,
}

impl WasiView for StoreState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

#[derive(Clone, Debug)]
pub struct PolicyRequest {
    pub route_id: String,
    pub request_id: String,
    pub method: String,
    pub path: String,
    pub query: Option<String>,
    pub headers: Vec<PolicyHeader>,
    pub sensitive_headers: Vec<String>,
    pub client_address: String,
    pub body_preview: Option<Vec<u8>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PolicyHeader {
    pub name: String,
    pub value: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HeaderMutation {
    pub name: String,
    pub value: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PolicyDeny {
    pub status: u16,
    pub message: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PolicyDecisionRecord {
    pub plugin_id: String,
    pub route_id: String,
    pub action: String,
    pub deny_status: Option<u16>,
    pub set_headers: Vec<String>,
    pub remove_headers: Vec<String>,
    pub events: Vec<PolicyEvent>,
    pub duration: Duration,
    pub timed_out: bool,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PolicyEvent {
    pub name: String,
    pub code: Option<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PolicyEvaluation {
    pub denied: Option<PolicyDeny>,
    pub set_headers: Vec<HeaderMutation>,
    pub remove_headers: Vec<String>,
    pub records: Vec<PolicyDecisionRecord>,
}

#[derive(Clone, Debug, Serialize)]
pub struct PluginInspection {
    pub path: String,
    pub compatible: bool,
    pub contract: String,
    pub imports: Vec<String>,
    pub exports: Vec<String>,
}

impl PolicyEngine {
    pub fn new(configs: &[PluginConfig]) -> Result<Self, PolicyError> {
        let engine = policy_engine()?;
        let mut plugins = Vec::with_capacity(configs.len());
        let mut route_plugins: HashMap<String, Vec<usize>> = HashMap::new();

        for config in configs {
            if config.hook != PluginHook::BeforeRequest {
                continue;
            }

            let component =
                Component::from_file(&engine, &config.path).map_err(|err| PolicyError::Load {
                    plugin_id: config.id.clone(),
                    path: config.path.clone(),
                    reason: err.to_string(),
                })?;
            validate_component(&engine, &component, config)?;

            let index = plugins.len();
            for route in &config.routes {
                route_plugins.entry(route.clone()).or_default().push(index);
            }
            plugins.push(Arc::new(LoadedPlugin {
                engine: engine.clone(),
                config: config.clone(),
                component,
            }));
        }

        Ok(Self {
            plugins,
            route_plugins,
        })
    }

    pub fn empty() -> Result<Self, PolicyError> {
        Ok(Self {
            plugins: Vec::new(),
            route_plugins: HashMap::new(),
        })
    }

    pub fn has_plugins_for_route(&self, route_id: &str) -> bool {
        self.route_plugins
            .get(route_id)
            .map(|plugins| !plugins.is_empty())
            .unwrap_or(false)
    }

    pub fn max_body_preview_bytes(&self, route_id: &str) -> u64 {
        self.route_plugins
            .get(route_id)
            .into_iter()
            .flatten()
            .filter_map(|index| self.plugins.get(*index))
            .map(|plugin| plugin.config.body_preview_bytes)
            .max()
            .unwrap_or(0)
    }

    pub async fn evaluate(&self, request: PolicyRequest) -> PolicyEvaluation {
        let mut evaluation = PolicyEvaluation::default();
        let Some(plugin_indexes) = self.route_plugins.get(&request.route_id) else {
            return evaluation;
        };

        for plugin_index in plugin_indexes {
            let Some(plugin) = self.plugins.get(*plugin_index).cloned() else {
                continue;
            };
            let input = request.input_for(&plugin.config);
            let route_id = request.route_id.clone();
            let plugin_id = plugin.config.id.clone();
            let interrupt_engine = plugin.engine.clone();
            let started = Instant::now();
            let timeout_duration = plugin.config.timeout;
            let task = tokio::task::spawn_blocking(move || plugin.call(input));

            let result = match timeout(timeout_duration, task).await {
                Ok(Ok(Ok(decision))) => {
                    let record = record_from_decision(
                        &plugin_id,
                        &route_id,
                        &decision,
                        started.elapsed(),
                        false,
                        None,
                    );
                    Ok((decision, record))
                }
                Ok(Ok(Err(err))) => {
                    let reason = err.to_string();
                    let record = error_record(
                        &plugin_id,
                        &route_id,
                        "deny",
                        started.elapsed(),
                        false,
                        reason,
                    );
                    Err(record)
                }
                Ok(Err(err)) => {
                    let record = error_record(
                        &plugin_id,
                        &route_id,
                        "deny",
                        started.elapsed(),
                        false,
                        err.to_string(),
                    );
                    Err(record)
                }
                Err(_) => {
                    interrupt_engine.increment_epoch();
                    let record = error_record(
                        &plugin_id,
                        &route_id,
                        "deny",
                        timeout_duration,
                        true,
                        "plugin invocation timed out".to_owned(),
                    );
                    Err(record)
                }
            };

            match result {
                Ok((decision, record)) => {
                    evaluation
                        .set_headers
                        .extend(decision.set_headers.iter().cloned());
                    evaluation
                        .remove_headers
                        .extend(decision.remove_headers.iter().cloned());
                    let denied = decision.deny.clone();
                    evaluation.records.push(record);
                    if let Some(deny) = denied {
                        evaluation.denied = Some(deny);
                        break;
                    }
                }
                Err(record) => {
                    evaluation.denied = Some(PolicyDeny {
                        status: 403,
                        message: "request denied by policy".to_owned(),
                    });
                    evaluation.records.push(record);
                    break;
                }
            }
        }

        evaluation
    }

    pub fn inspect(path: impl AsRef<Path>) -> Result<PluginInspection, PolicyError> {
        let path = path.as_ref();
        let config = PluginConfig {
            id: "inspect".to_owned(),
            path: path.to_path_buf(),
            hook: PluginHook::BeforeRequest,
            routes: vec!["inspect".to_owned()],
            timeout: Duration::from_millis(5),
            memory_limit_bytes: 16 * 1024 * 1024,
            fuel: 1_000_000,
            body_preview_bytes: 0,
            raw_headers: Vec::new(),
            config: Vec::new(),
        };
        let engine = policy_engine()?;
        let component = Component::from_file(&engine, path).map_err(|err| PolicyError::Load {
            plugin_id: config.id.clone(),
            path: path.to_path_buf(),
            reason: err.to_string(),
        })?;
        let inspection = inspect_component(path, &engine, &component);
        validate_component(&engine, &component, &config)?;
        Ok(inspection)
    }
}

fn inspect_component(path: &Path, engine: &Engine, component: &Component) -> PluginInspection {
    let component_type = component.component_type();
    let imports = component_import_names(engine, component);
    let exports = component_type
        .exports(engine)
        .map(|(name, _)| name.to_owned())
        .collect();

    PluginInspection {
        path: path.display().to_string(),
        compatible: true,
        contract: "tracegate:policy/policy-plugin@0.1.0".to_owned(),
        imports,
        exports,
    }
}

fn component_import_names(engine: &Engine, component: &Component) -> Vec<String> {
    component
        .component_type()
        .imports(engine)
        .map(|(name, _)| name.to_owned())
        .collect()
}

fn is_allowed_component_import(name: &str) -> bool {
    name.starts_with("tracegate:policy/types@")
        || name.starts_with("wasi:io/poll@")
        || name.starts_with("wasi:io/error@")
        || name.starts_with("wasi:io/streams@")
        || name.starts_with("wasi:cli/environment@")
        || name.starts_with("wasi:cli/exit@")
        || name.starts_with("wasi:cli/stdin@")
        || name.starts_with("wasi:cli/stdout@")
        || name.starts_with("wasi:cli/stderr@")
        || name.starts_with("wasi:cli/terminal-input@")
        || name.starts_with("wasi:cli/terminal-output@")
        || name.starts_with("wasi:cli/terminal-stdin@")
        || name.starts_with("wasi:cli/terminal-stdout@")
        || name.starts_with("wasi:cli/terminal-stderr@")
}

impl LoadedPlugin {
    fn call(&self, input: wit::RequestPolicyInput) -> Result<NormalizedDecision, PolicyError> {
        let mut store = store_for(&self.engine, &self.config, &self.config.id)?;
        let linker = linker_for(&self.engine, &self.config.id)?;
        let bindings = bindings::PolicyPlugin::instantiate(&mut store, &self.component, &linker)
            .map_err(|err| PolicyError::Invocation {
                plugin_id: self.config.id.clone(),
                reason: err.to_string(),
            })?;
        let decision = bindings
            .call_before_request(&mut store, &input)
            .map_err(|err| PolicyError::Invocation {
                plugin_id: self.config.id.clone(),
                reason: err.to_string(),
            })?;
        normalize_decision(decision, &self.config.id)
    }
}

impl PolicyRequest {
    fn input_for(&self, config: &PluginConfig) -> wit::RequestPolicyInput {
        let sensitive_headers = self
            .sensitive_headers
            .iter()
            .map(|value| value.to_ascii_lowercase())
            .collect::<HashSet<_>>();
        let raw_headers = config
            .raw_headers
            .iter()
            .map(|value| value.to_ascii_lowercase())
            .collect::<HashSet<_>>();
        let headers = self
            .headers
            .iter()
            .filter(|header| {
                !sensitive_headers.contains(&header.name) || raw_headers.contains(&header.name)
            })
            .map(|header| wit::Header {
                name: header.name.clone(),
                value: header.value.clone(),
            })
            .collect();
        let body_preview = self.body_preview.as_ref().and_then(|body| {
            if config.body_preview_bytes == 0 {
                None
            } else {
                let limit = config.body_preview_bytes.min(usize::MAX as u64) as usize;
                Some(body.iter().copied().take(limit).collect::<Vec<_>>())
            }
        });

        wit::RequestPolicyInput {
            route_id: self.route_id.clone(),
            request_id: self.request_id.clone(),
            method: self.method.clone(),
            path: self.path.clone(),
            query: self.query.clone(),
            headers,
            config: config_values(&config.config),
            client_address: self.client_address.clone(),
            body_preview,
        }
    }
}

#[derive(Clone, Debug)]
struct NormalizedDecision {
    deny: Option<PolicyDeny>,
    set_headers: Vec<HeaderMutation>,
    remove_headers: Vec<String>,
    events: Vec<PolicyEvent>,
}

fn validate_component(
    engine: &Engine,
    component: &Component,
    config: &PluginConfig,
) -> Result<(), PolicyError> {
    let disallowed_imports = component_import_names(engine, component)
        .into_iter()
        .filter(|name| !is_allowed_component_import(name))
        .collect::<Vec<_>>();
    if !disallowed_imports.is_empty() {
        return Err(PolicyError::Contract {
            plugin_id: config.id.clone(),
            reason: format!("disallowed imports: {}", disallowed_imports.join(", ")),
        });
    }

    let mut store = store_for(engine, config, &config.id)?;
    let linker = linker_for(engine, &config.id)?;
    bindings::PolicyPlugin::instantiate(&mut store, component, &linker)
        .map(|_| ())
        .map_err(|err| PolicyError::Contract {
            plugin_id: config.id.clone(),
            reason: err.to_string(),
        })
}

fn store_for(
    engine: &Engine,
    config: &PluginConfig,
    plugin_id: &str,
) -> Result<Store<StoreState>, PolicyError> {
    let limits = StoreLimitsBuilder::new()
        .memory_size(config.memory_limit_bytes as usize)
        .instances(32)
        .memories(16)
        .tables(32)
        .build();
    let mut store = Store::new(
        engine,
        StoreState {
            limits,
            wasi: WasiCtxBuilder::new().build(),
            table: ResourceTable::new(),
        },
    );
    store.limiter(|state| &mut state.limits);
    store.set_epoch_deadline(1);
    store
        .set_fuel(config.fuel)
        .map_err(|err| PolicyError::Contract {
            plugin_id: plugin_id.to_owned(),
            reason: err.to_string(),
        })?;
    Ok(store)
}

fn linker_for(engine: &Engine, plugin_id: &str) -> Result<Linker<StoreState>, PolicyError> {
    let mut linker = Linker::new(engine);
    wasmtime_wasi::p2::add_to_linker_sync(&mut linker).map_err(|err| PolicyError::Contract {
        plugin_id: plugin_id.to_owned(),
        reason: err.to_string(),
    })?;
    Ok(linker)
}

fn policy_engine() -> Result<Engine, PolicyError> {
    let mut config = Config::new();
    config.wasm_component_model(true);
    config.consume_fuel(true);
    config.epoch_interruption(true);
    Engine::new(&config).map_err(|err| PolicyError::Engine(err.to_string()))
}

fn config_values(values: &[PluginConfigValue]) -> Vec<wit::KeyValue> {
    values
        .iter()
        .map(|value| wit::KeyValue {
            key: value.key.clone(),
            value: value.value.clone(),
        })
        .collect()
}

fn normalize_decision(
    decision: wit::RequestPolicyDecision,
    plugin_id: &str,
) -> Result<NormalizedDecision, PolicyError> {
    let deny = decision.deny.map(|deny| PolicyDeny {
        status: if (400..=599).contains(&deny.status) {
            deny.status
        } else {
            403
        },
        message: truncate(deny.message, 512),
    });
    let set_headers = decision
        .set_headers
        .into_iter()
        .map(|header| normalize_set_header(plugin_id, header.name, header.value))
        .collect::<Result<Vec<_>, _>>()?;
    let remove_headers = decision
        .remove_headers
        .into_iter()
        .map(|header| normalize_remove_header(plugin_id, header))
        .collect::<Result<Vec<_>, _>>()?;
    let events = decision
        .events
        .into_iter()
        .map(|event| PolicyEvent {
            name: truncate(event.name, 128),
            code: event.code.map(|code| truncate(code, 128)),
        })
        .collect();

    Ok(NormalizedDecision {
        deny,
        set_headers,
        remove_headers,
        events,
    })
}

fn normalize_set_header(
    plugin_id: &str,
    name: String,
    value: String,
) -> Result<HeaderMutation, PolicyError> {
    let name = normalize_remove_header(plugin_id, name)?;
    HeaderValue::from_str(&value).map_err(|err| PolicyError::Invocation {
        plugin_id: plugin_id.to_owned(),
        reason: format!("invalid set header `{name}` value: {err}"),
    })?;
    Ok(HeaderMutation {
        name,
        value: truncate(value, 4096),
    })
}

fn normalize_remove_header(plugin_id: &str, name: String) -> Result<String, PolicyError> {
    let name = name.trim().to_ascii_lowercase();
    HeaderName::from_bytes(name.as_bytes()).map_err(|err| PolicyError::Invocation {
        plugin_id: plugin_id.to_owned(),
        reason: format!("invalid header name `{name}`: {err}"),
    })?;
    Ok(name)
}

fn record_from_decision(
    plugin_id: &str,
    route_id: &str,
    decision: &NormalizedDecision,
    duration: Duration,
    timed_out: bool,
    error: Option<String>,
) -> PolicyDecisionRecord {
    let action = if decision.deny.is_some() {
        "deny"
    } else {
        "allow"
    };
    PolicyDecisionRecord {
        plugin_id: plugin_id.to_owned(),
        route_id: route_id.to_owned(),
        action: action.to_owned(),
        deny_status: decision.deny.as_ref().map(|deny| deny.status),
        set_headers: decision
            .set_headers
            .iter()
            .map(|header| header.name.clone())
            .collect(),
        remove_headers: decision.remove_headers.clone(),
        events: decision.events.clone(),
        duration,
        timed_out,
        error,
    }
}

fn error_record(
    plugin_id: &str,
    route_id: &str,
    action: &str,
    duration: Duration,
    timed_out: bool,
    error: String,
) -> PolicyDecisionRecord {
    PolicyDecisionRecord {
        plugin_id: plugin_id.to_owned(),
        route_id: route_id.to_owned(),
        action: action.to_owned(),
        deny_status: Some(403),
        set_headers: Vec::new(),
        remove_headers: Vec::new(),
        events: Vec::new(),
        duration,
        timed_out,
        error: Some(truncate(error, 512)),
    }
}

fn truncate(value: String, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        value
    } else {
        value.chars().take(max_chars).collect()
    }
}

pub fn status_from_deny(deny: &PolicyDeny) -> StatusCode {
    StatusCode::from_u16(deny.status).unwrap_or(StatusCode::FORBIDDEN)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn import_allowlist_rejects_host_capabilities() {
        assert!(is_allowed_component_import("tracegate:policy/types@0.1.0"));
        assert!(is_allowed_component_import("wasi:io/poll@0.2.6"));
        assert!(is_allowed_component_import("wasi:cli/environment@0.2.6"));

        assert!(!is_allowed_component_import(
            "wasi:filesystem/preopens@0.2.6"
        ));
        assert!(!is_allowed_component_import(
            "wasi:sockets/tcp-create-socket@0.2.6"
        ));
        assert!(!is_allowed_component_import(
            "wasi:http/outgoing-handler@0.2.6"
        ));
    }

    #[test]
    fn normalize_decision_rejects_invalid_header_mutations() {
        let decision = wit::RequestPolicyDecision {
            allow: true,
            deny: None,
            set_headers: vec![wit::Header {
                name: "bad header".to_owned(),
                value: "value".to_owned(),
            }],
            remove_headers: Vec::new(),
            events: Vec::new(),
        };

        let err = normalize_decision(decision, "test-plugin").unwrap_err();
        assert!(err.to_string().contains("invalid header name"));
    }
}
