// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Client-side `pre-commit` hook (fork-only).
//!
//! The hook is declared in `<repository root>/.lorehooks.toml`, runs in the CLI process just
//! before `lore commit` / `lore revision amend` hand the commit to the core, and receives a JSON
//! payload describing the staged change. See `docs/how-to/use-pre-commit-hooks.md`.
use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use lore::call_delegation::run_command;
use lore::interface::LoreArray;
use lore::interface::LoreEvent;
use lore::interface::LoreFileAction;
use lore::interface::LoreGlobalArgs;
use lore::interface::LoreNodeType;
use lore::interface::LoreRepositoryStatusArgs;
use lore::interface::LoreString;
use parking_lot::Mutex;
use serde::Deserialize;
use serde::Serialize;

use crate::config::config;
use crate::eprintln;
use crate::styling::CommonStyles;

/// Versioned hook configuration at the repository root.
pub const HOOKS_FILE: &str = ".lorehooks.toml";
/// Exit code of the CLI when the hook rejects the commit (or cannot run).
pub const HOOK_REJECTED_EXIT_CODE: u8 = 100;
const DEFAULT_TIMEOUT_SECS: u64 = 60;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operation {
    Commit,
    Amend,
}

impl Operation {
    pub fn name(self) -> &'static str {
        match self {
            Operation::Commit => "commit",
            Operation::Amend => "amend",
        }
    }
}

pub struct PreCommitInput<'a> {
    pub operation: Operation,
    pub message: &'a str,
    pub link: Option<&'a str>,
    pub layer: Option<&'a str>,
    /// Paths and messages, parallel arrays.
    pub link_messages: (&'a [LoreString], &'a [LoreString]),
    pub layer_messages: (&'a [LoreString], &'a [LoreString]),
    pub no_verify: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HooksFile {
    pub version: u32,
    pub pre_commit: Option<PreCommitConfig>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreCommitConfig {
    pub command: Option<Vec<String>>,
    pub command_windows: Option<Vec<String>>,
    pub command_unix: Option<Vec<String>>,
    pub timeout_secs: Option<u64>,
}

/// Parses and validates the contents of `.lorehooks.toml`.
pub fn parse_hooks_file(text: &str) -> Result<HooksFile, String> {
    let file: HooksFile = toml::from_str(text).map_err(|e| e.to_string())?;
    if file.version != 1 {
        return Err(format!(
            "unsupported version {} (only version 1 is supported)",
            file.version
        ));
    }
    if let Some(cfg) = &file.pre_commit {
        if let Some(timeout) = cfg.timeout_secs
            && !(1..=600).contains(&timeout)
        {
            return Err(format!("timeout_secs = {timeout} is outside 1..=600"));
        }
        for (name, command) in [
            ("command", &cfg.command),
            ("command_windows", &cfg.command_windows),
            ("command_unix", &cfg.command_unix),
        ] {
            if let Some(argv) = command
                && argv.first().is_none_or(|program| program.is_empty())
            {
                return Err(format!("[pre_commit] {name} must not be empty"));
            }
        }
    }
    Ok(file)
}

/// The argv to run on this OS: the per-OS override when present, else `command`.
pub fn select_command(cfg: &PreCommitConfig, windows: bool) -> Option<&[String]> {
    let specific = if windows {
        cfg.command_windows.as_deref()
    } else {
        cfg.command_unix.as_deref()
    };
    specific.or(cfg.command.as_deref())
}

/// A program containing a path separator is relative to the repository root, a bare name is left
/// to the PATH lookup.
pub fn resolve_program(argv0: &str, repo_root: &Path) -> PathBuf {
    if argv0.contains('/') || argv0.contains('\\') {
        repo_root.join(argv0)
    } else {
        PathBuf::from(argv0)
    }
}

/// `1|true|yes|on` (case-insensitive). Blank or unset is false.
pub fn env_truthy(value: Option<&str>) -> bool {
    matches!(
        value.map(|v| v.trim().to_ascii_lowercase()).as_deref(),
        Some("1" | "true" | "yes" | "on")
    )
}

/// Whether the user trusts this repository to run hooks. Entries are prefixes of the remote URL
/// or of the absolute repository root (matched on a boundary), or `*`.
pub fn is_trusted(
    entries: &[String],
    remote_url: Option<&str>,
    repo_root: &Path,
    env_trust: bool,
    windows: bool,
) -> bool {
    if env_trust {
        return true;
    }
    let normalize_path = |p: &str| {
        let p = p.replace('\\', "/");
        if windows { p.to_lowercase() } else { p }
    };
    let root = normalize_path(&repo_root.to_string_lossy());
    let remote = remote_url.map(|r| r.trim().to_lowercase());
    entries.iter().any(|entry| {
        let entry = entry.trim();
        if entry.is_empty() {
            return false;
        }
        if entry == "*" {
            return true;
        }
        if let Some(remote) = &remote
            && prefix_at_boundary(remote, &entry.to_lowercase(), &['/', ':', '?', '#'])
        {
            return true;
        }
        prefix_at_boundary(&root, &normalize_path(entry), &['/'])
    })
}

/// `value` starts with `prefix`, and the prefix ends where a component ends: it carries its own
/// trailing `/`, or the next character in `value` is one of `boundaries` (or there is none).
fn prefix_at_boundary(value: &str, prefix: &str, boundaries: &[char]) -> bool {
    let Some(rest) = value.strip_prefix(prefix) else {
        return false;
    };
    prefix.ends_with('/') || rest.chars().next().is_none_or(|c| boundaries.contains(&c))
}

pub fn action_name(action: LoreFileAction) -> &'static str {
    match action {
        LoreFileAction::Add => "add",
        LoreFileAction::Delete => "delete",
        LoreFileAction::Move => "move",
        LoreFileAction::Copy => "copy",
        LoreFileAction::Keep => "modify",
    }
}

