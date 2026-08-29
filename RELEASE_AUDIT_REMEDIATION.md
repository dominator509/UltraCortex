# Independent Audit Remediation

Status date: 2026-08-27

Audit target: dominator509/UltraCortex, main, 8aa52264ee495262824a80edbd261870bd87e54d

This record reconciles the supplied independent release audit with the
current checkout. The audit target predates the remediation changes below;
release evidence must therefore be collected against the final commit that
contains them.

## Executive status

The source-confirmed implementation findings AUD-001 through AUD-014 and
AUD-017 have remediation code in this checkout. AUD-015 and AUD-016 have
structural mitigations but still need runtime stress and real-process
validation. AUD-018 remains an owner/legal decision and is intentionally not
closed by engineering. Hosted CI, branch protection, production model
availability, and external security review also remain external release
gates.

This is not a declaration that the project is release-complete. It is the
source-of-truth map from each audit item to code, tests, and remaining
evidence.

## Finding reconciliation

| Finding | Current checkout disposition | Remaining evidence or boundary |
|---|---|---|
| AUD-001 | Fresh provisioning persists the initial manifest before listeners accept requests. Recovery also discovers durable snapshots/WAL when the manifest is absent. | The abrupt-recovery regression covers a dropped node and WAL-only restart. An OS-level kill drill is still required for the release artifact. |
| AUD-002 | Recovery replays Fact, Blob, Timeline, Scratchpad, Subscription, CuratorOutput, and the dedicated CrossCheck WAL. Automatic snapshots trigger on logical time and WAL bytes. | Run the full crash matrix, including every state class and a CrossCheck tail. Low-disk-trigger behavior remains outside the current implementation. |
| AUD-003 | Supersession validates both sides before a single WAL-first transition, applies the reciprocal state change, and replays it deterministically. | Fault injection around WAL and apply boundaries should still be run against the release build. |
| AUD-004 | Admin transitions use AdminOp WAL records. Curator output, quarantine, governance, CrossCheck, and audit-chain persistence errors propagate; an audit persistence failure moves the node to shutdown rather than acknowledging the operation. | Bootstrap-only initialization writes are not request mutations. Verify operator recovery behavior after an injected persistence failure. |
| AUD-005 | T1 and above now seal WAL payloads, CAS payloads, snapshots, and prefix-cache data at their storage boundaries. Legacy raw data is accepted only through the compatibility reader. | The local keyring is a custody seam, not an external KMS/HSM. Inspect raw T1/T2/T3 files and perform key-custody review before public release. |
| AUD-006 | Warden semantic checks receive independently retrieved public source text plus the claim. Private facets remain outside SubstrateView and FrozenPublicView. | Run with the pinned production model and verify semantic disagreement behavior; development backends are not production evidence. |
| AUD-007 | Librarian, Warden, and Adjudicator artifacts pass the same durable Trinity pre-validation path before they are indexed or made active. | Production model runs and governance audit coverage remain operator validation. |
| AUD-008 | Blind re-audit creates fresh Warden state and an isolated frozen public view captured at the original review boundary. | This is historical for the curation boundary, not a general historical database. Validate the intended as-of semantics with a production replay drill. |
| AUD-009 | Strict production configuration requires a verified runner and SHA pin for every configured Adjudicator pool member. Deterministic backends are available only through explicit development mode. | Provide and verify every real pool model; no repository-only test can establish model independence. |
| AUD-010 | The wire contract now exposes authenticated pull delivery through events and events_ack. Pending queues and since replay are served on the connection; the contract no longer promises unsolicited push frames. | Pull liveness and reconnect behavior should be exercised by an external client. |
| AUD-011 | ResponseEnvelope now mirrors the request seed. MCP documentation is aligned with the implemented Envelope, response, handshake, and pull-event shapes. | Any future wire change must update the versioned schema and conformance vectors together. |
| AUD-012 | Admin dispatch requires an active operator capability token on every transport, including loopback TCP. UDS remains the preferred transport and TCP remains loopback-only. | Review operator-token custody and test unauthorized local clients against the release binary. |
| AUD-013 | Mandatory audit events are appended synchronously and flushed to disk before acknowledgment. CrossCheck append failures propagate and required Curator/admin/security events are emitted from their state-changing paths. | Run secret scanning, tamper/failure injection, and hosted artifact verification. Derived cache invalidation is intentionally non-authoritative and is not forensic evidence. |
| AUD-014 | A degraded calibration state now forces Curator outputs through Adjudicator escalation before activation. | Exercise recovery from degraded mode with real model failures and confirm the operator-facing queue. |
| AUD-015 | Normal Router/admin mutations and snapshot capture share a node-wide mutation barrier, giving the snapshot a coherent boundary for supported write paths. | Direct public cell mutation is not an external API guarantee. Run concurrent mutation/snapshot stress and recovery invariant checks. |
| AUD-016 | External GGUF execution uses a 30-second deadline, bounded output, kill, and reap behavior, and fails closed on timeout. | Run a real hung-process drill and verify no request thread or child process remains stuck. |
| AUD-017 | CI Clippy is now a hard gate instead of continue-on-error. Formatting and warning cleanup are part of the local release gate. | The red hosted run on the audited SHA was not independently reclassified; require a green run for the final SHA. |
| AUD-018 | Not closed by engineering. Cargo metadata still uses LicenseRef-TBD and a root license/notice has not been selected. | Owner/legal review must choose and document the public-release license. |

## Code and regression evidence

The main regression for crash durability is
bootstrap::tests::abrupt_recovery_replays_all_normal_wal_state_classes. It
boots a fresh node, writes Fact, Blob, Timeline, and Scratchpad state, drops
the node without clean shutdown, and verifies recovery from the WAL.

The persistence tests cover:

- T1 storage-boundary sealing for WAL, CAS, snapshots, and prefix-cache data.
- Snapshot logical-time triggering.
- Subscription identity, sequence, and activation-cursor replay.
- Reciprocal Fact supersession and deterministic WAL replay.
- CrossCheck WAL replay and same-timestamp Curator ordering.
- Response seed round-trip and authenticated admin/event pull behavior.
- Abrupt restart recovery with no initial manifest, including Fact, Blob,
  Timeline, and Scratchpad WAL state.

Before release, run these commands from the repository root:

    cargo fmt --check
    cargo clippy --all-targets -- -D warnings
    cargo test --all-targets
    git diff --check

The current local working-tree results on 2026-08-27 are: formatter check
passed, strict Clippy passed, `cargo test --all-targets` passed with
`171 passed` across 6 suites, and `git diff --check` reported no
whitespace errors. These results must be repeated on the final release SHA.

## Remaining release gates

The following cannot be established by source edits alone:

- abrupt OS process-death matrix for each persistent state class;
- concurrent snapshot stress and recovered cross-cell invariants;
- real pinned Librarian, Warden, and every Adjudicator model, including a
  hung-process timeout drill;
- raw-file inspection under T1, T2, and T3 plus key-custody review;
- external secret, supply-chain, and bespoke crypto/storage review;
- green hosted CI and branch-protection/status-check verification on the final
  commit;
- owner/legal license and notice approval.

Until those gates are evidenced, the appropriate release status remains
conditional rather than complete.
