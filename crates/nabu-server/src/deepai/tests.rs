use super::*;
use ncore::{Format, HttpRequest, HttpResponse, Transport, TransportError};
use ncore_deepaiacc::{AccountSnapshot, Allowance, Amount, Catalog, CheckIn, LaneStore};
use serde_json::json;
use std::path::PathBuf;

struct TestAccounts {
    manager: Arc<DeepAIAccounts>,
    path: PathBuf,
    events: broadcast::Receiver<Event>,
}

impl TestAccounts {
    fn new() -> Self {
        let mut random = [0u8; 8];
        getrandom::fill(&mut random).unwrap();
        let path =
            std::env::temp_dir().join(format!("nabu-deepai-{:x}", u64::from_ne_bytes(random)));
        let (sender, events) = broadcast::channel(64);
        let manager = DeepAIAccounts::load(&path, Arc::new(HyperTransport::new()), sender);
        Self {
            manager,
            path,
            events,
        }
    }

    fn drain(&mut self) -> Vec<Event> {
        let mut events = vec![];
        while let Ok(event) = self.events.try_recv() {
            events.push(event);
        }
        events
    }

    fn block_writes(&self) {
        std::fs::create_dir_all(&self.path).unwrap();
        let file = self.path.join("deepai-accounts.json");
        if file.is_file() {
            std::fs::remove_file(&file).unwrap();
        }
        std::fs::create_dir(file).unwrap();
    }
}

impl Drop for TestAccounts {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn login_account(key: &str, username: &str) -> oauth::LoginAccount {
    oauth::LoginAccount {
        credentials: Credentials::new(format!("test-deepai-key-not-real-{key}")).unwrap(),
        username: Some(username.into()),
        is_pro: false,
    }
}

fn snapshot(remaining: &str, granted: &str) -> AccountSnapshot {
    AccountSnapshot {
        allowance: Allowance {
            enabled: true,
            balance: Some(Amount::new(remaining).unwrap()),
            granted: Some(Amount::new(granted).unwrap()),
            refresh_at: None,
            lane_version: Some(6),
        },
        catalog: Catalog::from_json(json!({"models":[
            {"id":"glm-5.3-flash","name":"GLM 5.3 Flash","reasoning":true,"input":["text","image"],"contextWindow":320000,"maxTokens":64000,"theosPerRequest":"0.000"},
            {"id":"deepai-cyber","name":"Cyber","reasoning":true,"input":["text"],"contextWindow":200000,"maxTokens":32000,"premier":false,"theosPerRequest":"1000","api":"openai-responses"}
        ]}), 0).unwrap(),
    }
}

fn add(manager: &DeepAIAccounts, id: &str, remaining: &str, granted: &str) -> String {
    let generation = manager.state.lock().unwrap().generation;
    manager
        .accept_login(generation, login_account(id, id))
        .unwrap();
    let mut state = manager.state.lock().unwrap();
    let entry = state
        .entries
        .iter_mut()
        .find(|entry| entry.label == id)
        .unwrap();
    let tier = entry.report.tier.clone();
    entry.report = check::report(snapshot(remaining, granted)).unwrap();
    entry.report.tier = tier;
    entry.key.clone()
}

fn reference(model: &str, variant: Option<&str>) -> ModelRef {
    ModelRef {
        provider_id: "deepai-account".into(),
        model_id: model.into(),
        variant: variant.map(str::to_owned),
        fast: false,
    }
}

fn refusal(status: Option<u16>, code: &str) -> ProviderError {
    ProviderError::parse(
        Format::OpenAiChat,
        status,
        json!({"error":{"code":code,"message":"Request refused."}})
            .to_string()
            .into(),
    )
}

#[test]
fn session_ids_have_a_stable_version_independent_hash() {
    let session = session_ids("session-fixture", "account-fixture", "glm-5.3-flash").unwrap();
    assert_eq!(
        session.affinity,
        "nabu-7cfad66557770021374e68931f54ae2a22e6350c3c6a09c9fde7d3f1582bc62b-affinity"
    );
    assert_eq!(
        session.cache_key,
        "nabu-7cfad66557770021374e68931f54ae2a22e6350c3c6a09c9fde7d3f1582bc62b-cache"
    );
    assert_ne!(
        session_ids("ab", "c", "m").unwrap().cache_key,
        session_ids("a", "bc", "m").unwrap().cache_key
    );
}

#[tokio::test]
#[ignore = "reads an explicitly supplied app account and calls only /theos and /models"]
async fn live_metadata_from_saved_app_account_uses_current_protocol() {
    let path = std::env::var("NABUCODE_DEEPAI_METADATA_PROFILE")
        .expect("supply the private account file explicitly");
    let value: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    let saved: crate::accounts_file::Saved<SavedAccount> =
        serde_json::from_value(value["accounts"][0].clone()).unwrap();
    let original = saved.credentials.credentials.clone();
    let test = TestAccounts::new();
    let key = test.manager.file.try_add(saved.credentials).unwrap();
    let manager = DeepAIAccounts::load(
        &test.path,
        Arc::new(HyperTransport::with_proxy(crate::proxy_from_env())),
        broadcast::channel(32).0,
    );
    manager.check_all().await;
    let accounts = manager.list();
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0].status, AccountStatus::Work);
    let provider = manager.provider().expect("DeepAI catalog unavailable");
    let credits = manager.credits().expect("DeepAI balance unavailable");
    let saved = manager.file.credentials(&key).unwrap();
    assert_eq!(saved.client.version, "0.3.40");
    assert_eq!(saved.credentials.api_key, original.api_key);
    assert_eq!(saved.credentials.machine_uuid, original.machine_uuid);
    assert_eq!(saved.credentials.instance, original.instance);
    println!(
        "DeepAI metadata: {} models, credits {} / {}, models invoked: 0",
        provider.models.len(),
        credits.remaining,
        credits.total
    );
}

