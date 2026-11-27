// Mirrors crates/nabu-protocol.

export const protocolVersion = 1

export type ServerInfo = { name: string; version: string; protocol: number }

export type Model = {
  id: string
  name: string
  family: string
  efforts: string[]
  context: number
  output: number
  images: boolean
  fast: boolean
  /** An older model, listed apart from the current ones. */
  older: boolean
}

export type Provider = { id: string; name: string; models: Model[]; custom?: boolean }

export type ProviderProtocol = "openAiChat" | "openAiResponses" | "anthropicMessages" | "gemini"
export type CustomModel = {
  id: string
  family: string
  developer: string
  version: string
  manualFamily?: boolean
  manualDeveloper?: boolean
}
export type CustomProviderConfig = {
  id: string
  name: string
  protocol: ProviderProtocol
  baseUrl: string
  smartRouting: boolean
  models: CustomModel[]
  headers: Record<string, string>
}
export type CustomProvider = CustomProviderConfig & { keys: string[] }
export type CustomKeyInput = { type: "keep" | "value" | "env"; value: string }

export type AccountStatus =
  | { state: "unchecked" }
  | { state: "checking" }
  | { state: "work" }
  | { state: "limit"; message: string }
  | { state: "error"; message: string }

export type Quota = { family: string; remaining: number; reset: string | null }
export type Credits = { remaining: number; total: number; reset?: string }
export type CoArenaQuota = { totalUsd: number; usedUsd: number; remainingUsd: number; reset: string | null; partial: boolean }

export type Account = { email: string; id?: string; maxCredits?: number; tier: string | null; status: AccountStatus }

export type ProviderAuth = "account" | "key" | "custom"
/** `accounts`: accounts or keys added; a custom provider counts as one. */
export type ProviderEntry = { id: string; name: string; auth: ProviderAuth; accounts: number; enabled: boolean }
/**
 * One bar of a provider's usage. `share`: `used` of `total` = 1; `label` is the provider's own window or family name.
 * `idle`: the window starts with the first request. `partial`: some keys weren't checked, so the sum is short.
 */
/** Estimated tokens of the next request by kind; deferred tools are not in it until loaded. */
export type ContextParts = { system: number; tools: number; messages: number; deferred: number }

export type UsageBar = {
  label: string | null
  unit: "share" | "credits" | "usd"
  used: number
  total: number
  reset: string | null
  idle: boolean
  partial: boolean
}


// Sessions and messages keep the field names the UI renders them with.

export type Session = {
  id: string
  directory: string
  title: string
  time: { created: number; updated: number; archived?: number }
  /** The session whose agent started this one as a subagent. */
  parentID?: string
  compactions?: SessionCompaction[]
  /** No project: `directory` is the session's own folder. */
  noProject?: boolean
  /** Never saved; ends when the connection that made it closes. */
  temporary?: boolean
}

export type SessionCompaction = {
  id: string
  afterMessageID: string
  time: number
  automatic: boolean
  model: ModelRef
  modelName: string
  tokensBefore: number
  tokensAfter: number
}

export type CompactionPhase = "waiting" | "streaming"
export type SessionStatus =
  | { type: "idle" }
  | { type: "busy"; compaction?: CompactionPhase }
  | { type: "retry"; attempt: number; message: string; next: number }

export type ModelKey = { providerID: string; modelID: string }
export type ModelRef = { providerID: string; modelID: string; variant?: string; fast?: boolean }

export type CompactionSettings = {
  auto: boolean
  threshold: number
  customModel: boolean
  model?: ModelRef
}

export type UserMessage = {
  id: string
  sessionID: string
  role: "user"
  time: { created: number }
  agent: string
  model: ModelRef
}

export type MessageError = {
  name: "APIError" | "MessageAbortedError" | "UnknownError"
  data: { message: string }
}

export type AssistantMessage = {
  id: string
  sessionID: string
  role: "assistant"
  parentID: string
  providerID: string
  modelID: string
  variant?: string
  agent: string
  time: { created: number; completed?: number }
  error?: MessageError
  tokens: { input: number; output: number; reasoning: number; cache: { read: number; write: number } }
  cost: number
  finish?: string
}

export type Message = UserMessage | AssistantMessage

type TextBody = {
  id: string
  sessionID: string
  messageID: string
  text: string
  time?: { start: number; end?: number }
  /** Written by the server for the model; not shown. */
  synthetic?: boolean
}

/** An attachment; `url` is a `data:` URL with the file itself. */
export type FilePart = {
  id: string
  sessionID: string
  messageID: string
  type: "file"
  mime: string
  filename?: string
  url: string
}

