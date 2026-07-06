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
    tracegate::policy::types::{DenyResponse, PolicyEvent},
};

struct Component;

impl Guest for Component {
    fn before_request(request: RequestPolicyInput) -> RequestPolicyDecision {
        let header_name = config_value(&request, "header").unwrap_or("x-api-key");
        let expected = config_value(&request, "expected").unwrap_or("tracegate-demo-key");
        let message = config_value(&request, "message").unwrap_or("missing or invalid API key");
        let has_valid_key = request.headers.iter().any(|header| {
            header.name.eq_ignore_ascii_case(header_name) && header.value == expected
        });

        if has_valid_key {
            RequestPolicyDecision {
                allow: true,
                deny: None,
                set_headers: Vec::new(),
                remove_headers: Vec::new(),
                events: vec![PolicyEvent {
                    name: "api-key-accepted".to_owned(),
                    code: Some("auth".to_owned()),
                }],
            }
        } else {
            RequestPolicyDecision {
                allow: false,
                deny: Some(DenyResponse {
                    status: 403,
                    message: message.to_owned(),
                }),
                set_headers: Vec::new(),
                remove_headers: Vec::new(),
                events: vec![PolicyEvent {
                    name: "api-key-denied".to_owned(),
                    code: Some("auth".to_owned()),
                }],
            }
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

bindings::export!(Component with_types_in bindings);
