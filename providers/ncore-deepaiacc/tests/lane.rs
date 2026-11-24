use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use futures_util::stream;
use ncore::rt::{BoxFuture, boxed, header};
use ncore::{HttpRequest, HttpResponse, Transport, TransportError};
use ncore_deepaiacc::{
    CheckIn, ClientInfo, CredentialStore, Credentials, DEFAULT_LANE, DeepAIAcc, LaneStore, Lanes,
    MAX_LANE, MemoryLaneStore, MemoryStore,
};
use serde_json::{Value, json};

#[derive(Clone)]
enum Reply {
    Json(u16, Value),
    Raw(u16, Bytes),
    Network,
    BodyNetwork,
}

impl Reply {
    fn response(self) -> Result<HttpResponse, TransportError> {
        let (status, body) = match self {
            Self::Json(status, value) => (status, Bytes::from(value.to_string())),
            Self::Raw(status, bytes) => (status, bytes),
            Self::Network => return Err(TransportError::new("private network diagnostic")),
            Self::BodyNetwork => {
                return Ok(HttpResponse {
                    status: 200,
                    headers: Vec::new(),
                    body: Box::pin(stream::iter([Err(TransportError::new(
                        "body disconnected",
                    ))])),
                });
            }
        };
        Ok(HttpResponse {
            status,
            headers: vec![("content-type".into(), "application/json".into())],
            body: Box::pin(stream::iter([Ok(body)])),
        })
    }
}

fn refused(status: u16, code: &str) -> Reply {
    Reply::Json(
        status,
        json!({"error":{"code":code,"message":"private account diagnostic"}}),
    )
}

fn unsupported() -> Reply {
    refused(426, "lane_version_unsupported")
}

fn allowance(lane: u32) -> Value {
    json!({
        "enabled":true,
        "balance":"78.00000",
        "granted":"78",
        "lane_version":lane,
        "check_in":{"nonce":format!("{lane:032x}"),"seconds":600}
    })
}

fn catalog() -> Value {
    json!({"models":[{
        "id":"glm-5.3-flash","name":"GLM 5.3 Flash","reasoning":true,
        "input":["text"],"contextWindow":320000,"maxTokens":131072,
        "theosPerRequest":"0"
    }]})
}

struct Mock {
    seen: Mutex<Vec<HttpRequest>>,
    real_instances: BTreeSet<String>,
    rejected: u32,
    initial: Reply,
    theos: BTreeMap<u32, Reply>,
    models: BTreeMap<u32, Reply>,
    active: AtomicUsize,
    peak: AtomicUsize,
    completed: AtomicUsize,
    catalog_after: Mutex<Vec<usize>>,
}

impl Mock {
    fn new(credentials: &[Credentials], rejected: u32) -> Self {
        Self {
            seen: Mutex::default(),
            real_instances: credentials
                .iter()
                .map(|saved| saved.instance.clone())
                .collect(),
            rejected,
            initial: unsupported(),
            theos: BTreeMap::new(),
            models: BTreeMap::new(),
            active: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            completed: AtomicUsize::new(0),
            catalog_after: Mutex::default(),
        }
    }

    fn accept(&mut self, lane: u32) {
        self.theos.insert(lane, Reply::Json(200, allowance(lane)));
    }

    fn requests(&self) -> Vec<HttpRequest> {
        self.seen.lock().unwrap().clone()
    }

    fn probes(&self) -> Vec<HttpRequest> {
        self.requests()
            .into_iter()
            .filter(|request| {
                request.url.ends_with("/theos")
                    && !self
                        .real_instances
                        .contains(header(&request.headers, "x-deepai-code-instance").unwrap())
            })
            .collect()
    }

    fn probe_lanes(&self) -> Vec<u32> {
        self.probes().iter().map(request_lane).collect()
    }
}

fn request_lane(request: &HttpRequest) -> u32 {
    header(&request.headers, "x-deepai-code-theos")
        .unwrap()
        .parse()
        .unwrap()
}

