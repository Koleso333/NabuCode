use super::*;
use ncore_deepaiacc::Catalog;
use serde_json::json;

fn allowance(balance: &str, granted: &str) -> Allowance {
    Allowance {
        enabled: true,
        balance: Some(Amount::new(balance).unwrap()),
        granted: Some(Amount::new(granted).unwrap()),
        refresh_at: None,
        lane_version: Some(6),
    }
}

fn catalog() -> Catalog {
    Catalog::from_json(json!({"models": [
        {"id":"glm-5.3-flash", "name":"GLM 5.3 Flash", "reasoning":true, "input":["text","image"], "contextWindow":320000, "maxTokens":64000, "premier":true, "theosPerRequest":"0.000"},
        {"id":"deepai-cyber", "name":"Cyber", "reasoning":true, "input":["text"], "contextWindow":200000, "maxTokens":32000, "premier":false, "theosPerRequest":"250", "api":"openai-responses"},
        {"id":"unknown-price", "name":"Unknown price", "reasoning":false, "input":["text"], "contextWindow":100000, "maxTokens":8000, "premier":false},
        {"id":"muse-ember", "name":"Ember", "reasoning":true, "input":["text"], "contextWindow":200000, "maxTokens":16000, "theosPerRequest":"0.01"}
    ]}), 0).unwrap()
}

#[test]
fn metadata_error_keeps_status_code_and_safe_operation_details() {
    let error = ncore::core::ProviderError {
        kind: ncore::core::ErrorKind::InvalidRequest,
        status: Some(426),
        code: Some("lane_version_unsupported".into()),
        message: "/theos: The server does not support this client lane.".into(),
        raw: b"private response body must not be shown"[..].into(),
    };
    let message = super::error(ncore::Error::Provider(error));
    assert!(message.contains("HTTP 426"));
    assert!(message.contains("lane_version_unsupported"));
    assert!(message.contains("/theos:"));
    assert!(!message.contains("private response body"));
    let message = super::error(ncore::Error::Plugin {
        name: "deepaiacc".into(),
        message: "/models: invalid DeepAI model catalog".into(),
    });
    assert!(message.contains("/models: invalid DeepAI model catalog"));
    let message = super::error(ncore::Error::Transport(ncore::TransportError::new(
        "private transport details",
    )));
    assert!(!message.contains("private transport details"));
}

#[test]
fn native_catalog_maps_exact_limits_images_apis_and_one_off_alias() {
    let report = report(AccountSnapshot {
        catalog: catalog(),
        allowance: allowance("0", "78"),
    })
    .unwrap();
    assert_eq!(report.status, AccountStatus::Work);
    assert_eq!(report.free, ["glm-5.3-flash"]);
    assert_eq!(report.models.len(), 4);
    let flash = report
        .models
        .iter()
        .find(|model| model.id == "glm-5.3-flash")
        .unwrap();
    assert_eq!(flash.name, "GLM 5.3 Flash (Free)");
    let ember = report.models.iter().find(|model| model.id == "muse-ember");
    assert_eq!(ember.unwrap().name, "Ember");
    assert_eq!(flash.context, 320000);
    assert_eq!(flash.output, 64000);
    assert!(flash.images);
    assert_eq!(flash.efforts, ["off", "minimal", "low", "medium", "high"]);
    assert_eq!(report.apis["deepai-cyber"], ModelApi::Responses);
    assert!(
        report
            .models
            .iter()
            .find(|model| model.id == "deepai-cyber")
            .unwrap()
            .efforts
            .contains(&"max".into())
    );
    let ember = report
        .models
        .iter()
        .find(|model| model.id == "muse-ember")
        .unwrap();
    assert!(ember.efforts.contains(&"xhigh".into()));
    assert!(ember.efforts.contains(&"max".into()));
}

#[test]
fn allowance_display_uses_granted_not_a_fixed_price_and_never_negative_capacity() {
    let credits = balance(&allowance("120.125", "100")).unwrap().unwrap();
    assert_eq!(credits.remaining, 120.125);
    assert_eq!(credits.total, 120.125);
    let mut report = report(AccountSnapshot {
        catalog: catalog(),
        allowance: allowance("0.001", "78"),
    })
    .unwrap();
    assert!(report.paid);
    assert_eq!(report.credits.as_ref().unwrap().total, 78.0);
    assert_eq!(report.max_credits, Some(78.0));
    refresh(&mut report, &allowance("0", "78")).unwrap();
    assert!(!report.paid);
    assert_eq!(report.status, AccountStatus::Work);
    report.free.clear();
    assert!(matches!(status(&report), AccountStatus::Limit { .. }));
    refresh(&mut report, &allowance("120.125", "100")).unwrap();
    assert_eq!(report.max_credits, Some(100.0));
}

#[test]
fn combined_balance_reports_the_earliest_future_reset() {
    let now = Utc::now();
    let next = now + chrono::Duration::hours(1);
    let resets = [
        now + chrono::Duration::hours(3),
        now - chrono::Duration::hours(1),
        next,
    ];
    let balances = resets.map(|reset| {
        let mut allowance = allowance("20", "78");
        allowance.refresh_at = Some(
            reset
                .with_timezone(&chrono::FixedOffset::east_opt(10800).unwrap())
                .to_rfc3339(),
        );
        balance(&allowance).unwrap().unwrap()
    });
    let credits = crate::quota::combine_credits(balances.iter()).unwrap();
    assert_eq!(credits.remaining, 60.0);
    assert_eq!(
        credits.reset,
        Some(next.to_rfc3339_opts(SecondsFormat::Millis, true))
    );
}

#[test]
fn exact_zero_does_not_mistake_a_tiny_nonzero_price_or_balance_for_free() {
    let tiny = format!("0.{}1", "0".repeat(400));
    assert!(!is_zero(&Amount::new(&tiny).unwrap()));
    assert!(is_zero(&Amount::new("000.0000").unwrap()));
    let mut catalog = catalog();
    catalog.models[0].theos_per_request = Some(Amount::new(&tiny).unwrap());
    let report = report(AccountSnapshot {
        catalog,
        allowance: allowance(&tiny, "78"),
    })
    .unwrap();
    assert!(report.free.is_empty());
    assert!(report.paid);
    assert_eq!(report.status, AccountStatus::Work);
    assert_eq!(report.credits.unwrap().remaining, 0.0);
}

#[test]
fn disabled_allowance_cannot_route_free_models_and_incomplete_enabled_balance_fails() {
    let disabled = Allowance {
        enabled: false,
        balance: None,
        granted: None,
        refresh_at: None,
        lane_version: Some(6),
    };
    let report = report(AccountSnapshot {
        catalog: catalog(),
        allowance: disabled,
    })
    .unwrap();
    assert!(!report.enabled);
    assert!(report.credits.is_none());
    assert!(matches!(report.status, AccountStatus::Limit { .. }));
    let mut invalid = allowance("10", "78");
    invalid.granted = None;
    assert!(balance(&invalid).is_err());
}
