//! @file install_tests.rs
//! @brief The tests of the installer, in a temporary home directory, with fake client programs.

use std::cell::RefCell;
use std::collections::BTreeSet;

use super::*;

/// @brief Finds only the programs that the test gives, and keeps each command.
struct FakeRunner {
    programs: BTreeSet<&'static str>,
    calls: RefCell<Vec<String>>,
}

impl FakeRunner {
    /// @brief Makes a fake runner that finds these programs.
    fn new(programs: &[&'static str]) -> Self {
        Self {
            programs: programs.iter().copied().collect(),
            calls: RefCell::new(Vec::new()),
        }
    }

    /// @brief Gives the commands that the installer ran.
    fn calls(&self) -> Vec<String> {
        self.calls.borrow().clone()
    }
}

impl Runner for FakeRunner {
    /// @brief Gives `/usr/bin/<program>` for a known program.
    fn find(&self, program: &str) -> Option<PathBuf> {
        self.programs
            .contains(program)
            .then(|| PathBuf::from(format!("/usr/bin/{program}")))
    }

    /// @brief Keeps the command, and tells that it succeeded.
    fn run(&self, program: &Path, args: &[&str]) -> (bool, String) {
        self.calls
            .borrow_mut()
            .push(format!("{} {}", program.display(), args.join(" ")));
        (true, String::new())
    }
}

/// @brief Makes a home directory with the configuration directories of the three clients.
fn home(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("inband-install-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    for sub in [".claude", ".codex", ".config/opencode"] {
        std::fs::create_dir_all(dir.join(sub)).unwrap();
    }
    std::fs::write(dir.join("inband-build"), "binary").unwrap();
    dir
}

/// @brief Gives the install options for a home directory.
fn options(home: &Path, client_only: bool) -> InstallOptions {
    InstallOptions {
        home: home.to_owned(),
        source_binary: home.join("inband-build"),
        client_only,
        service: true,
        env: [("HOME".to_owned(), home.display().to_string())].into(),
    }
}

/// @brief Reads a JSON file.
fn json_file(path: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// @brief Gives the permission bits of a file.
fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

/// @brief Gives the commands of all the hooks of one event.
fn commands(settings: &Value, event: &str) -> Vec<String> {
    settings["hooks"][event]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|group| group["hooks"].as_array().cloned().unwrap_or_default())
        .filter_map(|hook| hook["command"].as_str().map(str::to_owned))
        .collect()
}

/// @brief Gives the install directory of a home directory.
fn data(home: &Path) -> PathBuf {
    home.join(".local/share/mcp-servers/inband")
}

#[test]
fn a_fresh_install_sets_up_the_daemon_and_every_client() {
    let home = home("fresh");
    let runner = FakeRunner::new(&["claude", "codex", "systemctl"]);
    let report = install(&options(&home, false), &runner).unwrap();
    let binary = home.join(".local/bin/inband");
    assert_eq!(report.binary, binary);
    assert_eq!(mode(&binary), 0o755);

    let tokens = std::fs::read_to_string(data(&home).join("tokens.env")).unwrap();
    assert_eq!(mode(&data(&home).join("tokens.env")), 0o600);
    for name in TOKEN_VARS {
        let line = tokens.lines().find(|line| line.starts_with(name)).unwrap();
        assert_eq!(line.len(), name.len() + 1 + 64, "{line}");
    }
    let config = json_file(&data(&home).join("config.json"));
    assert_eq!(mode(&data(&home).join("config.json")), 0o600);
    assert_eq!(config["wake"]["codex"]["command"], "/usr/bin/codex");
    assert_eq!(report.port, Some(7447));

    let quoted = format!("'{}'", binary.display());
    let settings = json_file(&home.join(".claude/settings.json"));
    for event in [
        "SessionStart",
        "UserPromptSubmit",
        "PostToolUse",
        "SessionEnd",
    ] {
        assert_eq!(
            commands(&settings, event),
            vec![format!("{quoted} hook claude")],
            "{event}"
        );
    }
    let codex_hooks = json_file(&home.join(".codex/hooks.json"));
    for event in ["SessionStart", "UserPromptSubmit", "Stop"] {
        assert_eq!(
            commands(&codex_hooks, event),
            vec![format!("{quoted} hook codex")]
        );
    }
    for name in TEAM_COMMANDS {
        assert!(home.join(format!(".claude/commands/{name}.md")).exists());
        assert!(home.join(format!(".codex/skills/{name}/SKILL.md")).exists());
        assert!(
            home.join(format!(".config/opencode/command/{name}.md"))
                .exists()
        );
    }
    let plugin = std::fs::read_to_string(home.join(".config/opencode/plugin/inband.js")).unwrap();
    assert!(
        plugin.contains(&format!(
            "process.env.INBAND_BIN || \"{}\"",
            binary.display()
        )),
        "the plugin runs the installed binary"
    );
    let unit = std::fs::read_to_string(home.join(".config/systemd/user/inband.service")).unwrap();
    assert!(
        unit.contains(&format!("ExecStart={quoted} daemon")),
        "{unit}"
    );

    let calls = runner.calls();
    let shim = format!(
        "/usr/bin/claude mcp add --scope user inband -- {} shim",
        binary.display()
    );
    assert!(calls.contains(&shim), "{calls:#?}");
    assert!(calls.contains(&format!(
        "/usr/bin/codex mcp add inband -- {} shim --codex",
        binary.display()
    )));
    assert!(
        !report
            .todo
            .iter()
            .any(|item| item.contains("INBAND_CODEX_TOKEN")),
        "Codex needs no token in its environment"
    );
    assert!(calls.contains(&"/usr/bin/systemctl --user restart inband".to_owned()));
}

/// @brief Writes a file and creates its directory.
fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// @brief Makes a v1 install, with hooks and files of the user near it.
fn v1_install(home: &Path) {
    let data = data(home);
    write(
        &data.join("config.json"),
        r#"{"port": 7447, "maxMessageBytes": 65536, "routing": "mesh",
            "auth": {"required": true, "clients": {
              "claude": {"tokenEnv": "INBAND_CLAUDE_TOKEN", "agents": ["claude-*"]},
              "opencode": {"tokenEnv": "INBAND_OPENCODE_TOKEN", "agents": ["opencode"]},
              "admin": {"tokenEnv": "INBAND_ADMIN_TOKEN", "agents": ["*"], "admin": true}}},
            "wake": {"opencode": {"type": "opencode", "baseUrl": "http://127.0.0.1:14096",
              "prompt": "Call get_messages with for=\"opencode\"", "debounceSeconds": 30, "maxWakesPerHour": 20}}}"#,
    );
    write(
        &data.join("tokens.env"),
        &format!(
            "AGENT_BRIDGE_CLAUDE_TOKEN={}\nINBAND_ADMIN_TOKEN={}\n",
            "c".repeat(64),
            "a".repeat(64)
        ),
    );
    write(
        &home.join(".claude/settings.json"),
        r#"{"model": "opus", "hooks": {
            "SessionStart": [{"hooks": [{"type": "command", "command": "bash \"/h/.claude/hooks/inband-name.sh\""},
                                        {"type": "command", "command": "my-own-start"}]}],
            "PostToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "bash \"/h/.claude/hooks/inband-mailcheck.sh\""}]}],
            "Stop": [{"hooks": [{"type": "command", "command": "notify-send done"}]}]}}"#,
    );
    write(&home.join(".claude/hooks/inband-name.sh"), "#!/bin/sh\n");
    write(
        &home.join(".claude/commands/lead.md"),
        "Call the inband tool `claim_lead`",
    );
    write(
        &home.join(".claude/commands/solo.md"),
        "my own solo command",
    );
    write(
        &home.join(".codex/hooks.json"),
        r#"{"hooks": {"SessionStart": [{"hooks": [{"type": "command", "command": "bun run /x/scripts/codex-hook.ts"}]}],
                      "PreToolUse": [{"hooks": [{"type": "command", "command": "my-guard"}]}]}}"#,
    );
    write(
        &home.join(".codex/prompts/lead.md"),
        "Call the inband tool `claim_lead`",
    );
    write(
        &home.join(".config/opencode/opencode.json"),
        r#"{"mcp": {"inband": {"type": "remote", "url": "http://127.0.0.1:7447/mcp"}, "other": {"type": "local"}}}"#,
    );
    write(
        &home.join(".config/opencode/opencode.jsonc"),
        r#"{"mcp": {"inband": {"type": "remote", "headers": {"Authorization": "Bearer x"}}}}"#,
    );
    write(
        &home.join(".config/opencode/commands/lead.md"),
        "Call the inband tool `claim_lead`",
    );
}