impl Transport for Mock {
    fn send(&self, request: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, TransportError>> {
        boxed(async move {
            assert_eq!(
                request.method, "GET",
                "lane recovery must never make a model POST"
            );
            assert!(request.body.is_empty());
            assert!(matches!(
                request.url.as_str(),
                "https://api.deepai.org/deepai_code/theos"
                    | "https://api.deepai.org/deepai_code/models"
            ));
            let lane = request_lane(&request);
            let real = self
                .real_instances
                .contains(header(&request.headers, "x-deepai-code-instance").unwrap());
            self.seen.lock().unwrap().push(request.clone());
            if request.url.ends_with("/models") {
                self.catalog_after
                    .lock()
                    .unwrap()
                    .push(self.completed.load(Ordering::SeqCst));
                return self
                    .models
                    .get(&lane)
                    .cloned()
                    .unwrap_or_else(|| Reply::Json(200, catalog()))
                    .response();
            }
            if real {
                return if lane == self.rejected {
                    self.initial.clone().response()
                } else {
                    Reply::Json(200, allowance(lane)).response()
                };
            }
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(active, Ordering::SeqCst);
            // Vary arrival order so response timing cannot choose the winner.
            tokio::time::sleep(Duration::from_millis(u64::from(15 - lane % 5))).await;
            self.active.fetch_sub(1, Ordering::SeqCst);
            self.completed.fetch_add(1, Ordering::SeqCst);
            self.theos
                .get(&lane)
                .cloned()
                .unwrap_or_else(unsupported)
                .response()
        })
    }
}

fn credentials(key: &str) -> Credentials {
    let mut saved = Credentials::new(key).unwrap();
    saved.check_in = Some(CheckIn {
        nonce: "f".repeat(32),
        seconds: 600.0,
        checked_at: 1,
    });
    saved
}

fn plugin(saved: Credentials, lanes: Arc<Lanes>) -> (DeepAIAcc, Arc<MemoryStore>) {
    let store = Arc::new(MemoryStore::new(saved));
    let plugin = DeepAIAcc::new(store.clone(), ClientInfo::default())
        .unwrap()
        .with_lanes(lanes);
    (plugin, store)
}

#[derive(Default)]
struct Store {
    lane: Mutex<Option<u32>>,
    loads: AtomicUsize,
    saves: Mutex<Vec<u32>>,
    fail_load: AtomicBool,
    fail_save: AtomicBool,
}

impl Store {
    fn new(lane: Option<u32>) -> Self {
        Self {
            lane: Mutex::new(lane),
            ..Self::default()
        }
    }
}

impl LaneStore for Store {
    fn load(&self) -> BoxFuture<'_, Result<Option<u32>, String>> {
        boxed(async {
            self.loads.fetch_add(1, Ordering::SeqCst);
            if self.fail_load.load(Ordering::SeqCst) {
                return Err("private lane persistence diagnostic".into());
            }
            Ok(*self.lane.lock().unwrap())
        })
    }

    fn save(&self, lane: u32) -> BoxFuture<'_, Result<(), String>> {
        boxed(async move {
            self.saves.lock().unwrap().push(lane);
            if self.fail_save.load(Ordering::SeqCst) {
                return Err("private lane persistence diagnostic".into());
            }
            *self.lane.lock().unwrap() = Some(lane);
            Ok(())
        })
    }
}

#[tokio::test]
async fn current_loads_once_and_validates_every_store() {
    assert_eq!(Lanes::default().current().await.unwrap(), DEFAULT_LANE);
    assert_eq!(MemoryLaneStore::default().load().await.unwrap(), None);
    let store = Arc::new(Store::new(Some(12)));
    let lanes = Lanes::new(store.clone());
    let (first, second) = futures_util::join!(lanes.current(), lanes.current());
    assert_eq!(first.unwrap(), 12);
    assert_eq!(second.unwrap(), 12);
    assert_eq!(store.loads.load(Ordering::SeqCst), 1);
    for lane in [0, MAX_LANE + 1, u32::MAX] {
        assert!(MemoryLaneStore::new(Some(lane)).load().await.is_err());
        assert!(MemoryLaneStore::default().save(lane).await.is_err());
        let store = Arc::new(Store::new(Some(lane)));
        let lanes = Lanes::new(store.clone());
        assert!(lanes.current().await.is_err());
        assert!(lanes.current().await.is_err());
        assert_eq!(store.loads.load(Ordering::SeqCst), 1);
    }
    for lane in [1, MAX_LANE] {
        assert_eq!(
            Lanes::new(Arc::new(MemoryLaneStore::new(Some(lane))))
                .current()
                .await
                .unwrap(),
            lane
        );
    }
}

