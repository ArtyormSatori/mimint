# Mimint Phase 2 — Egress Verification Record

Date: 2026-11-17. Machine: vega (Debian, user `adminis`, no sudo used).
Full log: `egress-verify.log` held with the Phase 2 coordinator evidence
(not in this repo). This file states the method exactly and summarizes
the results so a reviewer can reproduce them.

## Method — read this first

**No packet capture was performed.** `tcpdump`, `tshark`, and `dumpcap`
are not installed on vega, an `AF_PACKET` raw socket fails with
`PermissionError: [Errno 1] Operation not permitted` for this user, and
sudo is out of scope. Any claim of "packet capture" for Phase 2 would
be false.

Egress was verified three equivalent ways instead:

1. **Syscall tracing (strongest).** A user-land `strace` 6.13 binary
   (Debian package unpacked with `dpkg-deb -x`, no install, no
   privileges) traced `connect()` syscalls of the real `mimint-cli`
   binary during real runs. `connect()` is the syscall that opens a
   socket; if no `connect()` targets a non-loopback address, no packet
   left the machine from this process.
2. **Recording loopback stub.** The stub server logs every request it
   receives; request counts prove how many HTTP calls a turn made and
   that all of them hit loopback.
3. **Integration tests** (`crates/mimint-core/tests/egress.rs`): denial
   of non-loopback URLs returns instantly (before any socket/DNS),
   redirects to external URLs are not followed, and proxy environment
   variables are ignored.

Structural guarantee behind all three: `reqwest::Client::builder`
appears exactly once in the workspace
(`crates/mimint-core/src/egress.rs`), building a client with redirects
disabled (`Policy::none()`) and proxies disabled (`no_proxy()`). Every
provider sends HTTP only through `GuardedHttpClient`, which validates
the URL against the fixed egress policy before building or sending the
request.

## Results

- **Run A — Privacy-ON turn through a loopback stub:** exit 0, 2 steps,
  `notebook_add` executed and verified. The trace shows exactly 2
  `connect()` calls, both `AF_INET` to `127.0.0.1:18098`, 0
  non-loopback `connect()` calls, 0 `AF_UNIX` `connect()` calls. The
  stub log shows exactly 2 `POST /v1/chat/completions` requests.
- **Run B — Privacy-ON attempt with `--base-url https://example.com`:**
  exit 1 with a Privacy Mode error, and the trace shows **0**
  `connect()` calls: the denial happens before any socket opens.
- **Cargo suites** `egress` (5 tests), `claude_stub` (3), `fallback`
  (6): all pass, including the pre-socket denial timing test and the
  redirect/proxy hardening tests.

## What this does NOT prove

- It does not prove anything about DNS: the traced runs used IP
  literals. Hostname-based endpoints (e.g. `localhost`) resolve via the
  system resolver; the policy check runs on the hostname string before
  resolution, and only the exact strings `127.0.0.1`, `localhost`,
  `::1` count as loopback.
- It does not cover processes other than `mimint-cli` itself. A
  compromised or buggy provider binary outside this process is out of
  scope; the kill-switch governs this process's HTTP only.
- A future packet capture (with `CAP_NET_RAW` on a machine where it is
  available) would still be a useful independent check; it was not
  possible here.

## Fix-pass re-verification (2026-11-17)

The Phase 2 review fix pass added a second enforcement layer: when
Privacy Mode is on, the reqwest client uses a custom DNS resolver
(`LoopbackGuardResolver` in `crates/mimint-core/src/egress.rs`) that
checks the RESOLVED address. `localhost` resolves directly to
127.0.0.1 with no DNS query (a poisoned `/etc/hosts` cannot redirect
it); any other name resolving to a non-loopback address is refused
before `connect()`. The hostname-string check stays as the first
layer. The single-client-site rule is now enforced mechanically:
`clippy.toml` disallows `reqwest::Client::builder` /
`reqwest::Client::new` everywhere except `GuardedHttpClient::new`
(verified by negative control: removing the `#[allow]` makes clippy
fail with `disallowed_methods`).

Re-verified with the same strace method (strace 6.13 user-land binary
at `mimint-run/strace-try/usr/bin/strace`, `connect()` tracing of the
real binary; no packet capture, same limits as above):

- **Run A — Privacy-ON turn through a `127.0.0.1` stub:** exit 0, 2
  steps, `notebook_add` executed and verified. Exactly 2 `connect()`
  calls, both `AF_INET` to `127.0.0.1:18098`, 0 non-loopback.
- **Run A2 — Privacy-ON turn through a `localhost` stub:** exit 0, 2
  steps, `notebook_add` executed and verified. Exactly 2 `connect()`
  calls, both to `127.0.0.1:18099`: the resolver short-circuit is
  visible at the syscall layer (the URL said `localhost`, the socket
  went to 127.0.0.1).
- **Run B — Privacy-ON attempt with `--base-url https://example.com`
  and a `[cost]` opt-in (so the request reaches the Privacy check):**
  exit 1 with a Privacy Mode error, **0** `connect()` calls.
- **Run B2 — same attempt with no config file:** exit 1 with the
  cost-lock error (absent `[cost]` locks paid backends), **0**
  `connect()` calls: the cost gate fires before the Privacy check.
- **Fake-resolver unit tests** (in `egress.rs`): a resolver returning
  a public IP is refused when Privacy is on, passes when off; a mixed
  loopback+public answer is refused; `localhost` never consults the
  inner resolver (0 inner calls across 3 spellings).

Updated limits: the DNS gap from the original verification is now
closed for the resolve path itself (resolved addresses are checked,
and `localhost` bypasses DNS entirely). The guarantee still covers
only this process's own connections; a local proxy or another
process forwarding traffic onward is still out of scope, as is what
a loopback backend itself does with received data.

## Reproduce

```sh
# From the repo root on vega (strace binary under mimint-run/strace-try):
STRACE=/home/adminis/mimint-run/strace-try/root/usr/bin/strace
./target/debug/examples/stub-server 18098 &  # in one shell
$STRACE -f -s 128 -e trace=connect -o trace.log \
  ./target/debug/mimint-cli --base-url http://127.0.0.1:18098 \
  --model stub --auto-approve --db /tmp/e.db --audit /tmp/e.jsonl \
  "Save a note titled Stub note."
grep "connect(" trace.log  # expect only 127.0.0.1 targets
```
