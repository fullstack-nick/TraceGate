#![no_std]

extern crate alloc;

mod bindings {
    wit_bindgen::generate!({
        path: "../../../crates/tracegate-wasm/wit",
        world: "policy-plugin",
    });
}

use alloc::{borrow::ToOwned, vec, vec::Vec};
use bindings::{
    Guest, RequestPolicyDecision, RequestPolicyInput,
    tracegate::policy::types::{Header, PolicyEvent},
};

struct Component;

impl Guest for Component {
    fn before_request(request: RequestPolicyInput) -> RequestPolicyDecision {
        let spin_marker = spin(spin_iterations(&request));
        let mut set_headers = Vec::new();
        if let (Some(name), Some(value)) = (
            config_value(&request, "set_header"),
            config_value(&request, "set_value"),
        ) {
            set_headers.push(Header {
                name: name.to_owned(),
                value: value.to_owned(),
            });
        }

        let remove_headers = config_value(&request, "remove_header")
            .map(|value| vec![value.to_owned()])
            .unwrap_or_default();

        let body_seen = request
            .body_preview
            .as_ref()
            .map(|body| !body.is_empty())
            .unwrap_or(false);
        let event_name = if body_seen {
            "headers-normalized-with-body-preview"
        } else {
            "headers-normalized"
        };
        let event_code = if spin_marker == u64::MAX {
            "headers-spin"
        } else {
            "headers"
        };

        RequestPolicyDecision {
            allow: true,
            deny: None,
            set_headers,
            remove_headers,
            events: vec![PolicyEvent {
                name: event_name.to_owned(),
                code: Some(event_code.to_owned()),
            }],
        }
    }
}

fn config_value<'a>(request: &'a RequestPolicyInput, key: &str) -> Option<&'a str> {
    request
        .config
        .iter()
        .find(|value| value.key == key)
        .map(|value| value.value.as_str())
}

fn spin_iterations(request: &RequestPolicyInput) -> u64 {
    config_value(request, "spin_iterations")
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0)
}

fn spin(iterations: u64) -> u64 {
    let mut index = 0_u64;
    let mut marker = 0_u64;
    while index < iterations {
        marker = marker.wrapping_add(core::hint::black_box(index));
        index = index.wrapping_add(1);
    }
    marker
}

bindings::export!(Component with_types_in bindings);