#[tokio::test]
async fn unreadable_lane_store_blocks_all_network_calls() {
    let saved = credentials("test-key");
    let mock = Mock::new(std::slice::from_ref(&saved), DEFAULT_LANE);
    let store = Arc::new(Store::default());
    store.fail_load.store(true, Ordering::SeqCst);
    let lanes = Arc::new(Lanes::new(store.clone()));
    let (plugin, _) = plugin(saved, lanes.clone());
    assert!(plugin.check_in(&mock).await.is_err());
    let error = lanes.current().await.unwrap_err().to_string();
    assert!(!error.contains("private lane persistence diagnostic"));
    assert_eq!(store.loads.load(Ordering::SeqCst), 1);
    assert!(mock.requests().is_empty());
}

#[tokio::test]
async fn selects_minimum_valid_lane_only_after_whole_batch_and_catalog() {
    let saved = credentials("test-key");
    let mut mock = Mock::new(std::slice::from_ref(&saved), DEFAULT_LANE);
    mock.accept(10);
    mock.accept(11);
    mock.accept(12);
    let store = Arc::new(Store::default());
    let lanes = Arc::new(Lanes::new(store.clone()));
    let (plugin, _) = plugin(saved, lanes.clone());
    assert_eq!(plugin.check_in(&mock).await.unwrap().lane_version, Some(10));
    assert_eq!(mock.probe_lanes(), vec![8, 9, 10, 11, 12]);
    assert_eq!(mock.peak.load(Ordering::SeqCst), 5);
    assert_eq!(mock.active.load(Ordering::SeqCst), 0);
    assert_eq!(*mock.catalog_after.lock().unwrap(), vec![5]);
    let catalogs: Vec<_> = mock
        .requests()
        .iter()
        .filter(|request| request.url.ends_with("/models"))
        .map(request_lane)
        .collect();
    assert_eq!(catalogs, vec![10]);
    assert_eq!(*store.saves.lock().unwrap(), vec![10]);
    assert_eq!(lanes.current().await.unwrap(), 10);
    assert_eq!(Lanes::new(store.clone()).current().await.unwrap(), 10);
}

#[tokio::test]
async fn probe_identity_is_private_nonce_free_and_machine_identity_is_unchanged() {
    let saved = credentials("test-key");
    let mut mock = Mock::new(std::slice::from_ref(&saved), DEFAULT_LANE);
    mock.accept(9);
    let lanes = Arc::new(Lanes::default());
    let (plugin, store) = plugin(saved.clone(), lanes);
    plugin.check_in(&mock).await.unwrap();
    let probes = mock.probes();
    let instances: BTreeSet<_> = probes
        .iter()
        .map(|request| {
            let instance = header(&request.headers, "x-deepai-code-instance").unwrap();
            assert_eq!(instance.len(), 32);
            assert!(
                instance
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            );
            assert_ne!(instance, saved.instance);
            assert!(header(&request.headers, "x-deepai-code-check-in").is_none());
            instance.to_owned()
        })
        .collect();
    assert_eq!(instances.len(), 5);
    let requests = mock.requests();
    for request in &requests {
        assert_eq!(
            header(&request.headers, "api-key"),
            Some(saved.api_key.as_str())
        );
        assert_eq!(
            header(&request.headers, "x-deepai-code-machine"),
            Some(saved.machine_digest().as_str())
        );
        assert_eq!(header(&request.headers, "user-agent"), Some("node"));
        if request.url.ends_with("/theos") {
            assert!(
                header(&request.headers, "x-deepai-code-beacon")
                    .unwrap()
                    .starts_with("v=0.3.40;up=")
            );
        }
        if request.url.ends_with("/models") {
            assert!(header(&request.headers, "x-deepai-code-check-in").is_none());
            assert!(header(&request.headers, "x-deepai-code-beacon").is_none());
            assert_eq!(
                header(&request.headers, "x-deepai-code-instance"),
                Some(header(&probes[1].headers, "x-deepai-code-instance").unwrap())
            );
        }
    }
    let current = store.load().await.unwrap();
    assert_eq!(current.machine_uuid, saved.machine_uuid);
    assert_eq!(current.instance, saved.instance);
    assert_eq!(current.api_key, saved.api_key);
    assert_eq!(
        current.check_in.as_ref().unwrap().nonce,
        format!("{:032x}", 9)
    );
    assert_eq!(
        header(&requests[0].headers, "x-deepai-code-check-in"),
        Some("f".repeat(32).as_str())
    );
    assert_eq!(
        header(&requests.last().unwrap().headers, "x-deepai-code-instance"),
        Some(saved.instance.as_str())
    );
    assert!(!instances.contains(&current.instance));
}

