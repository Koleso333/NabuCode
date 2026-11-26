//! Codex CLI shaped User-Agent with a fake machine pinned to the account.

use sha2::{Digest, Sha256};

const CODEX_VERSION: &str = "0.171.0";

// Repeats act as weights.
const WIN_BUILDS: [&str; 6] = ["19045", "22631", "22631", "26100", "26100", "26200"];
const WIN_ARM_BUILDS: [&str; 3] = ["22631", "26100", "26200"];
const WIN_TERMINALS: [&str; 6] = [
    "WindowsTerminal",
    "WindowsTerminal",
    "WindowsTerminal",
    "vscode/1.105.1",
    "vscode/1.106.3",
    "unknown",
];
const MAC_ARM_VERSIONS: [&str; 6] = ["14.7.6", "15.6.1", "15.7.1", "15.7.1", "26.0.1", "26.1.0"];
const MAC_INTEL_VERSIONS: [&str; 3] = ["14.7.6", "15.6.1", "15.7.1"];
const MAC_TERMINALS: [&str; 6] = [
    "iTerm.app/3.5.14",
    "iTerm.app/3.6.5",
    "Apple_Terminal/455.1",
    "Apple_Terminal/455.1",
    "vscode/1.106.3",
    "ghostty/1.2.0",
];

/// `codex_cli_rs/<ver> (<os> <os ver>; <arch>) <terminal>`, same for an account forever.
pub(crate) fn for_account(account_id: &str) -> String {
    let seed = Sha256::digest(account_id.as_bytes());
    let pick = |slot: usize, list: &[&'static str]| list[seed[slot] as usize % list.len()];
    let (os, version, arch, terminal) = match seed[0] % 10 {
        0..=5 => ("Windows", format!("10.0.{}", pick(1, &WIN_BUILDS)), "x86_64", pick(2, &WIN_TERMINALS)),
        6 => ("Windows", format!("10.0.{}", pick(1, &WIN_ARM_BUILDS)), "aarch64", pick(2, &WIN_TERMINALS)),
        7 | 8 => ("Mac OS", pick(1, &MAC_ARM_VERSIONS).into(), "arm64", pick(2, &MAC_TERMINALS)),
        _ => ("Mac OS", pick(1, &MAC_INTEL_VERSIONS).into(), "x86_64", pick(2, &MAC_TERMINALS)),
    };
    format!("codex_cli_rs/{CODEX_VERSION} ({os} {version}; {arch}) {terminal}")
}

#[cfg(test)]
mod tests {
    use super::for_account;

    #[test]
    fn pinned_to_account() {
        assert_eq!(for_account("acc-1"), for_account("acc-1"));
        let agents: std::collections::HashSet<_> = (0..50).map(|i| for_account(&format!("acc-{i}"))).collect();
        assert!(agents.len() > 10);
        for agent in &agents {
            println!("{agent}");
            assert!(agent.starts_with("codex_cli_rs/0.171.0 ("));
            assert!(agent.bytes().all(|b| (b' '..=b'~').contains(&b)));
        }
    }
}
