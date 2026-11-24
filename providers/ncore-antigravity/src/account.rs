use serde::{Deserialize, Serialize};

/// One Google account; `refresh_token` is the composite `token|project|managedProject`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    pub email: String,
    pub refresh_token: String,
    #[serde(default)]
    pub project_id: Option<String>,
    #[serde(default = "yes")]
    pub enabled: bool,
}

fn yes() -> bool {
    true
}

#[derive(Deserialize)]
struct File {
    #[serde(default)]
    accounts: Vec<Account>,
}

impl Account {
    /// Bare OAuth refresh token without the project suffixes.
    pub fn oauth_refresh_token(&self) -> &str {
        self.refresh_token.split('|').next().unwrap_or("")
    }

    /// Managed project from the composite token, else the stored one, else the plain one.
    pub fn project(&self) -> Option<String> {
        let mut parts = self.refresh_token.split('|').skip(1);
        let plain = parts.next().filter(|s| !s.is_empty());
        let managed = parts.next().filter(|s| !s.is_empty());
        managed
            .map(str::to_owned)
            .or_else(|| self.project_id.clone().filter(|s| !s.is_empty()))
            .or_else(|| plain.map(str::to_owned))
    }

    /// Parses the ReProxy accounts file `{"accounts": [...]}`, keeping enabled accounts with a token.
    pub fn parse_file(json: &[u8]) -> Result<Vec<Account>, serde_json::Error> {
        let f: File = serde_json::from_slice(json)?;
        Ok(f.accounts
            .into_iter()
            .filter(|a| a.enabled && !a.refresh_token.is_empty())
            .collect())
    }
}

/// Builds the composite refresh token stored in account files.
pub fn composite_token(refresh: &str, project: Option<&str>, managed: Option<&str>) -> String {
    let base = format!("{refresh}|{}", project.unwrap_or(""));
    match managed {
        Some(m) => format!("{base}|{m}"),
        None => base,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_precedence() {
        let a = Account {
            email: "a".into(),
            refresh_token: "1//x|plain|managed".into(),
            project_id: Some("stored".into()),
            enabled: true,
        };
        assert_eq!(a.oauth_refresh_token(), "1//x");
        assert_eq!(a.project().as_deref(), Some("managed"));
        let b = Account {
            refresh_token: "1//x|plain".into(),
            project_id: None,
            ..a
        };
        assert_eq!(b.project().as_deref(), Some("plain"));
    }

    #[test]
    fn parses_reproxy_file() {
        let f = br#"{"accounts":[{"email":"a","refresh_token":"t"},{"email":"b","refresh_token":"t","enabled":false},{"email":"c","refresh_token":""}]}"#;
        let accts = Account::parse_file(f).unwrap();
        assert_eq!(accts.len(), 1);
        assert_eq!(accts[0].email, "a");
    }
}
