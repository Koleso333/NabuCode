use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Raised on every incompatible change of the messages; a window only works with a server of the same version.
pub const PROTOCOL_VERSION: u32 = 1;

/// First stdout line of the server process: where and how to connect.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Listening {
    pub port: u16,
    pub token: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientMessage {
    pub id: u64,
    #[serde(flatten)]
    pub request: Request,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "camelCase")]
pub enum Request {
    ServerInfo,
    Providers,
    /// Every provider that can be connected, with whether it is and whether it is on.
    ProviderCatalog,
    SetProviderEnabled { provider: String, enabled: bool },
    ProviderAccounts { provider: String },
    ProviderUsage { provider: String },
    /// Starts a browser sign-in; ZCode also wants the person's language and time zone.
    ProviderLogin {
        provider: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        language: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timezone: Option<String>,
    },
    /// The code a sign-in page shows for pasting back (Claude).
    ProviderLoginFinish { provider: String, code: String },
    ProviderAddKeys { provider: String, keys: Vec<String> },
    ProviderRemove {
        provider: String,
        email: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
    },
    ProviderRemoveAll { provider: String },
    ProviderCheckAll { provider: String },
    /// Models switched off in the models list: still usable, just not offered.
    HiddenModels,
    SetModelsHidden { models: Vec<ModelKey>, hidden: bool },
    CustomProvider { id: String },
    SaveCustomProvider(SaveCustomProvider),
    RemoveCustomProvider { id: String },
    AntigravityAccounts,
    AntigravityLogin,
    AntigravityRemove { email: String },
    AntigravityRemoveAll,
    AntigravityRefresh,
    AntigravityCheckAll,
    AntigravityQuota,
    ClaudeAccounts,
    ClaudeQuota,
    ClaudeLogin,
    /// The `code#state` text the Claude sign-in page shows.
    ClaudeLoginFinish { code: String },
    ClaudeRemove { email: String },
    ClaudeRemoveAll,
    ClaudeCheckAll,
    ChatGptAccounts,
    ChatGptQuota,
    /// Opens a local page for the browser to return to; the account then arrives as an event.
    ChatGptLogin,
    ChatGptImportAuth { path: String },
    ChatGptRemove { email: String },
    ChatGptRemoveAll,
    ChatGptCheckAll,
    ChatGptPreferFree,
    SetChatGptPreferFree { enabled: bool },
    GrokAccounts,
    GrokQuota,
    GrokLogin,
    GrokRemove { email: String },
    GrokRemoveAll,
    GrokCheckAll,
    ZCodeAccounts,
    ZCodeQuota,
    ZCodeLogin { language: String, timezone: String },
    ZCodeRemove { email: String },
    ZCodeRemoveAll,
    ZCodeCheckAll,
    WorkBuddyAccounts,
    WorkBuddyCredits,
    WorkBuddyLogin,
    WorkBuddyRemove { email: String },
    WorkBuddyRemoveAll,
    WorkBuddyCheckAll,
    DeepAiAccounts,
    DeepAiCredits,
    DeepAiLogin,
    DeepAiRemove {
        email: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
    },
    DeepAiRemoveAll,
    DeepAiCheckAll,
    AgentRouterAccounts,
    AgentRouterAdd { keys: Vec<String> },
    AgentRouterRemove { email: String },
    AgentRouterRemoveAll,
    AgentRouterCheckAll,
    CoArenaAccounts,
    CoArenaQuota,
    CoArenaDirectChecks,
    SetCoArenaDirectChecks { enabled: bool },
    CoArenaExperimentalContext,
    SetCoArenaExperimentalContext { enabled: bool },
    CoArenaAdd { keys: Vec<String> },
    CoArenaRemove { email: String },
    CoArenaRemoveAll,
    CoArenaCheckAll,
    ClaudenAccounts,
    ClaudenAdd { keys: Vec<String> },
    ClaudenRemove { email: String },
    ClaudenRemoveAll,
    ClaudenCheckAll,
    GeminiAccounts,
    GeminiAdd { keys: Vec<String> },
    GeminiRemove { email: String },
    GeminiRemoveAll,
    GeminiCheckAll,
    NvidiaBuildAccounts,
    NvidiaBuildAdd { keys: Vec<String> },
    NvidiaBuildRemove { email: String },
    NvidiaBuildRemoveAll,
    NvidiaBuildCheckAll,
    SessionList {
        directory: String,
    },
    /// The newest top-level sessions of every folder.
    SessionRecent {
        limit: usize,
    },
    /// An empty `directory` makes a session without a project, in a folder of its own.
    /// A temporary session is never saved and ends when the connection that made it closes.
    SessionCreate {
        #[serde(default)]
        directory: String,
        #[serde(default)]
        temporary: bool,
        /// The only tools the session may use, by tool or group name (`["browser"]`); a group named here is loaded from the start.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tools: Option<Vec<String>>,
        /// The NabuCode extension's id of the Chrome the session works in: its browser tools open tabs there by default.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        chrome: Option<String>,
    },
    SessionGet {
        #[serde(rename = "sessionID")]
        session_id: String,
    },
    /// Status of every session in the directory that is not idle.
    SessionStatuses {
        directory: String,
    },
    SessionMessages {
        #[serde(rename = "sessionID")]
        session_id: String,
    },
    /// Up to `limit` messages before `before` (the newest without it); kept attachments come as `/file` links.
    SessionPage {
        #[serde(rename = "sessionID")]
        session_id: String,
        #[serde(default)]
        before: Option<String>,
        limit: usize,
    },
    /// Starts a turn, or queues the prompt while the session answers.
    SessionPrompt(Prompt),
    /// Skills found for a session in `directory`, turned off ones too, in the order the sources give them.
    Skills { directory: String },
    /// Where skills are looked for; the order says whose skill wins when names repeat.
    SkillSources,
    SetSkillSources { sources: Vec<SkillSource> },
    /// Skills the agent doesn't get, by their file; another skill of the same name takes their place.
    SetDisabledSkills { paths: Vec<String> },
    /// Commands the server runs from a message's command mark.
    Commands,
    /// Prompts waiting for the running turn, oldest first.
    SessionQueue {
        #[serde(rename = "sessionID")]
        session_id: String,
    },
    SessionDequeue {
        #[serde(rename = "sessionID")]
        session_id: String,
        #[serde(rename = "messageID")]
        message_id: String,
    },
    SetCompaction(CompactionSettings),
    SessionCompact {
        #[serde(rename = "sessionID")]
        session_id: String,
        model: ModelRef,
        #[serde(default)]
        focus: String,
    },
    SessionAbort {
        #[serde(rename = "sessionID")]
        session_id: String,
    },
    /// The queued message goes to the model now: a reply being written is cut off, running tools wrap up.
    /// The rest of the queue keeps waiting.
    SessionInterrupt {
        #[serde(rename = "sessionID")]
        session_id: String,
        #[serde(rename = "messageID")]
        message_id: String,
    },
    SessionRename {
        #[serde(rename = "sessionID")]
        session_id: String,
        title: String,
    },
    /// What the session's next request would hold, by kind.
    SessionContext {
        #[serde(rename = "sessionID")]
        session_id: String,
    },
    SessionArchive {
        #[serde(rename = "sessionID")]
        session_id: String,
    },
    /// Brings an archived session back to the lists.
    SessionUnarchive {
        #[serde(rename = "sessionID")]
        session_id: String,
    },
    SessionDelete {
        #[serde(rename = "sessionID")]
        session_id: String,
    },
    /// Questions of the directory's sessions still waiting for the user.
    QuestionList {
        directory: String,
    },
    QuestionReply {
        #[serde(rename = "requestID")]
        request_id: String,
        /// The labels picked or the text typed, for each question in order.
        answers: Vec<Vec<String>>,
    },
    /// The user closed the questions without answering: the turn stops.
    QuestionReject {
        #[serde(rename = "requestID")]
        request_id: String,
    },
    /// Shells installed on the server machine; the first is the default.
    TerminalShells,
    /// Output and input then go through `/terminal?id=` on the same port.
    TerminalCreate {
        directory: String,
        /// A `Shell::id`; the default shell when absent.
        shell: Option<String>,
        /// Input syntax colors, for shells that have them.
        #[serde(default = "yes")]
        highlight: bool,
        cols: u16,
        rows: u16,
    },
    /// Live terminals of a project folder, oldest first.
    TerminalList { directory: String },
    TerminalResize { id: String, cols: u16, rows: u16 },
    TerminalClose { id: String },
    /// Browser tabs of the session's chat: its own, or its top session's for a subagent.
    BrowserTabs {
        #[serde(rename = "sessionID")]
        session_id: String,
    },
    /// Opens a tab in the session's chat; frames and input then go through `/browser?id=` on the same port.
    BrowserOpen {
        #[serde(rename = "sessionID")]
        session_id: String,
        /// Nothing to open: an empty tab.
        url: String,
    },
    /// `url` is what the user typed: text that is no address goes to `search`, an address with `%s` for the words.
    BrowserGo {
        id: String,
        url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        search: Option<String>,
    },
    /// `sites` clears cookies, site storage and history too; it closes every chat's tabs.
    BrowserClear { sites: bool },
    /// Whether the browser keeps a cache and site data: sites were opened since the last clearing.
    BrowserStored,
    /// The NabuCode extension for the user's Chrome: where it lies and whether it is connected.
    ChromeExtension,
    /// `step` -1 goes back, 1 forward.
    BrowserHistory { id: String, step: i64 },
    BrowserReload { id: String },
    BrowserClose { id: String },
    /// The user's project folders in their order.
    ProjectList,
    /// A folder already in the list stays where it is.
    ProjectAdd { path: String, name: String },
    ProjectRename { path: String, name: String },
    ProjectRemove { path: String },
    /// Folders left out of `paths` keep their order after the listed ones.
    ProjectReorder { paths: Vec<String> },
    /// Local branches, the most recently committed first.
    GitBranches { directory: String },
    GitSwitch { directory: String, branch: String },
    /// Entries of a folder inside the project, `path` relative to it ("" is the project itself); folders first.
    FileList { directory: String, path: String },
    /// Files that differ from HEAD, with paths relative to the project.
    GitChanges { directory: String },
    /// The unified patch of one file against HEAD; a new file comes as all added.
    GitDiff { directory: String, path: String },
    /// Starts sending `FilesChanged` for the folder; watching stays while the server lives.
    FilesWatch { directory: String },
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ServerMessage {
    Reply { id: u64, result: Reply },
    Error { id: Option<u64>, error: ProtocolError },
    Event { event: Event },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Reply {
    ServerInfo(ServerInfo),
    CustomProvider(CustomProvider),
    Providers(Vec<Provider>),
    Catalog(Vec<ProviderEntry>),
    Usage(Vec<UsageBar>),
    Context(ContextParts),
    Skills(Vec<SkillInfo>),
    SkillSources(Vec<SkillSource>),
    Commands(Vec<CommandInfo>),
    HiddenModels(Vec<ModelKey>),
    Accounts(Vec<Account>),
    Login(LoginStarted),
    Quota(Vec<Quota>),
    CoArenaQuota(CoArenaQuota),
    CoArenaDirectChecks(bool),
    CoArenaExperimentalContext(bool),
    ChatGptPreferFree(bool),
    Credits(Option<Credits>),
    Sessions(Vec<Session>),
    Session(Session),
    Statuses(std::collections::HashMap<String, SessionStatus>),
    Messages(Vec<MessageWithParts>),
    Page(MessagePage),
    Queue(Vec<Prompt>),
    Questions(Vec<QuestionRequest>),
    Shells(Vec<Shell>),
    Terminal(TerminalInfo),
    Terminals(Vec<TerminalInfo>),
    BrowserTabs(Vec<BrowserTab>),
    BrowserTab(BrowserTab),
    BrowserStored(BrowserStored),
    ChromeExtension(ChromeExtension),
    Projects(Vec<Project>),
    Branches(Vec<String>),
    Files(Vec<FileEntry>),
    Changes(Vec<GitChange>),
    Patch(String),
    Done(Done),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Project {
    pub path: String,
    pub name: String,
    /// The checked-out branch, or `detached at <hash>`; none outside git.
    pub branch: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessagePage {
    pub messages: Vec<MessageWithParts>,
    /// Older messages are left.
    pub more: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileEntry {
    pub name: String,
    pub dir: bool,
    /// Ignored by git.
    pub ignored: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GitChange {
    pub path: String,
    /// `M` changed, `A` added, `U` untracked, `D` deleted, `R` renamed.
    pub status: String,
    pub additions: usize,
    pub deletions: usize,
}

/// Reply for requests whose effect arrives as events.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Done {}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Event {
    Connected,
    ProvidersChanged,
    /// The provider's accounts, usage or switch changed.
    ProviderUpdated { provider: String },
    ProviderLoginFailed { provider: String, message: String },
    AntigravityAccounts { accounts: Vec<Account> },
    AntigravityLoginFailed { message: String },
    AntigravityQuota { quota: Vec<Quota> },
    ClaudeAccounts { accounts: Vec<Account> },
    ClaudeQuota { quota: Vec<Quota> },
    ChatGptAccounts { accounts: Vec<Account> },
    ChatGptQuota { quota: Vec<Quota> },
    ChatGptLoginFailed { message: String },
    GrokAccounts { accounts: Vec<Account> },
    GrokQuota { quota: Vec<Quota> },
    GrokLoginFailed { message: String },
    ZCodeAccounts { accounts: Vec<Account> },
    ZCodeQuota { quota: Vec<Quota> },
    ZCodeLoginFailed { message: String },
    WorkBuddyAccounts { accounts: Vec<Account> },
    WorkBuddyCredits { credits: Option<Credits> },
    WorkBuddyLoginFailed { message: String },
    DeepAiAccounts { accounts: Vec<Account> },
    DeepAiCredits { credits: Option<Credits> },
    DeepAiLoginFailed { message: String },
    AgentRouterAccounts { accounts: Vec<Account> },
    CoArenaAccounts { accounts: Vec<Account> },
    ClaudenAccounts { accounts: Vec<Account> },
    GeminiAccounts { accounts: Vec<Account> },
    NvidiaBuildAccounts { accounts: Vec<Account> },
    CoArenaQuota { quota: CoArenaQuota },
    SessionUpdated { session: Session },
    SessionDeleted { session: Session },
    SessionStatus {
        directory: String,
        #[serde(rename = "sessionID")]
        session_id: String,
        status: SessionStatus,
    },
    MessageUpdated { directory: String, message: Message },
    /// The session's queued prompts after one was added, removed or delivered.
    QueueUpdated {
        directory: String,
        #[serde(rename = "sessionID")]
        session_id: String,
        queue: Vec<Prompt>,
    },
    PartUpdated { directory: String, part: Part },
    /// Text appended to a part's `field` while it streams.
    PartDelta {
        directory: String,
        #[serde(rename = "messageID")]
        message_id: String,
        #[serde(rename = "partID")]
        part_id: String,
        field: String,
        delta: String,
    },
    QuestionAsked { directory: String, request: QuestionRequest },
    /// Answered, dismissed, or gone with its turn.
    QuestionClosed {
        directory: String,
        #[serde(rename = "sessionID")]
        session_id: String,
        #[serde(rename = "requestID")]
        request_id: String,
    },
    TerminalExited { id: String },
    /// A tab of the chat opened, closed, or changed its title, address, icon or loading.
    BrowserTabsChanged {
        #[serde(rename = "sessionID")]
        session_id: String,
        tabs: Vec<BrowserTab>,
    },
    /// The list changed, or a project's branch did.
    ProjectsChanged { projects: Vec<Project> },
    /// Something changed on disk inside a watched folder; changes close together come as one.
    FilesChanged { directory: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Shell {
    pub id: String,
    pub name: String,
    /// The shell colors what is typed, and NabuCode can turn it off.
    pub highlight: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalInfo {
    pub id: String,
    /// The tab title number: the lowest free one in the project folder.
    pub number: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrowserStored {
    pub cache: bool,
    pub sites: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChromeExtension {
    pub connected: bool,
    pub version: Option<String>,
    pub folder: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserTab {
    /// The DevTools target id.
    pub id: String,
    /// The number the chat's model knows the tab by.
    pub number: u32,
    pub title: String,
    pub url: String,
    /// A `data:` URL of the site's icon.
    pub favicon: Option<String>,
    pub loading: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProtocolError {
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerInfo {
    pub name: String,
    pub version: String,
    pub protocol: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Provider {
    pub id: String,
    pub name: String,
    pub models: Vec<Model>,
    #[serde(default)]
    pub custom: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ProviderAuth {
    Account,
    Key,
    Custom,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderEntry {
    pub id: String,
    pub name: String,
    pub auth: ProviderAuth,
    /// Accounts or keys added; a custom provider counts as one.
    pub accounts: usize,
    pub enabled: bool,
}

/// What a usage bar counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum UsageUnit {
    /// `used` and `total` are shares, `total` is 1.
    Share,
    Credits,
    Usd,
}

/// The tool whose skill folders a source reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SkillHarness {
    Nabucode,
    Claude,
    Codex,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SkillScope {
    Project,
    Global,
    /// Claude desktop's own skills in `~/.claude/skills/synced`, written for its tools.
    Synced,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SkillSource {
    pub harness: SkillHarness,
    pub scope: SkillScope,
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SkillInfo {
    pub name: String,
    /// Without one in the header, the first line of the text.
    pub description: String,
    /// The header has a description.
    pub described: bool,
    /// The start of the text, in one line.
    pub preview: String,
    pub enabled: bool,
    /// The SKILL.md, or the command file of an old Claude Code command.
    pub path: String,
    pub harness: SkillHarness,
    pub scope: SkillScope,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommandInfo {
    pub name: String,
    pub description: String,
}

/// Estimated tokens of a request by kind; deferred tools are not in it until loaded.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextParts {
    pub system: u64,
    pub tools: u64,
    pub messages: u64,
    pub deferred: u64,
}

/// One bar of a provider's usage, the same for quotas, credits and money.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageBar {
    /// The provider's own name of the window or model family (`5h`, `week`, `claude`); none for credits and money.
    pub label: Option<String>,
    pub unit: UsageUnit,
    pub used: f64,
    pub total: f64,
    /// RFC 3339 time.
    pub reset: Option<String>,
    /// The window starts with the first request, so its reset isn't counting yet.
    #[serde(default)]
    pub idle: bool,
    /// Some keys weren't checked, so the sum is short.
    #[serde(default)]
    pub partial: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ProviderProtocol {
    OpenAiChat,
    OpenAiResponses,
    AnthropicMessages,
    Gemini,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomModel {
    pub id: String,
    pub family: String,
    pub developer: String,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manual_family: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manual_developer: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomProviderConfig {
    pub id: String,
    pub name: String,
    pub protocol: ProviderProtocol,
    pub base_url: String,
    pub smart_routing: bool,
    pub models: Vec<CustomModel>,
    pub headers: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CustomProvider {
    #[serde(flatten)]
    pub config: CustomProviderConfig,
    pub keys: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "camelCase")]
pub enum CustomKeyInput {
    Keep(String),
    Value(String),
    Env(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SaveCustomProvider {
    #[serde(flatten)]
    pub config: CustomProviderConfig,
    pub keys: Vec<CustomKeyInput>,
    pub edit: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Model {
    pub id: String,
    pub name: String,
    pub family: String,
    /// Reasoning effort levels the model accepts; empty when it has none.
    pub efforts: Vec<String>,
    pub context: u64,
    pub output: u64,
    pub images: bool,
    /// Accepts fast mode.
    #[serde(default)]
    pub fast: bool,
    /// An older model, listed apart from the current ones.
    #[serde(default)]
    pub older: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Account {
    pub email: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_credits: Option<f64>,
    /// Plan: `free`, `pro`, `ultra`, `max`, ...; `None` until checked.
    pub tier: Option<String>,
    pub status: AccountStatus,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Credits {
    pub remaining: f64,
    pub total: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CoArenaQuota {
    pub total_usd: f64,
    pub used_usd: f64,
    pub remaining_usd: f64,
    pub reset: Option<String>,
    pub partial: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "camelCase")]
pub enum AccountStatus {
    Unchecked,
    Checking,
    Work,
    Limit { message: String },
    Error { message: String },
}

/// Quota of one model family: the share still left and when it next resets.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Quota {
    pub family: String,
    /// 0.0..=1.0
    pub remaining: f64,
    /// RFC 3339 time.
    pub reset: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginStarted {
    pub url: String,
}

// Sessions and messages keep the field names the UI renders them with.

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    pub id: String,
    pub directory: String,
    pub title: String,
    pub time: SessionTime,
    /// The session whose agent started this one as a subagent.
    #[serde(rename = "parentID", default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub compactions: Vec<SessionCompaction>,
    /// The session has no project: `directory` is its own folder, removed with it.
    #[serde(rename = "noProject", default, skip_serializing_if = "std::ops::Not::not")]
    pub no_project: bool,
    /// Never saved; ends when the connection that made it closes.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub temporary: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionCompaction {
    pub id: String,
    #[serde(rename = "afterMessageID")]
    pub after_message_id: String,
    pub time: u64,
    pub automatic: bool,
    pub model: ModelRef,
    pub model_name: String,
    pub tokens_before: u64,
    pub tokens_after: u64,
}

/// Times are Unix milliseconds.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionTime {
    pub created: u64,
    pub updated: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum SessionStatus {
    Idle,
    Busy {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        compaction: Option<CompactionPhase>,
    },
    /// The request failed and is sent again at `next` (ms since epoch).
    Retry { attempt: u32, message: String, next: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CompactionPhase {
    Waiting,
    Streaming,
}

/// Questions an `ask` call puts to the user; the turn waits for the answers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuestionRequest {
    pub id: String,
    #[serde(rename = "sessionID")]
    pub session_id: String,
    pub questions: Vec<Question>,
    pub tool: QuestionTool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Question {
    pub question: String,
    /// A short label for the question's tab.
    pub header: String,
    pub options: Vec<QuestionOption>,
    /// Several options may be picked.
    #[serde(default)]
    pub multiple: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuestionOption {
    pub label: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuestionTool {
    #[serde(rename = "messageID")]
    pub message_id: String,
    #[serde(rename = "callID")]
    pub call_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct CompactionSettings {
    pub auto: bool,
    pub threshold: u8,
    pub custom_model: bool,
    pub model: Option<ModelRef>,
}

impl Default for CompactionSettings {
    fn default() -> Self {
        Self { auto: true, threshold: 90, custom_model: false, model: None }
    }
}

/// A user turn sent to the model; ids come from the client so its optimistic copy matches.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Prompt {
    #[serde(rename = "sessionID")]
    pub session_id: String,
    #[serde(rename = "messageID")]
    pub message_id: String,
    pub agent: String,
    pub model: ModelRef,
    pub parts: Vec<PromptPart>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum PromptPart {
    Text {
        id: String,
        text: String,
    },
    /// An attachment; `url` is a `data:` URL.
    File {
        id: String,
        mime: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        filename: Option<String>,
        url: String,
    },
    /// A skill the person marked at this place of the message.
    Skill {
        id: String,
        name: String,
        /// Marked with `@` rather than `/`.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        at: bool,
    },
    /// A command the person picked; the server runs it first, the rest of the message waits for it in the queue.
    Command {
        id: String,
        name: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ModelKey {
    #[serde(rename = "providerID")]
    pub provider_id: String,
    #[serde(rename = "modelID")]
    pub model_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelRef {
    #[serde(rename = "providerID")]
    pub provider_id: String,
    #[serde(rename = "modelID")]
    pub model_id: String,
    /// Reasoning effort.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub fast: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageWithParts {
    pub info: Message,
    pub parts: Vec<Part>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "camelCase")]
pub enum Message {
    User(UserMessage),
    Assistant(AssistantMessage),
}

impl Message {
    pub fn id(&self) -> &str {
        match self {
            Message::User(m) => &m.id,
            Message::Assistant(m) => &m.id,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserMessage {
    pub id: String,
    #[serde(rename = "sessionID")]
    pub session_id: String,
    pub time: Created,
    pub agent: String,
    pub model: ModelRef,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Created {
    pub created: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssistantMessage {
    pub id: String,
    #[serde(rename = "sessionID")]
    pub session_id: String,
    /// The user message this one answers.
    #[serde(rename = "parentID")]
    pub parent_id: String,
    #[serde(rename = "providerID")]
    pub provider_id: String,
    #[serde(rename = "modelID")]
    pub model_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
    pub agent: String,
    pub time: Completed,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<MessageError>,
    pub tokens: Tokens,
    pub cost: f64,
    /// Why the model stopped, e.g. `stop` or `length`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Completed {
    pub created: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Tokens {
    pub input: u64,
    pub output: u64,
    pub reasoning: u64,
    pub cache: CacheTokens,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CacheTokens {
    pub read: u64,
    pub write: u64,
}

/// `name` is `APIError`, `MessageAbortedError` or `UnknownError`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageError {
    pub name: String,
    pub data: ErrorData,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorData {
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Part {
    Text(TextPart),
    Reasoning(TextPart),
    File(FilePart),
    Tool(ToolPart),
    Retry(RetryPart),
    Skill(SkillPart),
    Command(CommandPart),
}

impl Part {
    pub fn id(&self) -> &str {
        match self {
            Part::Text(p) | Part::Reasoning(p) => &p.id,
            Part::File(p) => &p.id,
            Part::Tool(p) => &p.id,
            Part::Retry(p) => &p.id,
            Part::Skill(p) => &p.id,
            Part::Command(p) => &p.id,
        }
    }

    pub fn session_id(&self) -> &str {
        match self {
            Part::Text(p) | Part::Reasoning(p) => &p.session_id,
            Part::File(p) => &p.session_id,
            Part::Tool(p) => &p.session_id,
            Part::Retry(p) => &p.session_id,
            Part::Skill(p) => &p.session_id,
            Part::Command(p) => &p.session_id,
        }
    }

    pub fn message_id(&self) -> &str {
        match self {
            Part::Text(p) | Part::Reasoning(p) => &p.message_id,
            Part::File(p) => &p.message_id,
            Part::Tool(p) => &p.message_id,
            Part::Retry(p) => &p.message_id,
            Part::Skill(p) => &p.message_id,
            Part::Command(p) => &p.message_id,
        }
    }

    /// Text of a text or reasoning part.
    pub fn text_mut(&mut self) -> Option<&mut TextPart> {
        match self {
            Part::Text(p) | Part::Reasoning(p) => Some(p),
            Part::File(_) | Part::Tool(_) | Part::Retry(_) | Part::Skill(_) | Part::Command(_) => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextPart {
    pub id: String,
    #[serde(rename = "sessionID")]
    pub session_id: String,
    #[serde(rename = "messageID")]
    pub message_id: String,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time: Option<Span>,
    /// Provider signature the model needs back with this text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sig: Option<Value>,
    /// Written by the server for the model, not by the person; the UI does not show it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub synthetic: bool,
}

/// A skill the person marked in a message, with its text as it was when sent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillPart {
    pub id: String,
    #[serde(rename = "sessionID")]
    pub session_id: String,
    #[serde(rename = "messageID")]
    pub message_id: String,
    pub name: String,
    /// Marked with `@` rather than `/`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub at: bool,
    /// The skill's folder, which its text names files from.
    pub path: String,
    pub body: String,
}

/// A command that ran before its message was sent; it stays where the person put it, the model doesn't see it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandPart {
    pub id: String,
    #[serde(rename = "sessionID")]
    pub session_id: String,
    #[serde(rename = "messageID")]
    pub message_id: String,
    pub name: String,
}

/// Repeats of a failed request; stays in history once a request went through.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetryPart {
    pub id: String,
    #[serde(rename = "sessionID")]
    pub session_id: String,
    #[serde(rename = "messageID")]
    pub message_id: String,
    pub attempt: u32,
    /// None when the attempts are unlimited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<u32>,
    /// Why the last request failed: the status code and the provider's text.
    pub error: String,
    /// When the next attempt goes, while waiting for it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<u64>,
}

/// One tool call and, once it ran, its result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolPart {
    pub id: String,
    #[serde(rename = "sessionID")]
    pub session_id: String,
    #[serde(rename = "messageID")]
    pub message_id: String,
    #[serde(rename = "callID")]
    pub call_id: String,
    pub tool: String,
    pub state: ToolState,
    /// Arguments exactly as the model wrote them.
    pub args: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sig: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "camelCase")]
pub enum ToolState {
    Pending {
        input: Value,
        raw: String,
    },
    Running {
        input: Value,
        time: Span,
        #[serde(default, skip_serializing_if = "Value::is_null")]
        metadata: Value,
    },
    Completed {
        input: Value,
        output: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        attachments: Vec<FilePart>,
        title: String,
        metadata: Value,
        time: Span,
    },
    Error {
        input: Value,
        error: String,
        time: Span,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FilePart {
    pub id: String,
    #[serde(rename = "sessionID")]
    pub session_id: String,
    #[serde(rename = "messageID")]
    pub message_id: String,
    pub mime: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    /// `data:` URL with the file itself; for a folder (`inode/directory`) a `file:` URL of its place.
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Span {
    pub start: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end: Option<u64>,
}
