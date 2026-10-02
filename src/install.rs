//! The `inband install` command: installs the daemon and connects each agent client of this
//! machine.
//!
//! The installer is safe to run again, and migrates a v1 install: it removes the v1 hooks, MCP
//! entries and commands, keeps a `.bak` copy of each configuration file before it changes it, and
//! leaves the files and hooks of the user as they are.

use std::fmt::Write as _;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::assets::{ClientDirs, TEAM_COMMANDS};
use crate::config::{EnvMap, load_bridge_config};
use crate::tokens::{apply_legacy_env, read_token_lines};

const CONFIG_EXAMPLE: &str = include_str!("../assets/config.example.json");
/// The systemd user unit. `@BIN@` becomes the path of the binary.
const SERVICE_UNIT: &str = include_str!("../assets/inband.service");
/// The marker of the Codex skills of an earlier version, which the installer now removes.
const CODEX_SKILL_MARKER: &str = "InBand team command";
const TOKEN_VARS: [&str; 4] = [
    "INBAND_ADMIN_TOKEN",
    "INBAND_CLAUDE_TOKEN",
    "INBAND_CODEX_TOKEN",
    "INBAND_OPENCODE_TOKEN",
];
/// A text that only the files of InBand contain. A file without it belongs to the user.
const OURS_MARKER: &str = "InBand";
/// A text that only the v1 command files contain: the name of the v1 tool `claim_lead`.
const V1_MARKER: &str = "claim_lead";
const DEFAULT_PORT: u64 = 7447;

#[derive(Debug, thiserror::Error)]
pub enum InstallError {
    #[error("no home directory: set HOME")]
    NoHome,
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{path} is not valid JSON: {source}")]
    Json {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("{0}")]
    Invalid(String),
}

fn io_error(path: &Path) -> impl FnOnce(std::io::Error) -> InstallError + '_ {
    move |source| InstallError::Io {
        path: path.to_owned(),
        source,
    }
}

/// Finds and runs the programs of the clients: `claude`, `codex`, `systemctl`. The tests replace
/// it.
pub trait Runner {
    /// Returns the path of a program on `PATH`.
    fn find(&self, program: &str) -> Option<PathBuf>;
    /// Runs a program, and returns whether it succeeded, with its output.
    fn run(&self, program: &Path, args: &[&str]) -> (bool, String);
}

/// The real programs of this machine.
pub struct SystemRunner {
    pub path: Option<String>,
}

impl Runner for SystemRunner {
    fn find(&self, program: &str) -> Option<PathBuf> {
        std::env::split_paths(self.path.as_deref()?)
            .map(|dir| dir.join(program))
            .find(|candidate| {
                candidate
                    .metadata()
                    .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
            })
    }

    /// Runs the program without a shell, with an empty input.
    fn run(&self, program: &Path, args: &[&str]) -> (bool, String) {
        match std::process::Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::null())
            .output()
        {
            Ok(output) => {
                let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
                text.push_str(&String::from_utf8_lossy(&output.stderr));
                (output.status.success(), text)
            }
            Err(error) => (false, error.to_string()),
        }
    }
}

pub struct InstallOptions {
    pub home: PathBuf,
    /// The binary to install, usually the running one.
    pub source_binary: PathBuf,
    /// This machine runs agents only: the daemon runs on another machine.
    pub client_only: bool,
    pub service: bool,
    pub env: EnvMap,
}

/// What the installer did, step by step, and what the user must still do.
#[derive(Default)]
pub struct Report {
    pub lines: Vec<String>,
    /// What the user still has to do.
    pub todo: Vec<String>,
    /// The installed binary.
    pub binary: PathBuf,
    /// The daemon port, when this machine runs the daemon.
    pub port: Option<u64>,
}

impl Report {
    fn step(&mut self, title: &str) {
        self.lines.push(format!("\n== {title}"));
    }

    fn say(&mut self, line: impl Into<String>) {
        self.lines.push(format!("  {}", line.into()));
    }
}