#[tokio::test]
async fn loading_an_old_client_uses_current_lane_without_replacing_identity() {
    let test = TestAccounts::new();
    let key = add(&test.manager, "alice", "100", "100");
    test.manager
        .file
        .try_update(&key, |saved| {
            saved.credentials.client.version = "0.3.37".into()
        })
        .unwrap();
    let original = test.manager.file.credentials(&key).unwrap().credentials;
    let path = test.path.join("deepai-accounts.json");
    let mut value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    value["accounts"][0]["credentials"]["credentials"]
        .as_object_mut()
        .unwrap()
        .remove("lane");
    std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    let reloaded = DeepAIAccounts::load(
        &test.path,
        Arc::new(HyperTransport::new()),
        broadcast::channel(32).0,
    );
    let plugin = reloaded.state.lock().unwrap().entries[0].plugin.clone();
    let net = MetadataMock::default();
    plugin.check_in(&net).await.unwrap();
    let requests = net.requests.lock().unwrap();
    assert_eq!(
        ncore::rt::header(&requests[0].headers, "x-deepai-code-theos"),
        Some("7")
    );
    assert!(
        ncore::rt::header(&requests[0].headers, "x-deepai-code-beacon")
            .unwrap()
            .starts_with("v=0.3.40;")
    );
    let saved = reloaded.file.credentials(&key).unwrap();
    assert_eq!(saved.client.version, "0.3.40");
    assert_eq!(saved.credentials.lane, 7);
    assert_eq!(saved.credentials.api_key, original.api_key);
    assert_eq!(saved.credentials.machine_uuid, original.machine_uuid);
    assert_eq!(saved.credentials.instance, original.instance);
}

#[test]
fn reconnect_and_reload_keep_uuid_instance_nonce_key_plugin_and_affinity() {
    let test = TestAccounts::new();
    let manager = &test.manager;
    let key = add(manager, "alice", "120", "120");
    let mut saved = manager.file.credentials(&key).unwrap();
    saved.credentials.lane = 23;
    saved.credentials.check_in = Some(CheckIn {
        nonce: "a".repeat(32),
        seconds: 600.0,
        checked_at: 1,
    });
    manager.file.try_replace(&key, &saved).unwrap();
    let original = saved.credentials.clone();
    let original_plugin = manager.state.lock().unwrap().entries[0].plugin.clone();
    let model = reference("glm-5.3-flash", Some("off"));
    let first = manager
        .route("conversation", &model, &mut ir::Request::default())
        .unwrap();
    let mut reconnect = login_account("alice", "alice-native");
    assert_ne!(reconnect.credentials.machine_uuid, original.machine_uuid);
    reconnect.is_pro = true;
    manager.accept_login(0, reconnect).unwrap();
    assert_eq!(manager.file.list().len(), 1);
    assert_eq!(manager.file.list()[0].key, key);
    let saved = manager.file.credentials(&key).unwrap();
    assert_eq!(saved.credentials, original);
    assert_eq!(saved.username.as_deref(), Some("alice-native"));
    assert!(saved.is_pro);
    assert!(Arc::ptr_eq(
        &original_plugin,
        &manager.state.lock().unwrap().entries[0].plugin
    ));
    assert_eq!(manager.list()[0].email, "alice-native");
    assert_eq!(manager.list()[0].tier.as_deref(), Some("Pro"));
    assert_eq!(manager.state.lock().unwrap().sessions["conversation"], key);
    let next = manager
        .route("conversation", &model, &mut ir::Request::default())
        .unwrap();
    assert_eq!(first.account, next.account);
    assert_eq!(first.endpoint.headers, next.endpoint.headers);
    let reloaded = DeepAIAccounts::load(
        &test.path,
        Arc::new(HyperTransport::new()),
        broadcast::channel(32).0,
    );
    assert_eq!(
        reloaded.file.credentials(&key).unwrap().credentials,
        original
    );
    assert_eq!(reloaded.list()[0].email, "alice-native");
    assert_eq!(reloaded.list()[0].tier.as_deref(), Some("Pro"));
    reloaded.state.lock().unwrap().entries[0].report =
        check::report(snapshot("120", "120")).unwrap();
    let route = reloaded
        .route("conversation", &model, &mut ir::Request::default())
        .unwrap();
    assert_eq!(first.account, route.account);
    assert_eq!(first.endpoint.headers, route.endpoint.headers);
}

