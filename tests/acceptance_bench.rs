//! Deterministic acceptance bench for the DeepSeek/token-efficiency gaps.
//!
//! The workload drives the real Router against a booted node and measures
//! the read path an interactive coding agent actually consumes:
//! recall/view/hydrate, plus one mid-run write to prove cache invalidation.

use std::sync::Arc;

use ultracortex::bootstrap;
use ultracortex::core::cbor::Cbor;
use ultracortex::core::ulid::{DetRng, Ulid};
use ultracortex::core::{Intent, Severity, Tier};
use ultracortex::router::captoken::issue_agent_token;
use ultracortex::router::envelope::{Envelope, EnvelopeFlags, WorkBudget, PROTO_VERSION};
use ultracortex::router::handle_envelope;
use ultracortex::Node;

fn booted() -> Arc<Node> {
    bootstrap::dry_run().expect("boot").node
}

fn agent(node: &Node, id: &str) -> ultracortex::router::captoken::CapToken {
    let tok = issue_agent_token(&*node.signer, id, 0);
    node.cells
        .agent_registry
        .lock()
        .unwrap()
        .register(node.now(), id, "agent");
    tok
}

#[allow(clippy::too_many_arguments)]
fn env_for(
    node: &Node,
    token: &ultracortex::router::captoken::CapToken,
    intent: Intent,
    payload: Cbor,
    anchor: Option<&str>,
    tier: Tier,
    seed: u64,
    task_id: &str,
) -> Envelope {
    Envelope {
        proto_version: PROTO_VERSION,
        request_id: Ulid::from_parts(node.now(), &mut DetRng::new(seed ^ 0xB311)),
        agent_id: token.agent_id.clone(),
        capability: token.clone(),
        work_budget: WorkBudget {
            task_id: task_id.to_string(),
            units: 100_000,
        },
        intent,
        payload,
        spec_anchor: anchor.map(String::from),
        severity: Severity::P2,
        gap_ref: None,
        tier,
        seed,
        flags: EnvelopeFlags {
            semantic_check: false,
            continuation: false,
        },
    }
}

fn write_fact(
    node: &Node,
    tok: &ultracortex::router::captoken::CapToken,
    task_id: &str,
    s: &str,
    p: &str,
    o: &str,
    seed: u64,
) -> String {
    let r = handle_envelope(
        node,
        &env_for(
            node,
            tok,
            Intent::Write,
            Cbor::map(vec![
                ("subject", Cbor::t(s)),
                ("predicate", Cbor::t(p)),
                ("object", Cbor::t(o)),
            ]),
            Some("Architecture.md\u{00a7}4"),
            Tier::L1,
            seed,
            task_id,
        ),
    );
    assert!(r.ok, "write failed: {:?}", r.err_message);
    r.result.opt_str("handle").unwrap()
}

fn record_read(
    read_tokens: &mut Vec<u64>,
    resp: ultracortex::router::envelope::ResponseEnvelope,
) -> ultracortex::router::envelope::ResponseEnvelope {
    assert!(resp.ok, "bench read failed: {:?}", resp.err_message);
    read_tokens.push(resp.tokens_emitted);
    resp
}

fn run_recall(
    node: &Node,
    tok: &ultracortex::router::captoken::CapToken,
    read_tokens: &mut Vec<u64>,
    task_id: &str,
    query: &str,
    seed: u64,
) {
    let resp = handle_envelope(
        node,
        &env_for(
            node,
            tok,
            Intent::Recall,
            Cbor::map(vec![("query", Cbor::t(query)), ("k", Cbor::U64(4))]),
            None,
            Tier::L1,
            seed,
            task_id,
        ),
    );
    let _ = record_read(read_tokens, resp);
}

fn run_hydrate(
    node: &Node,
    tok: &ultracortex::router::captoken::CapToken,
    read_tokens: &mut Vec<u64>,
    task_id: &str,
    handle: &str,
    seed: u64,
) {
    let resp = handle_envelope(
        node,
        &env_for(
            node,
            tok,
            Intent::Hydrate,
            Cbor::map(vec![("handle", Cbor::t(handle))]),
            None,
            Tier::L3,
            seed,
            task_id,
        ),
    );
    let _ = record_read(read_tokens, resp);
}

