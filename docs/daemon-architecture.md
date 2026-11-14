# dotfilesync daemon architecture (issue #10)

Status: implemented as the foundation for epic #3 (issue #10). This document describes what
exists in the code today, and marks the parts reserved for later issues.

## 1. Process model

```
 dotfilesync (CLI) ─┐                      ┌─ dotfilesync-core (scan/status/plan/apply, brew, configs)
 future GPUI app  ──┼── dotfilesync-protocol ──► dotfilesyncd ─┤
                    │   (typed client, UDS)                 └─ job manager, locks, approvals, journal, logs
                    └── standalone fallback for read-only commands and `plan` (same core::ops functions)
```

| Crate | Role | Platform gate |
|---|---|---|
| `dotfilesync-core` | Business logic. `core::ops` holds the scan/status orchestration that both the daemon and the standalone CLI call, so there is one implementation. | `unix` (was macOS-only) |
| `dotfilesync-protocol` | Wire types, framing, version handshake, data-dir layout and checks, peer-credential lookup, and the blocking `Client`. No server code. | `unix` |
| `dotfilesync-daemon` | `dotfilesyncd` binary plus a library: listener, connection handling, job manager, resource locks, approval contract, journal, rotated logs, launchd service management. | macOS (product gate) |
| `dotfilesync-cli` | `dotfilesync` binary. A client of the daemon for scan/status/jobs, with standalone fallback, plus `service` commands. | macOS (product gate) |

- There is one daemon per user and per data directory. A single-instance lock enforces this (see §3).
- Clients never run jobs themselves when they use the daemon. The standalone fallback runs the same
  `core::ops` code in-process for scan, status, and plan when no daemon is running, and the CLI says
  which mode it used. `apply` has no standalone path: the journal, backups, and locks live in the daemon.
- The daemon owns its jobs. When a client disconnects, the jobs it started keep running.
- Nothing listens on TCP or UDP, nothing runs as root, and no endpoint executes caller-supplied commands.

### Compile gates

`core` and `protocol` now compile on any Unix, which keeps #12 possible and lets the protocol be
unit-tested anywhere. The CLI and daemon keep the macOS-only `compile_error!`, because this is not
the Linux service implementation. The existing CI job that checks unsupported platforms still
expects `cargo check --workspace` to fail on Linux and Windows. A new CI job checks that `core` and
`protocol` build on Linux.

## 2. Filesystem layout

The data directory defaults to `~/Library/Application Support/dotfilesync`. `DOTFILESYNC_DATA_DIR`
or `--data-dir` overrides it, and tests always use the override.

```
<data>/            0700  owned by the daemon user
  run/             0700
    dotfilesyncd.sock   0600  Unix-domain socket
    dotfilesyncd.lock   0600  flock() single-instance lock
  state/           0700
    jobs.jsonl          0600  job journal (metadata only; no tokens, paths, or plan bodies)
    runs/            0700
      <run_id>/      0700  one apply attempt
        journal.jsonl     append-only step journal
        plan.json         copy of the plan that was applied
        meta.json    0600  run id, plan id, job id, status (never an approval token)
  backups/         0700
    <run_id>/             files replaced by that run, relative to home
  logs/            0700
    dotfilesyncd.log    0600  rotated: .1 .. .3, 1 MiB each
```

When the daemon creates or opens any directory it uses `lstat`, so symlinks are not followed. It
refuses to start if a directory is:

- not a real directory, or
- not owned by the effective UID, or
- accessible to group or others (`mode & 0o077 != 0`).

It never silently changes the permissions of a directory that already exists. The error tells the
user what to fix. Clients run the same check (without creating anything) before they connect, so
they will not talk to a socket planted in a directory someone else controls.

`scan`/`status` never create the data directory. Only the daemon does, when it starts.

## 3. Socket lifecycle and single instance

1. Validate or create `<data>/run` (0700).
2. Open `dotfilesyncd.lock` (`O_NOFOLLOW`, 0600) and take a non-blocking exclusive `flock`.
   - If the lock is already held, another daemon owns this data dir. Exit with `AlreadyRunning`
     and do not touch its socket.
   - The kernel releases the lock when the process dies, so a crash cannot leave a stale lock.
3. Holding the lock proves that any existing socket path is stale. The daemon unlinks it only
   when all of these hold:
   - `lstat` says it is a socket (not a symlink, regular file or directory),
   - it is owned by the effective UID, and
   - a connect probe gets `ECONNREFUSED`.

   Otherwise it refuses to start and leaves the path alone.
4. Bind, `chmod 0600`, then verify mode and owner with `lstat`. Record the socket's `(dev, ino)`.
5. On shutdown, unlink the socket only if the path still has the recorded `(dev, ino)`.

