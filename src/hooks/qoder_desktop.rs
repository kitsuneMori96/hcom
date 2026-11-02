//! Qoder Desktop integration.
//!
//! Desktop agents run inside the Qoder GUI app, outside hcom's PTY, so the
//! daemon cannot wake them by typing `<hcom>`. Instead hcom installs one
//! persistent Stop hook into `~/.qoder/settings.json` (Claude-style hook
//! layout) with `asyncRewake: true`. The hook process (`hcom qoder-desktop-stop`)
//! resolves the session's hcom identity, registers it when new, then watches
//! the inbox; when peer mail arrives it exits 2 with the mail on stderr, which
//! the Desktop app turns into a system reminder that wakes the idle model.
//!
//! The file this replaces (`~/.qoder/hooks/hcom-rewake.py`) predates the Rust
//! integration; its session map and legacy command entries are still honored.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::db::{HcomDb, Message};
use crate::hooks::{HookPayload, runtime};
use crate::instance_binding;
use crate::instance_names;
use crate::instances;
use crate::log;
use crate::paths;
use crate::runtime_env;
use crate::shared::time::now_epoch_i64;

pub const DESKTOP_STOP_COMMAND: &str = "qoder-desktop-stop";
const DESKTOP_TOOL: &str = "qoder-desktop";
const REWAKE_ENTRY_NAME: &str = "hcom-rewake";
/// The app enforces this on the hook process; keep it above the wait budget
/// so the watcher decides its own deadline and exits cleanly.
const HOOK_TIMEOUT_SECS: u64 = 1860;
const WAIT_ENV: &str = "HCOM_REWAKE_WAIT";
const DEFAULT_WAIT_SECS: u64 = 1800;
const POLL_TICK: Duration = Duration::from_millis(2000);
const HEARTBEAT_EVERY: Duration = Duration::from_millis(15_000);
const SIGTERM_GRACE_TICK: Duration = Duration::from_millis(250);

// ── Settings file location ──────────────────────────────────────────────

pub fn get_qoder_desktop_settings_path() -> PathBuf {
    if let Ok(dir) = std::env::var("QODER_CONFIG_DIR")
        && !dir.is_empty()
    {
        return PathBuf::from(dir).join("settings.json");
    }
    runtime_env::tool_home()
        .join(".qoder")
        .join("settings.json")
}

// ── Hook install / verify / remove (persistent-tool installer surface) ──