/// Installs InBand for the user of `options.home`.
///
/// The steps are: the binary in `~/.local/bin`; `tokens.env` and `config.json` (or, on a machine
/// without a daemon, a check of the tokens); Claude Code, Codex and `OpenCode` when they are on
/// this machine; the systemd user service.
///
/// # Errors
///
/// Returns an error when a file cannot be read or written, or when the configuration is not valid.
pub fn install(options: &InstallOptions, runner: &dyn Runner) -> Result<Report, InstallError> {
    let mut report = Report::default();
    let home = &options.home;
    let data = crate::daemon::default_directory(&options.env)
        .unwrap_or_else(|| home.join(".local/share/mcp-servers/inband"));
    create_private_dir(&data)?;

    report.step("Installing the binary");
    let binary = install_binary(&options.source_binary, home)?;
    report.say(format!("{}", binary.display()));
    report.binary.clone_from(&binary);

    if options.client_only {
        report.step("Checking tokens.env");
        check_client_tokens(&data)?;
        report.say("tokens.env found, no token generated");
    } else {
        report.step("Writing tokens.env");
        write_tokens(&data, &mut report)?;
        report.step("Writing config.json");
        report.port = Some(write_config(&data, runner, &options.env, &mut report)?);
    }

    let dirs = ClientDirs::new(home, &options.env);
    report.step("Claude Code");
    if client_present(&dirs.claude, "claude", runner) {
        install_claude(&dirs, runner, &mut report)?;
    } else {
        report.say("not found, skipped");
    }
    report.step("Codex");
    if client_present(&dirs.codex, "codex", runner) {
        install_codex(&dirs, runner, &mut report)?;
    } else {
        report.say("not found, skipped");
    }
    report.step("OpenCode");
    if client_present(&dirs.opencode(), "opencode", runner) {
        install_opencode(&dirs, &mut report)?;
    } else {
        report.say("not found, skipped");
    }

    if options.service && !options.client_only {
        report.step("Daemon service");
        install_service(&dirs.config_home, &binary, runner, &mut report)?;
    }
    if options.client_only {
        report.todo.push(format!(
            "The agents here expect the daemon on 127.0.0.1:{DEFAULT_PORT}. Forward it from the daemon host, for example: ssh -N -R 127.0.0.1:{DEFAULT_PORT}:127.0.0.1:{DEFAULT_PORT} <this machine>"
        ));
    }
    Ok(report)
}

/// Returns `true` when the configuration directory or the program of a client exists. Some clients
/// create their directory only at their first start.
fn client_present(dir: &Path, program: &str, runner: &dyn Runner) -> bool {
    dir.is_dir() || runner.find(program).is_some()
}

fn create_private_dir(dir: &Path) -> Result<(), InstallError> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(io_error(dir))
}

fn create_dir(dir: &Path) -> Result<(), InstallError> {
    std::fs::create_dir_all(dir).map_err(io_error(dir))
}

/// Writes a file through a temporary file, so a crash never leaves half a file. The file keeps its
/// permissions, unless `mode` gives others.
fn write_file(path: &Path, text: &str, mode: Option<u32>) -> Result<(), InstallError> {
    let mode = mode.or_else(|| {
        std::fs::metadata(path)
            .ok()
            .map(|meta| meta.permissions().mode() & 0o777)
    });
    let temporary = path.with_extension("inband-tmp");
    std::fs::write(&temporary, text).map_err(io_error(&temporary))?;
    if let Some(mode) = mode {
        std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(mode))
            .map_err(io_error(&temporary))?;
    }
    std::fs::rename(&temporary, path).map_err(io_error(path))
}

/// Reads a file, or returns `None` when it does not exist.
fn read_optional(path: &Path) -> Result<Option<String>, InstallError> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(InstallError::Io {
            path: path.to_owned(),
            source,
        }),
    }
}

/// Reads a file that contains a JSON object, and returns its text with the object. A missing file
/// gives no text and an empty object.
fn read_json(path: &Path) -> Result<(Option<String>, Value), InstallError> {
    let Some(text) = read_optional(path)? else {
        return Ok((None, json!({})));
    };
    let value = if text.trim().is_empty() {
        json!({})
    } else {
        serde_json::from_str(&text).map_err(|source| InstallError::Json {
            path: path.to_owned(),
            source,
        })?
    };
    if !value.is_object() {
        return Err(InstallError::Invalid(format!(
            "{} must hold a JSON object",
            path.display()
        )));
    }
    Ok((Some(text), value))
}

