# Dragon Server

An independent, Indian-built HTTP and application server with a locally tested
Phase 1 HTTP foundation and initial Phase 2 process primitives. Backend hosting
and Niral integration are not yet available features.

## What Can Dragon Host Today?

Dragon currently serves static frontends, not full-stack applications. Its Rust
implementation does not imply support for executing Rust application code.

| Workload | Current support |
| --- | --- |
| HTML, CSS, browser JavaScript and public assets | Static files served over HTTP; JavaScript executes in the browser |
| TypeScript or framework-based frontend | Prebuilt browser output only; Dragon does not compile or build it |
| Single-page application | Static assets supported; no automatic index fallback for client-side deep links |
| Configured JSON/text endpoint | Fixed response only, not dynamic backend logic |
| Node.js/TypeScript, Python, PHP, Java, Go, Rust or .NET backend | Not yet: no application execution, process management or reverse proxy |
| Server-side rendering | Not yet: requires a backend runtime |
| Niral full-stack application | Planned; runtime integration has not been inspected or implemented |

Configure a dedicated public output directory as a static root and an explicit
directory index such as `index.html`. Do not expose backend source or secrets as
static files: Dragon serves files, it does not execute server-side source.
The intended product supports frontend and backend hosting through
language-neutral integration; that backend capability is future work.

## Run Locally

Requires Rust 1.89 or newer with Cargo on macOS or Linux. From the repository root:

```sh
cargo run --locked -- start --config examples/minimal/dragon.toml
```

The example listens on `127.0.0.1:8080`:

```sh
curl -i http://127.0.0.1:8080/hello
curl -i http://127.0.0.1:8080/assets/
curl -I http://127.0.0.1:8080/assets/index.txt
```

Stop with Ctrl+C or SIGTERM. Dragon stops accepting connections, drains existing
work up to the configured shutdown deadline, then closes remaining connections.
An invalid configuration exits with status 2 before binding; a bind/runtime
failure exits with status 1.

## Configuration

The working schema is illustrated by [examples/minimal/dragon.toml](examples/minimal/dragon.toml).
Unknown fields, conflicting hosts/routes, invalid limits and unsafe paths are
rejected. Configuration reads are capped at 1 MiB. Static roots are relative to
the configuration file. Root directories must already exist.

Host matching prefers the full authority, then hostname, then a configured `*`.
Exact routes take precedence over the longest segment-boundary prefix. GET
automatically permits HEAD. Static directory indexes require an explicit `index`;
there is no directory listing. Hidden paths and symlinks beneath the static root
are denied using descriptor-relative file access. Serve only trusted, dedicated
public directories; filesystem permissions and hard-link policy remain the
operator's responsibility.

Optional `[limits]` defaults:

| Setting | Default |
| --- | ---: |
| max_connections | 1024 |
| max_in_flight_requests | 256 |
| max_headers | 100 |
| max_header_bytes | 16384 |
| max_target_bytes | 8192 |
| max_body_bytes | 1048576 |
| header_timeout_ms | 10000 |
| body_timeout_ms | 30000 |
| response_timeout_ms | 30000 |
| idle_timeout_ms | 15000 |

`server.shutdown_timeout_ms` defaults to 10000. Deadlines bound each phase rather
than resetting with every byte. Connection overload closes newly accepted
sockets; request overload returns 503. Malformed framing and raw header-limit
violations may close the connection without an HTTP error response. Conflicting
Transfer-Encoding and Content-Length are rejected before routing, including on
keep-alive connections. Transfer coding support is limited to chunked.

Logs are JSON on stderr, with a bounded 1024-entry nonblocking queue. Overflow
drops log entries; a periodic `log_overflow` event reports the cumulative count
when the sink can accept it. Log delivery is best-effort, not an audit guarantee.
Request logs include generated IDs, selected route, status, duration and
`body_bytes_produced`, not guaranteed socket-delivered bytes. Request headers,
query strings and bodies are not logged.

## Verification

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

Tests cover configuration, routing, static confinement and pathname replacement,
framing across every split in selected fixtures, fragmented and pipelined requests,
premature EOF recovery, repeated concurrent connections, limits, overload,
deadlines, CLI errors and Unix shutdown. Short deterministic regressions do not
replace coverage-guided fuzzing, resource-leak measurement or long soak tests.
GitHub Actions passed formatting, strict Clippy and all 36 tests on Ubuntu
24.04.5 x86-64 and macOS ARM64 using Rust 1.89.0 for commit `ecc1894`.
One native subprocess fixture is ignored by the normal runner and explicitly
invoked by the process tests. See [PROGRESS.md](PROGRESS.md) for the run evidence.

### Extended Validation

Run the resource test in isolation so other tests do not contaminate measurements:

```sh
DRAGON_SOAK_SECONDS=600 cargo test --locked --test http_integration sustained_traffic_has_bounded_resources -- --ignored --exact --nocapture
```