#[derive(Debug, thiserror::Error)]
pub enum SetupError {
    #[error("existing Qoder settings at {} could not be read: {source}", path.display())]
    ExistingReadFailed {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("existing Qoder settings at {} is not valid JSON: {source}", path.display())]
    ExistingParseFailed {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("existing Qoder settings at {} must be a JSON object", path.display())]
    ExistingRootNotObject { path: PathBuf },
    #[error("failed to create settings directory {}: {source}", path.display())]
    DirCreateFailed {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("JSON serialization failed: {0}")]
    SerializationFailed(#[from] serde_json::Error),
    #[error("atomic write to {} failed: {source}", path.display())]
    AtomicWriteFailed {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("post-write Qoder Desktop hook verification failed for {}", .0.display())]
    PostWriteVerifyFailed(PathBuf),
}

fn build_desktop_hook_command() -> String {
    let mut parts = runtime_env::get_hcom_prefix();
    parts.push(DESKTOP_STOP_COMMAND.to_string());
    parts.join(" ")
}

fn is_hcom_desktop_command(command: &str) -> bool {
    // The pre-Rust bridge ran the same watcher as an external py script;
    // treat its entries as ours so a re-install replaces them instead of
    // stacking a second watcher next to the legacy one.
    runtime::is_hcom_command(command, &[DESKTOP_STOP_COMMAND]) || command.contains("hcom-rewake.py")
}

fn is_hcom_entry(entry: &Value) -> bool {
    entry
        .get("command")
        .and_then(Value::as_str)
        .is_some_and(is_hcom_desktop_command)
}

fn expected_stop_entry() -> Value {
    json!({
        "type": "command",
        "command": build_desktop_hook_command(),
        "name": REWAKE_ENTRY_NAME,
        "timeout": HOOK_TIMEOUT_SECS,
        "asyncRewake": true,
        "rewakeMessage": "[hcom] 来信唤醒",
        "rewakeSummary": "hcom mail arrived for this session",
    })
}

/// Drop hcom-owned entries from Claude-style Stop hook groups, then drop
/// groups left without hooks. Groups the user wrote themselves are kept.
fn strip_desktop_hooks(groups: &mut Vec<Value>) {
    for group in groups.iter_mut() {
        if let Some(list) = group.get_mut("hooks").and_then(Value::as_array_mut) {
            list.retain(|entry| !is_hcom_entry(entry));
        }
    }
    groups.retain(|group| match group.get("hooks").and_then(Value::as_array) {
        Some(list) => !list.is_empty(),
        None => true,
    });
}

fn merge_hcom_hook(root: &mut Value) {
    if !root.is_object() {
        *root = json!({});
    }
    let obj = root.as_object_mut().unwrap();
    let hooks = obj.entry("hooks".to_string()).or_insert_with(|| json!({}));
    if !hooks.is_object() {
        *hooks = json!({});
    }
    let hooks = hooks.as_object_mut().unwrap();
    let groups = hooks.entry("Stop".to_string()).or_insert_with(|| json!([]));
    if !groups.is_array() {
        *groups = json!([]);
    }
    let groups = groups.as_array_mut().unwrap();
    strip_desktop_hooks(groups);
    groups.push(json!({ "hooks": [expected_stop_entry()] }));
}

fn remove_hcom_hook(root: &mut Value) {
    let Some(hooks) = root
        .get_mut("hooks")
        .and_then(Value::as_object_mut)
        .and_then(|hooks| hooks.get_mut("Stop"))
    else {
        return;
    };
    let Some(groups) = hooks.as_array_mut() else {
        return;
    };
    strip_desktop_hooks(groups);
}

fn read_settings(path: &Path) -> Result<Value, SetupError> {
    if !path.exists() {
        return Ok(json!({}));
    }
    let content =
        std::fs::read_to_string(path).map_err(|source| SetupError::ExistingReadFailed {
            path: path.to_path_buf(),
            source,
        })?;
    let value = serde_json::from_str::<Value>(&content).map_err(|source| {
        SetupError::ExistingParseFailed {
            path: path.to_path_buf(),
            source,
        }
    })?;
    if !value.is_object() {
        return Err(SetupError::ExistingRootNotObject {
            path: path.to_path_buf(),
        });
    }
    Ok(value)
}

fn write_settings(path: &Path, value: &Value) -> Result<(), SetupError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| SetupError::DirCreateFailed {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    let content = serde_json::to_string_pretty(value)?;
    paths::atomic_write_io(path, &content).map_err(|source| SetupError::AtomicWriteFailed {
        path: path.to_path_buf(),
        source,
    })
}

fn verify_at(path: &Path) -> bool {
    let Ok(content) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(root) = serde_json::from_str::<Value>(&content) else {
        return false;
    };
    root.get("hooks")
        .and_then(|hooks| hooks.get("Stop"))
        .and_then(Value::as_array)
        .is_some_and(|groups| {
            groups.iter().any(|group| {
                group
                    .get("hooks")
                    .and_then(Value::as_array)
                    .is_some_and(|list| {
                        list.iter().any(|entry| {
                            // Any hcom-basename exe path is accepted: dev
                            // installs may pin an absolute binary. Legacy
                            // py-bridge entries fail this check on purpose.
                            entry
                                .get("command")
                                .and_then(Value::as_str)
                                .is_some_and(|cmd| {
                                    runtime::is_hcom_command(cmd, &[DESKTOP_STOP_COMMAND])
                                })
                                && entry.get("asyncRewake").and_then(Value::as_bool) == Some(true)
                        })
                    })
            })
        })
}

/// Install (or refresh) the asyncRewake Stop hook. `~/.qoder/settings.json`
/// belongs to the user: every other key and every other hook is preserved,
/// and the file is only ever replaced atomically with its parsed content.
pub fn try_setup_qoder_desktop_hooks() -> Result<(), SetupError> {
    let path = get_qoder_desktop_settings_path();
    let mut root = read_settings(&path)?;
    merge_hcom_hook(&mut root);
    write_settings(&path, &root)?;
    if !verify_at(&path) {
        return Err(SetupError::PostWriteVerifyFailed(path));
    }
    Ok(())
}

pub fn verify_qoder_desktop_hooks_installed() -> bool {
    verify_at(&get_qoder_desktop_settings_path())
}

pub fn remove_qoder_desktop_hooks() -> bool {
    let path = get_qoder_desktop_settings_path();
    if !path.exists() {
        return true;
    }
    match read_settings(&path) {
        Ok(mut root) => {
            remove_hcom_hook(&mut root);
            write_settings(&path, &root).is_ok()
        }
        Err(_) => false,
    }
}

// ── Rewake watcher (`hcom qoder-desktop-stop`) ──────────────────────────

pub fn dispatch_qoder_desktop_hook(hook_name: &str) -> i32 {
    match hook_name {
        DESKTOP_STOP_COMMAND => run_rewake(),
        _ => 0,
    }
}

fn run_rewake() -> i32 {
    let raw: Value = match serde_json::from_reader(std::io::stdin().lock()) {
        Ok(value) => value,
        Err(_) => json!({}),
    };
    let db = match HcomDb::open() {
        Ok(db) => db,
        Err(err) => {
            log::log_warn("hooks", "qoder_desktop.db_error", &err.to_string());
            return 0;
        }
    };
    let shutdown = Arc::new(AtomicBool::new(false));
    crate::sys::signal::register_term(&shutdown);

    let payload = HookPayload::from_qoder(DESKTOP_STOP_COMMAND, raw);
    let Some(name) = resolve_watched_name(&db, &payload) else {
        return 0;
    };
    // hcom-launched CLI rows (tool = qoder etc.) keep the daemon's own wake
    // paths; watching them here would steal their queue.
    if !is_desktop_owned(&db, &name) {
        return 0;
    }
    if already_watching(&name) {
        return 0;
    }
    let Some(_guard) = WatchGuard::acquire(&name) else {
        return 0;
    };

    let budget = wait_budget_secs();
    log::log_info(
        "hooks",
        "qoder_desktop.rewake_start",
        &format!("name={name} budget={budget}s"),
    );
    let delivered = wait_for_mail(&db, &name, budget, &shutdown);
    // Delivered mail exits 2: asyncRewake turns our stderr into the reminder.
    if delivered { 2 } else { 0 }
}

fn resolve_watched_name(db: &HcomDb, payload: &HookPayload) -> Option<String> {
    if let Some(name) = std::env::var("HCOM_NAME").ok().filter(|s| !s.is_empty()) {
        return Some(name);
    }
    let session_id = payload
        .session_id
        .as_deref()
        .filter(|s| !s.is_empty() && *s != "no-session")?;
    if let Some(name) = instance_by_session(db, session_id) {
        return Some(name);
    }
    if let Some(name) = legacy_session_map(session_id)
        && db.get_instance_full(&name).ok().flatten().is_some()
    {
        return Some(name);
    }
    register_desktop_instance(db, session_id, payload)
}

fn instance_by_session(db: &HcomDb, session_id: &str) -> Option<String> {
    db.conn()
        .prepare_cached("SELECT name FROM instances WHERE session_id = ?1 LIMIT 1")
        .ok()?
        .query_row(rusqlite::params![session_id], |row| row.get::<_, String>(0))
        .ok()
}

/// session→name map written by the pre-Rust py bridge; read-only support so
/// sessions registered before this integration keep their identity.
fn legacy_session_map(session_id: &str) -> Option<String> {
    let content = std::fs::read_to_string(paths::hcom_path(&["desktop-sessions.json"])).ok()?;
    let map: Value = serde_json::from_str(&content).ok()?;
    map.get(session_id)?
        .as_str()
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
}

fn register_desktop_instance(
    db: &HcomDb,
    session_id: &str,
    payload: &HookPayload,
) -> Option<String> {
    let name = instance_names::generate_unique_name(db).ok()?;
    let transcript = payload.transcript_path.as_deref().filter(|s| !s.is_empty());
    let cwd = payload.raw.get("cwd").and_then(Value::as_str);
    let created = instance_binding::initialize_instance_in_position_file(
        db,
        &name,
        Some(session_id),
        None,
        None,
        None,
        transcript,
        Some(DESKTOP_TOOL),
        false,
        None,
        None,
        None,
        None,
        cwd,
    );
    if !created {
        return None;
    }
    log::log_info(
        "hooks",
        "qoder_desktop.registered",
        &format!("name={name} session_id={session_id}"),
    );
    Some(name)
}

fn is_desktop_owned(db: &HcomDb, name: &str) -> bool {
    db.get_instance_full(name)
        .ok()
        .flatten()
        .is_some_and(|row| matches!(row.tool.as_str(), "adhoc" | "ad-hoc" | DESKTOP_TOOL))
}

fn watch_dir() -> PathBuf {
    paths::hcom_path(&["rewake"])
}

fn pid_path(name: &str) -> PathBuf {
    watch_dir().join(format!("{name}.pid"))
}

fn watcher_alive(pid: u32) -> bool {
    if !crate::sys::process::is_alive(pid) {
        return false;
    }
    #[cfg(unix)]
    {
        // PID reuse in WSL is fast, so only count a live pid running this hook.
        let Ok(cmdline) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
            return false;
        };
        String::from_utf8_lossy(&cmdline).contains(DESKTOP_STOP_COMMAND)
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn already_watching(name: &str) -> bool {
    let path = pid_path(name);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return false;
    };
    let Ok(pid) = text.trim().parse::<u32>() else {
        let _ = std::fs::remove_file(&path);
        return false;
    };
    if pid == std::process::id() {
        return false;
    }
    if watcher_alive(pid) {
        return true;
    }
    let _ = std::fs::remove_file(&path);
    false
}

struct WatchGuard {
    path: PathBuf,
}

impl WatchGuard {
    fn acquire(name: &str) -> Option<Self> {
        let dir = watch_dir();
        if std::fs::create_dir_all(&dir).is_err() {
            return None;
        }
        let path = dir.join(format!("{name}.pid"));
        if std::fs::write(&path, std::process::id().to_string()).is_err() {
            return None;
        }
        Some(Self { path })
    }
}

impl Drop for WatchGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn wait_budget_secs() -> u64 {
    std::env::var(WAIT_ENV)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_WAIT_SECS)
}

