use std::path::PathBuf;

use portable_pty::CommandBuilder;

#[derive(Clone)]
pub struct Shell {
    pub id: &'static str,
    pub name: &'static str,
    program: PathBuf,
    args: Vec<&'static str>,
    /// Gets our setup at start; also the only shell whose input coloring we can switch off.
    powershell: bool,
}

impl Shell {
    pub fn info(&self) -> nabu_protocol::Shell {
        nabu_protocol::Shell {
            id: self.id.into(),
            name: self.name.into(),
            highlight: self.powershell,
        }
    }

    pub fn command(&self, highlight: bool) -> CommandBuilder {
        let mut command = CommandBuilder::new(&self.program);
        command.args(&self.args);
        if self.powershell {
            let setup = if highlight {
                CYRILLIC_CTRL.to_string()
            } else {
                format!("{CYRILLIC_CTRL}; {PLAIN_POWERSHELL}")
            };
            command.args(["-NoExit", "-Command", &setup]);
        }
        command
    }
}

// PSReadLine 2.0 (Windows PowerShell) reads Ctrl+A under the Russian layout as Ctrl+ф and types «ф»,
// so every Ctrl+letter binding is repeated for the Russian letter on the same key.
const CYRILLIC_CTRL: &str = "$ru = @{ q='й'; w='ц'; e='у'; r='к'; t='е'; y='н'; u='г'; i='ш'; o='щ'; p='з'; a='ф'; s='ы'; d='в'; \
    f='а'; g='п'; h='р'; j='о'; k='л'; l='д'; z='я'; x='ч'; c='с'; v='м'; b='и'; n='т'; m='ь' }; \
    Get-PSReadLineKeyHandler -Bound | ForEach-Object { if ($_.Key -cmatch '^Ctrl\\+([a-z])$') { \
    try { Set-PSReadLineKeyHandler -Chord \"Ctrl+$($ru[$Matches[1]])\" -Function $_.Function } catch {} } }";

// Paints every PSReadLine token in the default color; predictions and selection keep theirs.
const PLAIN_POWERSHELL: &str = "$e = [char]27; (Get-PSReadLineOption).PSObject.Properties \
    | Where-Object { $_.Name -like '*Color' -and $_.Name -notmatch 'Prediction|Selection' } \
    | ForEach-Object { try { Set-PSReadLineOption -Colors @{ ($_.Name -replace '(Token)?Color$', '') = \"$($e)[39m\" } } catch {} }";

/// Installed shells, the default first.
#[cfg(windows)]
pub fn installed() -> Vec<Shell> {
    let powershell = |id, name, program| Shell {
        id,
        name,
        program,
        args: vec!["-NoLogo"],
        powershell: true,
    };
    let mut shells = vec![powershell(
        "powershell",
        "PowerShell",
        which("powershell").unwrap_or("powershell.exe".into()),
    )];
    if let Some(pwsh) = which("pwsh")
        .or_else(|| existing(program_files().map(|dir| dir.join(r"PowerShell\7\pwsh.exe"))))
    {
        shells.push(powershell("pwsh", "PowerShell 7", pwsh));
    }
    let cmd = std::env::var_os("COMSPEC")
        .map(PathBuf::from)
        .unwrap_or("cmd.exe".into());
    shells.push(Shell {
        id: "cmd",
        name: "CMD",
        program: cmd,
        // `/k` skips the version banner
        args: vec!["/k"],
        powershell: false,
    });
    if let Some(bash) = git_bash() {
        shells.push(Shell {
            id: "git-bash",
            name: "Git Bash",
            program: bash,
            args: vec!["--login", "-i"],
            powershell: false,
        });
    }
    shells
}

#[cfg(windows)]
fn git_bash() -> Option<PathBuf> {
    let near_git = which("git").and_then(|git| Some(git.parent()?.parent()?.join(r"bin\bash.exe")));
    existing(near_git)
        .or_else(|| existing(program_files().map(|dir| dir.join(r"Git\bin\bash.exe"))))
}

#[cfg(windows)]
fn program_files() -> Option<PathBuf> {
    std::env::var_os("ProgramFiles").map(PathBuf::from)
}

#[cfg(windows)]
fn existing(path: Option<PathBuf>) -> Option<PathBuf> {
    path.filter(|path| path.is_file())
}

#[cfg(not(windows))]
pub fn installed() -> Vec<Shell> {
    let names: [&'static str; 4] = ["zsh", "bash", "fish", "sh"];
    let mut shells: Vec<Shell> = names
        .into_iter()
        .filter_map(|name| {
            Some(Shell {
                id: name,
                name,
                program: which(name)?,
                args: vec!["-l"],
                powershell: false,
            })
        })
        .collect();
    let preferred = if cfg!(target_os = "macos") {
        Some("zsh".to_string())
    } else {
        login_shell()
    };
    if let Some(index) = shells
        .iter()
        .position(|shell| Some(shell.id) == preferred.as_deref())
    {
        let shell = shells.remove(index);
        shells.insert(0, shell);
    }
    shells
}

#[cfg(not(windows))]
fn login_shell() -> Option<String> {
    let shell = PathBuf::from(std::env::var_os("SHELL")?);
    Some(shell.file_name()?.to_string_lossy().into_owned())
}

fn which(name: &str) -> Option<PathBuf> {
    let file = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    };
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(&file))
        .find(|path| path.is_file())
}