#[tokio::test]
async fn catalog_unsupported_tries_the_next_verified_candidate_not_the_maximum() {
    let saved = credentials("test-key");
    let mut mock = Mock::new(std::slice::from_ref(&saved), DEFAULT_LANE);
    for lane in [8, 9, 12] {
        mock.accept(lane);
    }
    mock.models.insert(8, unsupported());
    let lanes = Arc::new(Lanes::default());
    let (plugin, _) = plugin(saved, lanes.clone());
    plugin.check_in(&mock).await.unwrap();
    assert_eq!(lanes.current().await.unwrap(), 9);
    assert_eq!(mock.probe_lanes(), vec![8, 9, 10, 11, 12]);
    let catalogs: Vec<_> = mock
        .requests()
        .iter()
        .filter(|request| request.url.ends_with("/models"))
        .map(request_lane)
        .collect();
    assert_eq!(catalogs, vec![8, 9]);
}

#[tokio::test]
async fn catalog_rejection_in_snapshot_triggers_recovery_without_a_model_post() {
    let saved = credentials("test-key");
    let mut mock = Mock::new(std::slice::from_ref(&saved), DEFAULT_LANE);
    mock.initial = Reply::Json(200, allowance(DEFAULT_LANE));
    mock.models.insert(DEFAULT_LANE, unsupported());
    mock.accept(8);
    let lanes = Arc::new(Lanes::default());
    let (plugin, _) = plugin(saved, lanes.clone());
    let snapshot = plugin.snapshot(&mock).await.unwrap();
    assert_eq!(snapshot.allowance.lane_version, Some(8));
    assert_eq!(snapshot.catalog.models.len(), 1);
    assert_eq!(lanes.current().await.unwrap(), 8);
    assert_eq!(mock.probe_lanes(), vec![8, 9, 10, 11, 12]);
    assert_eq!(*mock.catalog_after.lock().unwrap(), vec![0, 5, 5]);
}

#[tokio::test]
async fn all_verified_catalogs_unsupported_continue_to_the_next_batch() {
    let saved = credentials("test-key");
    let mut mock = Mock::new(std::slice::from_ref(&saved), DEFAULT_LANE);
    for lane in 8..=12 {
        mock.accept(lane);
        mock.models.insert(lane, unsupported());
    }
    mock.accept(13);
    let lanes = Arc::new(Lanes::default());
    let (plugin, _) = plugin(saved, lanes.clone());
    plugin.check_in(&mock).await.unwrap();
    assert_eq!(lanes.current().await.unwrap(), 13);
    assert_eq!(mock.probe_lanes(), (8..=17).collect::<Vec<_>>());
    assert_eq!(*mock.catalog_after.lock().unwrap(), vec![5, 5, 5, 5, 5, 10]);
}