fn node_type_name(node_type: LoreNodeType) -> &'static str {
    match node_type {
        LoreNodeType::Directory => "directory",
        LoreNodeType::File => "file",
        LoreNodeType::Link => "link",
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct HookFile {
    pub path: String,
    pub action: &'static str,
    pub r#type: &'static str,
    pub from_path: Option<String>,
    pub merged: bool,
}

#[derive(Debug, Serialize)]
pub struct Payload {
    pub version: u32,
    pub hook: &'static str,
    pub operation: &'static str,
    pub repository_root: String,
    pub repository_id: String,
    pub remote_url: Option<String>,
    pub branch: String,
    pub revision: String,
    pub revision_number: u64,
    pub merge: bool,
    pub dry_run: bool,
    pub force: bool,
    pub interactive: bool,
    pub message: String,
    pub link: Option<String>,
    pub layer: Option<String>,
    pub link_messages: BTreeMap<String, String>,
    pub layer_messages: BTreeMap<String, String>,
    pub files: Vec<HookFile>,
}

impl Payload {
    /// Sorts the files by path so the output is stable.
    pub fn sort_files(&mut self) {
        self.files.sort_by(|a, b| a.path.cmp(&b.path));
    }
}

#[derive(Debug)]
pub enum HookOutcome {
    Passed,
    /// Exit code, or -1 when killed by a signal.
    Failed(i32),
    Timeout,
    SpawnError(std::io::Error),
}

/// Spawns `argv` with the environment and working directory, feeds `payload` to its stdin and
/// waits for it up to `timeout`. Touches nothing of Lore, so it can be tested on its own.
pub fn execute(
    argv: &[String],
    cwd: &Path,
    env: &[(String, String)],
    payload: &[u8],
    timeout: Duration,
    stdout_to_stderr: bool,
) -> HookOutcome {
    let Some((program, args)) = argv.split_first() else {
        return HookOutcome::SpawnError(std::io::Error::other("empty command"));
    };
    let mut command = Command::new(resolve_program(program, cwd));
    command.args(args).current_dir(cwd).stdin(Stdio::piped());
    for (key, value) in env {
        command.env(key, value);
    }
    if stdout_to_stderr {
        command.stdout(Stdio::from(std::io::stderr()));
    } else {
        command.stdout(Stdio::inherit());
    }
    command.stderr(Stdio::inherit());

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => return HookOutcome::SpawnError(e),
    };
    if let Some(mut stdin) = child.stdin.take() {
        let bytes = payload.to_vec();
        // A hook that never reads stdin makes this fail with BrokenPipe, which is fine. The thread
        // is detached: it ends when the child's pipe closes.
        std::thread::spawn(move || {
            let _ = stdin.write_all(&bytes);
        });
    }

    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return if status.success() {
                    HookOutcome::Passed
                } else {
                    HookOutcome::Failed(status.code().unwrap_or(-1))
                };
            }
            Ok(None) => {}
            Err(e) => return HookOutcome::SpawnError(e),
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return HookOutcome::Timeout;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Removes the payload file when dropped, on every exit path.
struct TempPayload(PathBuf);

