use std::sync::{Arc, Mutex as StoreMutex};
use std::time::{Duration, Instant};

use futures_util::{future::join_all, lock::Mutex};
use ncore::Error;
use ncore::rt::{BoxFuture, MaybeSend, MaybeSync, boxed};

use crate::api::{self, Sender, fail};
use crate::credentials::unix_ms;
use crate::{Catalog, ClientInfo, Credentials, quota};

pub const DEFAULT_LANE: u32 = 7;
pub const MAX_LANE: u32 = 50;

/// Host-owned persistence for the accepted lane, shared across accounts. Contains no credentials.
pub trait LaneStore: MaybeSend + MaybeSync {
    fn load(&self) -> BoxFuture<'_, Result<Option<u32>, String>>;
    fn save(&self, lane: u32) -> BoxFuture<'_, Result<(), String>>;
}

#[derive(Debug, Default)]
pub struct MemoryLaneStore(StoreMutex<Option<u32>>);

impl MemoryLaneStore {
    pub fn new(lane: Option<u32>) -> Self {
        Self(StoreMutex::new(lane))
    }
}

impl LaneStore for MemoryLaneStore {
    fn load(&self) -> BoxFuture<'_, Result<Option<u32>, String>> {
        boxed(async {
            let lane = *self.0.lock().map_err(|_| "DeepAI lane store poisoned")?;
            if let Some(lane) = lane {
                validate(lane)?;
            }
            Ok(lane)
        })
    }

    fn save(&self, lane: u32) -> BoxFuture<'_, Result<(), String>> {
        boxed(async move {
            validate(lane)?;
            *self.0.lock().map_err(|_| "DeepAI lane store poisoned")? = Some(lane);
            Ok(())
        })
    }
}

#[derive(Default)]
struct State {
    current: Option<Result<u32, Error>>,
    pending: Option<u32>,
    failure: Option<(Instant, Error)>,
}

struct SearchCooldown<'a>(&'a mut Option<(Instant, Error)>);

impl Drop for SearchCooldown<'_> {
    fn drop(&mut self) {
        *self.0 = Some((
            Instant::now(),
            fail("DeepAI lane search was interrupted; retry after the brief cooldown"),
        ));
    }
}

/// Reuse one manager across all host accounts; probes never update an account's identity or nonce.
pub struct Lanes {
    state: Mutex<State>,
    store: Arc<dyn LaneStore>,
}

impl Default for Lanes {
    fn default() -> Self {
        Self::new(Arc::new(MemoryLaneStore::default()))
    }
}

impl Lanes {
    pub fn new(store: Arc<dyn LaneStore>) -> Self {
        Self {
            state: Mutex::default(),
            store,
        }
    }

    /// Loads the accepted lane once. Invalid or unreadable saved state fails closed.
    pub async fn current(&self) -> Result<u32, Error> {
        let mut state = self.state.lock().await;
        self.load(&mut state).await
    }

    async fn load(&self, state: &mut State) -> Result<u32, Error> {
        if let Some(current) = &state.current {
            return current.clone();
        }
        let current = match self.store.load().await {
            Ok(lane) => {
                let lane = lane.unwrap_or(DEFAULT_LANE);
                validate(lane).map(|()| lane).map_err(fail)
            }
            Err(_) => Err(fail("could not load DeepAI lane")),
        };
        state.current = Some(current.clone());
        current
    }

    /// Called only after HTTP 426 with the exact `lane_version_unsupported` code.
    pub(crate) async fn recover(
        &self,
        sender: Sender<'_>,
        credentials: &Credentials,
        client: &ClientInfo,
        uptime: u64,
        rejected: u32,
    ) -> Result<u32, Error> {
        validate(rejected).map_err(fail)?;
        let mut state = self.state.lock().await;
        let current = self.load(&mut state).await?;
        if current != rejected {
            return Ok(current);
        }
        if state.pending.is_some() {
            return self.persist(&mut state).await;
        }
        if let Some((failed_at, error)) = &state.failure
            && failed_at.elapsed() < Duration::from_secs(60)
        {
            return Err(error.clone());
        }
        let cooldown = SearchCooldown(&mut state.failure);
        let result = find_lane(sender, credentials, client, uptime, rejected).await;
        drop(cooldown);
        match result {
            Ok(lane) => {
                // Keep the verified choice on failed or cancelled persistence, without probe credentials.
                state.pending = Some(lane);
                self.persist(&mut state).await
            }
            Err(error) => {
                state.failure = Some((Instant::now(), error.clone()));
                Err(error)
            }
        }
    }

