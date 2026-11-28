mod accounts_file;
mod antigravity;
mod api_keys;
mod callback;
pub mod chrome_extension;
pub mod chrome_host;
mod chatgpt;
mod claude;
mod custom_providers;
mod deepai;
mod files;
#[cfg(test)]
mod gemini_tests;
mod grok;
mod local_proxy;
mod models;
mod projects;
mod prompt;
mod providers;
mod quota;
mod sessions;
mod skills;
pub mod terminal;
mod tools;
pub use tools::browser_panel;
mod workbuddy;
mod zcode;

use std::path::Path;
use std::sync::Arc;

use nabu_protocol::{Done, Event, ProtocolError, Reply, Request, ServerInfo};
use ncore::Core;
use ncore_tokio::{HyperTransport, Proxy};
use tokio::sync::{broadcast, watch};

pub struct Server {
    events: broadcast::Sender<Event>,
    providers_ready: watch::Sender<bool>,
    antigravity: Arc<antigravity::Accounts>,
    claude: Arc<claude::ClaudeAccounts>,
    chatgpt: Arc<chatgpt::ChatGptAccounts>,
    zcode: Arc<zcode::ZCodeAccounts>,
    grok: Arc<grok::GrokAccounts>,
    workbuddy: Arc<workbuddy::WorkBuddyAccounts>,
    deepai: Arc<deepai::DeepAIAccounts>,
    sessions: Arc<sessions::Sessions>,
    projects: Arc<projects::Projects>,
    file_watchers: files::FileWatchers,
    models: Arc<models::Models>,
    switches: Arc<providers::Switches>,
    agentrouter: Arc<api_keys::ApiKeyAccounts>,
    coarena: Arc<api_keys::ApiKeyAccounts>,
    clauden: Arc<api_keys::ApiKeyAccounts>,
    gemini: Arc<api_keys::ApiKeyAccounts>,
    nvidia_build: Arc<api_keys::ApiKeyAccounts>,
    pub terminals: Arc<terminal::Terminals>,
}

impl Server {
    pub fn new(data_dir: &Path) -> Self {
        let net = Arc::new(HyperTransport::with_proxy(proxy_from_env()));
        Self::with_transport(data_dir, net.clone(), net)
    }

    fn with_transport(
        data_dir: &Path,
        net: Arc<HyperTransport>,
        model_transport: Arc<dyn ncore::Transport>,
    ) -> Self {
        let (events, _) = broadcast::channel(256);
        tools::browser_panel::send_events_to(events.clone());
        let antigravity = antigravity::Accounts::load(
            data_dir.join("antigravity-accounts.json"),
            net.clone(),
            events.clone(),
        );
        let claude = claude::ClaudeAccounts::load(data_dir, net.clone(), events.clone());
        let chatgpt = chatgpt::ChatGptAccounts::load(data_dir, net.clone(), events.clone());
        let zcode = zcode::ZCodeAccounts::load(data_dir, net.clone(), events.clone());
        let grok = grok::GrokAccounts::load(data_dir, net.clone(), events.clone());
        let workbuddy = workbuddy::WorkBuddyAccounts::load(data_dir, net.clone(), events.clone());
        let deepai = deepai::DeepAIAccounts::load(data_dir, net.clone(), events.clone());
        let agentrouter = api_keys::ApiKeyAccounts::load(
            api_keys::KeyProvider::AgentRouter,
            data_dir,
            model_transport.clone(),
            events.clone(),
        );
        let coarena = api_keys::ApiKeyAccounts::load(
            api_keys::KeyProvider::CoArena,
            data_dir,
            model_transport.clone(),
            events.clone(),
        );
        let clauden = api_keys::ApiKeyAccounts::load(
            api_keys::KeyProvider::Clauden,
            data_dir,
            model_transport.clone(),
            events.clone(),
        );
        let gemini = api_keys::ApiKeyAccounts::load(
            api_keys::KeyProvider::Gemini,
            data_dir,
            model_transport.clone(),
            events.clone(),
        );
        let nvidia_build = api_keys::ApiKeyAccounts::load(
            api_keys::KeyProvider::NvidiaBuild,
            data_dir,
            model_transport.clone(),
            events.clone(),
        );
        let custom = custom_providers::CustomProviders::load(data_dir);
        let switches = providers::Switches::load(data_dir);
        let models = Arc::new(models::Models::new(
            Core::builder(model_transport).build(),
            antigravity.clone(),
            claude.clone(),
            chatgpt.clone(),
            zcode.clone(),
            workbuddy.clone(),
            deepai.clone(),
            grok.clone(),
            agentrouter.clone(),
            coarena.clone(),
            clauden.clone(),
            gemini.clone(),
            nvidia_build.clone(),
            custom,
            switches.clone(),
        ));
        let sessions =
            sessions::Sessions::load(data_dir, models.clone(), events.clone());
        let projects = projects::Projects::load(data_dir, events.clone());
        tools::browser::set_home(data_dir);
        let terminals = terminal::Terminals::new(events.clone());
        let file_watchers = files::FileWatchers::new(events.clone());
        Self {
            events,
            providers_ready: watch::channel(false).0,
            antigravity,
            claude,
            chatgpt,
            zcode,
            workbuddy,
            deepai,
            grok,
            sessions,
            projects,
            file_watchers,
            models,
            switches,
            agentrouter,
            coarena,
            clauden,
            gemini,
            nvidia_build,
            terminals,
        }
    }