impl Drop for TempPayload {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[derive(Deserialize)]
struct RepositoryConfig {
    remote_url: Option<String>,
}

fn read_remote_url(root: &Path) -> Option<String> {
    [".lore", ".urc"].iter().find_map(|dir| {
        let text = std::fs::read_to_string(root.join(dir).join("config.toml")).ok()?;
        toml::from_str::<RepositoryConfig>(&text).ok()?.remote_url
    })
}

fn warn_json(value: serde_json::Value) {
    eprintln!("{value}");
}

/// Entry point. `Ok(())` lets the commit proceed, `Err(code)` is the exit code of the CLI.
pub fn run_pre_commit(globals: &LoreGlobalArgs, input: PreCommitInput<'_>) -> Result<(), u8> {
    let json = config().json;
    // A hook that calls `lore commit` must not recurse.
    if std::env::var_os("LORE_HOOK").is_some() {
        return Ok(());
    }
    let root = std::path::absolute(globals.repository_path())
        .unwrap_or_else(|_| PathBuf::from(globals.repository_path()));
    let hooks_path = root.join(HOOKS_FILE);
    if !hooks_path.is_file() {
        return Ok(());
    }

    let env_var = |name: &str| std::env::var(name).ok();
    if input.no_verify || env_truthy(env_var("LORE_NO_VERIFY").as_deref()) {
        if !json {
            eprintln!(
                "{}pre-commit hook skipped (--no-verify){}",
                anstyle::Style::new().effects(anstyle::Effects::DIMMED),
                anstyle::Reset
            );
        }
        return Ok(());
    }

    let remote_url = read_remote_url(&root);
    if !is_trusted(
        &config().hooks.trusted,
        remote_url.as_deref(),
        &root,
        env_truthy(env_var("LORE_TRUST_HOOKS").as_deref()),
        cfg!(windows),
    ) {
        let root_display = root.to_string_lossy().replace('\\', "/");
        if json {
            warn_json(serde_json::json!({
                "warning": "pre-commit hook not trusted",
                "repository_root": root_display,
                "remote_url": remote_url,
            }));
        } else {
            let target = remote_url.as_deref().unwrap_or(&root_display);
            let cli_toml = lore::interface::user_directory().map_or_else(
                || "your user-level cli.toml".to_string(),
                |dir| dir.join("cli.toml").display().to_string(),
            );
            eprintln!(
                "warning: {HOOKS_FILE} defines a pre-commit hook but this repository is not trusted, so the hook was skipped.\nTo trust it, add this to {cli_toml}:\n\n[hooks]\ntrusted = [\"{target}\"]\n"
            );
        }
        return Ok(());
    }

    let text = match std::fs::read_to_string(&hooks_path) {
        Ok(text) => text,
        Err(e) => return Err(reject_message(&format!("cannot read {HOOKS_FILE}: {e}"))),
    };
    let file = match parse_hooks_file(&text) {
        Ok(file) => file,
        Err(e) => return Err(reject_message(&format!("invalid {HOOKS_FILE}: {e}"))),
    };
    let Some(cfg) = file.pre_commit else {
        return Ok(());
    };
    let Some(argv) = select_command(&cfg, cfg!(windows)) else {
        return Err(reject_message(&format!(
            "{HOOKS_FILE}: [pre_commit] has no command for this OS"
        )));
    };
    let timeout = Duration::from_secs(cfg.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS));

    let mut payload = gather_payload(globals, &input, &root, remote_url)?;
    payload.sort_files();
    let bytes = match serde_json::to_vec(&payload) {
        Ok(bytes) => bytes,
        Err(e) => return Err(reject_message(&format!("cannot serialize payload: {e}"))),
    };

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let payload_path = std::env::temp_dir().join(format!(
        "lore-pre-commit-{}-{nanos}.json",
        std::process::id()
    ));
    if let Err(e) = std::fs::write(&payload_path, &bytes) {
        return Err(reject_message(&format!("cannot write payload file: {e}")));
    }
    let _cleanup = TempPayload(payload_path.clone());

    let flag = |b: bool| if b { "1" } else { "0" }.to_string();
    let env = vec![
        ("LORE_HOOK".to_string(), "pre-commit".to_string()),
        (
            "LORE_HOOK_PAYLOAD".to_string(),
            payload_path.to_string_lossy().into_owned(),
        ),
        (
            "LORE_HOOK_OPERATION".to_string(),
            input.operation.name().to_string(),
        ),
        (
            "LORE_REPOSITORY_ROOT".to_string(),
            root.to_string_lossy().into_owned(),
        ),
        ("LORE_BRANCH".to_string(), payload.branch.clone()),
        (
            "LORE_HOOK_INTERACTIVE".to_string(),
            flag(payload.interactive),
        ),
        ("LORE_HOOK_JSON".to_string(), flag(json)),
        ("LORE_HOOK_DRY_RUN".to_string(), flag(payload.dry_run)),
    ];