#[tokio::test]
async fn reloaded_manager_uses_persisted_lane_without_search() {
    let saved = credentials("test-key");
    let mut mock = Mock::new(std::slice::from_ref(&saved), DEFAULT_LANE);
    mock.accept(8);
    let store = Arc::new(MemoryLaneStore::default());
    let (first_plugin, _) = plugin(saved.clone(), Arc::new(Lanes::new(store.clone())));
    first_plugin.check_in(&mock).await.unwrap();
    let probes = mock.probes().len();
    let (reloaded_plugin, _) = plugin(saved, Arc::new(Lanes::new(store)));
    assert_eq!(
        reloaded_plugin.check_in(&mock).await.unwrap().lane_version,
        Some(8)
    );
    assert_eq!(mock.probes().len(), probes);
}

#[tokio::test]
async fn ascending_batches_are_bounded_at_fifty() {
    let saved = credentials("test-key");
    let mut mock = Mock::new(std::slice::from_ref(&saved), DEFAULT_LANE);
    mock.accept(MAX_LANE);
    let lanes = Arc::new(Lanes::default());
    let (plugin, _) = plugin(saved, lanes.clone());
    plugin.check_in(&mock).await.unwrap();
    assert_eq!(mock.probe_lanes(), (8..=MAX_LANE).collect::<Vec<_>>());
    assert_eq!(mock.peak.load(Ordering::SeqCst), 5);
    assert_eq!(lanes.current().await.unwrap(), MAX_LANE);
    assert_eq!(*mock.catalog_after.lock().unwrap(), vec![43]);
}

#[tokio::test]
async fn rejected_fifty_never_probes_and_exhaustion_never_persists() {
    for rejected in [48, MAX_LANE] {
        let saved = credentials("test-key");
        let mock = Mock::new(std::slice::from_ref(&saved), rejected);
        let store = Arc::new(Store::new(Some(rejected)));
        let lanes = Arc::new(Lanes::new(store.clone()));
        let (plugin, _) = plugin(saved, lanes.clone());
        assert!(plugin.check_in(&mock).await.is_err());
        assert_eq!(
            mock.probe_lanes(),
            (rejected + 1..=MAX_LANE).collect::<Vec<_>>()
        );
        assert!(store.saves.lock().unwrap().is_empty());
        assert_eq!(lanes.current().await.unwrap(), rejected);
    }
}

#[tokio::test]
async fn only_exact_http_426_and_code_can_start_recovery() {
    let refusals = [
        refused(200, "lane_version_unsupported"),
        refused(400, "lane_version_unsupported"),
        refused(401, "invalid_api_key"),
        refused(403, "check_in_refused"),
        refused(402, "theos_insufficient"),
        refused(429, "lane_version_unsupported"),
        refused(426, "lane_version_unsupported_extra"),
        refused(426, "LANE_VERSION_UNSUPPORTED"),
        refused(426, "theos_exhausted"),
        Reply::Json(426, json!({"error":{"message":"lane_version_unsupported"}})),
        Reply::Raw(426, Bytes::from_static(b"not JSON")),
        Reply::Network,
        Reply::BodyNetwork,
    ];
    for refusal in refusals {
        let saved = credentials("test-key");
        let mut mock = Mock::new(std::slice::from_ref(&saved), DEFAULT_LANE);
        mock.initial = refusal;
        let lanes = Arc::new(Lanes::default());
        let (plugin, _) = plugin(saved, lanes.clone());
        assert!(plugin.check_in(&mock).await.is_err());
        assert!(mock.probes().is_empty());
        assert_eq!(mock.requests().len(), 1);
        assert_eq!(lanes.current().await.unwrap(), DEFAULT_LANE);
    }
}