    /// Called when a successful check-in names a lane other than the one we sent.
    pub(crate) async fn adopt(&self, sent: u32, reported: u32) -> Result<u32, Error> {
        validate(reported).map_err(fail)?;
        let mut state = self.state.lock().await;
        let current = self.load(&mut state).await?;
        if current != sent {
            return Ok(current);
        }
        state.pending = Some(reported);
        self.persist(&mut state).await
    }

    async fn persist(&self, state: &mut State) -> Result<u32, Error> {
        let lane = state.pending.expect("verified DeepAI lane pending");
        self.store
            .save(lane)
            .await
            .map_err(|_| fail("could not persist DeepAI lane; active lane was not changed"))?;
        state.current = Some(Ok(lane));
        state.pending = None;
        state.failure = None;
        Ok(lane)
    }
}

fn validate(lane: u32) -> Result<(), String> {
    if !(1..=MAX_LANE).contains(&lane) {
        return Err("invalid DeepAI lane; expected 1 through 50".into());
    }
    Ok(())
}

pub(crate) fn unsupported(error: &Error) -> bool {
    matches!(error, Error::Provider(error)
        if error.status == Some(426)
            && error.code.as_deref() == Some("lane_version_unsupported"))
}

async fn find_lane(
    sender: Sender<'_>,
    credentials: &Credentials,
    client: &ClientInfo,
    uptime: u64,
    rejected: u32,
) -> Result<u32, Error> {
    credentials.validate().map_err(fail)?;
    client.validate().map_err(fail)?;
    let mut first = rejected + 1;
    while first <= MAX_LANE {
        let last = (first + 4).min(MAX_LANE);
        let results = join_all((first..=last).map(|lane| async move {
            probe(sender, credentials, client, uptime, lane)
                .await
                .map_err(|error| api::account_error("/theos", error))
        }))
        .await;
        // Finish the whole batch and reject any fatal result before validating or committing a winner.
        let candidates = results.into_iter().collect::<Result<Vec<_>, _>>()?;
        for (lane, credentials) in candidates.into_iter().flatten() {
            if verify_catalog(sender, &credentials, lane)
                .await
                .map_err(|error| api::account_error("/models", error))?
            {
                return Ok(lane);
            }
        }
        first = last + 1;
    }
    Err(fail("no supported DeepAI lane found through lane 50"))
}

async fn probe(
    sender: Sender<'_>,
    credentials: &Credentials,
    client: &ClientInfo,
    uptime: u64,
    lane: u32,
) -> Result<Option<(u32, Credentials)>, Error> {
    let old_nonce = credentials
        .check_in
        .as_ref()
        .map(|mark| mark.nonce.as_str());
    let mut credentials = credentials.clone();
    let mut instance = [0u8; 16];
    getrandom::fill(&mut instance)
        .map_err(|_| fail("could not create DeepAI lane probe identity"))?;
    credentials.instance = instance.iter().map(|byte| format!("{byte:02x}")).collect();
    credentials.check_in = None;
    credentials.lane = lane;
    let mut request = api::request(&credentials, "/theos", lane)?;
    request
        .headers
        .push(("x-deepai-code-beacon".into(), client.beacon(uptime)));
    let (status, value) = api::json_response(sender.send(request).await?).await?;
    if status != 200 || value.get("error").is_some_and(|error| !error.is_null()) {
        let error = Error::Provider(api::provider_error(Some(status), &value));
        return if unsupported(&error) {
            Ok(None)
        } else {
            Err(error)
        };
    }
    let allowance = quota::parse_allowance(&value).map_err(fail)?;
    if !allowance.enabled {
        return Err(fail("DeepAI lane probe allowance is disabled"));
    }
    if allowance.lane_version != Some(lane) {
        return Err(fail(
            "DeepAI lane probe reported a different or missing lane",
        ));
    }
    let mark = quota::parse_check_in(&value, unix_ms())
        .map_err(fail)?
        .ok_or_else(|| fail("DeepAI lane probe did not supply fresh check-in state"))?;
    if old_nonce == Some(mark.nonce.as_str()) {
        return Err(fail("DeepAI lane probe reused the account check-in nonce"));
    }
    credentials.check_in = Some(mark);
    Ok(Some((lane, credentials)))
}