#[test]
fn login_with_a_new_key_for_the_same_username_keeps_one_account() {
    let test = TestAccounts::new();
    let key = add(&test.manager, "alice", "100", "100");
    let original = test.manager.file.credentials(&key).unwrap().credentials;
    test.manager
        .accept_login(0, login_account("second-key", "Alice"))
        .unwrap();

    assert_eq!(test.manager.list().len(), 1);
    assert_eq!(test.manager.file.list().len(), 1);
    let saved = test.manager.file.credentials(&key).unwrap().credentials;
    assert_eq!(saved.account_id(), original.account_id());
    assert_eq!(saved.instance, original.instance);
}

#[test]
fn removing_a_legacy_duplicate_by_id_only_removes_that_row() {
    let test = TestAccounts::new();
    let first = add(&test.manager, "alice", "100", "100");
    let second = add(&test.manager, "bob", "100", "100");
    test.manager
        .file
        .try_update(&second, |saved| {
            saved.credentials.username = Some("alice".into());
        })
        .unwrap();
    let manager = DeepAIAccounts::load(
        &test.path,
        Arc::new(HyperTransport::new()),
        broadcast::channel(32).0,
    );
    assert!(
        manager
            .list()
            .iter()
            .all(|account| account.email == "alice")
    );

    manager.remove(Some("alice"), Some(&second)).unwrap();
    manager.remove(Some("alice"), Some(&second)).unwrap();
    assert_eq!(manager.list().len(), 1);
    assert_eq!(manager.list()[0].id.as_deref(), Some(first.as_str()));
    assert_eq!(manager.file.list().len(), 1);
}

#[test]
fn credits_sum_only_connected_accounts_and_removal_publishes_events() {
    let mut test = TestAccounts::new();
    add(&test.manager, "alice", "120", "120");
    add(&test.manager, "bob", "100", "100");
    let credits = test.manager.credits().unwrap();
    assert_eq!(credits.remaining, 220.0);
    assert_eq!(credits.total, 220.0);
    let provider = test.manager.provider().unwrap();
    assert_eq!(provider.id, "deepai-account");
    assert_eq!(provider.name, "DeepAI Account");
    assert_eq!(provider.models.len(), 2);
    test.drain();
    test.manager.remove(Some("alice"), None).unwrap();
    assert_eq!(test.manager.credits().unwrap().remaining, 100.0);
    assert_eq!(test.manager.credits().unwrap().total, 100.0);
    let events = test.drain();
    assert!(
        events.iter().any(
            |event| matches!(event, Event::DeepAiAccounts { accounts } if accounts.len() == 1)
        )
    );
    assert!(events.iter().any(|event| matches!(event, Event::DeepAiCredits { credits: Some(credits) } if credits.remaining == 100.0)));
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::ProvidersChanged))
    );
    test.manager.remove(None, None).unwrap();
    assert!(test.manager.file.list().is_empty());
    assert!(test.manager.credits().is_none());
    assert!(test.manager.provider().is_none());
}

