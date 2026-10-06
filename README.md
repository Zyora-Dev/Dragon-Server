# Dragon Server

An independent, Indian-built HTTP and application server with a tested HTTP
foundation and initial managed application hosting over bounded HTTP/1.1.
Niral production SSR, assets and browser RPC have been verified locally through
Dragon. This is a development foundation, not a production hosting release.

## What Can Dragon Host Today?

Dragon serves static frontends and can launch trusted application processes that
provide a private loopback HTTP endpoint. It does not compile application code or
provide language runtimes.

| Workload | Current support |
| --- | --- |
| HTML, CSS, browser JavaScript and public assets | Static files served over HTTP; JavaScript executes in the browser |
| TypeScript or framework-based frontend | Prebuilt browser output only; Dragon does not compile or build it |
| Single-page application | Static assets supported; no automatic index fallback for client-side deep links |
| Configured JSON/text endpoint | Fixed response only, not dynamic backend logic |
| Backend executable with a loopback HTTP endpoint | Explicit process launch, readiness gating and bounded proxy; runtime-specific compatibility requires testing |
| Server-side rendering | Buffered by default; opt-in streaming HTML and SSE |
| Niral application | Production SSR, assets, hydration, guestbook RPC and session cookies verified locally; not all Niral features supported |

Configure a dedicated public output directory as a static root and an explicit
directory index such as `index.html`. Do not expose backend source or secrets as
static files: Dragon serves files, it does not execute server-side source.
Backend integration is language-neutral; FastCGI, per-request interpreters and
automatic language detection are not implemented.

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
work up to the configured shutdown deadline, then closes remaining connections
and stops managed applications. Application cleanup errors cause a failed exit.
An invalid configuration exits with status 2 before binding; a bind/runtime
failure exits with status 1.

### Host Niral

[examples/niral/dragon.toml](examples/niral/dragon.toml) is a template: replace its
absolute paths with your Node executable (22+), Niral checkout, launcher and built
application directory. Build the Niral app before starting Dragon:

```sh
node /absolute/path/to/niral/bin/niral.js build /absolute/path/to/app
cargo run --locked -- start --config /absolute/path/to/configured-dragon.toml
```

The template serves `http://127.0.0.1:8081` through Dragon and keeps Niral on
`127.0.0.1:8199`. The example launcher uses Niral's production API, checks required
hook environment variables and binds explicitly to loopback. It is a local
checkout adapter, not a stable packaged Niral integration. Its 750 ms shutdown
grace fits within Dragon's current one-second process grace.

The machine-local demo uses an isolated copy of Niral's existing site example in
ignored `target/niral-demo/site`, with configuration in `target/niral-demo/dragon.toml`.
The VS Code **Dragon: Run Niral demo** task starts it; these generated files must
exist first and are removed by `cargo clean`. The source Niral checkout is not
modified. The example's separate Python worker page has not been configured or
verified; the Node SSR, counter and guestbook are the verified demo paths.

The template enables streaming and WebSocket forwarding. An additional machine-local
demo runs at `https://localhost:8443` with a separate backend on `127.0.0.1:8198`.
Its generated configuration, copied site and seven-day self-signed certificate are
under ignored `target/niral-tls-demo`. Browsers do not automatically trust this
test certificate. The original HTTP demos are unchanged.

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

### Managed Applications

Declare up to 64 `[[applications]]` with unique `id`, a `release`, executable,
argument array, working directory, explicit environment and readiness settings.
Executable/cwd paths resolve relative to the TOML file; argument and environment
strings are passed literally. No shell expansion or inherited environment is
provided. Supply any required PATH or runtime settings explicitly. Startup does
not run builds. Each endpoint must be numeric loopback, unique, and use a port
different from Dragon's listener. Endpoint ownership remains an operator contract.

Use `action = "proxy"` and `application = "your-id"` on a route. Proxy routes
cannot also specify static or fixed-response fields. Dragon binds its listener,
starts applications sequentially and waits for all to pass startup readiness
before accepting HTTP connections. Startup failure stops previously launched
applications. SIGINT/SIGTERM can interrupt readiness. After startup, proxy routes
return 503 while the application is not Ready; already admitted requests can
still fail if the application exits. Static routes remain available after an
application exits. There are no continuous health probes.

Restart defaults to `{ mode = "never" }`. Opt in with, for example:
`restart = { mode = "on_failure", max_restarts = 3, initial_delay_ms = 100, max_delay_ms = 1000 }`.
`always` also retries successful exits. Cleanup errors prevent replacement,
including macOS `PermissionDenied` unless native inspection proves the group
contains only zombies awaiting reaping.

