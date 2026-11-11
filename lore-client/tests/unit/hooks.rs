// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::path::Path;
use std::time::Duration;
use std::time::Instant;

use lore::interface::LoreFileAction;
use lore_client::hooks::HookFile;
use lore_client::hooks::HookOutcome;
use lore_client::hooks::Payload;
use lore_client::hooks::PreCommitConfig;
use lore_client::hooks::action_name;
use lore_client::hooks::env_truthy;
use lore_client::hooks::execute;
use lore_client::hooks::is_trusted;
use lore_client::hooks::parse_hooks_file;
use lore_client::hooks::resolve_program;
use lore_client::hooks::select_command;

fn argv(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| (*s).to_string()).collect()
}

mod parse {
    use super::*;

    #[test]
    fn a_full_file_parses() {
        let file = parse_hooks_file(
            r#"
version = 1
[pre_commit]
command = ["sh", "a.sh"]
command_windows = ["powershell.exe", "-File", "a.ps1"]
command_unix = ["sh", "u.sh"]
timeout_secs = 30
"#,
        )
        .unwrap();
        let cfg = file.pre_commit.unwrap();
        assert_eq!(cfg.timeout_secs, Some(30));
        assert_eq!(cfg.command_windows.unwrap()[0], "powershell.exe");
    }

    #[test]
    fn only_command_parses() {
        let file = parse_hooks_file("version = 1\n[pre_commit]\ncommand = [\"x\"]\n").unwrap();
        assert!(file.pre_commit.unwrap().timeout_secs.is_none());
    }

    #[test]
    fn an_unknown_key_is_an_error() {
        assert!(
            parse_hooks_file("version = 1\n[pre_commit]\ncommand = [\"x\"]\nbogus = 1\n").is_err()
        );
        assert!(parse_hooks_file("version = 1\nbogus = 1\n").is_err());
    }

    #[test]
    fn another_version_is_an_error() {
        assert!(parse_hooks_file("version = 2\n").is_err());
    }

    #[test]
    fn timeout_outside_the_range_is_an_error() {
        for t in [0, 601] {
            let text =
                format!("version = 1\n[pre_commit]\ncommand = [\"x\"]\ntimeout_secs = {t}\n");
            assert!(parse_hooks_file(&text).is_err(), "{t}");
        }
        let ok = "version = 1\n[pre_commit]\ncommand = [\"x\"]\ntimeout_secs = 600\n";
        assert!(parse_hooks_file(ok).is_ok());
    }

    #[test]
    fn an_empty_command_is_an_error() {
        assert!(parse_hooks_file("version = 1\n[pre_commit]\ncommand = []\n").is_err());
    }

    #[test]
    fn no_pre_commit_section_is_fine() {
        assert!(
            parse_hooks_file("version = 1\n")
                .unwrap()
                .pre_commit
                .is_none()
        );
    }
}

mod select {
    use super::*;

    fn cfg(
        command: Option<&[&str]>,
        win: Option<&[&str]>,
        unix: Option<&[&str]>,
    ) -> PreCommitConfig {
        PreCommitConfig {
            command: command.map(argv),
            command_windows: win.map(argv),
            command_unix: unix.map(argv),
            timeout_secs: None,
        }
    }

    #[test]
    fn windows_prefers_its_override_and_falls_back() {
        let c = cfg(Some(&["c"]), Some(&["w"]), Some(&["u"]));
        assert_eq!(select_command(&c, true).unwrap(), ["w"]);
        let c = cfg(Some(&["c"]), None, Some(&["u"]));
        assert_eq!(select_command(&c, true).unwrap(), ["c"]);
    }

    #[test]
    fn unix_prefers_its_override_and_falls_back() {
        let c = cfg(Some(&["c"]), Some(&["w"]), Some(&["u"]));
        assert_eq!(select_command(&c, false).unwrap(), ["u"]);
        let c = cfg(Some(&["c"]), Some(&["w"]), None);
        assert_eq!(select_command(&c, false).unwrap(), ["c"]);
    }

