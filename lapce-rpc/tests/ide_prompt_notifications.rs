//! Serde round-trip coverage for the three server -> client UI prompt
//! [`CoreNotification`] variants, pinning their exact wire `method` tags.

use lapce_rpc::{
    core::CoreNotification,
    ide_ext::QuickPickItem,
};
use serde_json::json;

fn method_tag(notification: &CoreNotification) -> String {
    let value = serde_json::to_value(notification).unwrap();
    value
        .get("method")
        .and_then(|m| m.as_str())
        .unwrap()
        .to_string()
}

#[test]
fn ide_show_input_box_round_trips() {
    let notification = CoreNotification::IdeShowInputBox {
        id: 7,
        title: Some("Token".to_string()),
        prompt: Some("Enter token".to_string()),
        place_holder: Some("paste here".to_string()),
        value: None,
        password: true,
    };
    assert_eq!(method_tag(&notification), "ide_show_input_box");

    let value = serde_json::to_value(&notification).unwrap();
    assert_eq!(
        value,
        json!({
            "method": "ide_show_input_box",
            "params": {
                "id": 7,
                "title": "Token",
                "prompt": "Enter token",
                "place_holder": "paste here",
                "value": null,
                "password": true,
            }
        })
    );

    let back: CoreNotification = serde_json::from_value(value).unwrap();
    match back {
        CoreNotification::IdeShowInputBox { id, password, .. } => {
            assert_eq!(id, 7);
            assert!(password);
        }
        other => panic!("unexpected variant: {other:?}"),
    }
}

#[test]
fn ide_show_quick_pick_round_trips() {
    let notification = CoreNotification::IdeShowQuickPick {
        id: 11,
        title: None,
        place_holder: Some("Pick one".to_string()),
        items: vec![
            QuickPickItem {
                label: "Alpha".to_string(),
                description: Some("first".to_string()),
                detail: None,
                handle: 1,
            },
            QuickPickItem {
                label: "Beta".to_string(),
                description: None,
                detail: Some("second".to_string()),
                handle: 2,
            },
        ],
    };
    assert_eq!(method_tag(&notification), "ide_show_quick_pick");

    let value = serde_json::to_value(&notification).unwrap();
    let back: CoreNotification = serde_json::from_value(value).unwrap();
    match back {
        CoreNotification::IdeShowQuickPick { id, items, .. } => {
            assert_eq!(id, 11);
            assert_eq!(items.len(), 2);
            assert_eq!(items[1].handle, 2);
        }
        other => panic!("unexpected variant: {other:?}"),
    }
}

#[test]
fn ide_show_message_request_round_trips() {
    let notification = CoreNotification::IdeShowMessageRequest {
        id: 42,
        typ: 2,
        message: "Install now?".to_string(),
        modal: true,
        actions: vec!["Install".to_string(), "Later".to_string()],
    };
    assert_eq!(method_tag(&notification), "ide_show_message_request");

    let value = serde_json::to_value(&notification).unwrap();
    assert_eq!(
        value,
        json!({
            "method": "ide_show_message_request",
            "params": {
                "id": 42,
                "typ": 2,
                "message": "Install now?",
                "modal": true,
                "actions": ["Install", "Later"],
            }
        })
    );

    let back: CoreNotification = serde_json::from_value(value).unwrap();
    match back {
        CoreNotification::IdeShowMessageRequest { id, actions, .. } => {
            assert_eq!(id, 42);
            assert_eq!(actions, vec!["Install", "Later"]);
        }
        other => panic!("unexpected variant: {other:?}"),
    }
}