#[tokio::test]
async fn fatal_probe_results_abort_even_when_a_lower_lane_succeeds() {
    let mut mismatched = allowance(9);
    mismatched["lane_version"] = json!(8);
    let mut missing_lane = allowance(9);
    missing_lane.as_object_mut().unwrap().remove("lane_version");
    let mut disabled = allowance(9);
    disabled["enabled"] = json!(false);
    let mut invalid_balance = allowance(9);
    invalid_balance["balance"] = json!("invalid");
    let mut invalid_nonce = allowance(9);
    invalid_nonce["check_in"]["nonce"] = json!("malformed");
    let mut missing_nonce = allowance(9);
    missing_nonce.as_object_mut().unwrap().remove("check_in");
    let mut reused_nonce = allowance(9);
    reused_nonce["check_in"]["nonce"] = json!("f".repeat(32));
    let mut expired_nonce = allowance(9);
    expired_nonce["check_in"]["seconds"] = json!(0);
    let mut invalid_grant = allowance(9);
    invalid_grant["granted"] = json!(-1);
    let mut missing_balance = allowance(9);
    missing_balance.as_object_mut().unwrap().remove("balance");
    let mut invalid_lane = allowance(9);
    invalid_lane["lane_version"] = json!(9.5);
    let failures = [
        refused(429, "rate_limit_exceeded"),
        refused(401, "invalid_api_key"),
        refused(402, "theos_exhausted"),
        refused(403, "check_in_refused"),
        refused(503, "check_in_unavailable"),
        refused(400, "lane_version_unsupported"),
        refused(426, "unknown_code"),
        Reply::Network,
        Reply::BodyNetwork,
        Reply::Raw(200, Bytes::from_static(b"not JSON")),
        Reply::Json(201, allowance(9)),
        Reply::Json(302, allowance(9)),
        Reply::Json(200, mismatched),
        Reply::Json(200, missing_lane),
        Reply::Json(200, disabled),
        Reply::Json(200, invalid_balance),
        Reply::Json(200, invalid_nonce),
        Reply::Json(200, missing_nonce),
        Reply::Json(200, reused_nonce),
        Reply::Json(200, expired_nonce),
        Reply::Json(200, invalid_grant),
        Reply::Json(200, missing_balance),
        Reply::Json(200, invalid_lane),
    ];
    for failure in failures {
        let saved = credentials("test-key");
        let mut mock = Mock::new(std::slice::from_ref(&saved), DEFAULT_LANE);
        mock.accept(8);
        mock.theos.insert(9, failure);
        let store = Arc::new(Store::default());
        let lanes = Arc::new(Lanes::new(store.clone()));
        let (plugin, account_store) = plugin(saved.clone(), lanes.clone());
        let error = plugin.check_in(&mock).await.unwrap_err().to_string();
        assert!(error.contains("/theos:"));
        assert!(!error.contains("private network diagnostic"));
        assert!(!error.contains("private account diagnostic"));
        assert_eq!(mock.probe_lanes(), vec![8, 9, 10, 11, 12]);
        assert_eq!(mock.completed.load(Ordering::SeqCst), 5);
        assert!(mock.catalog_after.lock().unwrap().is_empty());
        assert!(store.saves.lock().unwrap().is_empty());
        assert_eq!(lanes.current().await.unwrap(), DEFAULT_LANE);
        assert_eq!(account_store.load().await.unwrap(), saved);
    }
}

#[tokio::test]
async fn malformed_or_fatal_catalog_never_commits_or_tries_another_candidate() {
    let failures = [
        Reply::Json(200, json!({"models":[]})),
        Reply::Json(200, json!({"models":[{"id":"broken"}]})),
        Reply::Raw(200, Bytes::from_static(b"not JSON")),
        Reply::Raw(200, Bytes::from(vec![b'x'; 2 * 1024 * 1024 + 1])),
        Reply::Json(201, catalog()),
        Reply::Json(302, catalog()),
        refused(429, "rate_limit_exceeded"),
        refused(401, "invalid_api_key"),
        refused(402, "theos_insufficient"),
        refused(400, "lane_version_unsupported"),
        refused(426, "different_code"),
        Reply::Network,
        Reply::BodyNetwork,
    ];
    for failure in failures {
        let saved = credentials("test-key");
        let mut mock = Mock::new(std::slice::from_ref(&saved), DEFAULT_LANE);
        mock.accept(8);
        mock.accept(9);
        mock.models.insert(8, failure);
        let store = Arc::new(Store::default());
        let lanes = Arc::new(Lanes::new(store.clone()));
        let (plugin, _) = plugin(saved, lanes.clone());
        assert!(plugin.check_in(&mock).await.is_err());
        assert_eq!(mock.probe_lanes(), vec![8, 9, 10, 11, 12]);
        assert_eq!(*mock.catalog_after.lock().unwrap(), vec![5]);
        assert!(store.saves.lock().unwrap().is_empty());
        assert_eq!(lanes.current().await.unwrap(), DEFAULT_LANE);
    }
}

