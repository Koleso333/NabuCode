use ncore_deepaiacc::Credentials;
use serde_json::{Value, json};

#[test]
fn live_flash_record_is_credential_free_and_has_zero_observed_debit() {
    let record: Value =
        serde_json::from_str(include_str!("fixtures/live-flash-tools.json")).unwrap();
    assert_eq!(record["requested_model"], "glm-5.3-flash");
    assert_eq!(record["upstream_model"], "zai/glm-5.3-flash");
    assert_eq!(record["model_requests"], 2);
    assert_eq!(record["tool_calls"], 1);
    assert_eq!(record["passed"], true);
    assert_eq!(record["balance_before"], record["balance_after"]);
    assert_eq!(record["usage"]["prompt_tokens"], 337);
    for name in [
        "api_key",
        "apiKey",
        "machine_uuid",
        "machineUuid",
        "instance",
        "check_in",
        "checkIn",
        "nonce",
    ] {
        assert!(record.get(name).is_none());
    }
}

#[test]
fn research_profile_schema_reuses_saved_identity_and_nonce() {
    let value = json!({
        "version":1,"label":"fixture","apiKey":"synthetic-key",
        "machineUuid":"550e8400-e29b-41d4-a716-446655440000",
        "instance":"b".repeat(32),"lane":7,
        "checkIn":{"nonce":"a".repeat(32),"seconds":600,"checkedAt":1}
    });
    let credentials: Credentials = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(credentials.machine_uuid, value["machineUuid"]);
    assert_eq!(credentials.instance, "b".repeat(32));
    assert_eq!(credentials.check_in.as_ref().unwrap().nonce, "a".repeat(32));
    assert_eq!(credentials.check_in.as_ref().unwrap().checked_at, 1);
    let restored: Credentials =
        serde_json::from_slice(&serde_json::to_vec(&credentials).unwrap()).unwrap();
    assert_eq!(credentials, restored);
}