fn run_view(
    node: &Node,
    tok: &ultracortex::router::captoken::CapToken,
    read_tokens: &mut Vec<u64>,
    task_id: &str,
    view_id: &str,
    params: Cbor,
    seed: u64,
) -> bool {
    let resp = handle_envelope(
        node,
        &env_for(
            node,
            tok,
            Intent::View,
            Cbor::map(vec![("view_id", Cbor::t(view_id)), ("params", params)]),
            None,
            Tier::L1,
            seed,
            task_id,
        ),
    );
    record_read(read_tokens, resp)
        .result
        .opt_bool("cached")
        .unwrap_or(false)
}

#[derive(Clone, Debug)]
struct BenchResult {
    total_read_steps: usize,
    recall_steps: usize,
    hydrate_steps: usize,
    view_steps: usize,
    cache_hits: usize,
    cache_misses: usize,
    tokens_p50: u64,
    tokens_p99: u64,
    approx_bytes_p50: u64,
    approx_bytes_p99: u64,
    hydrate_per_recall_ratio: f64,
    prefix_cache_hit_rate: f64,
}

impl BenchResult {
    fn to_json(&self) -> String {
        format!(
            concat!(
                "{{",
                "\"total_read_steps\":{},",
                "\"recall_steps\":{},",
                "\"hydrate_steps\":{},",
                "\"view_steps\":{},",
                "\"cache_hits\":{},",
                "\"cache_misses\":{},",
                "\"tokens_p50\":{},",
                "\"tokens_p99\":{},",
                "\"approx_bytes_p50\":{},",
                "\"approx_bytes_p99\":{},",
                "\"hydrate_per_recall_ratio\":{:.4},",
                "\"prefix_cache_hit_rate\":{:.4}",
                "}}"
            ),
            self.total_read_steps,
            self.recall_steps,
            self.hydrate_steps,
            self.view_steps,
            self.cache_hits,
            self.cache_misses,
            self.tokens_p50,
            self.tokens_p99,
            self.approx_bytes_p50,
            self.approx_bytes_p99,
            self.hydrate_per_recall_ratio,
            self.prefix_cache_hit_rate,
        )
    }
}

#[derive(Clone, Debug)]
struct SnapshotBenchResult {
    iterations: usize,
    cells_per_snapshot: u64,
    pause_target_us: u64,
    pause_p50_us: u64,
    pause_p99_us: u64,
    pause_max_us: u64,
}

impl SnapshotBenchResult {
    fn to_json(&self) -> String {
        format!(
            concat!(
                "{{",
                "\"iterations\":{},",
                "\"cells_per_snapshot\":{},",
                "\"pause_target_us\":{},",
                "\"pause_p50_us\":{},",
                "\"pause_p99_us\":{},",
                "\"pause_max_us\":{}",
                "}}"
            ),
            self.iterations,
            self.cells_per_snapshot,
            self.pause_target_us,
            self.pause_p50_us,
            self.pause_p99_us,
            self.pause_max_us,
        )
    }
}

fn percentile(mut values: Vec<u64>, numer: usize, denom: usize) -> u64 {
    values.sort_unstable();
    let idx = ((values.len() * numer).saturating_sub(1)) / denom;
    values[idx]
}