/// Writes a JSON object when it changed, after a `.bak` copy of the previous file. Returns `true`
/// when the file changed.
fn write_json(path: &Path, before: Option<&str>, value: &Value) -> Result<bool, InstallError> {
    let text = format!(
        "{}\n",
        serde_json::to_string_pretty(value).unwrap_or_default()
    );
    if before == Some(text.as_str()) {
        return Ok(false);
    }
    if let Some(before) = before {
        let backup = PathBuf::from(format!("{}.bak", path.display()));
        write_file(&backup, before, None)?;
    }
    if let Some(parent) = path.parent() {
        create_dir(parent)?;
    }
    write_file(path, &text, None)?;
    Ok(true)
}

/// Copies the binary to `~/.local/bin/inband` through a temporary file, so the running daemons and
/// shims keep the old file.
fn install_binary(source: &Path, home: &Path) -> Result<PathBuf, InstallError> {
    let directory = home.join(".local/bin");
    create_dir(&directory)?;
    let target = directory.join("inband");
    let same = std::fs::canonicalize(source).ok() == std::fs::canonicalize(&target).ok();
    if !same {
        let temporary = directory.join(".inband.new");
        std::fs::copy(source, &temporary).map_err(io_error(&temporary))?;
        std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o755))
            .map_err(io_error(&temporary))?;
        std::fs::rename(&temporary, &target).map_err(io_error(&target))?;
    }
    Ok(target)
}

/// Reads the tokens of a `tokens.env` text, the pre-rename names included.
fn token_env(text: &str) -> EnvMap {
    let mut env = EnvMap::new();
    read_token_lines(text, &mut env);
    apply_legacy_env(&mut env);
    env
}

fn random_token() -> String {
    hex::encode(rand::random::<[u8; 32]>())
}

/// Adds the missing tokens to `tokens.env`, with mode 600. The other lines stay.
fn write_tokens(data: &Path, report: &mut Report) -> Result<(), InstallError> {
    let path = data.join("tokens.env");
    let mut text = read_optional(&path)?.unwrap_or_default();
    let present = token_env(&text);
    let mut added = 0;
    for name in TOKEN_VARS {
        if present.get(name).is_some_and(|value| !value.is_empty()) {
            continue;
        }
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        let _ = writeln!(text, "{name}={}", random_token());
        added += 1;
    }
    write_file(&path, &text, Some(0o600))?;
    report.say(if added == 0 {
        "every token exists, left untouched".to_owned()
    } else {
        format!("added {added} token(s) to {}", path.display())
    });
    Ok(())
}

/// Makes sure that a machine without a daemon has at least one client token. The tokens must come
/// from the daemon host: new tokens would not match.
fn check_client_tokens(data: &Path) -> Result<(), InstallError> {
    let path = data.join("tokens.env");
    let text = read_optional(&path)?.unwrap_or_default();
    let env = token_env(&text);
    if !TOKEN_VARS[1..]
        .iter()
        .any(|name| env.get(*name).is_some_and(|value| !value.is_empty()))
    {
        return Err(InstallError::Invalid(format!(
            "{} has no client token. Copy from the tokens.env of the daemon host only the lines of \
             the clients that run here (for example INBAND_CLAUDE_TOKEN), never the admin token, \
             then run this again",
            path.display()
        )));
    }
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).map_err(io_error(&path))
}