macOS limits Unix socket paths to 104 bytes. Longer paths fail with an explicit error.

## 4. Authentication

- After `accept`, the daemon reads the peer's effective UID from the kernel: `getpeereid(2)` on
  macOS, `SO_PEERCRED` on Linux. If it differs from the daemon's own `geteuid()`, the daemon sends
  `unauthorized` and closes. No UID field exists in any request, so the daemon cannot be tricked
  into trusting one.
- The client checks the same thing in reverse: the daemon's peer UID must equal the client's
  effective UID. Together with the directory checks, this stops a client from talking to an
  impostor.
- The credential lookup sits behind the `PeerAuthenticator` trait. Tests use a real lookup for the
  same-UID case. They inject an authenticator that reports a different UID for the wrong-UID case,
  because CI has no second OS user. **Limitation:** we have not tested against a real second user.
  In production the daemon always uses the OS lookup, and the test hook cannot be reached from the
  wire.

## 5. Protocol (version 1)

Framing: each frame is a 4-byte big-endian length followed by a UTF-8 JSON body.

| Limit | Value |
|---|---|
| Request frame (client → daemon) | ≤ 64 KiB |
| Response or notification frame (daemon → client) | ≤ 16 MiB |
| Handshake read timeout | 5 s |
| Idle read timeout between requests | 300 s |
| Write timeout | 10 s |
| Concurrent connections | 32; extra connections get a `busy` error and are closed |

An oversized length prefix is rejected before any allocation.

### Handshake

The first frame must be `{"type":"hello","protocol_version":1,"client":"<name>"}`. The daemon
replies with `hello_ack` (protocol version, daemon version, pid). If the client asked for an
unsupported version, it replies `error{code:"incompatible_version", supported:[1], requested:N}`
and closes. Anything other than a hello also gets an error and the connection is closed.

### Requests

Each request is `{"type":"request","id":<u64>,"op":"<name>","params":{...}}`. The daemon decodes
`id`/`op` before it decodes `params`, so even a request with malformed params gets an error carrying
the right `id`.

The daemon sends four kinds of frame:

| Frame | Shape |
|---|---|
| Handshake ack | `{"type":"hello_ack",...}` |
| Response | `{"type":"response","id":N,"outcome":{"ok":{"response":"<kind>",...}}}` or `{"outcome":{"err":{code,message,details?}}}` |
| Notification | `{"type":"notification","id":<watch request id>,"notification":{"kind":"progress"\|"heartbeat",...}}` |
| Connection error | `{"type":"error","error":{...}}`, always followed by close |

A JSON body that has no usable `id` gets a connection-level `malformed_request` error and the
connection is closed.

| op | Effect |
|---|---|
| `ping` | Liveness check |
| `daemon_info` | Version, pid, uptime, data dir, job counts |
| `start_job` | Start a typed job: `scan` or `status`, with absolute paths. Returns the job snapshot. |
| `job_status` | Get one job's snapshot |
| `list_jobs` | List jobs, including `interrupted` ones from the previous run |
| `job_progress` | Progress events with `seq > since_seq` (lets a client catch up after reconnecting) |
| `watch_job` | Stream `progress` notifications until the job finishes, then a final response |
| `job_result` | Typed result from a successful job (`ScanReport`, `StatusReport`, `Plan`, or apply `RunJournal`) |
| `cancel_job` | Request cancellation (idempotent) |
| `shutdown` | Graceful stop: cancel jobs, wait, unlink socket |
| `plan.create` | Start a `plan` job from `PlanInput`. Result is `JobOutput::Plan { plan, plan_id }`. |
| `plan.approve` | Bind a single-use token to a stored plan. Params: `plan_id`, `digest` (the digest the client reviewed). Returns `{token, expires_at_ms}`. |
| `plan.apply` | Recompute the state fingerprint, redeem `token`, start an `apply` job. Params: `plan_id`, `token`, `allow_sudo`. Returns the job snapshot. |
| `plan.resume` | Redeem a new token and continue a run. Params: `run_id`, `token`, `allow_sudo` (optional, default false). |
| `plan.runs` | List runs, including `interrupted` ones from the previous daemon process. |
| `plan.run_status` | One run's journal and status. |

Reserved ops return `unsupported_operation` with the issue that will implement them. They never
return success:

| Reserved op | Issue |
|---|---|
| `runtime.*` | #5 |
| `config.*` | #11 |
| `sync.*` | #6 / #7 |

An unknown op returns `unknown_operation`.

Errors are typed as `{code, message, details?}`. The codes are:

- `incompatible_version`, `unauthorized`, `handshake_required`, `malformed_request`, `frame_too_large`
- `unknown_operation`, `unsupported_operation`, `invalid_params`
- `job_not_found`, `job_not_finished`, `job_failed`, `busy`, `shutting_down`, `internal`
- `plan_not_found`, `approval_rejected` (`details.reason` is `unknown_token`, `expired`,
  `already_used`, `plan_mismatch`, `state_changed`, `wrong_owner`, or `entropy`)

Clients decode unknown codes as `unknown`, so a newer daemon can add codes without breaking older clients.

The daemon never accepts executable paths. The Homebrew binary comes from the daemon's own
configuration: `--brew-bin` / `DOTFILESYNC_BREW_BIN` when it starts, otherwise PATH and the
standard locations. The CLI's hidden `--brew-bin` test flag therefore forces standalone mode.

## 6. Jobs

- **Job ID:** `j-` followed by 16 random hex characters from `/dev/urandom`.
- **States:** `queued → running → succeeded | failed | cancelled`. A job journaled as started by a
  previous daemon process that never recorded a terminal state is shown as `interrupted`.
- **Execution:** each job runs on its own worker thread, away from the connection threads, so a slow
  `brew` does not block other clients. At most 16 jobs can be active at once; more returns `busy`.
- **Progress:** each event has a monotonic `seq`, a stage and a message. A job keeps its last 256
  events. Watchers block on a condvar and wake on every new event.
- **Disconnect:** a closed client connection only ends that client's watch. Jobs stop only through
  `cancel_job` or daemon shutdown.