#[test]
fn a_v1_install_is_migrated_and_the_files_of_the_user_are_kept() {
    let home = home("v1");
    v1_install(&home);
    let runner = FakeRunner::new(&["claude", "codex"]);
    let report = install(&options(&home, false), &runner).unwrap();
    let text = report.lines.join("\n");

    let config = json_file(&data(&home).join("config.json"));
    assert!(config.get("routing").is_none());
    assert_eq!(
        config["auth"]["clients"]["opencode"]["agents"],
        json!(["opencode", "opencode-*"])
    );
    assert!(config["auth"]["clients"]["codex"].is_object(), "{text}");
    assert!(
        config["wake"]["opencode"]["prompt"]
            .as_str()
            .unwrap()
            .contains("{mailbox}")
    );
    assert!(data(&home).join("config.json.bak").exists());

    let tokens = std::fs::read_to_string(data(&home).join("tokens.env")).unwrap();
    assert!(
        !tokens.contains("INBAND_CLAUDE_TOKEN"),
        "the legacy Claude token stays the Claude token"
    );
    assert!(tokens.contains("INBAND_CODEX_TOKEN=") && tokens.contains("INBAND_OPENCODE_TOKEN="));
    assert_eq!(tokens.matches("INBAND_ADMIN_TOKEN=").count(), 1);

    let settings = json_file(&home.join(".claude/settings.json"));
    assert_eq!(settings["model"], "opus");
    let start = commands(&settings, "SessionStart");
    assert!(start.contains(&"my-own-start".to_owned()), "{start:?}");
    assert!(
        !start
            .iter()
            .any(|command| command.contains("inband-name.sh"))
    );
    assert_eq!(commands(&settings, "PostToolUse").len(), 1);
    assert_eq!(commands(&settings, "Stop"), vec!["notify-send done"]);
    assert!(home.join(".claude/settings.json.bak").exists());
    assert!(!home.join(".claude/hooks/inband-name.sh").exists());
    assert!(
        std::fs::read_to_string(home.join(".claude/commands/lead.md"))
            .unwrap()
            .contains("InBand hook already applied it")
    );
    assert_eq!(
        std::fs::read_to_string(home.join(".claude/commands/solo.md")).unwrap(),
        "my own solo command"
    );
    assert!(text.contains("solo.md is yours, left untouched"), "{text}");

    let codex_hooks = json_file(&home.join(".codex/hooks.json"));
    assert_eq!(commands(&codex_hooks, "PreToolUse"), vec!["my-guard"]);
    assert_eq!(commands(&codex_hooks, "SessionStart").len(), 1);
    assert!(!home.join(".codex/prompts/lead.md").exists());

    let opencode = json_file(&home.join(".config/opencode/opencode.json"));
    assert!(opencode["mcp"].get("inband").is_none());
    assert!(opencode["mcp"]["other"].is_object());
    let jsonc = json_file(&home.join(".config/opencode/opencode.jsonc"));
    assert!(
        jsonc["mcp"].get("inband").is_none(),
        "opencode mcp add writes .jsonc"
    );
    assert!(!home.join(".config/opencode/commands/lead.md").exists());

    let calls = runner.calls();
    assert!(calls.contains(&"/usr/bin/claude mcp remove --scope user inband-channel".to_owned()));
    assert!(
        !calls.iter().any(|call| call.contains("systemctl")),
        "no systemctl found"
    );
    assert!(
        report
            .todo
            .iter()
            .any(|item| item.contains("daemon yourself"))
    );
}

