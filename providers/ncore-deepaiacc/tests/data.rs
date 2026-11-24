use std::collections::BTreeSet;

use futures_util::FutureExt;
use ncore_deepaiacc::{
    Amount, Catalog, CheckIn, CredentialStore, Credentials, MemoryStore, ModelApi,
};
use serde_json::{Value, json};

mod credentials {
    pub use ncore_deepaiacc::CheckIn;
}

#[path = "../src/quota.rs"]
mod quota;

fn credentials() -> Credentials {
    Credentials {
        api_key: "abc".into(),
        machine_uuid: "123e4567-e89b-42d3-a456-426614174000".into(),
        instance: "0123456789abcdef0123456789abcdef".into(),
        lane: 7,
        check_in: None,
    }
}

fn check_in() -> CheckIn {
    CheckIn {
        nonce: "fedcba9876543210fedcba9876543210".into(),
        seconds: 600.25,
        checked_at: 1_700_000_000_000,
    }
}

fn model(id: &str) -> Value {
    json!({
        "id": id,
        "name": "Synthetic Model",
        "reasoning": true,
        "input": ["text", "image"],
        "contextWindow": 262_144,
        "maxTokens": 16_384
    })
}

fn catalog(model: Value) -> Result<Catalog, String> {
    Catalog::from_json(json!({"models": [model]}), 123)
}

#[test]
fn legacy_nonce_with_unknown_lane_is_cleared_without_replacing_account_identity() {
    let mut saved = credentials();
    saved.check_in = Some(check_in());
    let mut value = serde_json::to_value(&saved).unwrap();
    value.as_object_mut().unwrap().remove("lane");
    let legacy: Credentials = serde_json::from_value(value).unwrap();
    assert_eq!(legacy.lane, 7);
    assert!(legacy.check_in.is_none());
    assert_eq!(legacy.api_key, saved.api_key);
    assert_eq!(legacy.machine_uuid, saved.machine_uuid);
    assert_eq!(legacy.instance, saved.instance);
    let explicit: Credentials =
        serde_json::from_value(serde_json::to_value(&saved).unwrap()).unwrap();
    assert_eq!(explicit, saved);
}