fn heartbeat(db: &HcomDb, name: &str) {
    let mut updates = serde_json::Map::new();
    updates.insert("last_stop".into(), json!(now_epoch_i64()));
    instances::update_instance_position(db, name, &updates);
}

fn wait_for_mail(db: &HcomDb, name: &str, budget_secs: u64, shutdown: &AtomicBool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(budget_secs);
    let mut last_heartbeat = Instant::now();
    // Claim the row as listening: a `new`-context registration would
    // otherwise be finalized launch_failed by the placeholder sweeper, and
    // `hcom send` refuses to DM launch_failed instances.
    let _ = db.set_status(name, crate::shared::ST_LISTENING, "");
    heartbeat(db, name);
    loop {
        if shutdown.load(Ordering::Relaxed) {
            return false;
        }
        // Session ended or the row was reaped: stop watching, stay exit-0.
        if db.get_instance_full(name).ok().flatten().is_none() {
            return false;
        }
        let messages = db.get_unread_messages(name);
        if !messages.is_empty() {
            return deliver_mail(db, name, &messages);
        }
        let now = Instant::now();
        if now >= deadline {
            return false;
        }
        if now - last_heartbeat >= HEARTBEAT_EVERY {
            heartbeat(db, name);
            last_heartbeat = now;
        }
        // Tick in short sleeps so SIGTERM ends the watch promptly.
        let tick_end = Instant::now() + POLL_TICK;
        while Instant::now() < tick_end && !shutdown.load(Ordering::Relaxed) {
            std::thread::sleep(SIGTERM_GRACE_TICK);
        }
    }
}