#[test]
fn running_it_again_changes_nothing() {
    let home = home("again");
    v1_install(&home);
    let runner = FakeRunner::new(&["claude"]);
    install(&options(&home, false), &runner).unwrap();
    let files = [
        data(&home).join("config.json"),
        data(&home).join("tokens.env"),
        home.join(".claude/settings.json"),
        home.join(".codex/hooks.json"),
        home.join(".config/opencode/plugin/inband.js"),
    ];
    let first: Vec<String> = files
        .iter()
        .map(|path| std::fs::read_to_string(path).unwrap())
        .collect();
    let backup = std::fs::read_to_string(home.join(".claude/settings.json.bak")).unwrap();
    install(&options(&home, false), &runner).unwrap();
    for (path, before) in files.iter().zip(&first) {
        assert_eq!(
            &std::fs::read_to_string(path).unwrap(),
            before,
            "{}",
            path.display()
        );
    }
    assert_eq!(
        std::fs::read_to_string(home.join(".claude/settings.json.bak")).unwrap(),
        backup,
        "the v1 backup is not overwritten by an unchanged file"
    );
}

#[test]
fn a_client_machine_needs_the_tokens_of_the_daemon_host() {
    let home = home("client");
    let runner = FakeRunner::new(&["claude", "systemctl"]);
    let refused = install(&options(&home, true), &runner);
    assert!(
        matches!(refused, Err(InstallError::Invalid(ref text)) if text.contains("never the admin token"))
    );
    write(
        &data(&home).join("tokens.env"),
        &format!("INBAND_CLAUDE_TOKEN={}\n", "c".repeat(64)),
    );
    let report = install(&options(&home, true), &runner).unwrap();
    assert_eq!(report.port, None);
    assert!(!data(&home).join("config.json").exists());
    assert_eq!(mode(&data(&home).join("tokens.env")), 0o600);
    assert!(!runner.calls().iter().any(|call| call.contains("systemctl")));
    assert!(report.todo.iter().any(|item| item.contains("ssh -N -R")));
}