/// Updates a v1 configuration, and returns one line for each change:
///
/// - `routing` goes away: the teams replace the mesh mode;
/// - the missing clients come from the example;
/// - the `OpenCode` token also covers `opencode-<session>`, because each session now has a mailbox;
/// - the v1 `OpenCode` wake prompt, which names the fixed `opencode` mailbox, becomes the v2
///   prompt.
fn migrate_config(config: &mut Value) -> Vec<String> {
    let example: Value = serde_json::from_str(CONFIG_EXAMPLE).unwrap_or_default();
    let mut changes = Vec::new();
    let Some(fields) = config.as_object_mut() else {
        return changes;
    };
    if fields.remove("routing").is_some() {
        changes.push("removed routing: teams replace the mesh mode".to_owned());
    }
    let auth = fields
        .entry("auth")
        .or_insert_with(|| example["auth"].clone());
    if auth["required"] == false {
        changes.push(
            "WARNING: auth.required is false: every local process can use the daemon".to_owned(),
        );
        return changes;
    }
    let Some(auth) = auth.as_object_mut() else {
        return changes;
    };
    let clients = auth.entry("clients").or_insert_with(|| json!({}));
    let Some(clients) = clients.as_object_mut() else {
        return changes;
    };
    for (name, client) in example["auth"]["clients"].as_object().into_iter().flatten() {
        if !clients.contains_key(name) {
            clients.insert(name.clone(), client.clone());
            changes.push(format!("added the auth client {name}"));
        }
    }
    if let Some(agents) = clients
        .get_mut("opencode")
        .and_then(|client| client.get_mut("agents"))
        .and_then(Value::as_array_mut)
        && !agents.iter().any(|agent| agent == "opencode-*")
    {
        agents.push(json!("opencode-*"));
        changes.push("the OpenCode token now covers opencode-<session> mailboxes".to_owned());
    }
    if let Some(prompt) = fields
        .get_mut("wake")
        .and_then(|wake| wake.get_mut("opencode"))
        .and_then(|target| target.get_mut("prompt"))
        && prompt
            .as_str()
            .is_some_and(|text| text.contains("for=\"opencode\""))
    {
        *prompt = example["wake"]["opencode"]["prompt"].clone();
        changes.push("the OpenCode wake prompt now names the mailbox of the session".to_owned());
    }
    changes
}

/// Writes or updates `config.json`, and returns the port of the daemon. The installer checks the
/// result with the loader of the daemon, so it never writes a configuration that is not valid.
fn write_config(
    data: &Path,
    runner: &dyn Runner,
    env: &EnvMap,
    report: &mut Report,
) -> Result<u64, InstallError> {
    let path = data.join("config.json");
    let (before, mut config) = read_json(&path)?;
    if before.is_none() {
        config = serde_json::from_str(CONFIG_EXAMPLE).unwrap_or_default();
        if let Some(codex) = runner.find("codex") {
            config["wake"]["codex"]["command"] = json!(codex.display().to_string());
        }
        report.say("created from the example");
    }
    for change in migrate_config(&mut config) {
        report.say(change);
    }
    let text = serde_json::to_string(&config).unwrap_or_default();
    let mut check_env = env.clone();
    if let Some(tokens) = read_optional(&data.join("tokens.env"))? {
        read_token_lines(&tokens, &mut check_env);
        apply_legacy_env(&mut check_env);
    }
    let parsed = load_bridge_config(&text, &check_env)
        .map_err(|error| InstallError::Invalid(format!("{}: {error}", path.display())))?;
    if write_json(&path, before.as_deref(), &config)? && before.is_some() {
        report.say(format!("updated, backup kept at {}.bak", path.display()));
    } else if before.is_some() {
        report.say("up to date");
    }
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
        .map_err(io_error(&path))?;
    Ok(u64::from(parsed.port))
}

/// Returns a path as one shell word.
fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', r"'\''"))
}

/// Replaces the hooks of InBand in the hook settings of a client, and keeps all other hooks.
///
/// The function removes the hooks that `is_ours` accepts (v1 included), then adds one group for
/// each event. Returns the number of removed hooks.
fn set_hooks(
    settings: &mut Value,
    events: &[(&str, Value)],
    is_ours: impl Fn(&str) -> bool,
) -> usize {
    let mut removed = 0;
    let Some(object) = settings.as_object_mut() else {
        return 0;
    };
    let hooks = object.entry("hooks").or_insert_with(|| json!({}));
    if !hooks.is_object() {
        *hooks = json!({});
    }
    let Some(hooks) = hooks.as_object_mut() else {
        return 0;
    };
    for groups in hooks.values_mut() {
        let Some(groups) = groups.as_array_mut() else {
            continue;
        };
        groups.retain_mut(|group| {
            let Some(list) = group.get_mut("hooks").and_then(Value::as_array_mut) else {
                return true;
            };
            let before = list.len();
            list.retain(|hook| !hook["command"].as_str().is_some_and(&is_ours));
            removed += before - list.len();
            !list.is_empty()
        });
    }
    for (event, hook) in events {
        let groups = hooks.entry(*event).or_insert_with(|| json!([]));
        if let Some(groups) = groups.as_array_mut() {
            groups.push(json!({ "hooks": [hook] }));
        }
    }
    hooks.retain(|_, groups| groups.as_array().is_none_or(|groups| !groups.is_empty()));
    removed
}