#[test]
fn session_selection_stays_sticky_but_next_explicit_route_can_use_another_account() {
    let test = TestAccounts::new();
    let alice = add(&test.manager, "alice", "120", "120");
    let bob = add(&test.manager, "bob", "100", "100");
    let model = reference("glm-5.3-flash", Some("high"));
    let first = test
        .manager
        .route("s", &model, &mut ir::Request::default())
        .unwrap();
    assert_eq!(first.account.as_deref(), Some(alice.as_str()));
    test.manager.state.lock().unwrap().entries[1]
        .report
        .credits
        .as_mut()
        .unwrap()
        .remaining = 200.0;
    let next = test
        .manager
        .route("s", &model, &mut ir::Request::default())
        .unwrap();
    assert_eq!(first.account, next.account);
    assert_eq!(first.endpoint.headers, next.endpoint.headers);
    for error in [
        refusal(Some(429), "deepai_error"),
        refusal(Some(429), "overloaded_error"),
        refusal(None, "billing_error"),
        refusal(Some(503), "billing_error"),
    ] {
        assert!(!test.manager.limited(&alice, &model.model_id, &error));
    }
    assert_eq!(test.manager.state.lock().unwrap().sessions["s"], alice);
    assert!(test.manager.limited(
        &alice,
        &model.model_id,
        &refusal(Some(429), "rate_limit_error")
    ));
    assert!(
        !test
            .manager
            .state
            .lock()
            .unwrap()
            .sessions
            .contains_key("s")
    );
    let next = test
        .manager
        .route("s", &model, &mut ir::Request::default())
        .unwrap();
    assert_eq!(next.account.as_deref(), Some(bob.as_str()));
    assert_ne!(first.endpoint.headers, next.endpoint.headers);
    assert_eq!(
        test.manager.state.lock().unwrap().entries[0].blocked.len(),
        1
    );
}

#[test]
fn native_responses_route_has_no_replays_and_bounds_output_without_a_fixed_cap() {
    let test = TestAccounts::new();
    add(&test.manager, "alice", "0.001", "78");
    let model = reference("deepai-cyber", Some("max"));
    let mut request = ir::Request {
        max_tokens: Some(100000),
        ..Default::default()
    };
    let route = test.manager.route("s", &model, &mut request).unwrap();
    assert_eq!(route.endpoint.format, Format::OpenAiResponses);
    assert!(!route.endpoint.allow_replays);
    assert_eq!(request.max_tokens, Some(32000));
    assert_eq!(request.reasoning.unwrap().effort.as_deref(), Some("max"));
    assert_eq!(
        route.options.encode.chat_reasoning_field.as_deref(),
        Some("reasoning_content")
    );
    let mut request = ir::Request {
        max_tokens: Some(1024),
        ..Default::default()
    };
    let route = test
        .manager
        .route("s", &reference("glm-5.3-flash", Some("off")), &mut request)
        .unwrap();
    assert_eq!(route.endpoint.format, Format::OpenAiChat);
    assert!(!route.endpoint.allow_replays);
    assert_eq!(request.max_tokens, Some(1024));
    assert_eq!(request.reasoning.unwrap().effort.as_deref(), Some("none"));
}

#[test]
fn free_models_run_at_zero_balance_but_paid_models_and_unknown_efforts_do_not() {
    let test = TestAccounts::new();
    add(&test.manager, "alice", "0", "78");
    test.manager.state.lock().unwrap().entries[0].report.status = AccountStatus::Checking;
    assert!(
        test.manager
            .route(
                "s",
                &reference("glm-5.3-flash", Some("none")),
                &mut ir::Request::default()
            )
            .is_ok()
    );
    assert!(
        test.manager
            .route(
                "s",
                &reference("deepai-cyber", Some("high")),
                &mut ir::Request::default()
            )
            .is_err()
    );
    assert!(
        test.manager
            .route(
                "s",
                &reference("glm-5.3-flash", Some("max")),
                &mut ir::Request::default()
            )
            .is_err()
    );
    assert!(
        test.manager
            .route(
                "s",
                &reference("not-in-catalog", None),
                &mut ir::Request::default()
            )
            .is_err()
    );
    let mut request = ir::Request {
        max_tokens: Some(0),
        ..Default::default()
    };
    assert!(
        test.manager
            .route("s", &reference("glm-5.3-flash", None), &mut request)
            .is_err()
    );
    test.manager.state.lock().unwrap().entries[0].report.enabled = false;
    assert!(
        test.manager
            .route(
                "s",
                &reference("glm-5.3-flash", Some("high")),
                &mut ir::Request::default()
            )
            .is_err()
    );
}

