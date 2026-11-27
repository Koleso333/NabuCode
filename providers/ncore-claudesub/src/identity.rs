//! Claude Code 2.1.322 wire identity with a fake machine pinned to the account.

use sha2::{Digest, Sha256};

pub const USER_AGENT: &str = "claude-cli/2.1.322 (external, cli)";

/// Same order as Claude Code with OAuth sign-in.
pub const BETAS: &str = "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,\
context-management-2025-06-27,per-turn-control-2026-07-01,mid-conversation-tool-changes-2026-07-01,\
inline-tools-2026-09-15,advisor-tool-2026-03-01,thinking-token-count-2026-05-13,\
advanced-tool-use-2025-11-20,effort-2025-11-24";

pub struct Machine {
    pub device_id: String,
    pub os: &'static str,
    pub arch: &'static str,
}

impl Machine {
    pub fn for_account(account_uuid: &str) -> Self {
        let seed = Sha256::digest(format!("claude-machine:{account_uuid}"));
        // Roughly where Claude Code runs: Windows and Mac first, some Linux.
        let (os, arch) = match seed[0] % 20 {
            0..=7 => ("Windows", "x64"),
            8 => ("Windows", "arm64"),
            9..=15 => ("MacOS", "arm64"),
            16 => ("MacOS", "x64"),
            17 | 18 => ("Linux", "x64"),
            _ => ("Linux", "arm64"),
        };
        let device_id = Sha256::digest(format!("claude-device:{account_uuid}"));
        Self {
            device_id: hex(&device_id),
            os,
            arch,
        }
    }

    /// Headers the Anthropic JS SDK inside Claude Code adds to every call.
    pub fn headers(&self) -> Vec<(String, String)> {
        [
            ("x-app", "cli"),
            ("anthropic-dangerous-direct-browser-access", "true"),
            ("x-stainless-lang", "js"),
            ("x-stainless-package-version", "0.128.0"),
            ("x-stainless-os", self.os),
            ("x-stainless-arch", self.arch),
            ("x-stainless-runtime", "node"),
            ("x-stainless-runtime-version", "v26.12.1"),
            ("x-stainless-retry-count", "0"),
            ("x-stainless-timeout", "900"),
        ]
        .into_iter()
        .map(|(k, v)| (k.into(), v.into()))
        .collect()
    }

    /// Value of `metadata.user_id`: a JSON string, keys in Claude Code's order.
    pub fn user_id(&self, account_uuid: &str, session: &str) -> String {
        let quote = |s: &str| serde_json::Value::from(s).to_string();
        format!(
            r#"{{"device_id":{},"account_uuid":{},"session_id":{}}}"#,
            quote(&self.device_id),
            quote(account_uuid),
            quote(session)
        )
    }
}

/// Claude Code session ids are v4 UUIDs; any host chat id maps to a stable one.
pub fn session_uuid(chat: &str) -> String {
    let mut b: [u8; 16] = Sha256::digest(format!("claude-session:{chat}"))[..16].try_into().unwrap();
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h = hex(&b);
    format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..])
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