#[test]
fn an_invalid_config_is_reported_and_left_alone() {
    let home = home("invalid");
    write(&data(&home).join("config.json"), r#"{"port": 0}"#);
    let before = std::fs::read_to_string(data(&home).join("config.json")).unwrap();
    let result = install(&options(&home, false), &FakeRunner::new(&[]));
    assert!(matches!(result, Err(InstallError::Invalid(_))));
    assert_eq!(
        std::fs::read_to_string(data(&home).join("config.json")).unwrap(),
        before
    );
}

#[test]
fn an_opencode_config_with_comments_is_left_to_the_user() {
    let home = home("jsonc");
    let text = "{\n  // mine\n  \"mcp\": {\"inband\": {\"type\": \"remote\"}}\n}\n";
    write(&home.join(".config/opencode/opencode.jsonc"), text);
    let report = install(&options(&home, false), &FakeRunner::new(&[])).unwrap();
    assert_eq!(
        std::fs::read_to_string(home.join(".config/opencode/opencode.jsonc")).unwrap(),
        text
    );
    assert!(
        report
            .todo
            .iter()
            .any(|item| item.contains("opencode.jsonc"))
    );
}

#[test]
fn codex_calls_the_inband_tools_without_approval_prompts() {
    let home = home("approve");
    let config = home.join(".codex/config.toml");
    write(
        &config,
        "model = \"o4\"\n\n[mcp_servers.inband]\ncommand = \"/b/inband\"\nargs = [\"shim\", \"--codex\"]\n\n[mcp_servers.other]\ncommand = \"x\"\n",
    );
    let mut report = Report::default();
    approve_codex_tools(&config, &mut report).unwrap();
    approve_codex_tools(&config, &mut report).unwrap();
    let text = std::fs::read_to_string(&config).unwrap();
    assert_eq!(
        text.matches("default_tools_approval_mode").count(),
        1,
        "{text}"
    );
    let inband = text.split("[mcp_servers.other]").next().unwrap();
    assert!(
        inband.contains("[mcp_servers.inband]\ndefault_tools_approval_mode = \"approve\""),
        "{text}"
    );
    assert!(text.ends_with("[mcp_servers.other]\ncommand = \"x\"\n"));
}