The proxy preserves method, raw path/query, Host, cookies and ordinary end-to-end
headers. It strips hop-by-hop headers and client-supplied forwarding headers,
sets `X-Forwarded-Host` and `X-Forwarded-Proto` to `http` or `https` according to
the listener, and does not forward a client IP. Trailers are not forwarded.
Request bodies and, by default, upstream responses are fully buffered within
configured limits; size memory budgets with request concurrency.
Each request uses one upstream connection, with no retry or pooling. Upstream
errors/oversized responses return 502; the total upstream connect/header/body
deadline returns 504. `response_timeout_ms` separately bounds downstream writing.

Set `streaming = true` on a proxy route for streaming SSR or SSE. Responses use
backpressure without the cumulative `max_proxy_response_bytes` cap. After headers,
`response_timeout_ms` bounds downstream-write inactivity, not total stream duration;
upstream failures abort the response rather than replacing it with a 502. Uploads
remain buffered and trailers are dropped. Each stream retains its admission slot.

Set `websocket = true` on a proxy route to allow validated WebSocket upgrades;
otherwise upgrades return 501. Subprotocol selection is validated, but extensions
are disabled. Tunnels retain connection/request admission, use bounded copy buffers
and close after `idle_timeout_ms` without traffic in either direction. Shutdown
closes the tunnel transport; Dragon does not generate a WebSocket close frame.
These two flags default to false and are only valid for proxy routes.

### HTTPS

Add TLS settings beneath the existing server configuration:

```toml
[server.tls]
certificate = "certificates/fullchain.pem"
private_key = "certificates/private-key.pem"
handshake_timeout_ms = 10000
```

PEM paths resolve relative to the configuration file. Each must be a regular file
of at most 1 MiB; the certificate chain must contain 1 to 32 certificates matching
the private key. Handshake timeout defaults to 10000 ms and accepts 1 to 60000 ms.
Failed or stalled handshakes release connection admission. Rustls serves TLS 1.2/1.3
with HTTP/1.1 ALPN; the configured listener no longer accepts plaintext HTTP.
Streaming and WebSockets also work over TLS. Certificate issuance, renewal, reload,
client certificates, HTTP/2 and TLS to the loopback backend are not implemented.
Protect the key using filesystem permissions and configure application secure-cookie
and trusted-proxy policy separately.

Optional `[limits]` defaults:

| Setting | Default |
| --- | ---: |
| max_connections | 1024 |
| max_in_flight_requests | 256 |
| max_headers | 100 |
| max_header_bytes | 16384 |
| max_target_bytes | 8192 |
| max_body_bytes | 1048576 |
| max_proxy_response_bytes | 16777216 |
| header_timeout_ms | 10000 |
| body_timeout_ms | 30000 |
| response_timeout_ms | 30000 |
| idle_timeout_ms | 15000 |

