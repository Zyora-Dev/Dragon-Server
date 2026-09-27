# Dragon Server Progress

## Current State

- Phase 1 HTTP foundation implemented and locally verified in the existing workspace; broader acceptance gates remain open.
- User approved continuing toward backend hosting and enhancing Phase 2. The library-only process manager now supports graceful shutdown deadlines and process-group cleanup; this does not close the remaining Phase 1 acceptance gates or enable backend hosting.
- Specification: [Dragon Server Architecture Specification](docs/architecture/dragon-server-specification.md).
- Product: independent, Indian-built HTTP and application server, separate from Szyora Server Engine.
- Niral is a supported workload goal; its actual runtime and service architecture have not been inspected.
- Selected Phase 1 stack: Rust, Tokio, Hyper, TOML/Serde, Clap and Tracing; Rustls remains deferred.
- Proposed delivery: Linux-first production, macOS development, later Windows capability support.
- Local Rust/Cargo 1.89.0 verified; rustfmt and Clippy installed for the active toolchain. Cargo.lock records resolved dependencies.
- HTTP listener, bounded admission, phase deadlines, static descriptor confinement, streamed bodies, strict configuration and routing implemented.
- Found Hyper normalizes conflicting transfer/length headers; added bounded httparse ingress inspection to reject ambiguity before route execution, including keep-alive messages.
- Bounded nonblocking log sink implemented; expanded checks passed: 10 real-socket tests and 2 CLI tests, including overload recovery, slow-reader timeout, SIGTERM and request-secret omission.
- Public server construction validates configuration; its invalid-limit regression test passed.
- Added run instructions and Linux/macOS CI workflow. Remote regression checks now pass on both platforms; coverage-guided fuzzing, soak tests and benchmarks have not been run.
- Git initialized on main with origin https://github.com/Zyora-Dev/Dragon-Server.git at the user's request to run GitHub Actions. Initial source commit ecc1894 pushed successfully to the public repository.
- GitHub Actions [run 36317834392](https://github.com/Zyora-Dev/Dragon-Server/actions/runs/36317834392) passed for commit ecc1894281500106564324b838abb9019ae44f7d on 2026-09-27. Ubuntu 24.04.5 x86-64 and macOS ARM64 both passed formatting, strict all-target Clippy and all 36 tests using Rust 1.89.0. Each platform reports one ignored subprocess fixture that is explicitly invoked by the process tests. Job durations: Ubuntu 55 seconds, macOS 1 minute 4 seconds. This verifies the existing Linux regression suite, not production readiness or complete Phase 2 acceptance.
- Latest local checks passed after the Phase 2 lifecycle enhancement: cargo fmt applied; cargo clippy --locked --all-targets -- -D warnings clean; cargo test --locked passed all 36 tests (9 unit, 3 configuration, 15 TCP, 2 CLI, 7 process). One subprocess fixture is ignored in normal runs and explicitly exercised by the process tests. These results are from macOS, not Linux.
- VS Code run task added in .vscode/tasks.json. Example server running at http://127.0.0.1:8080; /hello, /assets/ and HEAD /assets/index.txt returned 200 in live smoke checks.
- Production acceptance remains pending. No TLS, reverse proxy, complete runtime supervisor, Niral integration, deployment or distributed features implemented. The new process primitive is not connected to the CLI or HTTP configuration.

## Completed

- Phase 2 lifecycle enhancement: Linux/macOS child process groups, SIGTERM grace (default one second, configurable 1 ms to 60 seconds), SIGKILL escalation, and group cleanup before direct-child reaping on stop, dropped handle or natural leader exit. WNOWAIT exit observation preserves the leader PID through the final group signal. All seven focused process tests passed locally on macOS, including graceful flush/exit, uncooperative-child escalation and descendant listener closure. One native fixture is explicitly invoked by those tests and ignored by the normal runner. Final formatting applied, strict all-target Clippy clean, full suite 36 passed.
- Final group-signal failures are reported as `group_cleanup_error` while still attempting direct-child termination/reaping. Darwin can return EPERM for zombie-only groups; do not suppress the error because it can also indicate real signalling denial. Group signalling is not containment and does not reap arbitrary grandchildren. Runtime-alive requirement and one-second bounded output draining remain. No dependencies added, HTTP/CLI wiring changed, or demo-server restart performed.
- Initial Phase 2 process primitive: explicit absolute executable/cwd, separate argv, cleared inherited environment with explicit entries, bounded child admission, concurrent stdout/stderr capture retaining 32 KiB tails, exit status, forced direct-child stop and reaping. Handle drop signals background cleanup while the Tokio runtime remains alive. All four native process integration tests passed, including literal argv/environment, large stdout/stderr, launch failures, forced stop, dropped handles and cancelled wait recovery. The subprocess fixture is explicitly ignored in normal runs and invoked by those tests. Final full suite passed all 33 tests and strict Clippy was clean; no dependencies added or demo-server restart required.
- This is not a full supervisor: no containment, readiness, restart policy, lifecycle identity, adapters or local control API yet. Do not use it to manage daemonizing services; signal-based shutdown is not a traffic drain. Output drain stops after one second per stream and reports incompleteness if pipes remain open or reading fails. Captured output is not automatically written to Dragon's access logs.

- Clarified current hosting support in README: static browser output and fixed responses only; no backend execution/proxy, SSR or automatic SPA index fallback. Niral remains a planned integration. No server defect was found by the malformed-traffic campaign, so no runtime change was made for this clarification.

- Malformed-traffic campaign passed on isolated macOS loopback listeners: 41 curated cases plus 35 invalid header-byte mutations, each sent whole and in 7-byte and 1-byte writes. All 228 malformed exchanges were rejected; all 228 subsequent valid requests succeeded. Appended valid pipeline requests were not served after rejection.
- Covered conflicting/invalid lengths, transfer-coding ambiguity, malformed headers/chunks/trailers, unsafe targets/hosts and header/chunk-line limits. Focused tests passed in 1.47 seconds; this deterministic corpus is not coverage-guided fuzzing. No production or existing demo listener was targeted.

- Phase 1 follow-up: three deterministic ingress tests passed across every input split and several output buffer sizes, covering ambiguous heads, forbidden trailers and chunk-line limits. This is regression coverage, not a coverage-guided fuzz campaign.
- Static pathname-replacement/socket/index tests and TCP premature-EOF, fragmented-body and repeated-concurrency tests passed. The concurrency regression completes 256 static requests across 32 cycles of 8 clients and verifies continued availability.
- These short deterministic checks do not establish descriptor/memory leak freedom, concurrent symlink-race coverage, coverage-guided fuzzing or long-soak stability. Linux CI execution is now verified; the remaining acceptance gates still apply.

- Clarified ownership of HTTP serving, runtime adapters, process management, configuration, security, and deployment.
- Documented all 21 requested architecture sections, interface proposals, configuration example, phase gates, tests, benchmarks, and risks.
- Distinguished the Phase 1 minimal HTTP MVP from Phase 4 production application hosting.
- Checked primary library documentation for recommended networking/protocol boundaries; installed versions are recorded in Cargo.lock.
- Phase 0 documentation checks passed: 21 sequential architecture sections and balanced fenced blocks. The working example TOML is tested; broader architecture interfaces remain proposals.
- Saved a repository memory checkpoint with implemented scope, verification and remaining gates.

## Review Gate

Review the technology/dependency policy, TOML schema direction, trusted-workload scope, process model, and Phase 1 acceptance criteria in the specification.

Approval received: continue toward backend hosting after Phase 1 local regression tests. Phase 2 process/runtime work has begun; public forwarding, production deployment and distributed features remain out of scope for this increment.