#[test]
fn credits_refresh_keeps_model_refusals_and_session_ids_are_safe_for_arbitrary_host_ids() {
    let test = TestAccounts::new();
    let key = add(&test.manager, "alice", "100", "100");
    assert!(
        test.manager
            .limited(&key, "glm-5.3-flash", &refusal(Some(403), "billing_error"))
    );
    {
        let mut state = test.manager.state.lock().unwrap();
        let entry = &mut state.entries[0];
        check::refresh(&mut entry.report, &snapshot("100", "100").allowance).unwrap();
        mark_blocked(entry);
    }
    assert!(
        test.manager
            .route(
                "s",
                &reference("glm-5.3-flash", Some("high")),
                &mut ir::Request::default()
            )
            .is_err()
    );
    let one = session_ids(&"private / session\n🦀".repeat(1000), &key, "deepai-cyber").unwrap();
    let two = session_ids(&"private / session\n🦀".repeat(1000), &key, "deepai-cyber").unwrap();
    assert_eq!(one.affinity, two.affinity);
    assert_eq!(one.cache_key, two.cache_key);
    assert_ne!(one.affinity, one.cache_key);
    assert!(one.affinity.len() <= 256);
    assert!(one.affinity.is_ascii());
    assert_ne!(
        one.affinity,
        session_ids("other-session", &key, "deepai-cyber")
            .unwrap()
            .affinity
    );
    assert_ne!(
        one.affinity,
        session_ids(&"private / session\n🦀".repeat(1000), &key, "glm-5.3-flash")
            .unwrap()
            .affinity
    );
}

#[test]
fn only_successful_explicit_check_clears_refusals_and_keeps_native_tier() {
    let test = TestAccounts::new();
    let key = add(&test.manager, "alice", "0", "78");
    assert!(
        test.manager
            .limited(&key, "glm-5.3-flash", &refusal(Some(403), "billing_error"))
    );
    let mut state = test.manager.state.lock().unwrap();
    let entry = &mut state.entries[0];
    assert!(matches!(entry.report.status, AccountStatus::Limit { .. }));
    checked(entry, check::report(snapshot("0", "78")), false);
    assert!(entry.blocked.contains("glm-5.3-flash"));
    assert!(matches!(entry.report.status, AccountStatus::Limit { .. }));
    checked(entry, Err("metadata unavailable".into()), true);
    assert!(entry.blocked.contains("glm-5.3-flash"));
    checked(entry, check::report(snapshot("0", "78")), true);
    assert!(entry.blocked.is_empty());
    assert_eq!(entry.report.status, AccountStatus::Work);
    assert_eq!(entry.report.tier.as_deref(), Some("Free"));
}

#[test]
fn missing_name_reconnect_keeps_profile_and_corrupt_local_names_use_local_key_label() {
    let test = TestAccounts::new();
    let key = add(&test.manager, "alice", "100", "100");
    let mut reconnect = login_account("alice", "");
    reconnect.username = None;
    test.manager.accept_login(0, reconnect).unwrap();
    assert_eq!(test.manager.list()[0].email, "alice");
    test.manager
        .file
        .try_update(&key, |saved| {
            saved.credentials.username = Some("unsafe\nname".into())
        })
        .unwrap();
    let reloaded = DeepAIAccounts::load(
        &test.path,
        Arc::new(HyperTransport::new()),
        broadcast::channel(32).0,
    );
    assert_eq!(reloaded.list()[0].email, format!("DeepAI · {key}"));
    assert!(!reloaded.list()[0].email.contains("test-deepai-key"));
}

#[test]
fn failed_login_and_remove_all_are_transactional_and_publish_no_success_events() {
    let mut test = TestAccounts::new();
    test.block_writes();
    let error = test
        .manager
        .accept_login(0, login_account("alice", "alice"))
        .unwrap_err();
    assert_eq!(error, "Couldn't save the DeepAI account");
    assert!(test.manager.list().is_empty());
    assert!(test.manager.file.list().is_empty());
    assert!(test.drain().is_empty());
    std::fs::remove_dir(test.path.join("deepai-accounts.json")).unwrap();
    let key = add(&test.manager, "alice", "120", "120");
    add(&test.manager, "bob", "100", "100");
    let original = test.manager.file.credentials(&key).unwrap().credentials;
    test.drain();
    test.block_writes();
    let error = test
        .manager
        .accept_login(0, login_account("alice", "changed-name"))
        .unwrap_err();
    assert_eq!(error, "Couldn't save the DeepAI account");
    assert_eq!(test.manager.list()[0].email, "alice");
    assert_eq!(
        test.manager.file.credentials(&key).unwrap().credentials,
        original
    );
    assert!(test.drain().is_empty());
    assert!(test.manager.remove(None, None).is_err());
    assert_eq!(test.manager.list().len(), 2);
    assert_eq!(test.manager.file.list().len(), 2);
    assert_eq!(test.manager.credits().unwrap().remaining, 220.0);
    assert!(test.drain().is_empty());
}