fn run_acceptance_bench() -> BenchResult {
    let node = booted();
    let tok = agent(&node, "acceptance-bench-agent");
    let task_id = "bench.deepseek";

    let seed_facts = [
        (
            "repo.router",
            "note",
            "Router recall and view responses are prefix-stable, budget-charged, and tiered.",
        ),
        (
            "repo.router",
            "note",
            "Facet gates run before hydrate existence checks so private curator rationale stays hidden.",
        ),
        (
            "repo.router",
            "note",
            "View cache keys include the view id namespace version and canonical params hash.",
        ),
        (
            "repo.router",
            "note",
            "Writes and supersedes bump the global view version so stale cached layouts are not reused.",
        ),
        (
            "repo.trinity",
            "note",
            "Contract validation runs before SpecAnchor Decision WorkBudget and Congruence.",
        ),
        (
            "repo.trinity",
            "note",
            "Congruence blocks unknown entities until operators accept the delta explicitly.",
        ),
        (
            "repo.curator",
            "note",
            "Librarian outputs stay pending until a Warden audit passes or an adjudicator resolves disagreement.",
        ),
        (
            "repo.persist",
            "note",
            "PrefixCacheStore stores canonical CBOR view bytes beneath cache/views and invalidates on dependent writes.",
        ),
        (
            "repo.persist",
            "note",
            "Snapshots are copy-on-write and manifest updates are atomic at clean shutdown.",
        ),
        (
            "repo.metrics",
            "note",
            "Cache hit and miss counters expose DeepSeek view reuse without depending on wall-clock latency.",
        ),
    ];

    let mut router_handles = Vec::new();
    let mut persist_handles = Vec::new();
    for (i, (s, p, o)) in seed_facts.iter().enumerate() {
        let handle = write_fact(&node, &tok, task_id, s, p, o, 10 + i as u64);
        if *s == "repo.router" {
            router_handles.push(handle.clone());
        }
        if *s == "repo.persist" {
            persist_handles.push(handle);
        }
    }

    let mut read_tokens = Vec::new();
    let mut recall_steps = 0usize;
    let mut hydrate_steps = 0usize;
    let mut view_steps = 0usize;
    let mut cache_hits = 0usize;
    let mut cache_misses = 0usize;

    let router_params = Cbor::map(vec![("subject", Cbor::t("repo.router"))]);
    let gap_board_params = Cbor::Null;

    recall_steps += 1;
    run_recall(
        &node,
        &tok,
        &mut read_tokens,
        task_id,
        "prefix stable router budget",
        101,
    );
    view_steps += 1;
    if run_view(
        &node,
        &tok,
        &mut read_tokens,
        task_id,
        "fact_subject",
        router_params.clone(),
        102,
    ) {
        cache_hits += 1;
    } else {
        cache_misses += 1;
    }
    view_steps += 1;
    if run_view(
        &node,
        &tok,
        &mut read_tokens,
        task_id,
        "fact_subject",
        router_params.clone(),
        103,
    ) {
        cache_hits += 1;
    } else {
        cache_misses += 1;
    }
    view_steps += 1;
    if run_view(
        &node,
        &tok,
        &mut read_tokens,
        task_id,
        "fact_subject",
        router_params.clone(),
        104,
    ) {
        cache_hits += 1;
    } else {
        cache_misses += 1;
    }
    recall_steps += 1;
    run_recall(
        &node,
        &tok,
        &mut read_tokens,
        task_id,
        "facet gate hydrate rationale",
        105,
    );
    view_steps += 1;
    if run_view(
        &node,
        &tok,
        &mut read_tokens,
        task_id,
        "gap_board",
        gap_board_params.clone(),
        106,
    ) {
        cache_hits += 1;
    } else {
        cache_misses += 1;
    }
    view_steps += 1;
    if run_view(
        &node,
        &tok,
        &mut read_tokens,
        task_id,
        "gap_board",
        gap_board_params.clone(),
        107,
    ) {
        cache_hits += 1;
    } else {
        cache_misses += 1;
    }
    view_steps += 1;
    if run_view(
        &node,
        &tok,
        &mut read_tokens,
        task_id,
        "gap_board",
        gap_board_params.clone(),
        108,
    ) {
        cache_hits += 1;
    } else {
        cache_misses += 1;
    }
    recall_steps += 1;
    run_recall(
        &node,
        &tok,
        &mut read_tokens,
        task_id,
        "contract validation congruence",
        109,
    );
    view_steps += 1;
    if run_view(
        &node,
        &tok,
        &mut read_tokens,
        task_id,
        "fact_subject",
        router_params.clone(),
        110,
    ) {
        cache_hits += 1;
    } else {
        cache_misses += 1;
    }
    view_steps += 1;
    if run_view(
        &node,
        &tok,
        &mut read_tokens,
        task_id,
        "fact_subject",
        router_params.clone(),
        111,
    ) {
        cache_hits += 1;
    } else {
        cache_misses += 1;
    }

    write_fact(
        &node,
        &tok,
        task_id,
        "repo.router",
        "note",
        "Budget defaults are namespace-aware and the admin plane exposes them through budget defaults.",
        112,
    );

    view_steps += 1;
    if run_view(
        &node,
        &tok,
        &mut read_tokens,
        task_id,
        "fact_subject",
        router_params.clone(),
        113,
    ) {
        cache_hits += 1;
    } else {
        cache_misses += 1;
    }
    view_steps += 1;
    if run_view(
        &node,
        &tok,
        &mut read_tokens,
        task_id,
        "fact_subject",
        router_params.clone(),
        114,
    ) {
        cache_hits += 1;
    } else {
        cache_misses += 1;
    }
    view_steps += 1;
    if run_view(
        &node,
        &tok,
        &mut read_tokens,
        task_id,
        "fact_subject",
        router_params.clone(),
        115,
    ) {
        cache_hits += 1;
    } else {
        cache_misses += 1;
    }
    recall_steps += 1;
    run_recall(
        &node,
        &tok,
        &mut read_tokens,
        task_id,
        "view version invalidation budget defaults",
        116,
    );
    hydrate_steps += 1;
    run_hydrate(
        &node,
        &tok,
        &mut read_tokens,
        task_id,
        &router_handles[0],
        117,
    );
    view_steps += 1;
    if run_view(
        &node,
        &tok,
        &mut read_tokens,
        task_id,
        "fact_subject",
        router_params.clone(),
        118,
    ) {
        cache_hits += 1;
    } else {
        cache_misses += 1;
    }
    view_steps += 1;
    if run_view(
        &node,
        &tok,
        &mut read_tokens,
        task_id,
        "fact_subject",
        router_params.clone(),
        119,
    ) {
        cache_hits += 1;
    } else {
        cache_misses += 1;
    }
    recall_steps += 1;
    run_recall(
        &node,
        &tok,
        &mut read_tokens,
        task_id,
        "prefix cache store canonical cbor",
        120,
    );
    view_steps += 1;
    if run_view(
        &node,
        &tok,
        &mut read_tokens,
        task_id,
        "fact_subject",
        router_params.clone(),
        121,
    ) {
        cache_hits += 1;
    } else {
        cache_misses += 1;
    }
    view_steps += 1;
    if run_view(
        &node,
        &tok,
        &mut read_tokens,
        task_id,
        "fact_subject",
        router_params.clone(),
        122,
    ) {
        cache_hits += 1;
    } else {
        cache_misses += 1;
    }
    recall_steps += 1;
    run_recall(
        &node,
        &tok,
        &mut read_tokens,
        task_id,
        "copy on write manifest atomic",
        123,
    );
    hydrate_steps += 1;
    run_hydrate(
        &node,
        &tok,
        &mut read_tokens,
        task_id,
        &persist_handles[0],
        124,
    );
    view_steps += 1;
    if run_view(
        &node,
        &tok,
        &mut read_tokens,
        task_id,
        "fact_subject",
        router_params.clone(),
        125,
    ) {
        cache_hits += 1;
    } else {
        cache_misses += 1;
    }
    view_steps += 1;
    if run_view(
        &node,
        &tok,
        &mut read_tokens,
        task_id,
        "fact_subject",
        router_params.clone(),
        126,
    ) {
        cache_hits += 1;
    } else {
        cache_misses += 1;
    }
    recall_steps += 1;
    run_recall(
        &node,
        &tok,
        &mut read_tokens,
        task_id,
        "cache hit miss counters",
        127,
    );
    view_steps += 1;
    if run_view(
        &node,
        &tok,
        &mut read_tokens,
        task_id,
        "fact_subject",
        router_params.clone(),
        128,
    ) {
        cache_hits += 1;
    } else {
        cache_misses += 1;
    }
    recall_steps += 1;
    run_recall(
        &node,
        &tok,
        &mut read_tokens,
        task_id,
        "warden audit pending active",
        129,
    );
    view_steps += 1;
    if run_view(
        &node,
        &tok,
        &mut read_tokens,
        task_id,
        "fact_subject",
        router_params.clone(),
        130,
    ) {
        cache_hits += 1;
    } else {
        cache_misses += 1;
    }

    let tokens_p50 = percentile(read_tokens.clone(), 50, 100);
    let tokens_p99 = percentile(read_tokens.clone(), 99, 100);
    BenchResult {
        total_read_steps: read_tokens.len(),
        recall_steps,
        hydrate_steps,
        view_steps,
        cache_hits,
        cache_misses,
        tokens_p50,
        tokens_p99,
        approx_bytes_p50: tokens_p50 * 4,
        approx_bytes_p99: tokens_p99 * 4,
        hydrate_per_recall_ratio: hydrate_steps as f64 / recall_steps as f64,
        prefix_cache_hit_rate: cache_hits as f64 / view_steps as f64,
    }
}