It warms up 20 batches, then sustains 16 concurrent clients using streamed static
responses, chunked/pipelined requests, conflicting framing and body timeouts.
Every five seconds it checks open descriptors and process RSS against the warmed
baseline: at most four additional descriptors and 16 MiB RSS growth. It also
checks response bodies, recovery and resource release after shutdown. The default
duration is ten minutes; `DRAGON_SOAK_SECONDS` accepts 1 through 86,400 seconds.
The test is ignored by normal runs and requires Unix descriptor inspection and `ps`.
RSS covers the test process, including both server and clients; it is not a heap
leak detector or a throughput benchmark.

Coverage-guided fuzzing uses a separate development-only package:

```sh
rustup toolchain install nightly-2026-09-27 --profile minimal
cargo +nightly-2026-09-27 install cargo-fuzz --version 0.13.2 --locked
mkdir -p fuzz/corpus/ingress_policy
cargo +nightly-2026-09-27 fuzz run ingress_policy fuzz/corpus/ingress_policy fuzz/seeds -- -max_total_time=300 -max_len=65536 -timeout=10 -rss_limit_mb=2048 -seed=20260927 -print_final_stats=1
```

The target compiles the real ingress guard and exercises byte preservation,
progress, path/host normalization and TOML parsing/validation under AddressSanitizer.
Generated inputs belong in the first, ignored corpus directory; checked-in seeds
are a separate input directory. Crash artifacts remain under `fuzz/artifacts`.
The manual **Extended validation** GitHub Actions workflow runs these same bounded
campaigns on Linux and retains logs/artifacts for 14 days. These campaigns do not
prove leak freedom, exhaustive protocol coverage or production readiness.

On macOS, the five-minute ASan campaign completed 3,555,327 executions without a
crash or invariant failure. The ten-minute soak passed 61,408 connections:
descriptors stayed at 12 in all samples and fell to 10 after shutdown; RSS rose
from a 9,568 KiB baseline to a sampled peak of 10,368 KiB. Extended Linux results
are pending. See [PROGRESS.md](PROGRESS.md) for evidence and remaining limits.

## Current Boundaries

### Process Foundation

The library now exposes `process::ProcessManager`, `ProcessSpec` and
`ProcessHandle`. This is not yet wired into the CLI, HTTP configuration or routes.
It launches an explicit absolute executable with separate arguments and an
absolute working directory, without an implicit shell or inherited environment.
Callers must explicitly supply required environment variables.

Child admission is bounded. Standard input is closed, and stdout/stderr are
drained concurrently, each retaining only the latest 32 KiB with a truncation
flag. `wait` returns the exit status and captured output. On Linux and macOS,
each child starts in its own process group. `stop` sends group SIGTERM, waits
for the leader to exit, then sends group SIGKILL before reaping the direct child.
The default grace period is one second; `with_shutdown_timeout` accepts a period
from 1 ms to 60 seconds. `shutdown_escalated` means the leader exceeded that
period, not merely that leftover descendants received SIGKILL. Natural leader
exit also triggers immediate group cleanup. Exit observation keeps the leader
unreaped until the final group signal, preventing reuse of its PID during cleanup.
Dropping a handle or cancelling its wait
requests the same background cleanup; keep the Tokio runtime alive until cleanup
finishes. Captured application output may contain secrets and is not emitted to
Dragon's HTTP logs.

`group_cleanup_error` reports a failed final group signal separately from the
direct child's exit status. Dragon still attempts direct-child termination and
reaping. Inspect this field: macOS can return `PermissionDenied` for a group
containing only zombies, but the same error may indicate surviving processes
Dragon cannot signal. It is not silently treated as successful group cleanup.

This primitive is for trusted, non-daemonizing workloads only. Process groups
are not containment: descendants can escape into other groups or sessions.
Dragon reaps its direct child, not arbitrary grandchildren. There is no traffic
drain, readiness, restart policy, service identity or runtime adapter yet. The
grace period bounds time before escalation, not the OS's total termination time.
The process API is currently compiled only for Linux and macOS; these lifecycle
tests have passed on both platforms in GitHub Actions. Output
draining has a one-second deadline per stream after child exit and reports
`output_complete = false` if it times out or fails. It is not a full supervisor,
hostile-code sandbox or crash-recovery mechanism.

### Hosting Release

This is a development foundation, not a production hosting release. No TLS,
HTTP/2, reverse proxy, CLI-managed applications, runtime adapters, config reload,
WebSockets, FastCGI, compression, range requests or conditional caching yet.
Windows static serving is unsupported. Linux regression tests have passed;
bounded macOS fuzzing and resource-soak campaigns have passed. Extended Linux
campaigns, longer-duration soak tests and performance benchmarks remain
acceptance work.

The [architecture specification](docs/architecture/dragon-server-specification.md)
describes the broader roadmap; its future commands and interfaces are proposals.