#[tokio::test]
async fn account_store_persists_lane_and_nonce_without_overwriting_profile_or_identity() {
    let test = TestAccounts::new();
    let key = add(&test.manager, "alice", "100", "100");
    let store = AccountStore {
        file: test.manager.file.clone(),
        key: key.clone(),
    };
    let mut credentials = store.load().await.unwrap();
    credentials.lane = 23;
    credentials.check_in = Some(CheckIn {
        nonce: "b".repeat(32),
        seconds: 600.0,
        checked_at: 1,
    });
    test.manager
        .file
        .try_update(&key, |saved| {
            saved.credentials.username = Some("alice-native".into());
            saved.credentials.is_pro = true;
        })
        .unwrap();
    store.save(&credentials).await.unwrap();
    let saved = test.manager.file.credentials(&key).unwrap();
    assert_eq!(saved.credentials, credentials);
    assert_eq!(saved.username.as_deref(), Some("alice-native"));
    assert!(saved.is_pro);
    let fresh = Credentials::new(credentials.api_key.clone()).unwrap();
    let mut changed = credentials.clone();
    changed.api_key.push_str("-other-account");
    assert!(store.save(&changed).await.is_err());
    changed = credentials.clone();
    changed.machine_uuid = fresh.machine_uuid;
    assert!(store.save(&changed).await.is_err());
    changed = credentials.clone();
    changed.instance = fresh.instance;
    assert!(store.save(&changed).await.is_err());
    changed = credentials.clone();
    changed.lane = 0;
    assert!(store.save(&changed).await.is_err());
    assert_eq!(store.load().await.unwrap(), saved.credentials);
    test.block_writes();
    credentials.check_in.as_mut().unwrap().nonce = "c".repeat(32);
    assert!(store.save(&credentials).await.is_err());
    assert_eq!(store.load().await.unwrap(), saved.credentials);
}

#[tokio::test]
async fn lane_store_missing_file_is_empty_and_every_supported_lane_survives_reload() {
    let test = TestAccounts::new();
    let path = test.path.join("deepai-lane.json");
    let store = lane_store::FileLaneStore::new(path.clone());
    assert_eq!(store.load().await.unwrap(), None);
    assert!(!path.exists());
    for lane in 1..=50 {
        store.save(lane).await.unwrap();
        let reloaded = lane_store::FileLaneStore::new(path.clone());
        assert_eq!(reloaded.load().await.unwrap(), Some(lane));
        let value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(value, json!({"lane": lane}));
    }
    assert_eq!(std::fs::read_dir(&test.path).unwrap().count(), 1);
}

#[tokio::test]
async fn lane_store_rejects_empty_malformed_and_out_of_range_files_without_leaking_contents() {
    let test = TestAccounts::new();
    std::fs::create_dir_all(&test.path).unwrap();
    let path = test.path.join("deepai-lane.json");
    let store = lane_store::FileLaneStore::new(path.clone());
    for raw in [
        "",
        "not JSON: private-test-value",
        "null",
        "[]",
        "[7]",
        "{}",
        r#"{"lane":null}"#,
        r#"{"lane":"7"}"#,
        r#"{"lane":true}"#,
        r#"{"lane":7.0}"#,
        r#"{"lane":7.5}"#,
        r#"{"lane":0}"#,
        r#"{"lane":-1}"#,
        r#"{"lane":51}"#,
        r#"{"lane":4294967296}"#,
        r#"{"lane":7,"private-test-value":true}"#,
        r#"{"lane":7,"lane":8}"#,
        r#"{"lane":7} trailing"#,
    ] {
        std::fs::write(&path, raw).unwrap();
        assert_eq!(
            store.load().await.unwrap_err(),
            "Couldn't read the saved DeepAI version",
            "accepted {raw:?}"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), raw);
    }
    std::fs::write(&path, [0xff, 0xfe]).unwrap();
    assert_eq!(
        store.load().await.unwrap_err(),
        "Couldn't read the saved DeepAI version"
    );
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    assert_eq!(
        store.load().await.unwrap_err(),
        "Couldn't read the saved DeepAI version"
    );
    assert!(path.is_dir());
}

