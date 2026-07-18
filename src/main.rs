//! ctxguard — context-window budget enforcer for AI coding agents.
//!
//! Subcommands:
//!   parse <file.jsonl>     Parse a Claude Code session JSONL, print token summary
//!   profile [--days N]     Aggregate token usage across ~/.claude/projects/
//!   run --budget N --on-full warn|compress|kill -- <cmd>
//!                         Wrap an AI agent and enforce a context budget in real time.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::channel;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use notify::{RecursiveMode, Watcher};

mod session;
mod token;

use session::TokenSummary;

#[derive(Parser, Debug)]
#[command(name = "ctxguard", version, about)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Parse a single session JSONL and print its token summary.
    Parse {
        /// Path to a session .jsonl file
        file: PathBuf,
        /// Which AI agent produced the JSONL: claude | codex
        #[arg(long, default_value = "claude", value_parser = ["claude", "codex"])]
        tool: String,
    },

    /// Aggregate token usage across recent sessions.
    Profile {
        #[arg(long, default_value_t = 7)]
        days: u32,
        /// Which AI agent to aggregate: claude | codex
        #[arg(long, default_value = "claude", value_parser = ["claude", "codex"])]
        tool: String,
        /// Group and rank by model | day | hour | file
        #[arg(long, value_parser = ["model", "day", "hour", "file"])]
        by: Option<String>,
    },

    /// Wrap an AI agent and enforce a context budget in real time.
    ///
    /// Examples:
    ///   ctxguard run --budget 80000 --on-full warn -- claude "fix the auth bug"
    ///   ctxguard run --budget 80000 --on-full compress -- claude "refactor module X"
    ///   ctxguard run --budget 80000 --on-full kill -- claude "try everything"
    Run {
        /// Token budget; triggers --on-full when effective_context crosses this.
        #[arg(long)]
        budget: u64,

        /// What to do when budget is hit:
        ///   warn     — print a clear warning to stderr, keep the child running
        ///   compress — send SIGUSR1 (Claude Code compact hook) or run `/compact`
        ///   kill     — SIGTERM the child cleanly so you can resume later
        #[arg(long, default_value = "warn", value_parser = ["warn", "compress", "kill"])]
        on_full: String,

        /// Poll interval in milliseconds for the JSONL file watcher
        #[arg(long, default_value_t = 500)]
        poll_ms: u64,

        /// Which AI agent to watch: claude | codex
        #[arg(long, default_value = "claude", value_parser = ["claude", "codex"])]
        tool: String,

        /// Path to the session .jsonl that the child will write to.
        /// If omitted, ctxguard watches the most recently modified file.
        #[arg(long)]
        session: Option<PathBuf>,

        /// Command to run (everything after `--`)
        #[arg(last = true, required = true)]
        cmd: Vec<String>,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Parse { file, tool } => {
            let summary = match tool.as_str() {
                "codex" => parse_codex_file(&file),
                _ => parse_file(&file),
            }
            .with_context(|| format!("failed to parse {}", file.display()))?;
            summary.print_human();
        }
        Cmd::Profile { days, tool, by } => {
            let summaries = match tool.as_str() {
                "codex" => codex_profile_recent(days)?,
                _ => profile_recent(days)?,
            };
            match by.as_deref() {
                Some("model") => TokenSummary::print_by(&summaries, session::ByDim::Model),
                Some("day") => TokenSummary::print_by(&summaries, session::ByDim::Day),
                Some("hour") => TokenSummary::print_by(&summaries, session::ByDim::Hour),
                Some("file") => TokenSummary::print_by(&summaries, session::ByDim::File),
                _ => TokenSummary::print_table(&summaries),
            }
        }
        Cmd::Run {
            budget,
            on_full,
            poll_ms,
            tool,
            session,
            cmd,
        } => {
            run_with_budget(budget, &on_full, poll_ms, &tool, session, cmd)?;
        }
    }
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// W2: real-time budget enforcement
// ─────────────────────────────────────────────────────────────────────────────

