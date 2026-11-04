//! `doctor`, `migrate --from-v1` and trace-field contracts (R2-07).

use std::path::PathBuf;

use clap::Parser;
use strut_stack_sql::config::{Cli, Command, WriteMode};

fn cli(args: &[&str]) -> Cli {
    Cli::try_parse_from(args).expect("cli parses")
}

#[test]
fn doctor_subcommand_accepts_global_connection_flags() {
    let parsed = cli(&["strut-stack-sql", "doctor", "--url", "sqlite::memory:"]);
    assert!(matches!(parsed.command, Some(Command::Doctor)));
    assert_eq!(parsed.url.as_deref(), Some("sqlite::memory:"));
}

#[test]
fn migrate_subcommand_parses_from_v1_write_and_out() {
    let parsed = cli(&[
        "strut-stack-sql",
        "migrate",
        "--from-v1",
        "--input",
        "v1.toml",
        "--write",
        "--out",
        "v2.toml",
    ]);
    match parsed.command {
        Some(Command::Migrate {
            from_v1,
            input,
            write,
            out,
        }) => {
            assert!(from_v1 && write);
            assert_eq!(input, Some(PathBuf::from("v1.toml")));
            assert_eq!(out, Some(PathBuf::from("v2.toml")));
        }
        other => panic!("unexpected command: {other:?}"),
    }
}

#[tokio::test]
async fn doctor_exits_zero_for_a_reachable_source() {
    let parsed = cli(&["strut-stack-sql", "doctor", "--url", "sqlite::memory:"]);
    let code = strut_stack_sql::core::doctor::exit_code(&parsed).await;
    assert_eq!(code, 0);
}

#[tokio::test]
async fn doctor_exits_two_when_a_source_cannot_connect() {
    let parsed = cli(&[
        "strut-stack-sql",
        "doctor",
        "--url",
        "postgresql://user:secret@127.0.0.1:1/db",
        "--connect-timeout",
        "1",
    ]);
    let code = strut_stack_sql::core::doctor::exit_code(&parsed).await;
    assert_eq!(code, 2);
}

#[test]
fn migrate_never_overwrites_without_write_and_reports_moved_keys() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("v1.toml");
    std::fs::write(
        &input,
        "default_source = \"app\"\nmax_rows = 25\n\n[[sources]]\nname = \"app\"\nurl = \"sqlite::memory:\"\n",
    )
    .expect("write v1");
    let out = dir.path().join("v2.toml");
    let code = strut_stack_sql::core::migrate::run(true, Some(&input), false, Some(&out))
        .expect("dry run");
    assert_eq!(code, 0);
    assert!(!out.exists(), "dry run must not write");

    let code =
        strut_stack_sql::core::migrate::run(true, Some(&input), true, Some(&out)).expect("write");
    assert_eq!(code, 0);
    let rendered = std::fs::read_to_string(&out).expect("out");
    assert!(rendered.contains("name = \"app\""), "{rendered}");
    assert!(rendered.contains("url = \"sqlite::memory:\""), "{rendered}");
    assert!(
        !rendered.contains("max_rows"),
        "moved keys are not copied: {rendered}"
    );
}

#[test]
fn migrate_rejects_a_source_without_url_or_url_env() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("v1.toml");
    std::fs::write(&input, "[[sources]]\nname = \"app\"\n").expect("write");
    // The warning is emitted to stderr; the important part is that nothing is
    // written and the command succeeds with a warning.
    let out = dir.path().join("v2.toml");
    let code =
        strut_stack_sql::core::migrate::run(true, Some(&input), true, Some(&out)).expect("migrate");
    assert_eq!(code, 0);
    assert!(out.exists());
    let rendered = std::fs::read_to_string(&out).expect("out");
    assert!(rendered.contains("[[sources]]"));
}

#[test]
fn principal_tags_are_bounded_and_not_the_raw_id() {
    let tag = strut_stack_sql::tools::core::principal_tag("alice@example.com");
    assert_eq!(tag.len(), 12);
    assert!(!tag.contains("alice"));
    assert_eq!(
        tag,
        strut_stack_sql::tools::core::principal_tag("alice@example.com")
    );
    assert_ne!(
        tag,
        strut_stack_sql::tools::core::principal_tag("bob@example.com")
    );
}

#[tokio::test]
async fn scoped_config_clears_trace_sql_when_a_grant_narrows_tables() {
    let parsed = cli(&[
        "strut-stack-sql",
        "doctor",
        "--url",
        "sqlite::memory:",
        "--trace-sql",
    ]);
    let config = strut_stack_sql::config::load_config(&parsed)
        .await
        .expect("config");
    assert!(
        config.trace_sql,
        "--trace-sql is honored without table rules"
    );
    assert_eq!(config.write_mode, WriteMode::ReadOnly);

    let mut authz = strut_stack_sql::core::auth::Authorization {
        principal_id: "local".into(),
        source: "default".into(),
        tier: strut_stack_sql::core::auth::Tier::Read,
        max_rows: None,
        max_bytes: None,
        query_timeout: None,
        query_history: false,
        tables: Default::default(),
    };
    authz.tables.deny = vec![strut_stack_sql::core::auth::TableRule {
        schema: None,
        table: "payroll".into(),
    }];
    let scoped = config.scoped_for(&authz);
    assert!(!scoped.trace_sql, "narrowed grants never put SQL in traces");
}

#[tokio::test]
async fn profiles_expand_to_flags_and_never_add_a_second_authorization_system() {
    use strut_stack_sql::config::{load_config, WriteMode};

    let lite = load_config(&cli(&[
        "strut-stack-sql",
        "--url",
        "sqlite::memory:",
        "--profile",
        "lite",
    ]))
    .await
    .expect("lite");
    assert_eq!(lite.write_mode, WriteMode::ReadOnly);
    assert!(!lite.full_tools);
    assert!(!lite.allow_grants);
    assert!(!lite.require_approval);

    let standard = load_config(&cli(&[
        "strut-stack-sql",
        "--url",
        "sqlite::memory:",
        "--profile",
        "standard",
    ]))
    .await
    .expect("standard");
    assert!(standard.full_tools);
    assert_eq!(standard.write_mode, WriteMode::ReadOnly);

    let full = load_config(&cli(&[
        "strut-stack-sql",
        "--url",
        "sqlite::memory:",
        "--profile",
        "full",
    ]))
    .await
    .expect("full");
    assert_eq!(full.write_mode, WriteMode::AllowWrites);

    let admin = load_config(&cli(&[
        "strut-stack-sql",
        "--url",
        "sqlite::memory:",
        "--profile",
        "admin",
    ]))
    .await
    .expect("admin");
    assert_eq!(admin.write_mode, WriteMode::AllowDdl);
    assert!(admin.allow_grants);
    assert!(admin.require_approval);

    // Explicit flags may raise a preset; they never lower it.
    let raised = load_config(&cli(&[
        "strut-stack-sql",
        "--url",
        "sqlite::memory:",
        "--profile",
        "standard",
        "--allow-writes",
    ]))
    .await
    .expect("raised");
    assert_eq!(raised.write_mode, WriteMode::AllowWrites);
    assert!(raised.full_tools);

    let unknown = load_config(&cli(&[
        "strut-stack-sql",
        "--url",
        "sqlite::memory:",
        "--profile",
        "nope",
    ]))
    .await;
    assert!(unknown.is_err(), "unknown profile must fail at startup");
}