/// Port of the py bridge's wake text, kept byte-compatible in shape.
fn format_rewake_mail(name: &str, messages: &[Message]) -> String {
    let mut out = format!("[hcom] {} 封来信\n", messages.len());
    for msg in messages {
        out.push_str(&format!("── @{}: {}\n", msg.from, msg.text));
    }
    out.push_str(&format!(
        "回信: export PATH=\"$HOME/.local/bin:$PATH\"; {} send @<from> --name {name} -- \"<正文>\"\n",
        runtime_env::build_hcom_command()
    ));
    out
}

fn deliver_mail(db: &HcomDb, name: &str, messages: &[Message]) -> bool {
    let text = format_rewake_mail(name, messages);
    let written = {
        let mut stderr = std::io::stderr().lock();
        stderr
            .write_all(text.as_bytes())
            .and_then(|_| stderr.flush())
    };
    if let Err(err) = written {
        // Nothing was acknowledged: the mail stays unread for the next Stop.
        log::log_warn("hooks", "qoder_desktop.write_failed", &err.to_string());
        return false;
    }
    if let Some(id) = messages.last().and_then(|m| m.event_id)
        && let Err(err) = db.advance_instance_cursor(name, id)
    {
        log::log_warn("hooks", "qoder_desktop.cursor_failed", &err.to_string());
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings_with(groups: Vec<Value>) -> Value {
        json!({
            "ui": { "language": "zh-CN" },
            "mcpServers": { "secret": { "headers": { "Authorization": "Bearer redacted" } } },
            "hooks": { "Stop": groups }
        })
    }

    #[test]
    fn merge_installs_once_and_preserves_user_content() {
        let mut root = settings_with(vec![json!({
            "hooks": [{ "type": "command", "command": "my-other-hook.sh" }]
        })]);
        merge_hcom_hook(&mut root);
        merge_hcom_hook(&mut root);

        assert_eq!(root["ui"]["language"], json!("zh-CN"));
        assert!(
            root["mcpServers"]["secret"]["headers"]["Authorization"]
                .as_str()
                .unwrap()
                .starts_with("Bearer ")
        );
        let groups = root["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(groups.len(), 2, "idempotent merge keeps one user group");
        let user_group_commands: Vec<_> = groups[0]["hooks"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|e| e["command"].as_str())
            .collect();
        assert_eq!(user_group_commands, vec!["my-other-hook.sh"]);
        let hcom_entries: Vec<_> = groups
            .iter()
            .flat_map(|g| g["hooks"].as_array().cloned().unwrap_or_default())
            .filter(is_hcom_entry)
            .collect();
        assert_eq!(hcom_entries.len(), 1);
        assert_eq!(hcom_entries[0]["asyncRewake"], json!(true));
        assert_eq!(hcom_entries[0]["timeout"], json!(HOOK_TIMEOUT_SECS));
    }

    #[test]
    fn merge_replaces_legacy_py_bridge_entry() {
        let mut root = settings_with(vec![json!({
            "hooks": [{
                "type": "command",
                "command": "python3 /home/u/.qoder/hooks/hcom-rewake.py",
                "name": "hcom-rewake"
            }]
        })]);
        merge_hcom_hook(&mut root);
        let groups = root["hooks"]["Stop"].as_array().unwrap();
        let commands: Vec<_> = groups
            .iter()
            .flat_map(|g| g["hooks"].as_array().cloned().unwrap_or_default())
            .filter_map(|e| e["command"].as_str().map(String::from))
            .collect();
        assert_eq!(commands.len(), 1, "legacy entry replaced, not stacked");
        assert!(commands[0].ends_with(DESKTOP_STOP_COMMAND));
    }

    #[test]
    fn remove_strips_only_hcom_entries() {
        let other_command = format!("hcom {}", DESKTOP_STOP_COMMAND);
        let mut root = settings_with(vec![
            json!({ "hooks": [{ "type": "command", "command": "my-other-hook.sh" }] }),
            json!({ "hooks": [
                { "type": "command", "command": other_command },
                { "type": "command", "command": "python3 x/hcom-rewake.py" }
            ] }),
        ]);
        merge_hcom_hook(&mut root);
        remove_hcom_hook(&mut root);
        let groups = root["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(groups.len(), 1);
        let commands: Vec<_> = groups[0]["hooks"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|e| e["command"].as_str())
            .collect();
        assert_eq!(commands, vec!["my-other-hook.sh"]);
        assert_eq!(root["ui"]["language"], json!("zh-CN"));
    }

    #[test]
    fn keep_group_without_hooks_array() {
        let mut root = settings_with(vec![json!({ "matcher": "bash" })]);
        merge_hcom_hook(&mut root);
        let groups = root["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(groups.len(), 2, "foreign-shaped group survives");
    }

    #[test]
    fn verify_accepts_absolute_hcom_path_and_rejects_legacy_py() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let root = settings_with(vec![json!({
            "hooks": [{
                "type": "command",
                "command": "/home/u/.local/lib/hcom-fork/hcom qoder-desktop-stop",
                "name": "hcom-rewake",
                "asyncRewake": true
            }]
        })]);
        write_settings(&path, &root).unwrap();
        assert!(verify_at(&path), "absolute hcom path verifies as installed");

        let root = settings_with(vec![json!({
            "hooks": [{
                "type": "command",
                "command": "python3 /home/u/.qoder/hooks/hcom-rewake.py",
                "name": "hcom-rewake",
                "asyncRewake": true
            }]
        })]);
        write_settings(&path, &root).unwrap();
        assert!(!verify_at(&path), "legacy py entry is not the Rust watcher");
    }

    #[test]
    fn setup_verify_roundtrip_on_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(
            &path,
            serde_json::to_string_pretty(&settings_with(vec![])).unwrap(),
        )
        .unwrap();
        assert!(!verify_at(&path));

        let mut root = read_settings(&path).unwrap();
        merge_hcom_hook(&mut root);
        write_settings(&path, &root).unwrap();
        assert!(verify_at(&path));
    }

    #[test]
    fn wake_text_shape_matches_the_py_protocol() {
        let messages = vec![
            Message {
                from: "nibe".into(),
                text: "first line".into(),
                intent: None,
                thread: None,
                event_id: Some(10),
                timestamp: None,
                delivered_to: None,
                bundle_id: None,
                relay_ref: None,
            },
            Message {
                from: "bono".into(),
                text: "second".into(),
                intent: Some("task".into()),
                thread: None,
                event_id: Some(11),
                timestamp: None,
                delivered_to: None,
                bundle_id: None,
                relay_ref: None,
            },
        ];
        let text = format_rewake_mail("moba", &messages);
        assert!(text.starts_with("[hcom] 2 封来信\n"));
        assert!(text.contains("── @nibe: first line\n"));
        assert!(text.contains("── @bono: second\n"));
        assert!(text.contains("send @<from> --name moba"));
    }
}