#[tokio::test]
async fn lane_store_failed_saves_keep_durable_state_and_remove_atomic_temporary_files() {
    let test = TestAccounts::new();
    let path = test.path.join("deepai-lane.json");
    let store = lane_store::FileLaneStore::new(path.clone());
    store.save(23).await.unwrap();
    let original = std::fs::read(&path).unwrap();
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        let locked = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&path)
            .unwrap();
        assert_eq!(
            store.load().await.unwrap_err(),
            "Couldn't read the saved DeepAI version"
        );
        assert_eq!(
            store.save(24).await.unwrap_err(),
            "Couldn't save the DeepAI version"
        );
        drop(locked);
        assert_eq!(std::fs::read(&path).unwrap(), original);
        assert_eq!(store.load().await.unwrap(), Some(23));
        assert_eq!(std::fs::read_dir(&test.path).unwrap().count(), 1);
    }
    for lane in [0, 51, u32::MAX] {
        assert_eq!(
            store.save(lane).await.unwrap_err(),
            "Couldn't save the DeepAI version"
        );
        assert_eq!(std::fs::read(&path).unwrap(), original);
        assert_eq!(store.load().await.unwrap(), Some(23));
    }
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    let marker = path.join("unchanged");
    std::fs::write(&marker, b"previous state").unwrap();
    assert_eq!(
        store.save(24).await.unwrap_err(),
        "Couldn't save the DeepAI version"
    );
    assert!(path.is_dir());
    assert_eq!(std::fs::read(&marker).unwrap(), b"previous state");
    assert_eq!(std::fs::read_dir(&test.path).unwrap().count(), 1);
    assert_eq!(std::fs::read_dir(&path).unwrap().count(), 1);
}

#[tokio::test]
async fn every_loaded_and_new_account_reuses_the_manager_lane_state() {
    let test = TestAccounts::new();
    let store = lane_store::FileLaneStore::new(test.path.join("deepai-lane.json"));
    store.save(23).await.unwrap();
    add(&test.manager, "alice", "100", "100");
    let alice = test.manager.state.lock().unwrap().entries[0].plugin.clone();
    assert_eq!(alice.lane().await.unwrap(), 23);
    store.save(24).await.unwrap();
    add(&test.manager, "bob", "100", "100");
    let bob = test.manager.state.lock().unwrap().entries[1].plugin.clone();
    assert_eq!(alice.lane().await.unwrap(), 23);
    assert_eq!(bob.lane().await.unwrap(), 23);

    let reloaded = DeepAIAccounts::load(
        &test.path,
        Arc::new(HyperTransport::new()),
        broadcast::channel(32).0,
    );
    let plugins = reloaded
        .state
        .lock()
        .unwrap()
        .entries
        .iter()
        .map(|entry| entry.plugin.clone())
        .collect::<Vec<_>>();
    assert_eq!(plugins[0].lane().await.unwrap(), 24);
    store.save(25).await.unwrap();
    assert_eq!(plugins[1].lane().await.unwrap(), 24);
    add(&reloaded, "carol", "100", "100");
    let carol = reloaded.state.lock().unwrap().entries[2].plugin.clone();
    assert_eq!(carol.lane().await.unwrap(), 24);
    assert!(
        reloaded
            .file
            .list()
            .iter()
            .all(|saved| saved.credentials.credentials.lane == 7)
    );
}

#[tokio::test]
async fn persisted_global_lane_drives_metadata_and_saves_account_state_without_changing_identity() {
    let test = TestAccounts::new();
    let key = add(&test.manager, "alice", "100", "100");
    let original = test.manager.file.credentials(&key).unwrap().credentials;
    lane_store::FileLaneStore::new(test.path.join("deepai-lane.json"))
        .save(23)
        .await
        .unwrap();
    let reloaded = DeepAIAccounts::load(
        &test.path,
        Arc::new(HyperTransport::new()),
        broadcast::channel(32).0,
    );
    let plugin = reloaded.state.lock().unwrap().entries[0].plugin.clone();
    let net = MetadataMock::default();
    plugin.check_in(&net).await.unwrap();
    let requests = net.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        ncore::rt::header(&requests[0].headers, "x-deepai-code-theos"),
        Some("23")
    );
    let saved = reloaded.file.credentials(&key).unwrap().credentials;
    assert_eq!(saved.lane, 23);
    assert!(saved.check_in.is_some());
    assert_eq!(saved.api_key, original.api_key);
    assert_eq!(saved.machine_uuid, original.machine_uuid);
    assert_eq!(saved.instance, original.instance);
    let loaded = AccountsFile::<SavedAccount>::load(test.path.join("deepai-accounts.json"), None);
    assert_eq!(loaded.credentials(&key).unwrap().credentials, saved);
}