    #[test]
    fn nothing_selects_none() {
        assert!(select_command(&cfg(None, Some(&["w"]), None), false).is_none());
        assert!(select_command(&cfg(None, None, Some(&["u"])), true).is_none());
    }
}

#[test]
fn resolve_program_keeps_bare_names_and_joins_paths() {
    let root = Path::new("/repo");
    assert_eq!(resolve_program("sh", root), Path::new("sh"));
    assert_eq!(
        resolve_program("tools/h.exe", root),
        root.join("tools/h.exe")
    );
    assert_eq!(
        resolve_program("tools\\h.exe", root),
        root.join("tools\\h.exe")
    );
}

#[test]
fn env_truthy_parses_the_usual_spellings() {
    for v in ["1", "true", "YES", "on", " On "] {
        assert!(env_truthy(Some(v)), "{v}");
    }
    for v in ["0", "false", "", "garbage"] {
        assert!(!env_truthy(Some(v)), "{v}");
    }
    assert!(!env_truthy(None));
}

mod trust {
    use super::*;

    fn trusted(entries: &[&str], remote: Option<&str>, root: &str, env: bool, win: bool) -> bool {
        is_trusted(&argv(entries), remote, Path::new(root), env, win)
    }

    #[test]
    fn env_wins() {
        assert!(trusted(&[], None, "D:/Work/Game", true, false));
    }

    #[test]
    fn star_trusts_everything() {
        assert!(trusted(&["*"], None, "D:/Work/Game", false, false));
    }

    #[test]
    fn empty_list_trusts_nothing() {
        assert!(!trusted(
            &[],
            Some("lore://h/x"),
            "D:/Work/Game",
            false,
            false
        ));
        assert!(!trusted(
            &[""],
            Some("lore://h/x"),
            "D:/Work/Game",
            false,
            false
        ));
    }

    #[test]
    fn remote_prefix_is_case_insensitive() {
        let entry = ["lore://lore.int.evorsolutions.com"];
        for remote in [
            "lore://LORE.int.evorsolutions.com/game",
            "lore://lore.int.evorsolutions.com:41337/game",
            "lore://lore.int.evorsolutions.com",
        ] {
            assert!(
                trusted(&entry, Some(remote), "/r", false, false),
                "{remote}"
            );
        }
        assert!(!trusted(
            &entry,
            Some("lore://other.example.com/game"),
            "/r",
            false,
            false
        ));
    }

    #[test]
    fn remote_prefix_must_end_at_a_host_boundary() {
        let entry = ["lore://lore.int.evorsolutions.com"];
        let evil = Some("lore://lore.int.evorsolutions.com.evil.net/g");
        assert!(!trusted(&entry, evil, "/r", false, false));
    }

    #[test]
    fn missing_remote_does_not_match_a_remote_entry() {
        assert!(!trusted(
            &["lore://lore.int.evorsolutions.com"],
            None,
            "/r",
            false,
            false
        ));
    }

    #[test]
    fn path_prefix_matches_on_a_component_boundary() {
        assert!(trusted(&["D:/Work/"], None, "D:/Work/Game", false, false));
        assert!(trusted(&["D:/Work"], None, "D:/Work/Game", false, false));
        assert!(!trusted(&["D:/Wor"], None, "D:/Work/Game", false, false));
    }

    #[test]
    fn path_case_and_separators() {
        assert!(trusted(&["d:/work/"], None, "D:\\Work\\Game", false, true));
        assert!(!trusted(&["d:/work/"], None, "D:/Work/Game", false, false));
    }
}

#[test]
fn action_names() {
    assert_eq!(action_name(LoreFileAction::Add), "add");
    assert_eq!(action_name(LoreFileAction::Delete), "delete");
    assert_eq!(action_name(LoreFileAction::Move), "move");
    assert_eq!(action_name(LoreFileAction::Copy), "copy");
    assert_eq!(action_name(LoreFileAction::Keep), "modify");
}