type Input = Record<string, unknown>
type Span = { start: number; end?: number }

export type ToolState =
  | { status: "pending"; input: Input; raw: string }
  | { status: "running"; input: Input; time: Span; metadata?: Input }
  | {
      status: "completed"
      input: Input
      output: string
      attachments?: Omit<FilePart, "type">[]
      title: string
      metadata: Input
      time: Span
    }
  | { status: "error"; input: Input; error: string; time: Span }

/** One tool call and, once it ran, its result. */
export type ToolPart = {
  id: string
  sessionID: string
  messageID: string
  type: "tool"
  callID: string
  tool: string
  state: ToolState
  args: string
}

/** Repeats of a failed request; stays in history once a request went through. */
export type RetryPart = {
  id: string
  sessionID: string
  messageID: string
  type: "retry"
  attempt: number
  /** Absent when the attempts are unlimited. */
  max?: number
  /** Status code and the provider's text. */
  error: string
  /** When the next attempt goes, while waiting for it. */
  next?: number
}

/** A skill marked in the person's message, with its text as sent; `path` is the skill's folder, `at` says it was marked with `@`. */
export type SkillPart = { id: string; sessionID: string; messageID: string; type: "skill"; name: string; at?: boolean; path: string; body: string }
/** A command that ran before its message was sent, kept where the person put it. */
export type CommandPart = { id: string; sessionID: string; messageID: string; type: "command"; name: string }

export type Part = (TextBody & { type: "text" }) | (TextBody & { type: "reasoning" }) | FilePart | ToolPart | RetryPart | SkillPart | CommandPart

export type MessageWithParts = { info: Message; parts: Part[] }

/** `more`: older messages are left. Kept attachments come as `/file?…` links without the token. */
export type MessagePage = { messages: MessageWithParts[]; more: boolean }

/** `branch` is the checked-out one, or `detached at <hash>`; absent outside git. */
export type Project = { path: string; name: string; branch?: string | null }

export type FileEntry = { name: string; dir: boolean; ignored: boolean }

/** `M` changed, `A` added, `U` untracked, `D` deleted, `R` renamed; paths relative to the project. */
export type GitChange = { path: string; status: "M" | "A" | "U" | "D" | "R"; additions: number; deletions: number }

export type Prompt = {
  sessionID: string
  messageID: string
  agent: string
  model: ModelRef
  parts: PromptPart[]
}

/** Marks sit where the person put them; the server runs a message's one command first, the rest waits for it in the queue. */
export type PromptPart =
  | { type: "text"; id: string; text: string }
  | { type: "file"; id: string; mime: string; filename?: string; url: string }
  | { type: "skill"; id: string; name: string; at?: boolean }
  | { type: "command"; id: string; name: string }

export type SkillHarness = "nabucode" | "claude" | "codex"
export type SkillScope = "project" | "global" | "synced"
/** The order of sources says whose skill wins when names repeat. */
export type SkillSource = { harness: SkillHarness; scope: SkillScope; enabled: boolean }
/** `path` is the SKILL.md, or the file of an old Claude Code command. */
export type SkillInfo = { name: string; description: string; described: boolean; preview: string; enabled: boolean; path: string; harness: SkillHarness; scope: SkillScope }
export type CommandInfo = { name: string; description: string }

export type QuestionOption = { label: string; description: string }

export type Question = { question: string; header: string; options: QuestionOption[]; multiple?: boolean }

/** Questions an `ask` call puts to the user; the turn waits for the answers. */
export type QuestionRequest = {
  id: string
  sessionID: string
  questions: Question[]
  tool: { messageID: string; callID: string }
}

type Done = Record<string, never>

/** A shell installed on the server machine. */
/** `number` is the tab title number: the lowest free one in the project folder. */
export type TerminalInfo = { id: string; number: number }

/** `id` is the DevTools target; `number` is how the chat's model knows the tab; `favicon` is a `data:` URL. */
export type BrowserStored = { cache: boolean; sites: boolean }
export type ChromeExtension = { connected: boolean; version: string | null; folder: string | null }
export type BrowserTab = { id: string; number: number; title: string; url: string; favicon: string | null; loading: boolean }

export type Shell = {
  id: string
  name: string
  /** The shell colors what is typed, and NabuCode can turn it off. */
  highlight: boolean
}

