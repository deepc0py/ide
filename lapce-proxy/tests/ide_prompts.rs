//! Hermetic coverage for the native side of the server -> client UI prompts
//! (`window/showInputBox` / `window/showQuickPick` / `window/showMessageRequest`).
//!
//! No node / extension host is needed: we drive [`PluginHostHandler::process_request`]
//! directly and assert that each prompt method (a) registers a pending prompt +
//! emits the matching [`CoreNotification`], and (b) a subsequent
//! `IdePromptResponse` (via [`PluginCatalogRpcHandler::resolve_ide_prompt`], the
//! exact call `dispatch.rs` makes) resolves the host's reply channel with the
//! contract JSON. Cancellation is modelled by resolving with JSON `null`.

use crossbeam_channel::{Receiver, Sender};
use jsonrpc_lite::{JsonRpc, Params};
use lapce_proxy::plugin::psp::{
    PluginHostHandler, PluginServerRpcHandler, ResponseSender,
};
use lapce_rpc::{
    RpcError,
    core::{CoreNotification, CoreRpc, CoreRpcHandler},
    plugin::VoltID,
    proxy::ProxyRpcHandler,
};
use lapce_proxy::plugin::PluginCatalogRpcHandler;
use serde_json::{Value, json};

struct Harness {
    host: PluginHostHandler,
    core_rpc: CoreRpcHandler,
    catalog_rpc: PluginCatalogRpcHandler,
    #[allow(dead_code)]
    io_rx: Receiver<JsonRpc>,
}

fn harness() -> Harness {
    let core_rpc = CoreRpcHandler::new();
    let proxy_rpc = ProxyRpcHandler::new();
    let catalog_rpc =
        PluginCatalogRpcHandler::new(core_rpc.clone(), proxy_rpc.clone());

    let volt_id = VoltID {
        author: "test".to_string(),
        name: "sonar".to_string(),
    };
    let (io_tx, io_rx): (Sender<JsonRpc>, Receiver<JsonRpc>) =
        crossbeam_channel::unbounded();
    let server_rpc =
        PluginServerRpcHandler::new(volt_id.clone(), None, None, io_tx);

    let host = PluginHostHandler::new(
        None,
        None,
        volt_id,
        "Sonar".to_string(),
        Vec::new(),
        core_rpc.clone(),
        server_rpc,
        catalog_rpc.clone(),
    );

    Harness {
        host,
        core_rpc,
        catalog_rpc,
        io_rx,
    }
}

/// A reply channel standing in for the extension host's awaited response.
fn reply_channel() -> (ResponseSender, Receiver<Result<Value, RpcError>>) {
    let (tx, rx) = crossbeam_channel::unbounded();
    (ResponseSender::new(tx), rx)
}

fn next_notification(core_rpc: &CoreRpcHandler) -> CoreNotification {
    match core_rpc.rx().try_recv().expect("a core notification") {
        CoreRpc::Notification(n) => *n,
        _ => panic!("expected a core notification, got another CoreRpc variant"),
    }
}

fn params(value: Value) -> Params {
    serde_json::from_value(value).expect("params")
}

#[test]
fn input_box_registers_and_resolves_with_value() {
    let mut h = harness();
    let (resp, rx) = reply_channel();

    h.host
        .process_request(
            "window/showInputBox".to_string(),
            params(json!({
                "title": "SonarQube token",
                "prompt": "Paste your token",
                "placeHolder": "squ_...",
                "password": true,
            })),
            resp,
        )
        .expect("process_request");

    let id = match next_notification(&h.core_rpc) {
        CoreNotification::IdeShowInputBox {
            id,
            title,
            place_holder,
            password,
            ..
        } => {
            assert_eq!(title.as_deref(), Some("SonarQube token"));
            // camelCase `placeHolder` is parsed off the wire.
            assert_eq!(place_holder.as_deref(), Some("squ_..."));
            assert!(password);
            id
        }
        other => panic!("unexpected: {other:?}"),
    };

    // No reply yet.
    assert!(rx.try_recv().is_err());

    // The app returns the user's text; the host reply must be {"value": ...}.
    h.catalog_rpc
        .resolve_ide_prompt(id, json!({ "value": "squ_secret" }));
    assert_eq!(
        rx.recv().unwrap().unwrap(),
        json!({ "value": "squ_secret" })
    );
}