#[tokio::test]
async fn failed_save_keeps_active_lane_then_reuses_pending_choice_without_probes() {
    let saved = credentials("test-key");
    let mut mock = Mock::new(std::slice::from_ref(&saved), DEFAULT_LANE);
    mock.accept(8);
    let store = Arc::new(Store::default());
    store.fail_save.store(true, Ordering::SeqCst);
    let lanes = Arc::new(Lanes::new(store.clone()));
    let (plugin, account_store) = plugin(saved.clone(), lanes.clone());
    assert!(plugin.check_in(&mock).await.is_err());
    assert_eq!(lanes.current().await.unwrap(), DEFAULT_LANE);
    assert_eq!(*store.lane.lock().unwrap(), None);
    assert_eq!(account_store.load().await.unwrap(), saved);
    assert_eq!(mock.probes().len(), 5);
    store.fail_save.store(false, Ordering::SeqCst);
    plugin.check_in(&mock).await.unwrap();
    assert_eq!(lanes.current().await.unwrap(), 8);
    assert_eq!(*store.saves.lock().unwrap(), vec![8, 8]);
    assert_eq!(mock.probes().len(), 5);
    assert_eq!(*mock.catalog_after.lock().unwrap(), vec![5]);
}

#[tokio::test]
async fn shared_managers_coordinate_one_search_across_accounts() {
    let first = credentials("first-key");
    let second = credentials("second-key");
    let mut mock = Mock::new(&[first.clone(), second.clone()], DEFAULT_LANE);
    mock.accept(8);
    let store = Arc::new(Store::default());
    let lanes = Arc::new(Lanes::new(store.clone()));
    let (first_plugin, first_store) = plugin(first.clone(), lanes.clone());
    let (second_plugin, second_store) = plugin(second.clone(), lanes.clone());
    let (first_result, second_result) =
        futures_util::join!(first_plugin.check_in(&mock), second_plugin.check_in(&mock));
    assert_eq!(first_result.unwrap().lane_version, Some(8));
    assert_eq!(second_result.unwrap().lane_version, Some(8));
    assert_eq!(mock.probes().len(), 5);
    assert_eq!(*store.saves.lock().unwrap(), vec![8]);
    assert_eq!(first_store.load().await.unwrap().instance, first.instance);
    assert_eq!(second_store.load().await.unwrap().instance, second.instance);
}

#[tokio::test]
async fn concurrent_failed_metadata_shares_a_brief_search_cooldown() {
    let first = credentials("first-key");
    let second = credentials("second-key");
    let mut mock = Mock::new(&[first.clone(), second.clone()], DEFAULT_LANE);
    mock.theos.insert(8, refused(429, "rate_limit_exceeded"));
    let lanes = Arc::new(Lanes::default());
    let (first_plugin, _) = plugin(first, lanes.clone());
    let (second_plugin, _) = plugin(second, lanes.clone());
    let (first_result, second_result) =
        futures_util::join!(first_plugin.check_in(&mock), second_plugin.check_in(&mock));
    assert!(first_result.is_err());
    assert!(second_result.is_err());
    assert_eq!(mock.probes().len(), 5);
    assert!(first_plugin.check_in(&mock).await.is_err());
    assert!(second_plugin.check_in(&mock).await.is_err());
    assert_eq!(mock.probes().len(), 5);
    assert_eq!(lanes.current().await.unwrap(), DEFAULT_LANE);
}