export type Requests = {
  deepAiAccounts: { params: undefined; result: Account[] }
  deepAiCredits: { params: undefined; result: Credits | null }
  deepAiLogin: { params: undefined; result: { url: string } }
  deepAiRemove: { params: { email: string; id?: string }; result: Done }
  deepAiRemoveAll: { params: undefined; result: Done }
  deepAiCheckAll: { params: undefined; result: Done }
  workBuddyAccounts: { params: undefined; result: Account[] }
  workBuddyCredits: { params: undefined; result: Credits | null }
  workBuddyLogin: { params: undefined; result: { url: string } }
  workBuddyRemove: { params: { email: string }; result: Done }
  workBuddyRemoveAll: { params: undefined; result: Done }
  workBuddyCheckAll: { params: undefined; result: Done }
  serverInfo: { params: undefined; result: ServerInfo }
  providers: { params: undefined; result: Provider[] }
  providerCatalog: { params: undefined; result: ProviderEntry[] }
  setProviderEnabled: { params: { provider: string; enabled: boolean }; result: Done }
  providerAccounts: { params: { provider: string }; result: Account[] }
  providerUsage: { params: { provider: string }; result: UsageBar[] }
  sessionContext: { params: { sessionID: string }; result: ContextParts }
  providerLogin: { params: { provider: string; language?: string; timezone?: string }; result: { url: string } }
  providerLoginFinish: { params: { provider: string; code: string }; result: Done }
  providerAddKeys: { params: { provider: string; keys: string[] }; result: Done }
  providerRemove: { params: { provider: string; email: string; id?: string }; result: Done }
  providerRemoveAll: { params: { provider: string }; result: Done }
  providerCheckAll: { params: { provider: string }; result: Done }
  hiddenModels: { params: undefined; result: ModelKey[] }
  setModelsHidden: { params: { models: ModelKey[]; hidden: boolean }; result: Done }
  customProvider: { params: { id: string }; result: CustomProvider }
  saveCustomProvider: { params: CustomProviderConfig & { keys: CustomKeyInput[]; edit: boolean }; result: Done }
  removeCustomProvider: { params: { id: string }; result: Done }
  antigravityAccounts: { params: undefined; result: Account[] }
  antigravityLogin: { params: undefined; result: { url: string } }
  antigravityRemove: { params: { email: string }; result: Done }
  antigravityRemoveAll: { params: undefined; result: Done }
  antigravityRefresh: { params: undefined; result: Done }
  antigravityCheckAll: { params: undefined; result: Done }
  antigravityQuota: { params: undefined; result: Quota[] }
  claudeAccounts: { params: undefined; result: Account[] }
  claudeQuota: { params: undefined; result: Quota[] }
  claudeLogin: { params: undefined; result: { url: string } }
  claudeLoginFinish: { params: { code: string }; result: Done }
  claudeRemove: { params: { email: string }; result: Done }
  claudeRemoveAll: { params: undefined; result: Done }
  claudeCheckAll: { params: undefined; result: Done }
  chatGptAccounts: { params: undefined; result: Account[] }
  chatGptQuota: { params: undefined; result: Quota[] }
  chatGptLogin: { params: undefined; result: { url: string } }
  chatGptImportAuth: { params: { path: string }; result: Done }
  chatGptRemove: { params: { email: string }; result: Done }
  chatGptRemoveAll: { params: undefined; result: Done }
  chatGptCheckAll: { params: undefined; result: Done }
  chatGptPreferFree: { params: undefined; result: boolean }
  setChatGptPreferFree: { params: { enabled: boolean }; result: Done }
  grokAccounts: { params: undefined; result: Account[] }
  grokQuota: { params: undefined; result: Quota[] }
  grokLogin: { params: undefined; result: { url: string } }
  grokRemove: { params: { email: string }; result: Done }
  grokRemoveAll: { params: undefined; result: Done }
  grokCheckAll: { params: undefined; result: Done }
  zCodeAccounts: { params: undefined; result: Account[] }
  zCodeQuota: { params: undefined; result: Quota[] }
  zCodeLogin: { params: { language: string; timezone: string }; result: { url: string } }
  zCodeRemove: { params: { email: string }; result: Done }
  zCodeRemoveAll: { params: undefined; result: Done }
  zCodeCheckAll: { params: undefined; result: Done }
  agentRouterAccounts: { params: undefined; result: Account[] }
  agentRouterAdd: { params: { keys: string[] }; result: Done }
  agentRouterRemove: { params: { email: string }; result: Done }
  agentRouterRemoveAll: { params: undefined; result: Done }
  agentRouterCheckAll: { params: undefined; result: Done }
  coArenaAccounts: { params: undefined; result: Account[] }
  coArenaQuota: { params: undefined; result: CoArenaQuota }
  coArenaDirectChecks: { params: undefined; result: boolean }
  setCoArenaDirectChecks: { params: { enabled: boolean }; result: Done }
  coArenaExperimentalContext: { params: undefined; result: boolean }
  setCoArenaExperimentalContext: { params: { enabled: boolean }; result: Done }
  coArenaAdd: { params: { keys: string[] }; result: Done }
  coArenaRemove: { params: { email: string }; result: Done }
  coArenaRemoveAll: { params: undefined; result: Done }
  coArenaCheckAll: { params: undefined; result: Done }
  claudenAccounts: { params: undefined; result: Account[] }
  claudenAdd: { params: { keys: string[] }; result: Done }
  claudenRemove: { params: { email: string }; result: Done }
  claudenRemoveAll: { params: undefined; result: Done }
  claudenCheckAll: { params: undefined; result: Done }
  geminiAccounts: { params: undefined; result: Account[] }
  geminiAdd: { params: { keys: string[] }; result: Done }
  geminiRemove: { params: { email: string }; result: Done }
  geminiRemoveAll: { params: undefined; result: Done }
  geminiCheckAll: { params: undefined; result: Done }
  nvidiaBuildAccounts: { params: undefined; result: Account[] }
  nvidiaBuildAdd: { params: { keys: string[] }; result: Done }
  nvidiaBuildRemove: { params: { email: string }; result: Done }
  nvidiaBuildRemoveAll: { params: undefined; result: Done }
  nvidiaBuildCheckAll: { params: undefined; result: Done }
  sessionList: { params: { directory: string }; result: Session[] }
  sessionRecent: { params: { limit: number }; result: Session[] }
  /**
   * An empty `directory` makes a session without a project; a temporary one ends with the connection that made it.
   * `tools` limits the session to these tools or groups (`["browser"]`); `chrome` is the NabuCode extension's id of the Chrome it works in.
   */
  sessionCreate: { params: { directory?: string; temporary?: boolean; tools?: string[]; chrome?: string }; result: Session }
  sessionGet: { params: { sessionID: string }; result: Session }
  sessionStatuses: { params: { directory: string }; result: Record<string, SessionStatus> }
  sessionMessages: { params: { sessionID: string }; result: MessageWithParts[] }
  /** Up to `limit` messages before `before`, the newest ones without it. */
  sessionPage: { params: { sessionID: string; before?: string; limit: number }; result: MessagePage }
  /** Starts a turn, or queues the prompt while the session answers. */
  sessionPrompt: { params: Prompt; result: Done }
  /** Prompts waiting for the running turn, oldest first. */
  sessionQueue: { params: { sessionID: string }; result: Prompt[] }
  sessionDequeue: { params: { sessionID: string; messageID: string }; result: Done }
  setCompaction: { params: CompactionSettings; result: Done }
  sessionCompact: { params: { sessionID: string; model: ModelRef; focus?: string }; result: Done }
  skills: { params: { directory: string }; result: SkillInfo[] }
  skillSources: { params: undefined; result: SkillSource[] }
  setSkillSources: { params: { sources: SkillSource[] }; result: Done }
  setDisabledSkills: { params: { paths: string[] }; result: Done }
  commands: { params: undefined; result: CommandInfo[] }
  sessionAbort: { params: { sessionID: string }; result: Done }
  /** The queued prompt goes to the model now: a reply being written is cut off, running tools wrap up. The rest keeps waiting. */
  sessionInterrupt: { params: { sessionID: string; messageID: string }; result: Done }
  sessionRename: { params: { sessionID: string; title: string }; result: Done }
  sessionArchive: { params: { sessionID: string }; result: Done }
  /** Brings an archived session back to the lists. */
  sessionUnarchive: { params: { sessionID: string }; result: Done }
  sessionDelete: { params: { sessionID: string }; result: Done }
  questionList: { params: { directory: string }; result: QuestionRequest[] }
  /** The labels picked or the text typed, for each question in order. */
  questionReply: { params: { requestID: string; answers: string[][] }; result: Done }
  /** The user closed the questions without answering: the turn stops. */
  questionReject: { params: { requestID: string }; result: Done }
  /** The first is the default. */
  terminalShells: { params: undefined; result: Shell[] }
  /** Output and input then go through `NabuClient.terminalUrl`. */
  terminalCreate: {
    params: { directory: string; shell?: string; highlight: boolean; cols: number; rows: number }
    result: TerminalInfo
  }
  /** Live terminals of a project folder, oldest first. */
  terminalList: { params: { directory: string }; result: TerminalInfo[] }
  terminalResize: { params: { id: string; cols: number; rows: number }; result: Done }
  terminalClose: { params: { id: string }; result: Done }
  /** Tabs of the session's chat; frames and input go through the `/browser?id=` socket. */
  browserTabs: { params: { sessionID: string }; result: BrowserTab[] }
  /** An empty `url` opens an empty tab. */
  browserOpen: { params: { sessionID: string; url: string }; result: BrowserTab }
  browserGo: { params: { id: string; url: string; search?: string }; result: Done }
  browserClear: { params: { sites: boolean }; result: Done }
  browserStored: { params: undefined; result: BrowserStored }
  chromeExtension: { params: undefined; result: ChromeExtension }
  browserHistory: { params: { id: string; step: -1 | 1 }; result: Done }
  browserReload: { params: { id: string }; result: Done }
  browserClose: { params: { id: string }; result: Done }
  projectList: { params: undefined; result: Project[] }
  projectAdd: { params: { path: string; name: string }; result: Project[] }
  projectRename: { params: { path: string; name: string }; result: Project[] }
  projectRemove: { params: { path: string }; result: Project[] }
  projectReorder: { params: { paths: string[] }; result: Project[] }
  gitBranches: { params: { directory: string }; result: string[] }
  gitSwitch: { params: { directory: string; branch: string }; result: Done }
  fileList: { params: { directory: string; path: string }; result: FileEntry[] }
  gitChanges: { params: { directory: string }; result: GitChange[] }
  gitDiff: { params: { directory: string; path: string }; result: string }
  filesWatch: { params: { directory: string }; result: Done }
}

