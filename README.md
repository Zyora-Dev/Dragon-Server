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
CI runs these checks on Linux and macOS. A configured CI workflow is not evidence
that either remote runner has passed; see [PROGRESS.md](PROGRESS.md) for results.

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
tests have been run locally on macOS, not Linux. Output
draining has a one-second deadline per stream after child exit and reports
`output_complete = false` if it times out or fails. It is not a full supervisor,
hostile-code sandbox or crash-recovery mechanism.

### Hosting Release

This is a development foundation, not a production hosting release. No TLS,
HTTP/2, reverse proxy, CLI-managed applications, runtime adapters, config reload,
WebSockets, FastCGI, compression, range requests or conditional caching yet.
Windows static serving is unsupported. Linux execution, fuzzing, extended soak
tests and performance benchmarks remain acceptance work until recorded as run.

The [architecture specification](docs/architecture/dragon-server-specification.md)
describes the broader roadmap; its future commands and interfaces are proposals.