/// Writes a file of InBand, unless the user put another file at this path.
fn write_owned(path: &Path, text: &str, report: &mut Report) -> Result<(), InstallError> {
    if let Some(existing) = read_optional(path)?
        && existing != text
        && !existing.contains(OURS_MARKER)
        && !existing.contains(V1_MARKER)
    {
        report.say(format!("{} is yours, left untouched", path.display()));
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        create_dir(parent)?;
    }
    write_file(path, text, None)
}

/// Writes the files of InBand that are in `directory`.
fn write_managed(
    dirs: &ClientDirs,
    directory: &Path,
    report: &mut Report,
) -> Result<(), InstallError> {
    for file in dirs.managed_files() {
        if file.path.starts_with(directory) {
            write_owned(&file.path, &file.text, report)?;
        }
    }
    Ok(())
}

/// Removes a file when it is a v1 file of InBand.
fn remove_v1(path: &Path, report: &mut Report) -> Result<(), InstallError> {
    if read_optional(path)?.is_some_and(|text| text.contains(V1_MARKER)) {
        std::fs::remove_file(path).map_err(io_error(path))?;
        report.say(format!("removed the v1 file {}", path.display()));
    }
    Ok(())
}

fn is_claude_hook(command: &str) -> bool {
    [
        "inband-name.sh",
        "inband-disconnect.sh",
        "inband-mailcheck.sh",
        "/agent-bridge-",
    ]
    .iter()
    .any(|old| command.contains(old))
        || command.ends_with(" hook claude")
}

/// Writes the Claude Code hooks and commands, and replaces the v1 MCP servers with the shim. v1
/// used an HTTP server and a separate channel server; the shim does the work of both.
fn install_claude(
    dirs: &ClientDirs,
    runner: &dyn Runner,
    report: &mut Report,
) -> Result<(), InstallError> {
    let (dir, binary) = (&dirs.claude, &dirs.binary);
    let settings_path = dir.join("settings.json");
    let (before, mut settings) = read_json(&settings_path)?;
    let command = format!("{} hook claude", shell_quote(binary));
    let hook = |timeout: u64| json!({ "type": "command", "command": command, "timeout": timeout });
    let events = [
        ("SessionStart", hook(10)),
        ("UserPromptSubmit", hook(10)),
        ("PostToolUse", hook(5)),
        ("SessionEnd", hook(5)),
    ];
    let removed = set_hooks(&mut settings, &events, is_claude_hook);
    write_json(&settings_path, before.as_deref(), &settings)?;
    report.say(format!(
        "hooks set in {} ({removed} old hook(s) replaced)",
        settings_path.display()
    ));
    let hooks_dir = dir.join("hooks");
    for name in [
        "inband-name.sh",
        "inband-disconnect.sh",
        "inband-mailcheck.sh",
        "inband-compute-name.sh",
        "inband-env.sh",
    ] {
        let path = hooks_dir.join(name);
        if path.exists() {
            std::fs::remove_file(&path).map_err(io_error(&path))?;
        }
    }
    write_managed(dirs, &dir.join("commands"), report)?;
    report.say("commands /lead, /join and /solo installed");

    let shim = binary.display().to_string();
    if let Some(claude) = runner.find("claude") {
        for old in ["inband-channel", "inband"] {
            runner.run(&claude, &["mcp", "remove", "--scope", "user", old]);
        }
        let (ok, output) = runner.run(
            &claude,
            &[
                "mcp", "add", "--scope", "user", "inband", "--", &shim, "shim",
            ],
        );
        if ok {
            report.say("MCP server inband added: the shim of each session");
        } else {
            report.say(format!("claude mcp add failed: {}", output.trim()));
            report.todo.push(format!(
                "Run: claude mcp add --scope user inband -- {shim} shim"
            ));
        }
    } else {
        report.todo.push(format!(
            "Run: claude mcp add --scope user inband -- {shim} shim"
        ));
    }
    report.todo.push(
        "Start Claude Code with --dangerously-load-development-channels server:inband to receive mail as it arrives"
            .to_owned(),
    );
    Ok(())
}

