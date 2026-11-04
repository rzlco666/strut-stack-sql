//! `strut-stack-sql doctor` (R2-07).
//!
//! Checks configuration, source connectivity and the security-relevant
//! switches, then exits with a code scripts can branch on:
//!
//! | code | meaning |
//! |---|---:|
//! | 0 | every check passed |
//! | 1 | configuration could not be loaded (or is invalid) |
//! | 2 | at least one source failed to connect |
//! | 3 | reachable, but at least one capability is degraded |
//!
//! Output is plain lines (`[OK]`, `[WARN]`, `[FAIL]`) on stdout, diagnostics
//! on stderr. No credentials are printed: URLs are redacted.

use crate::config::{load_config, Cli};

/// Run the checks and translate the result into a process exit code.
pub async fn exit_code(cli: &Cli) -> i32 {
    match run(cli).await {
        Ok(code) => code,
        Err(err) => {
            eprintln!("doctor failed: {err}");
            1
        }
    }
}

pub async fn run(cli: &Cli) -> anyhow::Result<i32> {
    println!("strut-stack-sql doctor {}", env!("CARGO_PKG_VERSION"));
    let config = match load_config(cli).await {
        Ok(config) => config,
        Err(err) => {
            println!("[FAIL] config: {err}");
            return Ok(1);
        }
    };

    println!("[OK] config: {} source(s)", config.sources.len());
    println!(
        "[OK] write mode: {:?}; full tools: {}; approvals required: {}; grants gate: {}",
        config.write_mode, config.full_tools, config.require_approval, config.allow_grants
    );
    println!(
        "[OK] default source: {} (ephemeral: {})",
        config.default_source, config.ephemeral
    );
    if config.ephemeral {
        println!("[WARN] no database configured: running on ephemeral in-memory SQLite");
    }
    if config.http_auth_enabled() {
        println!(
            "[OK] HTTP auth configured ({} token(s))",
            config.http_tokens.len()
        );
    }
    if std::env::var_os("STRUT_STACK_SQL_CURSOR_KEY").is_none() {
        println!("[WARN] STRUT_STACK_SQL_CURSOR_KEY unset: cursors are invalid after a restart (per-process key)");
    }

    let mut names: Vec<&String> = config.sources.keys().collect();
    names.sort();
    let mut failed = false;
    let mut degraded = false;
    for name in names {
        let source = match config.source(Some(name)) {
            Ok(source) => source,
            Err(err) => {
                println!("[FAIL] source {name}: {err}");
                failed = true;
                continue;
            }
        };
        match source.driver().await {
            Ok(driver) => {
                if let Err(err) = driver.ping().await {
                    println!("[FAIL] source {name}: connect failed: {err}");
                    failed = true;
                    continue;
                }
                let engine = driver.kind();
                println!("[OK] source {name}: connected ({engine:?})");
                let notes = driver.capability_notes();
                if notes.is_empty() {
                    println!("[OK] source {name}: every requested capability is effective");
                } else {
                    degraded = true;
                    for note in notes {
                        println!("[WARN] source {name}: {} — {}", note.code, note.message);
                    }
                }
            }
            Err(err) => {
                println!("[FAIL] source {name}: {err}");
                failed = true;
            }
        }
    }

    Ok(if failed {
        2
    } else if degraded {
        3
    } else {
        0
    })
}