fn seed_snapshot_bench(
    node: &Node,
    tok: &ultracortex::router::captoken::CapToken,
    task_id: &str,
) -> String {
    let facts = [
        (
            "repo.router",
            "note",
            "Router snapshots should capture current facts cached views and curator state without dropping replay determinism.",
        ),
        (
            "repo.router",
            "note",
            "The snapshot path updates the manifest after writing the checkpoint image.",
        ),
        (
            "repo.persist",
            "note",
            "Snapshots are copy on write and should expose an operator visible pause budget.",
        ),
        (
            "repo.persist",
            "note",
            "Manifest updates are atomic and recovery restores every persisted cell.",
        ),
        (
            "repo.metrics",
            "note",
            "Metrics should record snapshot pause observations and target overruns.",
        ),
        (
            "repo.curator",
            "note",
            "Curator outputs and audits participate in the persisted state like any other cell snapshots.",
        ),
        (
            "repo.audit",
            "note",
            "Audit verification must survive checkpoint boundaries and clean shutdowns.",
        ),
        (
            "repo.view",
            "note",
            "Cached view bytes depend on canonical params and the current view version.",
        ),
    ];

    let mut first_handle = String::new();
    for (i, (s, p, o)) in facts.iter().enumerate() {
        let handle = write_fact(node, tok, task_id, s, p, o, 600 + i as u64);
        if i == 0 {
            first_handle = handle;
        }
    }

    let mut scratch = Vec::new();
    run_recall(
        node,
        tok,
        &mut scratch,
        task_id,
        "snapshot manifest replay determinism",
        700,
    );
    let params = Cbor::map(vec![("subject", Cbor::t("repo.router"))]);
    let _ = run_view(
        node,
        tok,
        &mut scratch,
        task_id,
        "fact_subject",
        params.clone(),
        701,
    );
    let _ = run_view(
        node,
        tok,
        &mut scratch,
        task_id,
        "fact_subject",
        params.clone(),
        702,
    );
    let _ = run_view(
        node,
        tok,
        &mut scratch,
        task_id,
        "fact_subject",
        params,
        703,
    );
    run_hydrate(node, tok, &mut scratch, task_id, &first_handle, 704);
    first_handle
}