#[test]
fn payload_serializes_with_stable_keys() {
    let file = |path: &str, action, from: Option<&str>| HookFile {
        path: path.to_string(),
        action,
        r#type: "file",
        from_path: from.map(str::to_string),
        merged: false,
    };
    let mut payload = Payload {
        version: 1,
        hook: "pre-commit",
        operation: "commit",
        repository_root: "C:\\Work\\MyGame".to_string(),
        repository_id: "id".to_string(),
        remote_url: None,
        branch: "main".to_string(),
        revision: "abc".to_string(),
        revision_number: 41,
        merge: false,
        dry_run: false,
        force: false,
        interactive: true,
        message: "GAME-1 zażółć\n\nbody \"q\"".to_string(),
        link: None,
        layer: None,
        link_messages: Default::default(),
        layer_messages: Default::default(),
        files: vec![
            file("b.txt", "move", Some("a.txt")),
            file("a.txt", "add", None),
        ],
    };
    payload.sort_files();
    let json: serde_json::Value =
        serde_json::from_slice(&serde_json::to_vec(&payload).unwrap()).unwrap();
    assert_eq!(json["repository_root"], "C:\\Work\\MyGame");
    assert_eq!(json["message"], "GAME-1 zażółć\n\nbody \"q\"");
    assert_eq!(json["revision_number"], 41);
    assert!(json["link"].is_null());
    assert_eq!(json["files"][0]["path"], "a.txt");
    assert!(json["files"][0]["from_path"].is_null());
    assert_eq!(json["files"][0]["type"], "file");
    assert_eq!(json["files"][1]["from_path"], "a.txt");
}

mod execute_tests {
    use super::*;

    #[cfg(unix)]
    fn shell(script: &str) -> Vec<String> {
        argv(&["sh", "-c", script])
    }
    #[cfg(windows)]
    fn shell(script: &str) -> Vec<String> {
        argv(&["cmd", "/c", script])
    }

    fn run(argv: &[String], payload: &[u8], timeout: Duration) -> HookOutcome {
        execute(argv, &std::env::temp_dir(), &[], payload, timeout, false)
    }

    #[test]
    fn exit_zero_passes() {
        #[cfg(unix)]
        let script = "exit 0";
        #[cfg(windows)]
        let script = "exit /b 0";
        let outcome = run(&shell(script), b"{}", Duration::from_secs(10));
        assert!(matches!(outcome, HookOutcome::Passed), "{outcome:?}");
    }

    #[test]
    fn a_failing_exit_code_is_reported() {
        #[cfg(unix)]
        let script = "exit 3";
        #[cfg(windows)]
        let script = "exit /b 3";
        let outcome = run(&shell(script), b"{}", Duration::from_secs(10));
        assert!(matches!(outcome, HookOutcome::Failed(3)), "{outcome:?}");
    }

    #[test]
    fn a_missing_binary_is_a_spawn_error() {
        let outcome = run(&argv(&["does-not-exist-xyz"]), b"", Duration::from_secs(10));
        assert!(matches!(outcome, HookOutcome::SpawnError(_)));
    }

    #[test]
    fn a_sleeper_times_out() {
        #[cfg(unix)]
        let script = "sleep 30";
        #[cfg(windows)]
        let script = "ping -n 30 127.0.0.1 >nul";
        let started = Instant::now();
        let outcome = run(&shell(script), b"", Duration::from_secs(1));
        assert!(matches!(outcome, HookOutcome::Timeout), "{outcome:?}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn the_payload_reaches_stdin() {
        let out = std::env::temp_dir().join(format!("lore-hook-stdin-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&out);
        let payload = b"0123456789";
        #[cfg(unix)]
        let script = format!("wc -c > \"{}\"", out.display());
        #[cfg(windows)]
        let script = format!("sort > {}", out.display());
        let outcome = run(&shell(&script), payload, Duration::from_secs(10));
        assert!(matches!(outcome, HookOutcome::Passed), "{outcome:?}");
        let written = std::fs::read_to_string(&out).unwrap();
        let _ = std::fs::remove_file(&out);
        #[cfg(unix)]
        assert_eq!(written.trim(), "10");
        #[cfg(windows)]
        assert!(written.contains("0123456789"), "{written}");
    }
}
