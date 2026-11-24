use nabu_protocol::{ClientMessage, Credits, Event, Request};
use serde_json::{Value, json};

#[test]
fn deepai_rpc_names_match_the_ui_contract() {
    for method in [
        "deepAiAccounts",
        "deepAiCredits",
        "deepAiLogin",
        "deepAiRemoveAll",
        "deepAiCheckAll",
    ] {
        let request: ClientMessage =
            serde_json::from_value(json!({"id":1,"method":method})).unwrap();
        assert_eq!(serde_json::to_value(request).unwrap()["method"], method);
    }
    let request: ClientMessage = serde_json::from_value(
        json!({"id":2,"method":"deepAiRemove","params":{"email":"DeepAI fixture"}}),
    )
    .unwrap();
    assert!(
        matches!(request.request, Request::DeepAiRemove { email, id: None } if email == "DeepAI fixture")
    );
}

#[test]
fn deepai_events_send_only_public_accounts_and_balances() {
    let events = [
        Event::DeepAiAccounts { accounts: vec![] },
        Event::DeepAiCredits {
            credits: Some(Credits {
                remaining: 77.98257,
                total: 78.0,
                reset: Some("2026-11-18T00:00:00.000Z".into()),
            }),
        },
        Event::DeepAiLoginFailed {
            message: "Sign-in failed".into(),
        },
    ];
    for (event, name) in
        events
            .into_iter()
            .zip(["deepAiAccounts", "deepAiCredits", "deepAiLoginFailed"])
    {
        let value: Value = serde_json::to_value(event).unwrap();
        assert_eq!(value["type"], name);
        assert!(value.get("credentials").is_none());
        if name == "deepAiCredits" {
            assert_eq!(value["credits"]["reset"], "2026-11-18T00:00:00.000Z");
        }
        serde_json::from_value::<Event>(value).unwrap();
    }
}