#[test]
fn hashes_match_known_synthetic_fixtures() {
    let credentials = credentials();
    credentials.validate().unwrap();
    assert_eq!(
        credentials.account_id(),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    assert_eq!(
        credentials.machine_digest(),
        "320159ebe3219112484baaa0aa1164dd93b25e98f4a1c7bee7fd602bc2a614c0"
    );
    let mut uppercase = credentials.clone();
    uppercase.machine_uuid = uppercase.machine_uuid.to_ascii_uppercase();
    uppercase.validate().unwrap();
    assert_eq!(uppercase.machine_digest(), credentials.machine_digest());
    assert_eq!(uppercase.account_id(), credentials.account_id());
}

#[test]
fn constructors_create_separate_random_v4_identities() {
    let mut uuids = BTreeSet::new();
    let mut instances = BTreeSet::new();
    let expected_account = credentials().account_id();
    for _ in 0..32 {
        let credentials = Credentials::new("abc").unwrap();
        credentials.validate().unwrap();
        assert_eq!(credentials.account_id(), expected_account);
        assert_eq!(credentials.machine_uuid.len(), 36);
        assert_eq!(credentials.machine_uuid.as_bytes()[14], b'4');
        assert!(matches!(
            credentials.machine_uuid.as_bytes()[19],
            b'8' | b'9' | b'a' | b'b'
        ));
        assert_eq!(
            credentials.machine_uuid,
            credentials.machine_uuid.to_ascii_lowercase()
        );
        assert_eq!(credentials.instance.len(), 32);
        assert!(
            credentials
                .instance
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        );
        assert!(credentials.check_in.is_none());
        assert!(uuids.insert(credentials.machine_uuid));
        assert!(instances.insert(credentials.instance));
    }
}

#[test]
fn importing_native_json_uses_only_the_host_supplied_key() {
    let first = Credentials::from_deepai_json(
        br#"{"apiKey":"abc","machineUuid":"ignored","instance":"ignored","username":"unused"}"#,
    )
    .unwrap();
    let second = Credentials::from_deepai_json(br#"{"apiKey":"abc"}"#).unwrap();
    assert_eq!(first.api_key, "abc");
    assert_eq!(first.account_id(), second.account_id());
    assert_ne!(first.machine_uuid, second.machine_uuid);
    assert_ne!(first.instance, second.instance);
    assert!(first.check_in.is_none());
    assert_ne!(first.machine_uuid, "ignored");
}

#[test]
fn keys_are_canonical_printable_ascii_without_whitespace() {
    for key in [
        "", " abc", "abc ", "a b", "a\tb", "a\nb", "a\rb", "a\0b", "a\u{7f}b", "clé", "\u{a0}",
    ] {
        assert_eq!(
            Credentials::new(key).unwrap_err(),
            "invalid DeepAI account key"
        );
        let mut credentials = credentials();
        credentials.api_key = key.into();
        assert!(credentials.validate().is_err());
        assert!(
            serde_json::from_value::<Credentials>(serde_json::to_value(credentials).unwrap())
                .is_err()
        );
    }
    Credentials::new("Az09!~._-/:+=")
        .unwrap()
        .validate()
        .unwrap();
}

#[test]
fn malformed_native_json_is_rejected_without_echoing_values() {
    for bytes in [
        b"not-json".as_slice(),
        b"null",
        b"[]",
        b"{}",
        br#"{"api_key":"private-synthetic-value"}"#,
        br#"{"apiKey":null}"#,
        br#"{"apiKey":true}"#,
        br#"{"apiKey":123}"#,
        br#"{"apiKey":"private synthetic value"}"#,
    ] {
        let error = Credentials::from_deepai_json(bytes).unwrap_err();
        assert!(!error.contains("private"));
    }
}

#[test]
fn persisted_credentials_preserve_identity_and_check_in() {
    let mut original = credentials();
    original.check_in = Some(check_in());
    let bytes = serde_json::to_vec(&original).unwrap();
    let restored: Credentials = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(restored, original);
    assert_eq!(restored.account_id(), original.account_id());
    assert_eq!(restored.machine_digest(), original.machine_digest());
    let before = serde_json::to_value(&restored).unwrap();
    for _ in 0..5 {
        restored.validate().unwrap();
        restored.account_id();
        restored.machine_digest();
    }
    assert_eq!(serde_json::to_value(&restored).unwrap(), before);
    assert_eq!(serde_json::to_vec(&restored).unwrap(), bytes);
}

#[test]
fn invalid_uuid_versions_variants_and_syntax_are_not_repaired() {
    for uuid in [
        "",
        "123e4567e89b42d3a456426614174000",
        "123e4567-e89b-12d3-a456-426614174000",
        "123e4567-e89b-52d3-a456-426614174000",
        "123e4567-e89b-42d3-7456-426614174000",
        "123e4567-e89b-42d3-c456-426614174000",
        "123e4567-e89b-42d3-f456-426614174000",
        "123e4567-e89b-42d3-a456-42661417400g",
        "123e4567_e89b-42d3-a456-426614174000",
        "123e4567-e89b-42d3-a456-426614174000 ",
        " 123e4567-e89b-42d3-a456-426614174000",
        "00000000-0000-0000-0000-000000000000",
        "123e4567-e89b-42d3-a456-4266141740é",
    ] {
        let mut credentials = credentials();
        credentials.machine_uuid = uuid.into();
        let before = credentials.clone();
        assert_eq!(
            credentials.validate().unwrap_err(),
            "invalid DeepAI account identity"
        );
        assert_eq!(credentials, before);
        assert!(
            serde_json::from_value::<Credentials>(serde_json::to_value(&credentials).unwrap())
                .is_err()
        );
    }
    for variant in ['8', '9', 'a', 'b', 'A', 'B'] {
        let mut credentials = credentials();
        credentials
            .machine_uuid
            .replace_range(19..20, &variant.to_string());
        credentials.validate().unwrap();
    }
}

#[test]
fn malformed_instance_or_missing_identity_is_rejected() {
    for instance in [
        "",
        "0",
        "0123456789ABCDEF0123456789ABCDEF",
        "0123456789abcdef0123456789abcdeg",
        "0123456789abcdef0123456789abcdef0",
        "0123456789abcdef0123456789abcde ",
    ] {
        let mut credentials = credentials();
        credentials.instance = instance.into();
        assert!(credentials.validate().is_err());
        assert!(
            serde_json::from_value::<Credentials>(serde_json::to_value(credentials).unwrap())
                .is_err()
        );
    }
    for field in ["api_key", "machine_uuid", "instance"] {
        let mut value = serde_json::to_value(credentials()).unwrap();
        value.as_object_mut().unwrap().remove(field);
        assert!(serde_json::from_value::<Credentials>(value).is_err());
    }
}

#[test]
fn debug_redacts_account_identity_and_nonce() {
    let mut credentials = credentials();
    credentials.api_key = "synthetic-secret-key-not-real".into();
    credentials.check_in = Some(check_in());
    let store = MemoryStore::new(credentials.clone());
    for debug in [
        format!("{credentials:?}"),
        format!("{:?}", credentials.check_in.as_ref().unwrap()),
        format!("{store:?}"),
    ] {
        for secret in [
            &credentials.api_key,
            &credentials.machine_uuid,
            &credentials.instance,
            &credentials.check_in.as_ref().unwrap().nonce,
        ] {
            assert!(!debug.contains(secret));
        }
    }
}

#[test]
fn check_in_requires_lowercase_hex_nonce_and_valid_duration() {
    check_in().validate().unwrap();
    for nonce in [
        "",
        "fedcba9876543210fedcba987654321",
        "fedcba9876543210fedcba98765432100",
        "FEDCBA9876543210FEDCBA9876543210",
        "gedcba9876543210fedcba9876543210",
        "fedcba9876543210fedcba987654321 ",
    ] {
        let mut check_in = check_in();
        check_in.nonce = nonce.into();
        assert_eq!(
            check_in.validate().unwrap_err(),
            "invalid DeepAI check-in state"
        );
        assert!(
            serde_json::from_value::<CheckIn>(serde_json::to_value(check_in).unwrap()).is_err()
        );
    }
    for seconds in [
        0.0,
        -0.0,
        -1.0,
        f64::NAN,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::MAX,
        u64::MAX as f64,
    ] {
        let mut check_in = check_in();
        check_in.seconds = seconds;
        assert_eq!(
            check_in.validate().unwrap_err(),
            "invalid DeepAI check-in state"
        );
    }
    let mut check_in = check_in();
    check_in.checked_at = 0;
    assert!(check_in.validate().is_err());
    assert!(serde_json::from_value::<CheckIn>(serde_json::to_value(check_in).unwrap()).is_err());
}

#[test]
fn persisted_invalid_check_in_is_rejected_with_credentials() {
    let mut credentials = credentials();
    credentials.check_in = Some(check_in());
    for (field, value) in [
        ("nonce", json!("synthetic-private-nonce")),
        ("seconds", json!(0)),
        ("seconds", json!(1e100)),
        ("checked_at", json!(0)),
    ] {
        let mut stored = serde_json::to_value(&credentials).unwrap();
        stored["check_in"][field] = value;
        let error = serde_json::from_value::<Credentials>(stored)
            .unwrap_err()
            .to_string();
        assert!(!error.contains("synthetic-private-nonce"));
    }
}

#[test]
fn memory_store_updates_check_in_without_changing_identity() {
    let original = credentials();
    let store = MemoryStore::new(original.clone());
    let mut loaded = store.load().now_or_never().unwrap().unwrap();
    assert_eq!(loaded, original);
    loaded.check_in = Some(check_in());
    assert_eq!(store.load().now_or_never().unwrap().unwrap(), original);
    store.save(&loaded).now_or_never().unwrap().unwrap();
    let saved = store.load().now_or_never().unwrap().unwrap();
    assert_eq!(saved, loaded);
    assert_eq!(saved.machine_uuid, original.machine_uuid);
    assert_eq!(saved.instance, original.instance);
    assert_eq!(saved.account_id(), original.account_id());
}

#[test]
fn memory_store_rejects_invalid_updates_without_losing_valid_state() {
    let original = credentials();
    let store = MemoryStore::new(original.clone());
    let mut invalid = original.clone();
    invalid.machine_uuid.clear();
    assert!(store.save(&invalid).now_or_never().unwrap().is_err());
    assert_eq!(store.load().now_or_never().unwrap().unwrap(), original);
    assert!(
        MemoryStore::new(invalid)
            .load()
            .now_or_never()
            .unwrap()
            .is_err()
    );
}

#[test]
fn multiple_accounts_have_independent_identity_and_check_in_state() {
    let first = Credentials::new("synthetic-account-one").unwrap();
    let second = Credentials::new("synthetic-account-two").unwrap();
    assert_ne!(first.account_id(), second.account_id());
    assert_ne!(first.machine_digest(), second.machine_digest());
    assert_ne!(first.instance, second.instance);
    let first_store = MemoryStore::new(first.clone());
    let second_store = MemoryStore::new(second.clone());
    let mut updated = first.clone();
    updated.check_in = Some(check_in());
    first_store.save(&updated).now_or_never().unwrap().unwrap();
    assert_eq!(first_store.load().now_or_never().unwrap().unwrap(), updated);
    assert_eq!(second_store.load().now_or_never().unwrap().unwrap(), second);
}

#[test]
fn exact_decimal_strings_survive_serialization_without_float_rounding() {
    for value in [
        "0",
        "000.0100",
        "12345678901234567890.12345678901234567890",
        "0.000000000000000000000000000001",
        "9007199254740993",
        "1.00",
    ] {
        let amount = Amount::new(value).unwrap();
        assert_eq!(amount.as_str(), value);
        assert_eq!(serde_json::to_value(&amount).unwrap(), json!(value));
        assert_eq!(
            serde_json::from_value::<Amount>(json!(value)).unwrap(),
            amount
        );
        assert!(amount.as_f64().is_some());
    }
    assert_eq!(Amount::new("1.25").unwrap().as_f64(), Some(1.25));
}

#[test]
fn malformed_or_negative_decimal_strings_are_rejected() {
    for value in [
        "", " ", " 1", "1 ", "-1", "-0", "+1", "1e2", "1E-2", ".1", "1.", "1.2.3", "NaN",
        "Infinity", "0x10", "1,000", "١", "1\n",
    ] {
        assert_eq!(Amount::new(value).unwrap_err(), "invalid DeepAI amount");
        assert!(serde_json::from_value::<Amount>(json!(value)).is_err());
    }
    let overflow = "9".repeat(400);
    assert!(Amount::new(&overflow).is_err());
    assert!(serde_json::from_value::<Amount>(json!(overflow)).is_err());
    for value in [Value::Null, json!(false), json!([]), json!({}), json!(-0.1)] {
        assert!(serde_json::from_value::<Amount>(value).is_err());
    }
}

#[test]
fn numeric_amounts_are_stored_as_nonnegative_decimal_text() {
    for (number, text) in [
        (json!(0), "0"),
        (json!(12.5), "12.5"),
        (json!(1e-7), "0.0000001"),
        (json!(1e20), "100000000000000000000"),
        (json!(u64::MAX), "18446744073709551615"),
    ] {
        let amount: Amount = serde_json::from_value(number).unwrap();
        assert_eq!(amount.as_str(), text);
        assert_eq!(serde_json::to_value(amount).unwrap(), json!(text));
    }
    assert_eq!(
        serde_json::from_value::<Amount>(json!(-0.0))
            .unwrap()
            .as_str(),
        "0.0"
    );
}

#[test]
fn allowance_keeps_exact_balances_and_does_not_export_nonce() {
    let payload = json!({"enabled":true,"balance":"12345678901234567890.12345678901234567890","granted":"000.0100","refresh_at":"2026-11-18T00:00:00Z","lane_version":6,"check_in":{"nonce":check_in().nonce,"seconds":600}});
    let allowance = quota::parse_allowance(&payload).unwrap();
    assert!(allowance.enabled);
    assert_eq!(
        allowance.balance.as_ref().unwrap().as_str(),
        "12345678901234567890.12345678901234567890"
    );
    assert_eq!(allowance.granted.as_ref().unwrap().as_str(), "000.0100");
    assert_eq!(
        allowance.refresh_at.as_deref(),
        Some("2026-11-18T00:00:00Z")
    );
    assert_eq!(allowance.lane_version, Some(6));
    assert!(allowance.balance.as_ref().unwrap().as_f64().is_some());
    let exported: ncore_deepaiacc::Allowance =
        serde_json::from_value(serde_json::to_value(&allowance).unwrap()).unwrap();
    assert_eq!(
        exported.balance.as_ref().unwrap().as_str(),
        allowance.balance.as_ref().unwrap().as_str()
    );
    for text in [
        format!("{allowance:?}"),
        serde_json::to_string(&allowance).unwrap(),
        format!("{exported:?}"),
    ] {
        assert!(!text.contains(&check_in().nonce));
        assert!(!text.contains("check_in"));
    }
}

#[test]
fn enabled_allowance_requires_both_nonnegative_balances() {
    for payload in [
        Value::Null,
        json!([]),
        json!({}),
        json!({"enabled":1}),
        json!({"enabled":true}),
        json!({"enabled":true,"balance":"1"}),
        json!({"enabled":true,"granted":"1"}),
        json!({"enabled":true,"balance":null,"granted":"1"}),
    ] {
        assert_eq!(
            quota::parse_allowance(&payload).unwrap_err(),
            "invalid DeepAI allowance"
        );
    }
    for field in ["balance", "granted"] {
        for value in [
            json!("-1"),
            json!(-0.1),
            json!("NaN"),
            json!("1e2"),
            json!("1."),
            json!({}),
            json!([]),
            json!(false),
        ] {
            let mut payload = json!({"enabled":true,"balance":"1","granted":"1"});
            payload[field] = value;
            assert!(quota::parse_allowance(&payload).is_err());
        }
    }
    let allowance =
        quota::parse_allowance(&json!({"enabled":true,"balance":0,"granted":1e-7})).unwrap();
    assert_eq!(allowance.balance.unwrap().as_str(), "0");
    assert_eq!(allowance.granted.unwrap().as_str(), "0.0000001");
}

#[test]
fn disabled_allowance_allows_absent_balances_but_checks_present_fields() {
    for payload in [
        json!({"enabled":false}),
        json!({"enabled":false,"balance":null,"granted":null,"refresh_at":null,"lane_version":null}),
    ] {
        let allowance = quota::parse_allowance(&payload).unwrap();
        assert!(!allowance.enabled);
        assert!(allowance.balance.is_none());
        assert!(allowance.granted.is_none());
    }
    let allowance = quota::parse_allowance(
        &json!({"enabled":false,"balance":"0.125","granted":0,"lane_version":6.0}),
    )
    .unwrap();
    assert_eq!(allowance.balance.unwrap().as_str(), "0.125");
    assert_eq!(allowance.lane_version, Some(6));
    for (field, value) in [
        ("balance", json!("-1")),
        ("granted", json!(false)),
        ("refresh_at", json!(123)),
        ("lane_version", json!("6")),
        ("lane_version", json!(-1)),
        ("lane_version", json!(6.5)),
        ("lane_version", json!(u32::MAX as u64 + 1)),
    ] {
        let mut payload = json!({"enabled":false});
        payload[field] = value;
        assert!(quota::parse_allowance(&payload).is_err());
    }
}

#[test]
fn check_in_is_read_independently_on_success_and_error_payloads() {
    let nonce = check_in().nonce;
    for payload in [
        json!({"enabled":true,"balance":"1","granted":"1","check_in":{"nonce":nonce,"seconds":600.25}}),
        json!({"error":"machine_required","check_in":{"nonce":nonce,"seconds":600.25}}),
        json!({"enabled":false,"check_in":{"nonce":nonce,"seconds":600.25}}),
    ] {
        let check_in = quota::parse_check_in(&payload, 42).unwrap().unwrap();
        assert_eq!(check_in.nonce, nonce);
        assert_eq!(check_in.seconds, 600.25);
        assert_eq!(check_in.checked_at, 42);
    }
    for payload in [
        json!({}),
        json!({"error":"refused"}),
        json!({"check_in":null}),
    ] {
        assert!(quota::parse_check_in(&payload, 0).unwrap().is_none());
    }
}

#[test]
fn malformed_check_in_errors_are_fixed_and_never_expose_nonce() {
    for payload in [
        Value::Null,
        json!([]),
        json!({"check_in":false}),
        json!({"check_in":[]}),
        json!({"check_in":{}}),
        json!({"check_in":{"nonce":"synthetic-private-nonce","seconds":600}}),
        json!({"check_in":{"nonce":check_in().nonce,"seconds":0}}),
        json!({"check_in":{"nonce":check_in().nonce,"seconds":-1}}),
        json!({"check_in":{"nonce":check_in().nonce,"seconds":"600"}}),
        json!({"check_in":{"nonce":check_in().nonce,"seconds":1e100}}),
    ] {
        let error = quota::parse_check_in(&payload, 42).unwrap_err();
        assert_eq!(error, "invalid DeepAI check-in state");
        assert!(!error.contains("synthetic-private-nonce"));
        assert!(!error.contains(&check_in().nonce));
    }
    let payload = json!({"check_in":{"nonce":check_in().nonce,"seconds":600}});
    assert!(quota::parse_check_in(&payload, 0).is_err());
}

#[test]
fn catalog_preserves_announced_budgets_and_optional_prices() {
    let mut source = model("custom-model");
    source["premier"] = json!(true);
    source["api"] = json!("openai-responses");
    source["theosPerRequest"] = json!("000.1234567890123456789");
    source["theosPerSecond"] =
        json!({"none":"0.00100","low":"0.002","medium":"0.003","high":"0.004"});
    let catalog = catalog(source).unwrap();
    assert_eq!(catalog.fetched_at, 123);
    let model = catalog.model("custom-model").unwrap();
    assert_eq!(model.name, "Synthetic Model");
    assert_eq!(model.context_window, 262_144);
    assert_eq!(model.max_tokens, 16_384);
    assert!(model.premier);
    assert_eq!(model.api, ModelApi::Responses);
    assert!(model.supports_images());
    assert_eq!(
        model.theos_per_request.as_ref().unwrap().as_str(),
        "000.1234567890123456789"
    );
    assert_eq!(
        model.theos_per_second.as_ref().unwrap()["none"].as_str(),
        "0.00100"
    );
    assert!(catalog.model("missing").is_none());
    assert!(catalog.model("CUSTOM-MODEL").is_none());
    assert_eq!(
        serde_json::from_value::<Catalog>(serde_json::to_value(&catalog).unwrap()).unwrap(),
        catalog
    );
}

#[test]
fn catalog_defaults_do_not_invent_capabilities_or_large_budgets() {
    let mut source = model("plain-model");
    source["reasoning"] = json!(false);
    source["input"] = json!(["text"]);
    source["contextWindow"] = json!(8192);
    source["maxTokens"] = json!(1024);
    let catalog = catalog(source).unwrap();
    let model = &catalog.models[0];
    assert_eq!(model.context_window, 8192);
    assert_eq!(model.max_tokens, 1024);
    assert!(!model.premier);
    assert_eq!(model.api, ModelApi::Chat);
    assert!(!model.supports_images());
    assert!(!model.reasoning.supported);
    assert!(!model.reasoning.can_disable);
    assert!(model.reasoning.efforts.is_empty());
    assert!(!model.accepts_effort("high"));
    assert!(model.theos_per_request.is_none());
    assert!(model.theos_per_second.is_none());
}

#[test]
fn reasoning_profiles_do_not_silently_clamp_extended_efforts() {
    let catalog = Catalog::from_json(
        json!({"models":[model("ordinary-model"),model("deepai-cyber"),model("muse-ember")]}),
        123,
    )
    .unwrap();
    for model in &catalog.models {
        assert!(model.reasoning.supported);
        assert!(model.reasoning.can_disable);
        for effort in ["off", "none", "minimal", "low", "medium", "high"] {
            assert!(model.accepts_effort(effort));
            assert!(model.reasoning.accepts_effort(effort));
        }
        for effort in ["", "HIGH", " high", "ultra", "disabled"] {
            assert!(!model.accepts_effort(effort));
        }
    }
    let ordinary = catalog.model("ordinary-model").unwrap();
    assert!(!ordinary.accepts_effort("xhigh"));
    assert!(!ordinary.accepts_effort("max"));
    let cyber = catalog.model("deepai-cyber").unwrap();
    assert!(!cyber.accepts_effort("xhigh"));
    assert!(cyber.accepts_effort("max"));
    let ember = catalog.model("muse-ember").unwrap();
    assert!(ember.accepts_effort("xhigh"));
    assert!(ember.accepts_effort("max"));
    assert_eq!(ordinary.context_window, 262_144);
}

#[test]
fn catalog_rejects_invalid_model_counts_and_duplicate_ids() {
    for value in [
        Value::Null,
        json!([]),
        json!({}),
        json!({"models":null}),
        json!({"models":[]}),
        json!({"models":[null]}),
    ] {
        assert_eq!(
            Catalog::from_json(value, 123).unwrap_err(),
            "invalid DeepAI model catalog"
        );
    }
    assert!(Catalog::from_json(json!({"models":[model("same"),model("same")]}), 123).is_err());
    let models = (0..100)
        .map(|index| model(&format!("model-{index}")))
        .collect::<Vec<_>>();
    assert_eq!(
        Catalog::from_json(json!({"models":models}), 123)
            .unwrap()
            .models
            .len(),
        100
    );
    let models = (0..101)
        .map(|index| model(&format!("model-{index}")))
        .collect::<Vec<_>>();
    assert!(Catalog::from_json(json!({"models":models}), 123).is_err());
}

#[test]
fn catalog_checks_ids_names_and_required_types() {
    for id in [
        "".to_string(),
        "a".repeat(129),
        "with space".into(),
        "path/model".into(),
        "model:tag".into(),
        "modèle".into(),
        "a\nb".into(),
    ] {
        assert!(catalog(model(&id)).is_err());
    }
    for id in ["a".to_string(), "a".repeat(128), "Az09_.-".into()] {
        assert!(catalog(model(&id)).is_ok());
    }
    for (field, value) in [
        ("id", json!(null)),
        ("name", json!("")),
        ("name", json!(" \t\n")),
        ("name", json!("n".repeat(101))),
        ("name", json!(123)),
        ("reasoning", json!(null)),
        ("reasoning", json!("true")),
        ("input", json!(null)),
        ("input", json!([])),
        ("input", json!(["image"])),
        ("input", json!(["text", "audio"])),
        ("input", json!(["text", null])),
    ] {
        let mut source = model("valid");
        source[field] = value;
        assert!(catalog(source).is_err(), "field: {field}");
    }
    for field in [
        "id",
        "name",
        "reasoning",
        "input",
        "contextWindow",
        "maxTokens",
    ] {
        let mut source = model("valid");
        source.as_object_mut().unwrap().remove(field);
        assert!(catalog(source).is_err());
    }
    let mut source = model("valid");
    source["name"] = json!("n".repeat(100));
    assert!(catalog(source).is_ok());
}

#[test]
fn catalog_requires_positive_safe_integer_budgets_without_fallbacks() {
    for field in ["contextWindow", "maxTokens"] {
        for value in [
            json!(0),
            json!(-1),
            json!(1.5),
            json!("8192"),
            Value::Null,
            json!(9_007_199_254_740_992u64),
            json!(u64::MAX),
            json!(1e30),
        ] {
            let mut source = model("valid");
            source[field] = value;
            assert!(catalog(source).is_err(), "field: {field}");
        }
    }
    let mut source = model("valid");
    source["maxTokens"] = json!(262_145);
    assert!(catalog(source).is_err());
    for (context, output) in [
        (json!(1), json!(1)),
        (json!(8192.0), json!(1024.0)),
        (
            json!(9_007_199_254_740_991u64),
            json!(9_007_199_254_740_991u64),
        ),
    ] {
        let mut source = model("valid");
        source["contextWindow"] = context.clone();
        source["maxTokens"] = output.clone();
        let catalog = catalog(source).unwrap();
        assert_eq!(
            catalog.models[0].context_window,
            context
                .as_u64()
                .unwrap_or_else(|| context.as_f64().unwrap() as u64)
        );
        assert_eq!(
            catalog.models[0].max_tokens,
            output
                .as_u64()
                .unwrap_or_else(|| output.as_f64().unwrap() as u64)
        );
    }
}

#[test]
fn catalog_verifies_optional_types_and_decimal_price_maps() {
    for (field, value) in [
        ("premier", Value::Null),
        ("premier", json!(1)),
        ("api", Value::Null),
        ("api", json!("chat")),
        ("api", json!("openai-completions")),
        ("theosPerRequest", Value::Null),
        ("theosPerRequest", json!(-1)),
        ("theosPerRequest", json!(0.1)),
        ("theosPerRequest", json!("-0.1")),
        ("theosPerRequest", json!("1e2")),
        ("theosPerSecond", Value::Null),
        ("theosPerSecond", json!([])),
        ("theosPerSecond", json!({"xhigh":"0.1"})),
        ("theosPerSecond", json!({"max":"0.1"})),
        ("theosPerSecond", json!({"off":"0.1"})),
        ("theosPerSecond", json!({"high":0.1})),
        ("theosPerSecond", json!({"low":"-0.1"})),
        ("theosPerSecond", json!({"medium":null})),
    ] {
        let mut source = model("valid");
        source[field] = value;
        assert_eq!(catalog(source).unwrap_err(), "invalid DeepAI model catalog");
    }
    let mut source = model("valid");
    source["theosPerRequest"] = json!("0");
    source["theosPerSecond"] = json!({});
    assert!(catalog(source).is_ok());
}