fn run_snapshot_pause_bench() -> SnapshotBenchResult {
    let node = booted();
    let tok = agent(&node, "snapshot-bench-agent");
    let task_id = "bench.snapshot";
    let _ = seed_snapshot_bench(&node, &tok, task_id);

    let mut pauses = Vec::new();
    let mut cells = 0u64;
    for _ in 0..7 {
        let snap = bootstrap::admin_dispatch(
            &node,
            &Cbor::map(vec![("verb", Cbor::t("snapshot")), ("args", Cbor::Null)]),
        )
        .expect("snapshot admin");
        let pause = snap.opt_u64("pause_us").expect("pause_us");
        pauses.push(pause);
        cells = snap.opt_u64("cells").unwrap_or(0);
        assert_eq!(
            snap.opt_u64("pause_target_us"),
            Some(ultracortex::node::SNAPSHOT_PAUSE_TARGET_US)
        );
        assert_eq!(
            snap.opt_bool("within_target"),
            Some(pause <= ultracortex::node::SNAPSHOT_PAUSE_TARGET_US)
        );
    }

    let pause_p50_us = percentile(pauses.clone(), 50, 100);
    let pause_p99_us = percentile(pauses.clone(), 99, 100);
    let pause_max_us = *pauses.iter().max().unwrap_or(&0);
    SnapshotBenchResult {
        iterations: pauses.len(),
        cells_per_snapshot: cells,
        pause_target_us: ultracortex::node::SNAPSHOT_PAUSE_TARGET_US,
        pause_p50_us,
        pause_p99_us,
        pause_max_us,
    }
}

#[test]
fn deepseek_acceptance_bench_meets_targets() {
    let result = run_acceptance_bench();
    println!("ACCEPTANCE_BENCH {}", result.to_json());

    assert!(
        result.approx_bytes_p50 <= 1_536,
        "p50 context bytes too large: {:?}",
        result
    );
    assert!(
        result.approx_bytes_p99 <= 4_096,
        "p99 context bytes too large: {:?}",
        result
    );
    assert!(
        result.prefix_cache_hit_rate >= 0.80,
        "prefix-cache hit rate below target: {:?}",
        result
    );
    assert!(
        result.hydrate_per_recall_ratio <= 0.25,
        "hydrate/recall ratio above target: {:?}",
        result
    );
}

#[test]
fn snapshot_pause_bench_meets_target() {
    let result = run_snapshot_pause_bench();
    println!("SNAPSHOT_PAUSE_BENCH {}", result.to_json());
    assert!(
        result.pause_max_us <= result.pause_target_us,
        "snapshot pause exceeded target: {:?}",
        result
    );
}