fn is_codex_hook(command: &str) -> bool {
    command.contains("codex-hook.ts") || command.ends_with(" hook codex")
}

/// Writes the Codex hooks and skills, and adds the shim as a stdio MCP server.
///
/// v1 used an HTTP server with the token in the environment. The shim reads the token file itself,
/// so Codex needs no token in its environment.
fn install_codex(
    dirs: &ClientDirs,
    runner: &dyn Runner,
    report: &mut Report,
) -> Result<(), InstallError> {
    let (dir, binary) = (&dirs.codex, &dirs.binary);
    let hooks_path = dir.join("hooks.json");
    let (before, mut hooks) = read_json(&hooks_path)?;
    let command = format!("{} hook codex", shell_quote(binary));
    let hook = |message: &str| json!({ "type": "command", "command": command, "statusMessage": message, "timeout": 5 });
    let events = [
        ("SessionStart", hook("Registering the inband mailbox")),
        (
            "UserPromptSubmit",
            hook("Checking for an inband team command"),
        ),
        ("Stop", hook("Checking the inband mailbox")),
    ];
    let removed = set_hooks(&mut hooks, &events, is_codex_hook);
    let changed = write_json(&hooks_path, before.as_deref(), &hooks)?;
    report.say(format!(
        "hooks set in {} ({removed} old hook(s) replaced)",
        hooks_path.display()
    ));
    if changed {
        report
            .todo
            .push("Codex asks you to trust the new hooks on its next start".to_owned());
    }
    remove_v1(&dir.join("prompts/lead.md"), report)?;
    remove_codex_skills(dir, report)?;

    let shim = binary.display().to_string();
    let add = [
        "mcp",
        "add",
        "inband",
        "--",
        shim.as_str(),
        "shim",
        "--codex",
    ];
    if let Some(codex) = runner.find("codex") {
        runner.run(&codex, &["mcp", "remove", "inband"]);
        let (ok, output) = runner.run(&codex, &add);
        if ok {
            report.say("MCP server inband added: the shim");
            approve_codex_tools(&dir.join("config.toml"), report)?;
        } else {
            report.say(format!("codex mcp add failed: {}", output.trim()));
            report.todo.push(format!("Run: codex {}", add.join(" ")));
        }
    } else {
        report.todo.push(format!("Run: codex {}", add.join(" ")));
    }
    Ok(())
}

/// Lets Codex call the InBand tools without a confirmation prompt.
///
/// A wake can start a Codex session in the background, where nobody can confirm a call: the session
/// would never read its mail. The daemon checks the session of each call, whatever the approval.
fn approve_codex_tools(path: &Path, report: &mut Report) -> Result<(), InstallError> {
    const TABLE: &str = "[mcp_servers.inband]";
    const APPROVE: &str = "default_tools_approval_mode = \"approve\"";
    let Some(text) = read_optional(path)? else {
        return Ok(());
    };
    let mut lines: Vec<&str> = text.lines().collect();
    let Some(start) = lines.iter().position(|line| line.trim() == TABLE) else {
        return Ok(());
    };
    let end = lines[start + 1..]
        .iter()
        .position(|line| line.trim_start().starts_with('['))
        .map_or(lines.len(), |offset| start + 1 + offset);
    if lines[start + 1..end]
        .iter()
        .any(|line| line.trim_start().starts_with("default_tools_approval_mode"))
    {
        return Ok(());
    }
    lines.insert(start + 1, APPROVE);
    let mut updated = lines.join("\n");
    updated.push('\n');
    write_file(path, &updated, None)?;
    report.say("the InBand tools run without approval prompts");
    Ok(())
}