fn run_with_budget(
    budget: u64,
    on_full: &str,
    poll_ms: u64,
    tool: &str,
    session_override: Option<PathBuf>,
    cmd: Vec<String>,
) -> Result<()> {
    if cmd.is_empty() {
        anyhow::bail!("run: no command provided (use `--` separator)");
    }

    let session_path = match session_override {
        Some(p) => p,
        None => most_recent_session_for(tool)?
            .context("no session file found; pass --session <path>")?,
    };

    eprintln!(
        "[ctxguard] watching {} · tool={} · budget={} tokens · on_full={}",
        session_path.display(),
        tool,
        budget,
        on_full
    );

    // Spawn child process
    let (program, args) = cmd.split_first().unwrap();
    let mut child = Command::new(program)
        .args(args)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .stdin(Stdio::inherit())
        .spawn()
        .with_context(|| format!("failed to spawn {}", program))?;

    let mut triggered = false;

    // Watch the JSONL file for new assistant turns
    let (tx, rx) = channel();
    let mut watcher: notify::RecommendedWatcher = match notify::recommended_watcher(move |res| {
        let _ = tx.send(res);
    }) {
        Ok(w) => w,
        Err(e) => {
            let _ = child.kill();
            anyhow::bail!("failed to create file watcher: {}", e);
        }
    };

    let watch_dir = session_path.parent().context("session has no parent dir")?;
    watcher
        .watch(watch_dir, RecursiveMode::NonRecursive)
        .with_context(|| format!("failed to watch {}", watch_dir.display()))?;

    loop {
        // 1. Drain watcher events
        while let Ok(res) = rx.try_recv() {
            match res {
                Ok(_event) => {
                    let s = match parse_for_tool(tool, &session_path) {
                        Ok(s) => s,
                        Err(_) => continue,
                    };
                    let ctx = s.effective_context();
                    if !triggered && ctx >= budget {
                        triggered = true;
                        eprintln!(
                            "\n[ctxguard] BUDGET HIT: effective_context={} >= budget={}",
                            ctx, budget
                        );
                        match on_full {
                            "warn" => eprintln!(
                                "[ctxguard] child process continues — pass --on-full kill|compress to enforce"
                            ),
                            "compress" => {
                                eprintln!("[ctxguard] requesting compact via stdin");
                                if let Some(mut stdin) = child.stdin.take() {
                                    use std::io::Write;
                                    let _ = writeln!(stdin, "/compact");
                                    let _ = stdin.flush();
                                    child.stdin = Some(stdin);
                                }
                            }
                            "kill" => {
                                eprintln!("[ctxguard] sending SIGTERM to child (pid={:?})", child.id());
                                let _ = child.kill();
                            }
                            _ => unreachable!("validated by clap"),
                        }
                    }
                }
                Err(e) => eprintln!("[ctxguard] watch error: {:?}", e),
            }
        }

        // 2. Reap child if done
        match child.try_wait() {
            Ok(Some(status)) => {
                eprintln!("\n[ctxguard] child exited: {}", status);
                return Ok(());
            }
            Ok(None) => {}
            Err(e) => {
                eprintln!("[ctxguard] waitpid error: {}", e);
                break;
            }
        }

        // 3. Heartbeat poll — no-op for now

        thread::sleep(Duration::from_millis(poll_ms));
    }
    Ok(())
}

fn most_recent_session() -> Result<Option<PathBuf>> {
    let root = dirs_root()?;
    most_recent_session_in(&root, "")
}

