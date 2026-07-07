//! `ultracortex` binary — SPEC-DERIVED-§B (Bootstrap.md), §D
//! (RouterScheduler.md admin verbs).
//!
//! ```text
//! ultracortex run [--config PATH] [--set key=value]... [--dry-run]
//! ultracortex status | snapshot | metrics | shutdown
//! ultracortex quarantine list | reinject <qid> | reject <qid>
//! ultracortex gap list
//! ultracortex audit verify
//! ultracortex kms status | rotate [--emergency]
//! ultracortex congruence audit
//! ultracortex contract list
//! ultracortex curator status | probe-now | verify-weights
//! ultracortex cross-check tail [n]
//! ultracortex adjudicator stats
//! ultracortex resolve <adjudication-handle> --uphold-auditor|--uphold-initiator
//! ```
//!
//! Admin verbs connect to a *running* node over the UDS/TCP wire; `run`
//! boots one. Exit codes: 0 ok, 1 usage, 2 runtime failure.

use std::path::PathBuf;
use std::process::ExitCode;
use ultracortex::bootstrap::{self, Config};
use ultracortex::core::cbor::Cbor;
use ultracortex::proto::Client;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("usage: ultracortex <run|status|snapshot|quarantine|gap|audit|kms|congruence|contract|curator|cross-check|adjudicator|resolve|metrics|shutdown> …");
        return ExitCode::from(1);
    }

    match run_cli(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(2)
        }
    }
}

fn run_cli(args: &[String]) -> Result<(), String> {
    match args[0].as_str() {
        "run" => cmd_run(&args[1..]),
        // Everything else is an admin verb against a running node.
        _ => cmd_admin(args),
    }
}

fn cmd_run(rest: &[String]) -> Result<(), String> {
    let mut config_path: Option<PathBuf> = None;
    let mut overrides: Vec<(String, String)> = Vec::new();
    let mut dry_run = false;

    let mut i = 0;
    while i < rest.len() {
        match rest[i].as_str() {
            "--config" => {
                i += 1;
                config_path = Some(PathBuf::from(
                    rest.get(i).ok_or("--config requires a path")?,
                ));
            }
            "--set" => {
                i += 1;
                let kv = rest.get(i).ok_or("--set requires key=value")?;
                let (k, v) = kv
                    .split_once('=')
                    .ok_or_else(|| format!("bad --set `{kv}` (want key=value)"))?;
                overrides.push((k.to_string(), v.to_string()));
            }
            "--dry-run" => dry_run = true,
            other => return Err(format!("unknown flag `{other}`")),
        }
        i += 1;
    }

    if dry_run {
        let report = bootstrap::dry_run().map_err(|e| e.message)?;
        println!(
            "dry-run ok: node_id={} self_test={}/11 recovered={}",
            report.node.node_id, report.self_test_passed, report.recovered
        );
        return Ok(());
    }

    let cfg = Config::load(config_path.as_deref(), &overrides).map_err(|e| e.message)?;
    bootstrap::run(&cfg).map_err(|e| e.message)
}

fn connect() -> Result<Client, String> {
    // Same resolution order as the node: config file (if present) → env →
    // defaults. Keep it light: honor UC_DATA_DIR/UC_UDS/UC_TCP + default
    // ./ultracortex-data/ultracortex.sock, then 127.0.0.1:7741.
    let cfg = Config::load(None, &[]).map_err(|e| e.message)?;
    let tcp = cfg
        .tcp_addr
        .clone()
        .unwrap_or_else(|| "127.0.0.1:7741".into());
    Client::connect(cfg.uds_path.as_deref(), Some(tcp.as_str())).map_err(|e| e.message)
}