    /// Background work after startup: checks the stored accounts of the providers that are on.
    pub async fn start(&self) {
        use nabu_protocol::ProviderAuth;
        use futures_util::future::join_all;
        self.forward_provider_events();
        // Key providers can hold dozens of slow keys, so their models arrive later through events.
        let registry = self.registry();
        let accounts = async {
            join_all(registry.enabled(ProviderAuth::Account).iter().map(|handle| handle.check_all())).await;
            self.providers_ready.send_replace(true);
        };
        let keys = registry.enabled(ProviderAuth::Key);
        tokio::join!(accounts, join_all(keys.iter().map(|handle| handle.check_all())));
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(5 * 60)).await;
                let handles = [ProviderAuth::Account, ProviderAuth::Key].into_iter().flat_map(|auth| registry.enabled(auth));
                join_all(handles.map(|handle| async move { handle.check_periodic().await })).await;
            }
        });
    }

    /// The kept attachment a `/file` link points to.
    pub fn attachment(&self, session: &str, name: &str) -> Option<std::path::PathBuf> {
        self.sessions.attachment(session, name)
    }

    pub fn upload(&self, session: &str, part: &str, mime: &str, filename: Option<String>) -> Result<(std::path::PathBuf, String), String> {
        self.sessions.upload(session, part, mime, filename)
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    pub async fn handle(&self, request: Request) -> Result<Reply, ProtocolError> {
        let done = Ok(Reply::Done(Done {}));
        match request {
            Request::ServerInfo => Ok(Reply::ServerInfo(self.info())),
            Request::Providers => {
                let _ = self
                    .providers_ready
                    .subscribe()
                    .wait_for(|ready| *ready)
                    .await;
                Ok(Reply::Providers(self.models.providers()))
            }
            Request::CustomProvider { id } => self
                .models
                .custom
                .get(&id)
                .map(Reply::CustomProvider)
                .map_err(failed),
            Request::SaveCustomProvider(provider) => {
                self.models.custom.save(provider).map_err(failed)?;
                let _ = self.events.send(Event::ProvidersChanged);
                done
            }
            Request::RemoveCustomProvider { id } => {
                self.models.custom.remove(&id).map_err(failed)?;
                let _ = self.events.send(Event::ProvidersChanged);
                done
            }
            Request::ProviderCatalog => Ok(Reply::Catalog(self.catalog())),
            Request::SetProviderEnabled { provider, enabled } => self.set_enabled(&provider, enabled),
            Request::ProviderAccounts { provider } => self.accounts(&provider),
            Request::ProviderUsage { provider } => self.usage(&provider),
            Request::ProviderLogin { provider, language, timezone } => self.login(&provider, language, timezone).await,
            Request::ProviderLoginFinish { provider, code } => self.finish_login(&provider, &code).await,
            Request::ProviderAddKeys { provider, keys } => self.add_keys(&provider, keys),
            Request::ProviderRemove { provider, email, id } => self.remove(&provider, Some(&email), id.as_deref()),
            Request::ProviderRemoveAll { provider } => self.remove(&provider, None, None),
            Request::ProviderCheckAll { provider } => self.check_all(&provider),
            Request::HiddenModels => Ok(self.hidden_models()),
            Request::SetModelsHidden { models, hidden } => self.set_models_hidden(models, hidden),
            // The old per-provider requests, kept for the old GUI
            Request::AntigravityAccounts => self.accounts("antigravity"),
            Request::AntigravityQuota => Ok(Reply::Quota(self.antigravity.quota())),
            Request::AntigravityLogin => self.login("antigravity", None, None).await,
            Request::AntigravityRemove { email } => self.remove("antigravity", Some(&email), None),
            Request::AntigravityRemoveAll => self.remove("antigravity", None, None),
            Request::AntigravityRefresh => {
                let accounts = self.antigravity.clone();
                tokio::spawn(async move { accounts.check_all(false).await });
                done
            }
            Request::AntigravityCheckAll => self.check_all("antigravity"),
            Request::ClaudeAccounts => self.accounts("claude-account"),
            Request::ClaudeQuota => Ok(Reply::Quota(self.claude.quota())),
            Request::ClaudeLogin => self.login("claude-account", None, None).await,
            Request::ClaudeLoginFinish { code } => self.finish_login("claude-account", &code).await,
            Request::ClaudeRemove { email } => self.remove("claude-account", Some(&email), None),
            Request::ClaudeRemoveAll => self.remove("claude-account", None, None),
            Request::ClaudeCheckAll => self.check_all("claude-account"),
            Request::ChatGptAccounts => self.accounts("chatgpt-account"),
            Request::ChatGptQuota => Ok(Reply::Quota(self.chatgpt.quota())),
            Request::ChatGptLogin => self.login("chatgpt-account", None, None).await,
            Request::ChatGptImportAuth { path } => {
                self.chatgpt.import_auth(Path::new(&path)).await.map_err(failed)?;
                done
            }
            Request::ChatGptRemove { email } => self.remove("chatgpt-account", Some(&email), None),
            Request::ChatGptRemoveAll => self.remove("chatgpt-account", None, None),
            Request::ChatGptCheckAll => self.check_all("chatgpt-account"),
            Request::ChatGptPreferFree => Ok(Reply::ChatGptPreferFree(self.chatgpt.prefer_free())),
            Request::SetChatGptPreferFree { enabled } => {
                self.chatgpt.set_prefer_free(enabled).map_err(|error| failed(error.to_string()))?;
                done
            }
            Request::GrokAccounts => self.accounts("grok-account"),
            Request::GrokQuota => Ok(Reply::Quota(self.grok.quota())),
            Request::GrokLogin => self.login("grok-account", None, None).await,
            Request::GrokRemove { email } => self.remove("grok-account", Some(&email), None),
            Request::GrokRemoveAll => self.remove("grok-account", None, None),
            Request::GrokCheckAll => self.check_all("grok-account"),
            Request::ZCodeAccounts => self.accounts("zcode-account"),
            Request::ZCodeQuota => Ok(Reply::Quota(self.zcode.quota())),
            Request::ZCodeLogin { language, timezone } => self.login("zcode-account", Some(language), Some(timezone)).await,
            Request::ZCodeRemove { email } => self.remove("zcode-account", Some(&email), None),
            Request::ZCodeRemoveAll => self.remove("zcode-account", None, None),
            Request::ZCodeCheckAll => self.check_all("zcode-account"),
            Request::WorkBuddyAccounts => self.accounts("workbuddy-account"),
            Request::WorkBuddyCredits => Ok(Reply::Credits(self.workbuddy.credits())),
            Request::WorkBuddyLogin => self.login("workbuddy-account", None, None).await,
            Request::WorkBuddyRemove { email } => self.remove("workbuddy-account", Some(&email), None),
            Request::WorkBuddyRemoveAll => self.remove("workbuddy-account", None, None),
            Request::WorkBuddyCheckAll => self.check_all("workbuddy-account"),
            Request::DeepAiAccounts => self.accounts("deepai-account"),
            Request::DeepAiCredits => Ok(Reply::Credits(self.deepai.credits())),
            Request::DeepAiLogin => self.login("deepai-account", None, None).await,
            Request::DeepAiRemove { email, id } => self.remove("deepai-account", Some(&email), id.as_deref()),
            Request::DeepAiRemoveAll => self.remove("deepai-account", None, None),
            Request::DeepAiCheckAll => self.check_all("deepai-account"),
            Request::AgentRouterAccounts => self.accounts("agentrouter"),
            Request::AgentRouterAdd { keys } => self.add_keys("agentrouter", keys),
            Request::AgentRouterRemove { email } => self.remove("agentrouter", Some(&email), None),
            Request::AgentRouterRemoveAll => self.remove("agentrouter", None, None),
            Request::AgentRouterCheckAll => self.check_all("agentrouter"),
            Request::ClaudenAccounts => self.accounts("clauden"),
            Request::ClaudenAdd { keys } => self.add_keys("clauden", keys),
            Request::ClaudenRemove { email } => self.remove("clauden", Some(&email), None),
            Request::ClaudenRemoveAll => self.remove("clauden", None, None),
            Request::ClaudenCheckAll => self.check_all("clauden"),
            Request::GeminiAccounts => self.accounts("gemini"),
            Request::GeminiAdd { keys } => self.add_keys("gemini", keys),
            Request::GeminiRemove { email } => self.remove("gemini", Some(&email), None),
            Request::GeminiRemoveAll => self.remove("gemini", None, None),
            Request::GeminiCheckAll => self.check_all("gemini"),
            Request::NvidiaBuildAccounts => self.accounts("nvidiabuild"),
            Request::NvidiaBuildAdd { keys } => self.add_keys("nvidiabuild", keys),
            Request::NvidiaBuildRemove { email } => self.remove("nvidiabuild", Some(&email), None),
            Request::NvidiaBuildRemoveAll => self.remove("nvidiabuild", None, None),
            Request::NvidiaBuildCheckAll => self.check_all("nvidiabuild"),
            Request::CoArenaAccounts => self.accounts("coarena"),
            Request::CoArenaDirectChecks => Ok(Reply::CoArenaDirectChecks(self.coarena.direct_checks())),
            Request::SetCoArenaDirectChecks { enabled } => {
                self.coarena.set_direct_checks(enabled).map_err(|error| failed(error.to_string()))?;
                done
            }
            Request::CoArenaExperimentalContext => Ok(Reply::CoArenaExperimentalContext(self.coarena.experimental_context())),
            Request::SetCoArenaExperimentalContext { enabled } => {
                self.coarena.set_experimental_context(enabled).map_err(|error| failed(error.to_string()))?;
                done
            }
            Request::CoArenaQuota => {
                let quota = self.coarena.coarena_quota();
                let accounts = self.coarena.clone();
                tokio::spawn(async move { accounts.refresh_coarena().await });
                Ok(Reply::CoArenaQuota(quota))
            }
            Request::CoArenaAdd { keys } => self.add_keys("coarena", keys),
            Request::CoArenaRemove { email } => self.remove("coarena", Some(&email), None),
            Request::CoArenaRemoveAll => self.remove("coarena", None, None),
            Request::CoArenaCheckAll => self.check_all("coarena"),
            Request::SessionList { directory } => {
                Ok(Reply::Sessions(self.sessions.list(&directory)))
            }
            Request::SessionRecent { limit } => Ok(Reply::Sessions(self.sessions.recent(limit))),
            Request::SessionCreate { directory, temporary, tools, chrome } => {
                self.sessions.create(directory, temporary, tools, chrome).map(Reply::Session).map_err(failed)
            }
            Request::SessionGet { session_id } => self
                .sessions
                .get(&session_id)
                .map(Reply::Session)
                .map_err(failed),
            Request::SessionStatuses { directory } => {
                Ok(Reply::Statuses(self.sessions.statuses(&directory)))
            }
            Request::SessionMessages { session_id } => self
                .sessions
                .messages(&session_id)
                .map(Reply::Messages)
                .map_err(failed),
            Request::SessionPage { session_id, before, limit } => self
                .sessions
                .page(&session_id, before.as_deref(), limit)
                .map(Reply::Page)
                .map_err(failed),
            Request::SessionPrompt(prompt) => self
                .sessions
                .send(prompt)
                .await
                .map(|_| Reply::Done(Done {}))
                .map_err(failed),
            Request::Skills { directory } => {
                Ok(Reply::Skills(self.sessions.found_skills(&directory).iter().map(skills::Skill::info).collect()))
            }
            Request::SkillSources => Ok(Reply::SkillSources(self.sessions.skill_sources())),
            Request::SetSkillSources { sources } => self
                .sessions
                .set_skill_sources(sources)
                .map(|_| Reply::Done(Done {}))
                .map_err(failed),
            Request::SetDisabledSkills { paths } => {
                self.sessions.set_disabled_skills(paths);
                Ok(Reply::Done(Done {}))
            }
            Request::Commands => Ok(Reply::Commands(sessions::commands())),
            Request::SetCompaction(settings) => self
                .sessions
                .set_compaction(settings)
                .map(|_| Reply::Done(Done {}))
                .map_err(failed),
            Request::SessionCompact {
                session_id,
                model,
                focus,
            } => self
                .sessions
                .compact(&session_id, model, &focus, None)
                .await
                .map(|_| Reply::Done(Done {}))
                .map_err(failed),
            Request::SessionQueue { session_id } => Ok(Reply::Queue(self.sessions.queue(&session_id))),
            Request::SessionDequeue { session_id, message_id } => self
                .sessions
                .dequeue(&session_id, &message_id)
                .map(|_| Reply::Done(Done {}))
                .map_err(failed),
            Request::SessionAbort { session_id } => {
                self.sessions.abort(&session_id);
                done
            }
            Request::SessionInterrupt { session_id, message_id } => {
                self.sessions.interrupt(&session_id, &message_id);
                done
            }
            Request::SessionContext { session_id } => self.sessions.context_parts(&session_id).map(Reply::Context).map_err(failed),
            Request::SessionRename { session_id, title } => self
                .sessions
                .rename(&session_id, title)
                .map(|_| Reply::Done(Done {}))
                .map_err(failed),
            Request::SessionArchive { session_id } => self
                .sessions
                .archive(&session_id)
                .map(|_| Reply::Done(Done {}))
                .map_err(failed),
            Request::SessionUnarchive { session_id } => self
                .sessions
                .unarchive(&session_id)
                .map(|_| Reply::Done(Done {}))
                .map_err(failed),
            Request::SessionDelete { session_id } => self
                .sessions
                .delete(&session_id)
                .map(|_| Reply::Done(Done {}))
                .map_err(failed),
            Request::QuestionList { directory } => {
                Ok(Reply::Questions(self.sessions.questions(&directory)))
            }
            Request::QuestionReply {
                request_id,
                answers,
            } => self
                .sessions
                .reply_question(&request_id, Some(answers))
                .map(|_| Reply::Done(Done {}))
                .map_err(failed),
            Request::QuestionReject { request_id } => self
                .sessions
                .reply_question(&request_id, None)
                .map(|_| Reply::Done(Done {}))
                .map_err(failed),
            Request::TerminalShells => Ok(Reply::Shells(self.terminals.shells())),
            Request::TerminalCreate {
                directory,
                shell,
                highlight,
                cols,
                rows,
            } => self
                .terminals
                .create(&directory, shell.as_deref(), highlight, (cols, rows))
                .map(Reply::Terminal)
                .map_err(failed),
            Request::TerminalList { directory } => Ok(Reply::Terminals(self.terminals.list(&directory))),
            Request::TerminalResize { id, cols, rows } => self
                .terminals
                .resize(&id, (cols, rows))
                .map(|_| Reply::Done(Done {}))
                .map_err(failed),
            Request::TerminalClose { id } => {
                self.terminals.close(&id);
                done
            }
            Request::BrowserTabs { session_id } => Ok(Reply::BrowserTabs(tools::browser_panel::tabs_of(&self.sessions.chat_of(&session_id)))),
            Request::BrowserOpen { session_id, url } => tools::browser_panel::open(&self.sessions.chat_of(&session_id), &url)
                .await
                .map(Reply::BrowserTab)
                .map_err(failed),
            Request::BrowserGo { id, url, search } => tools::browser_panel::go(&id, &url, search.as_deref()).await.map(|_| Reply::Done(Done {})).map_err(failed),
            Request::BrowserStored => Ok(Reply::BrowserStored(tools::browser_panel::stored())),
            Request::ChromeExtension => Ok(Reply::ChromeExtension(nabu_protocol::ChromeExtension {
                connected: tools::browser_panel::chrome_connected(),
                version: chrome_extension::installed_version(),
                folder: chrome_extension::folder().map(|folder| folder.display().to_string()),
            })),
            Request::BrowserClear { sites } => match sites {
                true => tools::browser_panel::clear_site_data().await,
                false => tools::browser_panel::clear_cache().await,
            }
            .map(|_| Reply::Done(Done {}))
            .map_err(failed),
            Request::BrowserHistory { id, step } => tools::browser_panel::history(&id, step).await.map(|_| Reply::Done(Done {})).map_err(failed),
            Request::BrowserReload { id } => tools::browser_panel::reload(&id).await.map(|_| Reply::Done(Done {})).map_err(failed),
            Request::BrowserClose { id } => tools::browser_panel::close(&id).await.map(|_| Reply::Done(Done {})).map_err(failed),
            Request::ProjectList => Ok(Reply::Projects(self.projects.list())),
            Request::ProjectAdd { path, name } => self.projects.add(path, name).map(Reply::Projects).map_err(failed),
            Request::ProjectRename { path, name } => self.projects.rename(&path, name).map(Reply::Projects).map_err(failed),
            Request::ProjectRemove { path } => self.projects.remove(&path).map(Reply::Projects).map_err(failed),
            Request::ProjectReorder { paths } => self.projects.reorder(&paths).map(Reply::Projects).map_err(failed),
            Request::GitBranches { directory } => blocking(move || projects::branches(&directory)).await.map(Reply::Branches),
            Request::GitSwitch { directory, branch } => {
                blocking(move || projects::switch(&directory, &branch)).await?;
                self.projects.announce();
                done
            }
            Request::FileList { directory, path } => blocking(move || files::list(&directory, &path)).await.map(Reply::Files),
            Request::GitChanges { directory } => blocking(move || files::changes(&directory)).await.map(Reply::Changes),
            Request::GitDiff { directory, path } => blocking(move || files::diff(&directory, &path)).await.map(Reply::Patch),
            Request::FilesWatch { directory } => self.file_watchers.watch(&directory).map(|_| Reply::Done(Done {})).map_err(failed),
        }
    }

    // Checks report progress through events, so the request returns at once.
    fn info(&self) -> ServerInfo {
        ServerInfo {
            name: "nabu".into(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            protocol: nabu_protocol::PROTOCOL_VERSION,
        }
    }
}

fn failed(message: String) -> ProtocolError {
    ProtocolError { message }
}

async fn blocking<T: Send + 'static>(work: impl FnOnce() -> Result<T, String> + Send + 'static) -> Result<T, ProtocolError> {
    tokio::task::spawn_blocking(work).await.map_err(|err| failed(err.to_string()))?.map_err(failed)
}