#[test]
fn quick_pick_registers_and_resolves_with_handle() {
    let mut h = harness();
    let (resp, rx) = reply_channel();

    h.host
        .process_request(
            "window/showQuickPick".to_string(),
            params(json!({
                "placeHolder": "Choose a server",
                "items": [
                    { "label": "SonarCloud", "handle": 10 },
                    { "label": "Self-managed", "description": "on-prem", "handle": 20 },
                ],
            })),
            resp,
        )
        .expect("process_request");

    let id = match next_notification(&h.core_rpc) {
        CoreNotification::IdeShowQuickPick { id, items, .. } => {
            assert_eq!(items.len(), 2);
            assert_eq!(items[1].handle, 20);
            assert_eq!(items[1].description.as_deref(), Some("on-prem"));
            id
        }
        other => panic!("unexpected: {other:?}"),
    };

    h.catalog_rpc.resolve_ide_prompt(id, json!({ "handle": 20 }));
    assert_eq!(rx.recv().unwrap().unwrap(), json!({ "handle": 20 }));
}

#[test]
fn message_request_registers_and_resolves_with_title() {
    let mut h = harness();
    let (resp, rx) = reply_channel();

    h.host
        .process_request(
            "window/showMessageRequest".to_string(),
            params(json!({
                "type": 3,
                "message": "Bind this project to SonarQube?",
                "modal": true,
                "actions": [ { "title": "Bind" }, { "title": "Not now" } ],
            })),
            resp,
        )
        .expect("process_request");

    let id = match next_notification(&h.core_rpc) {
        CoreNotification::IdeShowMessageRequest {
            id,
            typ,
            modal,
            actions,
            ..
        } => {
            assert_eq!(typ, 3);
            assert!(modal);
            assert_eq!(actions, vec!["Bind".to_string(), "Not now".to_string()]);
            id
        }
        other => panic!("unexpected: {other:?}"),
    };

    h.catalog_rpc.resolve_ide_prompt(id, json!({ "title": "Bind" }));
    assert_eq!(rx.recv().unwrap().unwrap(), json!({ "title": "Bind" }));
}

#[test]
fn cancel_resolves_with_null() {
    let mut h = harness();
    let (resp, rx) = reply_channel();

    h.host
        .process_request(
            "window/showInputBox".to_string(),
            params(json!({ "title": "Token" })),
            resp,
        )
        .expect("process_request");

    let id = match next_notification(&h.core_rpc) {
        CoreNotification::IdeShowInputBox { id, .. } => id,
        other => panic!("unexpected: {other:?}"),
    };

    // Esc / dismiss maps to a JSON `null` reply.
    h.catalog_rpc.resolve_ide_prompt(id, Value::Null);
    assert_eq!(rx.recv().unwrap().unwrap(), Value::Null);
}

#[test]
fn cancel_all_answers_pending_prompts_with_null() {
    let mut h = harness();
    let (resp, rx) = reply_channel();

    h.host
        .process_request(
            "window/showQuickPick".to_string(),
            params(json!({ "items": [ { "label": "X", "handle": 1 } ] })),
            resp,
        )
        .expect("process_request");

    // Drain the notification so the registry is the only thing holding `resp`.
    let _ = next_notification(&h.core_rpc);

    // Window close path: every still-pending prompt is answered with null.
    h.catalog_rpc.cancel_ide_prompts();
    assert_eq!(rx.recv().unwrap().unwrap(), Value::Null);
}