    match execute(argv, &root, &env, &bytes, timeout, json) {
        HookOutcome::Passed => Ok(()),
        HookOutcome::Failed(code) => {
            if json {
                eprintln!(
                    "{}",
                    serde_json::json!({
                        "error": "pre-commit hook rejected the commit",
                        "hook": "pre-commit",
                        "exit_code": code,
                    })
                );
            } else {
                eprintln!(
                    "{}pre-commit hook failed (exit code {code}); commit aborted. Bypass with --no-verify.{}",
                    CommonStyles::FAILURE,
                    anstyle::Reset
                );
            }
            Err(HOOK_REJECTED_EXIT_CODE)
        }
        HookOutcome::Timeout => Err(reject_message(&format!(
            "pre-commit hook timed out after {}s",
            timeout.as_secs()
        ))),
        HookOutcome::SpawnError(e) => Err(reject_message(&format!(
            "pre-commit hook could not be started: {}: {e}",
            argv[0]
        ))),
    }
}

fn reject_message(message: &str) -> u8 {
    if config().json {
        eprintln!(
            "{}",
            serde_json::json!({ "error": message, "hook": "pre-commit" })
        );
    } else {
        eprintln!("{}{message}{}", CommonStyles::FAILURE, anstyle::Reset);
    }
    HOOK_REJECTED_EXIT_CODE
}

#[derive(Default)]
struct StatusData {
    repository: String,
    branch: String,
    revision: String,
    revision_number: u64,
    merge: bool,
    files: Vec<HookFile>,
}

/// One staged-only status query with a private callback, so `--json` does not print its events.
fn gather_payload(
    globals: &LoreGlobalArgs,
    input: &PreCommitInput<'_>,
    root: &Path,
    remote_url: Option<String>,
) -> Result<Payload, u8> {
    let amend = input.operation == Operation::Amend;
    let args = LoreRepositoryStatusArgs {
        staged: u8::from(!amend),
        scan: 0,
        check_dirty: 0,
        reset: 0,
        sync_point: 0,
        revision_only: u8::from(amend),
        count: 0,
        paths: LoreArray::default(),
    };
    let data = Arc::new(Mutex::new(StatusData::default()));
    let sink = data.clone();
    let callback: lore::interface::LoreEventCallback =
        Some(Box::new(move |event: &LoreEvent| match event {
            LoreEvent::RepositoryStatusRevision(d) => {
                let mut data = sink.lock();
                data.repository = d.repository.to_string();
                data.branch = d.branch_name.as_str().to_string();
                data.revision = d.revision.to_string();
                data.revision_number = d.revision_number;
                data.merge = !d.revision_merged.is_zero();
            }
            LoreEvent::RepositoryStatusFile(d)
                if d.flag_staged != 0 && d.flag_conflict_unresolved == 0 =>
            {
                let from = d.from_path.as_str();
                let moved = matches!(d.action, LoreFileAction::Move | LoreFileAction::Copy);
                sink.lock().files.push(HookFile {
                    path: d.path.as_str().replace('\\', "/"),
                    action: action_name(d.action),
                    r#type: node_type_name(d.r#type),
                    from_path: (moved && !from.is_empty()).then(|| from.replace('\\', "/")),
                    merged: d.flag_merged != 0,
                });
            }
            _ => {}
        }));
    match run_command(globals.clone(), args.into(), callback) {
        0 => {}
        status => return Err(status as u8),
    }
    let status = std::mem::take(&mut *data.lock());

    let pairs = |(paths, messages): (&[LoreString], &[LoreString])| {
        paths
            .iter()
            .zip(messages)
            .map(|(p, m)| (p.as_str().to_string(), m.as_str().to_string()))
            .collect::<BTreeMap<_, _>>()
    };
    Ok(Payload {
        version: 1,
        hook: "pre-commit",
        operation: input.operation.name(),
        repository_root: root.to_string_lossy().into_owned(),
        repository_id: status.repository,
        remote_url,
        branch: status.branch,
        revision: status.revision,
        revision_number: status.revision_number,
        merge: status.merge,
        dry_run: globals.dry_run(),
        force: globals.force(),
        interactive: !config().non_interactive && !config().json,
        message: input.message.to_string(),
        link: input.link.map(str::to_string),
        layer: input.layer.map(str::to_string),
        link_messages: pairs(input.link_messages),
        layer_messages: pairs(input.layer_messages),
        files: status.files,
    })
}