/// Writes the `OpenCode` plugin, with the path of the binary in it, and the commands. Then removes
/// the v1 `inband` MCP server, because the plugin gives the same tools.
///
/// `opencode mcp add` writes `opencode.jsonc`. A rewrite would lose the comments of such a file, so
/// the user removes the entry there.
fn install_opencode(dirs: &ClientDirs, report: &mut Report) -> Result<(), InstallError> {
    let dir = &dirs.opencode();
    write_managed(dirs, dir, report)?;
    remove_v1(&dir.join("commands/lead.md"), report)?;
    report.say("plugin and commands /lead, /join and /solo installed");

    for name in ["opencode.json", "opencode.jsonc"] {
        let path = dir.join(name);
        let Some(text) = read_optional(&path)? else {
            continue;
        };
        if !text.contains("\"inband\"") {
            continue;
        }
        let Ok(mut config) = serde_json::from_str::<Value>(&text) else {
            report.todo.push(format!(
                "Remove the v1 \"inband\" MCP server from {}: the plugin replaces it",
                path.display()
            ));
            continue;
        };
        let removed = config
            .get_mut("mcp")
            .and_then(Value::as_object_mut)
            .and_then(|servers| servers.remove("inband"))
            .is_some();
        if removed {
            write_json(&path, Some(&text), &config)?;
            report.say(format!("removed the v1 MCP entry from {}", path.display()));
        }
    }
    report.todo.push(
        "OpenCode wakes need its server on the port of wake.opencode.baseUrl (default: opencode --port 14096)"
            .to_owned(),
    );
    Ok(())
}

/// Installs and starts the systemd user service. The old `agent-bridge` service uses the same port,
/// so the function stops and removes it.
/// Removes the `$lead`, `$join` and `$solo` skills of an earlier version.
///
/// Codex gives the hooks `$lead x` without a skill, and Codex puts the description of each skill in
/// the context of every session: a skill would only add text that an agent could change.
fn remove_codex_skills(dir: &Path, report: &mut Report) -> Result<(), InstallError> {
    for name in TEAM_COMMANDS {
        let skill = dir.join(format!("skills/{name}"));
        let file = skill.join("SKILL.md");
        if read_optional(&file)?.is_some_and(|text| text.contains(CODEX_SKILL_MARKER)) {
            std::fs::remove_file(&file).map_err(io_error(&file))?;
            let _ = std::fs::remove_dir(&skill);
            report.say(format!("removed the old skill {}", skill.display()));
        }
    }
    Ok(())
}

fn install_service(
    config_home: &Path,
    binary: &Path,
    runner: &dyn Runner,
    report: &mut Report,
) -> Result<(), InstallError> {
    let Some(systemctl) = runner.find("systemctl") else {
        report.say("no systemctl");
        report.todo.push(format!(
            "Start the daemon yourself: {} daemon",
            binary.display()
        ));
        return Ok(());
    };
    let units = config_home.join("systemd/user");
    create_dir(&units)?;
    let old = units.join("agent-bridge.service");
    if old.exists() {
        runner.run(&systemctl, &["--user", "disable", "--now", "agent-bridge"]);
        std::fs::remove_file(&old).map_err(io_error(&old))?;
        report.say("stopped and removed the old agent-bridge service");
    }
    let unit = SERVICE_UNIT.replace("@BIN@", &shell_quote(binary));
    write_file(&units.join("inband.service"), &unit, Some(0o644))?;
    for args in [
        &["--user", "daemon-reload"][..],
        &["--user", "enable", "inband"],
        &["--user", "restart", "inband"],
    ] {
        let (ok, output) = runner.run(&systemctl, args);
        if !ok {
            report.say(format!(
                "systemctl {} failed: {}",
                args.join(" "),
                output.trim()
            ));
            return Ok(());
        }
    }
    report.say("service inband enabled and restarted");
    Ok(())
}

#[cfg(test)]
#[path = "install_tests.rs"]
mod tests;