fn cmd_admin(args: &[String]) -> Result<(), String> {
    // Assemble "<verb words> [positional]" into (verb, args map).
    let (verb, cbor_args): (String, Cbor) = match args {
        [v] if matches!(v.as_str(), "status" | "snapshot" | "metrics" | "shutdown") => {
            (v.clone(), Cbor::Null)
        }
        [g, sub] if g == "gap" && sub == "list" => ("gap list".into(), Cbor::Null),
        [b, sub] if b == "budget" && sub == "defaults" => ("budget defaults".into(), Cbor::Null),
        [g, sub] if g == "quarantine" && sub == "list" => ("quarantine list".into(), Cbor::Null),
        [g, sub, qid] if g == "quarantine" && (sub == "reinject" || sub == "reject") => (
            format!("quarantine {sub}"),
            Cbor::map(vec![("qid", Cbor::t(qid.clone()))]),
        ),
        [a, sub] if a == "audit" && sub == "verify" => ("audit verify".into(), Cbor::Null),
        [k, sub] if k == "kms" && sub == "status" => ("kms status".into(), Cbor::Null),
        [k, sub] if k == "kms" && sub == "rotate" => (
            "kms rotate".into(),
            Cbor::map(vec![("emergency", Cbor::Bool(false))]),
        ),
        [k, sub, flag] if k == "kms" && sub == "rotate" && flag == "--emergency" => (
            "kms rotate".into(),
            Cbor::map(vec![("emergency", Cbor::Bool(true))]),
        ),
        [c, sub] if c == "congruence" && sub == "audit" => {
            ("congruence audit".into(), Cbor::Null)
        }
        xs if xs.len() >= 3 && xs[0] == "congruence" && xs[1] == "accept" => (
            "congruence accept".into(),
            Cbor::map(vec![("entities", Cbor::text_array(&xs[2..]))]),
        ),
        [c, sub] if c == "contract" && sub == "list" => ("contract list".into(), Cbor::Null),
        [c, sub, schema] if c == "contract" && sub == "verify-migration" => (
            "contract verify-migration".into(),
            Cbor::map(vec![("schema_id", Cbor::t(schema.clone()))]),
        ),
        [c, sub, schema] if c == "contract" && sub == "apply-migration" => (
            "contract apply-migration".into(),
            Cbor::map(vec![("schema_id", Cbor::t(schema.clone()))]),
        ),
        [c, sub, source, target, decision, plan, deprecated_after]
            if c == "contract" && sub == "plan-migration" =>
        (
            "contract plan-migration".into(),
            Cbor::map(vec![
                ("schema_id", Cbor::t(source.clone())),
                ("target_schema_id", Cbor::t(target.clone())),
                ("decision_handle", Cbor::t(decision.clone())),
                ("migration_plan_handle", Cbor::t(plan.clone())),
                ("deprecated_after", Cbor::t(deprecated_after.clone())),
            ]),
        ),
        [c, sub] if c == "curator" && matches!(sub.as_str(), "status" | "probe-now" | "verify-weights") => {
            (format!("curator {sub}"), Cbor::Null)
        }
        [c, sub] if c == "cross-check" && sub == "tail" => {
            ("cross-check tail".into(), Cbor::map(vec![("n", Cbor::U64(20))]))
        }
        [c, sub, n] if c == "cross-check" && sub == "tail" => (
            "cross-check tail".into(),
            Cbor::map(vec![(
                "n",
                Cbor::U64(n.parse::<u64>().map_err(|_| "n must be a number")?),
            )]),
        ),
        [a, sub] if a == "adjudicator" && sub == "stats" => {
            ("adjudicator stats".into(), Cbor::Null)
        }
        [r, handle, flag] if r == "resolve" => {
            let uphold = match flag.as_str() {
                "--uphold-auditor" => true,
                "--uphold-initiator" => false,
                other => return Err(format!("unknown resolve flag `{other}`")),
            };
            (
                "resolve".into(),
                Cbor::map(vec![
                    ("handle", Cbor::t(handle.clone())),
                    ("uphold_auditor", Cbor::Bool(uphold)),
                ]),
            )
        }
        other => {
            return Err(format!(
                "unknown or malformed command `{}`",
                other.join(" ")
            ))
        }
    };

    let mut client = connect()?;
    let result = client.admin(&verb, cbor_args).map_err(|e| e.message)?;
    println!("{}", render(&result, 0));
    Ok(())
}

/// Minimal human-readable CBOR rendering for terminal output.
fn render(c: &Cbor, indent: usize) -> String {
    let pad = "  ".repeat(indent);
    match c {
        Cbor::Map(pairs) => pairs
            .iter()
            .map(|(k, v)| {
                let key = k.as_str().map(String::from).unwrap_or_else(|| format!("{k:?}"));
                match v {
                    Cbor::Map(_) | Cbor::Array(_) => {
                        format!("{pad}{key}:\n{}", render(v, indent + 1))
                    }
                    _ => format!("{pad}{key}: {}", render_scalar(v)),
                }
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Cbor::Array(items) => items
            .iter()
            .map(|v| match v {
                Cbor::Map(_) | Cbor::Array(_) => format!("{pad}-\n{}", render(v, indent + 1)),
                _ => format!("{pad}- {}", render_scalar(v)),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        scalar => format!("{pad}{}", render_scalar(scalar)),
    }
}

fn render_scalar(c: &Cbor) -> String {
    match c {
        Cbor::Text(s) => s.clone(),
        Cbor::U64(n) => n.to_string(),
        Cbor::I64(n) => n.to_string(),
        Cbor::F64(f) => format!("{f:.4}"),
        Cbor::Bool(b) => b.to_string(),
        Cbor::Null => "-".into(),
        Cbor::Bytes(b) => format!("<{} bytes>", b.len()),
        other => format!("{other:?}"),
    }
}