- **Cancellation:** sets the job's cancel flag.
  - The job checks the flag between stages.
  - The core `RealCommandRunner` takes the flag through `with_cancellation(...)`. When the flag is
    set, it kills the process group and descendants it owns (the same cleanup as the timeout path
    from #2) and returns `RunnerError::Cancelled`.
  - A cancelled job never reports a partial result as success.
- **Retention:** finished jobs and their in-memory results stay up to 64 jobs. Scan and status
  reports are not written to disk. Apply writes its own journal and backups under `state/runs` and
  `backups` (see §2); that journal is what `plan.run_status` reads after a reconnect or restart.

## 7. Resource locks

`ResourceLocks` serializes access per resource:

- **Shared** locks for reads. Scan, status, and plan take shared locks on `homebrew` and
  `manifest:<path>`, so they run concurrently.
- **Exclusive** locks for apply and resume, on `homebrew` and `home:<path>`. An exclusive lock
  waits for every holder of the same key, and only that key.

Locks are acquired in a sorted order, which prevents deadlock. Waiting gives up when the job is
cancelled.

## 8. Plan/approval contract

`dotfilesync_daemon::approval` binds a single-use token to a core plan (`dotfilesync_core::Plan`).

- **Binding:** plan id, resource (`home:<path>`), the core plan digest (`Plan::digest()`), the
  state fingerprint the plan was computed from, and the owner UID. The daemon keeps the newest 32
  plans in memory, keyed by plan id. Restart drops them, except plans reloaded from
  `state/runs/*/plan.json` so an interrupted run can be approved again.
- **Issuing:** `plan.approve` checks that the client-supplied digest equals the stored plan, then
  `issue` returns a 256-bit random token. TTL is 10 minutes. The store keeps only `sha256(token)`,
  mapped to `{plan digest, owner uid, state fingerprint, expiry, consumed}`. Approving a new plan
  for the same home revokes older tokens for that home.
- **Redeeming:** `plan.apply` re-runs planning observation and redeems against that fingerprint.
  A mismatch is `approval_rejected` / `state_changed` and burns the token. `plan.resume` does not
  re-observe: a partial apply has already changed the machine, so the token is checked against the
  fingerprint stored with the plan. Resume still requires a new approval. Redeem fails with one of:
  - `unknown_token`
  - `expired`
  - `already_used` (replay)
  - `plan_mismatch` (the plan changed, or the reviewed digest does not match)
  - `state_changed` (the observed state moved since planning)
  - `wrong_owner`

  A failed check against a live token invalidates that token, so it cannot be retried with
  corrected inputs.
- **Storage:** tokens exist only in memory and in the caller's hands. They are never journaled or
  logged, and `Debug` prints `ApprovalToken(<redacted>)`. A daemon restart invalidates every
  approval.
- **Runs:** an apply job keeps going if the client disconnects. The journal and backups stay under
  the data directory. A run whose steps were unfinished when the daemon stopped is `interrupted`
  and can be continued with `plan.resume`.

## 9. Journal, restart recovery and logs

- **Journal:** `jobs.jsonl` is append-only. Each record holds `{job_id, kind, event, status, at}`.
  It contains no params, paths, results, config bodies, tokens or subprocess output. At startup the
  daemon:
  1. loads the journal,
  2. turns every job that has `started` without a terminal event into `interrupted`, and appends an
     `interrupted` record,
  3. compacts the journal to the newest 200 jobs (write to a temp file, then rename, 0600).

  Interrupted jobs are reported but **never replayed**. This is a separate case from client
  disconnect (§6): a disconnect leaves the job running, while a daemon crash interrupts it.
- **Logs:** `dotfilesyncd.log` is 0600. It rotates at 1 MiB and keeps three old files. Every line
  first passes through `redact()`, which masks:
  - GitHub tokens (`ghp_`, `gho_`, `ghs_`, `ghu_`, `ghr_`, `github_pat_`),
  - `Bearer` / `Basic` credentials,
  - `key=value` pairs whose key looks like a password, secret, token or API key,
  - URL userinfo.

  On top of that, the daemon logs only event metadata. It never logs request params, reports or
  subprocess output.
- **launchd:** launchd's stdout/stderr go to `/dev/null`, because the rotated log is the only log.

## 10. Service lifecycle (macOS launchd)

`dotfilesync service <cmd>` manages a per-user LaunchAgent with label
`io.github.jamallyons.dotfilesyncd`, stored at `~/Library/LaunchAgents/<label>.plist`. Commands are
run through core's `CommandRunner` with `/bin/launchctl`, so tests inject a recording runner and
never touch the real launchd.

| Command | Behavior |
|---|---|
| `install [--start-at-login] [--daemon-path P] [--force]` | Writes the plist only. It does **not** load or start the daemon. `RunAtLoad` equals `--start-at-login` and defaults to false; there is no `KeepAlive`. It refuses to replace a plist with different content unless `--force` is given. |
| `start` | `launchctl bootstrap gui/<uid> <plist>` if the agent is not loaded, then `kickstart`, then waits up to 5 s for the socket to answer `ping`. |
| `stop` | If loaded: `launchctl bootout gui/<uid>/<label>`, so launchd sends SIGTERM and the daemon shuts down gracefully. If not loaded but a daemon is reachable: IPC `shutdown`. |
| `status` | Reports three separate facts: plist installed (and whether it starts at login), agent loaded in launchd, and daemon reachable (version and pid). |
| `uninstall` | Unloads the agent if loaded, then removes the plist only if it is a regular file containing our label. It never deletes the data dir, user configurations or sync repositories, and it prints where they are. |

Notes:

- Because `RunAtLoad` is false and there is no `KeepAlive`, an installed plist is loaded at login
  but not started. Starting at login is truly opt-in.
- `scan`/`status` never install, load or start the service.

## 11. CLI modes

`scan`, `status`, and `plan` take `--mode auto|daemon|standalone`, default `auto`. `apply` always
uses the daemon.

| Mode | Behavior |
|---|---|
| `auto` | Uses the daemon if it is reachable. If it is not running (no socket, or `ECONNREFUSED`), the CLI runs standalone and prints a one-line note on stderr. Version, auth and permission failures are errors, never a silent fallback. |
| `daemon` | Requires the daemon. `--detach` prints the job ID and returns. Ctrl-C detaches without cancelling. |
| `standalone` | Never connects to the daemon. |

- stdout (human or `--json`) contains the same report in either mode. Daemon mode sends absolute
  paths, so paths in the report are absolute.
- `import-brewfile` always runs locally. It writes a new file only with `--output`, and it refuses
  to overwrite an existing file.
- `dotfilesync jobs list|show|watch|result|cancel` let you reconnect to jobs. `jobs follow` is an
  alias of `jobs watch`.
- `dotfilesync plan [--manifest] [--profile NAME] [--json]` prints one line per action
  as `[risk] [rev|irrev] [copy|link|brew|ensure|fisher|shell] summary`, then totals. `--json` prints
  the plan document. Two runs against the same machine produce the same JSON.
- `dotfilesync apply [--manifest] [--profile] [--yes] [--allow-sudo] [--dry-run]`
  prints the plan, then approves and applies it through the daemon. Without `--yes`, a TTY asks
  `Apply N actions? [y/N]`. A non-TTY without `--yes` is a dry run and exits 0. `--dry-run` never
  applies. `--allow-sudo` is the only way a login-shell step may run. Ctrl-C detaches; the job
  keeps running and can be followed with `dotfilesync jobs follow <id>`.
- `dotfilesync apply --resume <run_id>` approves the run's plan again and continues it.
  `dotfilesync runs [list|show <id>]` reads run status from the daemon.

## 12. Out of scope (later issues)

- Toolchains (#5), the config editor (#11), sync (#6) and the watcher (#7).
- GPUI (#8) and Linux systemd (#12).
- A Windows service and any remote API.
- KeepAlive or crash auto-restart policies.