`server.shutdown_timeout_ms` defaults to 10000. Ordinary HTTP deadlines bound each
phase; streaming writes and WebSocket traffic use the inactivity rules above.
Connection overload closes newly accepted
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
GitHub Actions passed formatting, strict Clippy and all 50 regular tests on Ubuntu
24.04 and macOS 26 ARM64 using Rust 1.89.0 for commit `df736b0` in
[run 37480435071](https://github.com/Zyora-Dev/Dragon-Server/actions/runs/37480435071).
The normal runner ignores the separate soak test and one native subprocess
fixture explicitly invoked by the process tests. See [PROGRESS.md](PROGRESS.md)
for the run evidence. The current suite passes 61 regular tests on macOS, including
stream delivery before EOF, admission recovery, early WebSocket frames, binary/ping
echo, shutdown, trusted/untrusted TLS, handshake timeout, secure streaming, WSS
and macOS live/missing-group rejection during zombie inspection.
The opt-in real Niral smoke also passes with streaming enabled. A short macOS ASan
run replayed 3,407 corpus inputs without failure; it is not an extended protocol
fuzz campaign. The current implementation at `8ae5188` passed formatting, strict
all-target Clippy and all 60 Linux / 61 macOS regular tests with Rust 1.89.0 in
[run 37517562790](https://github.com/Zyora-Dev/Dragon-Server/actions/runs/37517562790).
Ubuntu 24.04 completed in 54 seconds; macOS completed in 1 minute 28 seconds.
Extended fuzz/soak was not rerun. Normal runs also ignore the Niral test; its
real-application smoke result above remains macOS-only.

### Optional Niral Smoke Test

With a local Niral checkout and an absolute Node executable path (Node 22+):

```sh
DRAGON_NIRAL_ROOT=/absolute/path/to/niral \
DRAGON_NODE=/absolute/path/to/node \
cargo test --locked --test process_integration application_niral_production_smoke -- --ignored --exact --nocapture
```

This opt-in test builds an isolated temporary Niral app, starts its production
server through Dragon's configured application supervisor, and checks health
readiness, server-loaded HTML, referenced production assets and 404 handling
through Dragon's HTTP listener with streaming enabled, then checks both listeners
close on shutdown.
It leaves the Niral checkout and existing applications unchanged.
The bootstrap uses Niral's production API with an explicit loopback bind rather
than its CLI, which currently binds all interfaces; it does not exercise CLI
startup preflight (unlike the example launcher). Browser hydration, RPC, WebSockets
and restarts are not covered by this automated test.

Verified locally on macOS with Node 25.6.1 on 2026-10-07: proxied HTTP checks passed,
both listeners closed and cleanup returned `Ok(())`. The smoke now requires clean
cleanup for both Niral and the test client. The previous zombie-only macOS
`PermissionDenied` is resolved by bounded native inspection, not blanket error
suppression. Genuine or unverified cleanup errors still prevent replacement.
This is not complete hosting acceptance.
The test is ignored in normal CI because Niral and Node are external prerequisites.

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
from a 9,568 KiB baseline to a sampled peak of 10,368 KiB.
On Ubuntu 24.04.5, the five-minute ASan campaign passed 2,687,267 executions;
the ten-minute soak passed 61,888 connections. Descriptors stayed at 14 in all
samples and fell to 12 after shutdown; RSS rose from 10,672 KiB to a sampled
peak of 11,904 KiB. See [PROGRESS.md](PROGRESS.md) for evidence and remaining limits.

## Current Boundaries

### Process Foundation

The library exposes `process::ProcessManager`, `ProcessSpec` and
`ProcessHandle`. Managed HTTP hosting uses it through the application layer.
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
Dragon cannot signal. On macOS only, Dragon accepts this error when two matching
native group enumerations include the unreaped leader and every listed member
reports zombie status and the expected group ID. Inspection is capped at 4096
entries; a full buffer, failed query, live member, missing leader or changed
membership leaves the original error intact. The leader remains unreaped throughout
inspection to prevent group-ID reuse. Linux signal handling is unchanged.

This primitive is for trusted, non-daemonizing workloads only. Process groups
are not containment: descendants can escape into other groups or sessions.
Dragon reaps its direct child, not arbitrary grandchildren. The primitive itself
does not drain HTTP traffic or provide an OS service identity. Startup readiness, logical
instance identity and restart policy belong to the application layer below. The
grace period bounds time before escalation, not the OS's total termination time.
The process API is currently compiled only for Linux and macOS; these lifecycle
tests have passed on both platforms in GitHub Actions. Output
draining has a one-second deadline per stream after child exit and reports
`output_complete = false` if it times out or fails. It is not a full supervisor,
hostile-code sandbox or crash-recovery mechanism.

### Application Identity And Readiness

The Linux/macOS library exposes `application::ApplicationManager`,
`ApplicationSpec`, `ReadinessSpec` and `ApplicationHandle`. This layer owns a
bounded process manager and is used by configured application hosting.
`start` validates the full application/readiness specification before spawning.
Application and release IDs accept 1-128 ASCII letters, digits, dots, hyphens and
underscores. Each application start receives a distinct process-local numeric
instance ID and process generation 1. Automatic restarts retain the instance ID
and increment the generation after each successful replacement launch.
`identity()` retains the initial identity; `current_identity()` returns a current
generation snapshot, and completion carries the final launched generation.
IDs are not PIDs, authorization credentials or durable recovery identities.

Handles expose `identity()`, `state()` and `subscribe()` for watch snapshots:
`Starting -> Running -> Ready`, then `Stopping -> Stopped` for an explicit stop.
Without a restart policy, startup failure or unexpected exit, including exit
code zero, yields `Failed`.
Snapshots may skip intermediate transitions; they are not an audit event stream.
Exit notification removes readiness before waiting for output capture to finish.
Cleanup errors also leave the state `Failed`; inspect `ApplicationOutput.process`,
including `group_cleanup_error`, even when the lifecycle `failure` is `None`.

Readiness sends HTTP/1.1 HEAD to an explicitly configured loopback IP and nonzero
port, with the socket address as Host. A complete status-200 response head passes;
redirects, other statuses, malformed/truncated or oversized headers do not.
Responses are capped at 8 KiB and 64 headers. No DNS, redirects, TLS or response
body processing is performed. The origin-form path is capped at 2,048 bytes.
The caller sets an overall startup deadline, per-attempt timeout, retry interval
(each 1 ms through 300 seconds), and attempt cap (1 through 1,000). Probe timeout
cannot exceed startup timeout. Failed attempts retry within both budgets.

`wait_ready(&mut self)` waits for startup readiness without transferring ownership;
cancelling that borrowed wait does not stop the instance. `stop()` interrupts
startup probing and waits for process cleanup. Dropping the handle or cancelling
its consuming `wait()` requests the same background cleanup. Keep Tokio alive
until cleanup completes. The startup deadline bounds probing, not subsequent OS
termination or output-drain time. Completion retains process output and typed
startup/exit failure reasons.

This is a startup-only check, not continuous readiness or liveness monitoring.
A previously ready instance is marked failed when its process exits, but a hung
live process is not detected after startup. Endpoint ownership is a trusted
operator contract: the caller must provide a private endpoint belonging to this
instance. Dragon does not yet reserve that port, authenticate probe responses,
or verify socket ownership; an unrelated local service could satisfy the probe.
The server's proxy route uses the Ready snapshot to gate public traffic. Application
revision/operation serialization, durable identity and adapters
remain future work. The readiness tests pass locally on macOS and in Linux/macOS
GitHub Actions for commit `df736b0`.

### Bounded Restarts

`start(spec)` continues to use `RestartPolicy::Never`. Opt in with
`start_with_restart(spec, RestartPolicy::OnFailure(budget))` or
`RestartPolicy::Always(budget)`, where `RestartBudget` supplies `max_restarts`,
`initial_delay` and `max_delay`. Budgets permit 1-1,000 replacement launches;
delays must be at least 1 ms, with initial delay no greater than maximum delay
and maximum delay no greater than 300 seconds. Invalid policies never spawn.

`OnFailure` retries readiness failure and unsuccessful process exit; successful
exit completes as `Stopped`, suitable for one-shot jobs. `Always` also retries
successful exits and should only be used for long-running services. Explicit
stop, handle drop and cancellation of the consuming wait never request a restart.
Cancelling the borrowed readiness wait still leaves supervision active.

The supervisor removes readiness, enters `Restarting`, and awaits process-group
cleanup, direct-child reaping and bounded output draining before `Backoff`.
The first delay is `initial_delay`; subsequent delays double up to `max_delay`.
The application retains its admission slot through cleanup and backoff. Stop
interrupts backoff, and every replacement must pass a fresh startup readiness
check. `wait_ready()` can wait across recovery states. It is still a snapshot,
not a guarantee that a ready process will remain healthy.

The budget is a lifetime limit per application handle, excluding the initial
launch. It does not reset after readiness or elapsed time. Exhaustion yields
`Failed` with `InstanceFailure::RestartLimit`; there is no automatic recovery
from this terminal state. A replacement spawn error is terminal and reported as
`RestartSpawnFailed` plus `ApplicationOutput.restart_error`. Output retains only
the last launched process's status and bounded output, not generation history.

Any process cleanup error stops recovery without launching a replacement.
Confirmed zombie-only groups no longer block macOS restart; the regression requires
all configured replacement generations. Unverified permission failures still block
recovery. Incomplete output capture is
reported separately and does not itself prevent restart after clean termination.

This library increment uses deterministic backoff and a strict lifetime budget.
The specification's proposed jitter, rolling time window and stable-readiness
budget reset are not implemented. Continuous health checks and durable recovery
remain future work. Eight native restart tests,
all 50 regular tests, formatting and strict Clippy pass locally on macOS and in
Linux/macOS GitHub Actions for commit `df736b0`. The extended fuzz/soak campaign
was not rerun for this application-layer increment.

### Hosting Release

This is a development foundation, not a production hosting release. No HTTP/2,
streaming uploads, general packaged runtime adapters, config reload, automatic
certificate management, FastCGI, compression, range requests or conditional caching yet.
Windows static serving is unsupported. Linux regression tests have passed;
bounded Linux and macOS fuzzing and resource-soak campaigns have passed.
Longer-duration soak tests, exhaustive coverage and performance benchmarks
remain acceptance work.

The [architecture specification](docs/architecture/dragon-server-specification.md)
describes the broader roadmap; its future commands and interfaces are proposals.