async fn verify_catalog(
    sender: Sender<'_>,
    credentials: &Credentials,
    lane: u32,
) -> Result<bool, Error> {
    let request = api::request(credentials, "/models", lane)?;
    let (status, value) = api::json_response(sender.send(request).await?).await?;
    if status != 200 || value.get("error").is_some_and(|error| !error.is_null()) {
        let error = Error::Provider(api::provider_error(Some(status), &value));
        return if unsupported(&error) {
            Ok(false)
        } else {
            Err(error)
        };
    }
    Catalog::from_json(value, unix_ms()).map_err(fail)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::task::Poll;

    use bytes::Bytes;
    use futures_util::{FutureExt, future::poll_fn, stream};
    use ncore::{HttpRequest, HttpResponse, Transport, TransportError};
    use serde_json::json;
    use tokio::sync::Notify;

    use super::*;

    struct Mock {
        refuse: AtomicBool,
        requests: AtomicUsize,
    }

    impl Transport for Mock {
        fn send(
            &self,
            request: HttpRequest,
        ) -> BoxFuture<'_, Result<HttpResponse, TransportError>> {
            boxed(async move {
                assert_eq!(request.method, "GET");
                self.requests.fetch_add(1, Ordering::SeqCst);
                let lane: u32 = ncore::rt::header(&request.headers, "x-deepai-code-theos")
                    .unwrap()
                    .parse()
                    .unwrap();
                let (status, value) = if self.refuse.load(Ordering::SeqCst) {
                    (429, json!({"error":{"code":"rate_limit_exceeded"}}))
                } else if request.url.ends_with("/theos") {
                    (
                        200,
                        json!({"enabled":true,"balance":"78","granted":"78",
                        "lane_version":lane,"check_in":{"nonce":format!("{lane:032x}"),"seconds":600}}),
                    )
                } else {
                    (
                        200,
                        json!({"models":[{"id":"test","name":"Test","reasoning":false,
                        "input":["text"],"contextWindow":100,"maxTokens":50}]}),
                    )
                };
                Ok(HttpResponse {
                    status,
                    headers: Vec::new(),
                    body: Box::pin(stream::iter([Ok(Bytes::from(value.to_string()))])),
                })
            })
        }
    }

    struct PendingProbes {
        mock: Mock,
        real_instances: Vec<String>,
        paused: AtomicBool,
        probes: AtomicUsize,
        active: AtomicUsize,
        cancelled: AtomicUsize,
    }

    struct ActiveProbe<'a>(&'a PendingProbes);

    impl Drop for ActiveProbe<'_> {
        fn drop(&mut self) {
            self.0.active.fetch_sub(1, Ordering::SeqCst);
            self.0.cancelled.fetch_add(1, Ordering::SeqCst);
        }
    }

    impl Transport for PendingProbes {
        fn send(
            &self,
            request: HttpRequest,
        ) -> BoxFuture<'_, Result<HttpResponse, TransportError>> {
            boxed(async move {
                assert_eq!(request.method, "GET");
                let real = self.real_instances.iter().any(|instance| {
                    ncore::rt::header(&request.headers, "x-deepai-code-instance")
                        == Some(instance.as_str())
                });
                let lane: u32 = ncore::rt::header(&request.headers, "x-deepai-code-theos")
                    .unwrap()
                    .parse()
                    .unwrap();
                if real && lane == DEFAULT_LANE {
                    return Ok(HttpResponse {
                        status: 426,
                        headers: Vec::new(),
                        body: Box::pin(stream::iter([Ok(Bytes::from(
                            json!({"error":{"code":"lane_version_unsupported"}}).to_string(),
                        ))])),
                    });
                }
                if !real && request.url.ends_with("/theos") {
                    self.probes.fetch_add(1, Ordering::SeqCst);
                    if self.paused.load(Ordering::SeqCst) {
                        self.active.fetch_add(1, Ordering::SeqCst);
                        let _active = ActiveProbe(self);
                        std::future::pending::<()>().await;
                    }
                }
                self.mock.send(request).await
            })
        }
    }

    #[tokio::test]
    async fn cancelled_active_probes_block_another_account_until_cooldown_expires() {
        let first = Credentials::new("first-key").unwrap();
        let second = Credentials::new("second-key").unwrap();
        let mock = PendingProbes {
            mock: Mock {
                refuse: AtomicBool::new(false),
                requests: AtomicUsize::new(0),
            },
            real_instances: vec![first.instance.clone(), second.instance.clone()],
            paused: AtomicBool::new(true),
            probes: AtomicUsize::new(0),
            active: AtomicUsize::new(0),
            cancelled: AtomicUsize::new(0),
        };
        let lanes = Arc::new(Lanes::default());
        let first_store = Arc::new(crate::MemoryStore::new(first.clone()));
        let second_store = Arc::new(crate::MemoryStore::new(second.clone()));
        let first_plugin = crate::DeepAIAcc::new(first_store.clone(), ClientInfo::default())
            .unwrap()
            .with_lanes(lanes.clone());
        let second_plugin = crate::DeepAIAcc::new(second_store.clone(), ClientInfo::default())
            .unwrap()
            .with_lanes(lanes.clone());
        let mut first_check = Box::pin(first_plugin.check_in(&mock));
        assert!(
            poll_fn(|context| Poll::Ready(first_check.as_mut().poll(context).is_pending())).await
        );
        assert_eq!(mock.probes.load(Ordering::SeqCst), 5);
        assert_eq!(mock.active.load(Ordering::SeqCst), 5);
        assert_eq!(mock.mock.requests.load(Ordering::SeqCst), 0);

        let mut second_check = Box::pin(second_plugin.check_in(&mock));
        assert!(
            poll_fn(|context| Poll::Ready(second_check.as_mut().poll(context).is_pending())).await
        );
        drop(first_check);
        assert_eq!(mock.active.load(Ordering::SeqCst), 0);
        assert_eq!(mock.cancelled.load(Ordering::SeqCst), 5);
        assert_eq!(lanes.current().await.unwrap(), DEFAULT_LANE);
        assert!(lanes.state.lock().await.pending.is_none());
        mock.paused.store(false, Ordering::SeqCst);
        let error = second_check.await.unwrap_err();
        assert!(error.to_string().contains("interrupted"));
        assert!(second_plugin.check_in(&mock).await.is_err());
        assert_eq!(mock.probes.load(Ordering::SeqCst), 5);
        assert_eq!(mock.mock.requests.load(Ordering::SeqCst), 0);
        assert_eq!(
            crate::CredentialStore::load(&*first_store).await.unwrap(),
            first
        );
        assert_eq!(
            crate::CredentialStore::load(&*second_store).await.unwrap(),
            second
        );

        lanes.state.lock().await.failure.as_mut().unwrap().0 =
            Instant::now() - Duration::from_secs(61);
        assert_eq!(
            second_plugin.check_in(&mock).await.unwrap().lane_version,
            Some(8)
        );
        assert_eq!(mock.probes.load(Ordering::SeqCst), 10);
        assert_eq!(mock.mock.requests.load(Ordering::SeqCst), 7);
        assert_eq!(lanes.current().await.unwrap(), 8);
        assert!(lanes.state.lock().await.failure.is_none());
    }

    #[tokio::test]
    async fn failed_candidates_are_retested_after_cooldown() {
        let mock = Mock {
            refuse: AtomicBool::new(true),
            requests: AtomicUsize::new(0),
        };
        let lanes = Lanes::default();
        let credentials = Credentials::new("test-key").unwrap();
        let client = ClientInfo::default();
        let sender = Sender::Transport(&mock);
        assert!(
            lanes
                .recover(sender, &credentials, &client, 0, DEFAULT_LANE)
                .await
                .is_err()
        );
        assert_eq!(mock.requests.load(Ordering::SeqCst), 5);
        mock.refuse.store(false, Ordering::SeqCst);
        assert!(
            lanes
                .recover(sender, &credentials, &client, 0, DEFAULT_LANE)
                .await
                .is_err()
        );
        assert_eq!(mock.requests.load(Ordering::SeqCst), 5);
        lanes.state.lock().await.failure.as_mut().unwrap().0 =
            Instant::now() - Duration::from_secs(61);
        assert_eq!(
            lanes
                .recover(sender, &credentials, &client, 0, DEFAULT_LANE)
                .await
                .unwrap(),
            8
        );
        assert_eq!(mock.requests.load(Ordering::SeqCst), 11);
        assert!(lanes.state.lock().await.failure.is_none());
    }

    struct DeferredStore {
        release: Notify,
        attempts: AtomicUsize,
        saved: StoreMutex<Option<u32>>,
    }

    impl LaneStore for DeferredStore {
        fn load(&self) -> BoxFuture<'_, Result<Option<u32>, String>> {
            boxed(async { Ok(*self.saved.lock().unwrap()) })
        }

        fn save(&self, lane: u32) -> BoxFuture<'_, Result<(), String>> {
            boxed(async move {
                self.attempts.fetch_add(1, Ordering::SeqCst);
                self.release.notified().await;
                *self.saved.lock().unwrap() = Some(lane);
                Ok(())
            })
        }
    }

    #[tokio::test]
    async fn cancelled_save_retains_pending_lane_without_publishing_or_reprobing() {
        let store = Arc::new(DeferredStore {
            release: Notify::new(),
            attempts: AtomicUsize::new(0),
            saved: StoreMutex::new(None),
        });
        let lanes = Lanes::new(store.clone());
        let mock = Mock {
            refuse: AtomicBool::new(false),
            requests: AtomicUsize::new(0),
        };
        let credentials = Credentials::new("test-key").unwrap();
        let client = ClientInfo::default();
        let sender = Sender::Transport(&mock);
        let mut recovery = Box::pin(lanes.recover(sender, &credentials, &client, 0, DEFAULT_LANE));
        assert!(poll_fn(|context| Poll::Ready(recovery.as_mut().poll(context).is_pending())).await);
        assert_eq!(store.attempts.load(Ordering::SeqCst), 1);
        assert_eq!(*store.saved.lock().unwrap(), None);
        assert!(lanes.current().now_or_never().is_none());
        drop(recovery);
        assert_eq!(lanes.current().await.unwrap(), DEFAULT_LANE);
        assert_eq!(lanes.state.lock().await.pending, Some(8));
        store.release.notify_one();
        assert_eq!(
            lanes
                .recover(sender, &credentials, &client, 0, DEFAULT_LANE)
                .await
                .unwrap(),
            8
        );
        assert_eq!(store.attempts.load(Ordering::SeqCst), 2);
        assert_eq!(mock.requests.load(Ordering::SeqCst), 6);
        assert_eq!(*store.saved.lock().unwrap(), Some(8));
    }

    #[tokio::test]
    async fn invalid_or_already_changed_rejections_never_probe() {
        let mock = Mock {
            refuse: AtomicBool::new(false),
            requests: AtomicUsize::new(0),
        };
        let lanes = Lanes::new(Arc::new(MemoryLaneStore::new(Some(12))));
        let credentials = Credentials::new("test-key").unwrap();
        let client = ClientInfo::default();
        let sender = Sender::Transport(&mock);
        for rejected in [0, MAX_LANE + 1, u32::MAX] {
            assert!(
                lanes
                    .recover(sender, &credentials, &client, 0, rejected)
                    .await
                    .is_err()
            );
        }
        assert_eq!(
            lanes
                .recover(sender, &credentials, &client, 0, DEFAULT_LANE)
                .await
                .unwrap(),
            12
        );
        assert_eq!(mock.requests.load(Ordering::SeqCst), 0);
    }
}