#[tokio::test]
async fn missing_global_lane_defaults_to_seven_without_replacing_account_lane_or_nonce() {
    let test = TestAccounts::new();
    let key = add(&test.manager, "alice", "100", "100");
    test.manager
        .file
        .try_update(&key, |saved| {
            saved.credentials.credentials.lane = 23;
            saved.credentials.credentials.check_in = Some(CheckIn {
                nonce: "a".repeat(32),
                seconds: 600.0,
                checked_at: 1,
            });
        })
        .unwrap();
    let original = test.manager.file.credentials(&key).unwrap().credentials;
    let plugin = test.manager.state.lock().unwrap().entries[0].plugin.clone();
    assert_eq!(plugin.lane().await.unwrap(), 7);
    assert_eq!(
        test.manager.file.credentials(&key).unwrap().credentials,
        original
    );
    assert!(!test.path.join("deepai-lane.json").exists());
}

#[tokio::test]
async fn invalid_global_lane_stops_metadata_without_network_requests_or_account_changes() {
    let test = TestAccounts::new();
    let key = add(&test.manager, "alice", "100", "100");
    let original = test.manager.file.credentials(&key).unwrap().credentials;
    std::fs::write(test.path.join("deepai-lane.json"), r#"{"lane":0}"#).unwrap();
    let plugin = test.manager.state.lock().unwrap().entries[0].plugin.clone();
    assert!(plugin.lane().await.is_err());
    let net = MetadataMock::default();
    let error = plugin.snapshot(&net).await.unwrap_err();
    let message = check::error(error);
    assert!(message.starts_with("DeepAI — "));
    assert!(message.to_lowercase().contains("lane"));
    assert!(net.requests.lock().unwrap().is_empty());
    assert_eq!(
        test.manager.file.credentials(&key).unwrap().credentials,
        original
    );
}

#[derive(Default)]
struct MetadataMock {
    requests: Mutex<Vec<HttpRequest>>,
}

impl Transport for MetadataMock {
    fn send(&self, request: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, TransportError>> {
        boxed(async move {
            assert!(
                request.url.ends_with("/theos"),
                "no catalog or model call after failed nonce save"
            );
            self.requests.lock().unwrap().push(request);
            Ok(HttpResponse {
                status: 200,
                headers: vec![],
                body: Box::pin(futures_util::stream::iter([Ok(json!({
                    "enabled": true, "balance": "78", "granted": "78",
                    "check_in": {"nonce": "a".repeat(32), "seconds": 600}
                })
                .to_string()
                .into())])),
            })
        })
    }
}

#[tokio::test]
async fn failed_durable_check_in_stops_before_catalog_or_model_and_does_not_publish_success() {
    let mut test = TestAccounts::new();
    let key = add(&test.manager, "alice", "100", "100");
    test.drain();
    test.block_writes();
    let plugin = test.manager.state.lock().unwrap().entries[0].plugin.clone();
    let net = MetadataMock::default();
    let error = plugin.snapshot(&net).await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("could not persist DeepAI check-in")
    );
    assert_eq!(net.requests.lock().unwrap().len(), 1);
    assert!(
        test.manager
            .file
            .credentials(&key)
            .unwrap()
            .credentials
            .check_in
            .is_none()
    );
    assert!(test.drain().is_empty());
}

#[tokio::test]
async fn removal_cancels_login_generation_and_late_callback_cannot_add_or_emit_failure() {
    let mut test = TestAccounts::new();
    let task = tokio::spawn(std::future::pending::<()>());
    let abort = task.abort_handle();
    test.manager.state.lock().unwrap().login = Some(task);
    test.manager.remove(None, None).unwrap();
    tokio::task::yield_now().await;
    assert!(abort.is_finished());
    test.drain();
    assert!(
        test.manager
            .accept_login(0, login_account("late", "late"))
            .is_err()
    );
    test.manager.login_failed(0, "stale failure");
    assert!(test.manager.list().is_empty());
    assert!(test.drain().is_empty());
}