fn parse_file(path: &PathBuf) -> Result<TokenSummary> {
    use std::fs::File;
    use std::io::{BufRead, BufReader};

    let f = File::open(path)?;
    let reader = BufReader::with_capacity(64 * 1024, f);

    let mut turns = 0u64;
    let mut input_tokens = 0u64;
    let mut output_tokens = 0u64;
    let mut cache_read = 0u64;
    let mut cache_write = 0u64;
    let mut first_ts: Option<String> = None;
    let mut last_ts: Option<String> = None;
    let mut model: Option<String> = None;

    for line in reader.lines() {
        let line = line?;
        if line.is_empty() {
            continue;
        }
        let val: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let ts = val
            .get("timestamp")
            .and_then(|v| v.as_str())
            .map(String::from);
        if let Some(t) = &ts {
            if first_ts.is_none() {
                first_ts = Some(t.clone());
            }
            last_ts = Some(t.clone());
        }
        if val.get("type").and_then(|v| v.as_str()) != Some("assistant") {
            continue;
        }
        let msg = match val.get("message") {
            Some(m) => m,
            None => continue,
        };
        if model.is_none() {
            if let Some(m) = msg.get("model").and_then(|v| v.as_str()) {
                model = Some(m.to_string());
            }
        }
        if let Some(usage) = msg.get("usage") {
            turns += 1;
            input_tokens += usage
                .get("input_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            output_tokens += usage
                .get("output_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            cache_read += usage
                .get("cache_read_input_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            cache_write += usage
                .get("cache_creation_input_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
        }
    }

    Ok(TokenSummary {
        file: path.to_string_lossy().to_string(),
        turns,
        input_tokens,
        output_tokens,
        cache_read_input_tokens: cache_read,
        cache_creation_input_tokens: cache_write,
        model,
        first_ts,
        last_ts,
    })
}

fn profile_recent(days: u32) -> Result<Vec<TokenSummary>> {
    let claude_root = dirs_root()?;
    let mut out = Vec::new();

    for entry in walkdir::WalkDir::new(&claude_root)
        .max_depth(3)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let p = entry.path();
        if p.extension().and_then(|s| s.to_str()) != Some("jsonl") {
            continue;
        }
        if p.to_string_lossy().contains("subagents") {
            continue;
        }
        // Best-effort mtime check first (cheap), but also fall back to the
        // session's internal first_ts — archived Codex sessions are dated
        // by archive time, not session time, which can be much older.
        let modified = entry.metadata().ok().and_then(|m| m.modified().ok());
        let mtime_recent = modified
            .map(|m| m.elapsed().map(|e| e.as_secs() <= (days as u64) * 86400).unwrap_or(true))
            .unwrap_or(true);
        if let Ok(s) = parse_file(&p.to_path_buf()) {
            // Use session's own first_ts when available — that's the real session age.
            let session_recent = s
                .first_ts
                .as_deref()
                .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
                .map(|dt| {
                    let age = chrono::Utc::now().signed_duration_since(dt);
                    age.num_seconds() <= (days as i64) * 86400
                })
                .unwrap_or(mtime_recent);
            if session_recent {
                out.push(s);
            }
        }
    }
    Ok(out)
}

fn dirs_root() -> Result<PathBuf> {
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .context("neither USERPROFILE nor HOME is set")?;
    let mut p = PathBuf::from(home);
    p.push(".claude");
    p.push("projects");
    Ok(p)
}

// Silence unused warning on Windows where Child::stdin behaves differently
#[allow(dead_code)]
fn _unused(_: &mut Child) {}

// ─────────────────────────────────────────────────────────────────────────────
// Codex adapter
// ─────────────────────────────────────────────────────────────────────────────

fn parse_for_tool(tool: &str, path: &PathBuf) -> Result<TokenSummary> {
    match tool {
        "codex" => parse_codex_file(path),
        _ => parse_file(path),
    }
}

/// Parse a Codex CLI rollout JSONL.
/// Schema (per .codex/archived_sessions/rollout-<timestamp>-<uuid>.jsonl):
///   - session_meta: 1 line, contains payload.id and payload.cwd
///   - turn_context: model + instructions per turn
///   - response_item / message / function_call: per-step tool/assistant I/O
///   - event_msg / token_count: contains payload.info.{total_token_usage,
///       last_token_usage, model_context_window} (often info=null at start)
fn parse_codex_file(path: &PathBuf) -> Result<TokenSummary> {
    use std::fs::File;
    use std::io::{BufRead, BufReader};

    let f = File::open(path)?;
    let reader = BufReader::with_capacity(64 * 1024, f);

    let mut turns = 0u64;
    let mut input_tokens = 0u64;
    let mut output_tokens = 0u64;
    let mut cache_read = 0u64;
    let mut cache_write = 0u64;
    let mut first_ts: Option<String> = None;
    let mut last_ts: Option<String> = None;
    let mut model: Option<String> = None;
    let mut model_context_window: Option<u64> = None;

    for line in reader.lines() {
        let line = line?;
        if line.is_empty() {
            continue;
        }
        let val: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let ts = val.get("timestamp").and_then(|v| v.as_str()).map(String::from);
        if let Some(t) = &ts {
            if first_ts.is_none() {
                first_ts = Some(t.clone());
            }
            last_ts = Some(t.clone());
        }

        // turn_context carries model info
        if val.get("type").and_then(|v| v.as_str()) == Some("turn_context") {
            if let Some(p) = val.get("payload") {
                if model.is_none() {
                    if let Some(m) = p.get("model").and_then(|v| v.as_str()) {
                        model = Some(m.to_string());
                    }
                }
            }
        }

        // event_msg / token_count carries total_token_usage
        if val.get("type").and_then(|v| v.as_str()) == Some("event_msg") {
            if let Some(p) = val.get("payload") {
                if p.get("type").and_then(|v| v.as_str()) == Some("token_count") {
                    if let Some(info) = p.get("info") {
                        if !info.is_null() {
                            if let Some(tot) = info.get("total_token_usage") {
                                turns += 1;
                                input_tokens = tot.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                                output_tokens = tot.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                                cache_read = tot.get("cached_input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                            }
                            if let Some(ctx) = info.get("model_context_window").and_then(|v| v.as_u64()) {
                                model_context_window = Some(ctx);
                            }
                        }
                    }
                }
                // task_started carries initial model_context_window
                if p.get("type").and_then(|v| v.as_str()) == Some("task_started") {
                    if let Some(ctx) = p.get("model_context_window").and_then(|v| v.as_u64()) {
                        model_context_window = Some(ctx);
                    }
                }
            }
        }
    }

    // Effective context for Codex: input + cached_input (Codex has no cache_creation split)
    // We stash cache_read as effective cache, cache_write = 0.
    Ok(TokenSummary {
        file: path.to_string_lossy().to_string(),
        turns,
        input_tokens,
        output_tokens,
        cache_read_input_tokens: cache_read,
        cache_creation_input_tokens: cache_write,
        model,
        first_ts,
        last_ts,
    })
}

fn codex_root() -> Result<PathBuf> {
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .context("neither USERPROFILE nor HOME is set")?;
    let mut p = PathBuf::from(home);
    p.push(".codex");
    p.push("archived_sessions");
    Ok(p)
}

fn most_recent_session_for(tool: &str) -> Result<Option<PathBuf>> {
    match tool {
        "codex" => most_recent_session_in(&codex_root()?, "rollout-"),
        _ => most_recent_session_in(&dirs_root()?, ""),
    }
}

fn most_recent_session_in(root: &PathBuf, prefix: &str) -> Result<Option<PathBuf>> {
    let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in walkdir::WalkDir::new(root)
        .max_depth(2)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let p = entry.path();
        if p.extension().and_then(|s| s.to_str()) != Some("jsonl") {
            continue;
        }
        let fname = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if !fname.starts_with(prefix) {
            continue;
        }
        if let Ok(meta) = entry.metadata() {
            if let Ok(modified) = meta.modified() {
                if newest.as_ref().map(|(t, _)| modified > *t).unwrap_or(true) {
                    newest = Some((modified, p.to_path_buf()));
                }
            }
        }
    }
    Ok(newest.map(|(_, p)| p))
}

fn codex_profile_recent(days: u32) -> Result<Vec<TokenSummary>> {
    let root = codex_root()?;
    let mut out = Vec::new();
    for entry in walkdir::WalkDir::new(&root)
        .max_depth(2)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let p = entry.path();
        if p.extension().and_then(|s| s.to_str()) != Some("jsonl") {
            continue;
        }
        let fname = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if !fname.starts_with("rollout-") {
            continue;
        }
        // Cheap mtime check first; fall back to session's internal first_ts
        // since archived sessions often have mtime = archive time, not session time.
        let modified = entry.metadata().ok().and_then(|m| m.modified().ok());
        let mtime_recent = modified
            .map(|m| m.elapsed().map(|e| e.as_secs() <= (days as u64) * 86400).unwrap_or(true))
            .unwrap_or(true);
        if let Ok(s) = parse_codex_file(&p.to_path_buf()) {
            let session_recent = s
                .first_ts
                .as_deref()
                .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
                .map(|dt| {
                    let age = chrono::Utc::now().signed_duration_since(dt);
                    age.num_seconds() <= (days as i64) * 86400
                })
                .unwrap_or(mtime_recent);
            if session_recent {
                out.push(s);
            }
        }
    }
    Ok(out)
}