export type NabuEvent =
  | { type: "deepAiAccounts"; accounts: Account[] }
  | { type: "deepAiCredits"; credits: Credits | null }
  | { type: "deepAiLoginFailed"; message: string }
  | { type: "workBuddyAccounts"; accounts: Account[] }
  | { type: "workBuddyCredits"; credits: Credits | null }
  | { type: "workBuddyLoginFailed"; message: string }
  | { type: "connected" }
  | { type: "providersChanged" }
  | { type: "providerUpdated"; provider: string }
  | { type: "providerLoginFailed"; provider: string; message: string }
  | { type: "agentRouterAccounts"; accounts: Account[] }
  | { type: "coArenaAccounts"; accounts: Account[] }
  | { type: "claudenAccounts"; accounts: Account[] }
  | { type: "geminiAccounts"; accounts: Account[] }
  | { type: "nvidiaBuildAccounts"; accounts: Account[] }
  | { type: "coArenaQuota"; quota: CoArenaQuota }
  | { type: "antigravityAccounts"; accounts: Account[] }
  | { type: "antigravityLoginFailed"; message: string }
  | { type: "antigravityQuota"; quota: Quota[] }
  | { type: "claudeAccounts"; accounts: Account[] }
  | { type: "claudeQuota"; quota: Quota[] }
  | { type: "chatGptAccounts"; accounts: Account[] }
  | { type: "chatGptQuota"; quota: Quota[] }
  | { type: "chatGptLoginFailed"; message: string }
  | { type: "grokAccounts"; accounts: Account[] }
  | { type: "grokQuota"; quota: Quota[] }
  | { type: "grokLoginFailed"; message: string }
  | { type: "zCodeAccounts"; accounts: Account[] }
  | { type: "zCodeQuota"; quota: Quota[] }
  | { type: "zCodeLoginFailed"; message: string }
  | { type: "sessionUpdated"; session: Session }
  | { type: "sessionDeleted"; session: Session }
  | { type: "sessionStatus"; directory: string; sessionID: string; status: SessionStatus }
  | { type: "messageUpdated"; directory: string; message: Message }
  /** The session's queued prompts after one was added, removed or delivered. */
  | { type: "queueUpdated"; directory: string; sessionID: string; queue: Prompt[] }
  | { type: "partUpdated"; directory: string; part: Part }
  | { type: "partDelta"; directory: string; messageID: string; partID: string; field: "text"; delta: string }
  | { type: "questionAsked"; directory: string; request: QuestionRequest }
  /** Answered, dismissed, or gone with its turn. */
  | { type: "questionClosed"; directory: string; sessionID: string; requestID: string }
  | { type: "terminalExited"; id: string }
  | { type: "browserTabsChanged"; sessionID: string; tabs: BrowserTab[] }
  | { type: "projectsChanged"; projects: Project[] }
  | { type: "filesChanged"; directory: string }

/** Events about one session's contents; they carry the directory the session lives in. */
export type SessionEvent = Extract<
  NabuEvent,
  {
    type:
      | "sessionUpdated"
      | "sessionDeleted"
      | "sessionStatus"
      | "messageUpdated"
      | "queueUpdated"
      | "partUpdated"
      | "partDelta"
      | "questionAsked"
      | "questionClosed"
  }
>