fn proxy_from_env() -> Option<Proxy> {
    local_proxy::resolve()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::task::Poll;

    #[tokio::test]
    async fn deepai_account_commands_use_the_registered_manager() {
        let mut suffix = [0; 8];
        getrandom::fill(&mut suffix).unwrap();
        let path =
            std::env::temp_dir().join(format!("nabu-deepai-rpc-{:x}", u64::from_ne_bytes(suffix)));
        let server = Server::new(&path);
        assert!(
            matches!(server.handle(Request::DeepAiAccounts).await, Ok(Reply::Accounts(accounts)) if accounts.is_empty())
        );
        assert!(matches!(
            server.handle(Request::DeepAiCredits).await,
            Ok(Reply::Credits(None))
        ));
        assert!(matches!(
            server
                .handle(Request::DeepAiRemove {
                    email: "missing".into(),
                    id: None
                })
                .await,
            Ok(Reply::Done(_))
        ));
        assert!(matches!(
            server.handle(Request::DeepAiRemoveAll).await,
            Ok(Reply::Done(_))
        ));
        let _ = std::fs::remove_dir_all(path);
    }

    #[tokio::test]
    async fn providers_wait_for_startup() {
        let mut suffix = [0; 8];
        getrandom::fill(&mut suffix).unwrap();
        let path =
            std::env::temp_dir().join(format!("nabu-startup-{:x}", u64::from_ne_bytes(suffix)));
        let server = Server::new(&path);
        let mut request = std::pin::pin!(server.handle(Request::Providers));
        assert!(matches!(
            futures_util::poll!(request.as_mut()),
            Poll::Pending
        ));

        server.start().await;

        assert!(
            matches!(futures_util::poll!(request), Poll::Ready(Ok(Reply::Providers(items))) if items.is_empty())
        );
        assert!(matches!(
            futures_util::poll!(std::pin::pin!(server.handle(Request::Providers))),
            Poll::Ready(Ok(Reply::Providers(_)))
        ));
    }
}
