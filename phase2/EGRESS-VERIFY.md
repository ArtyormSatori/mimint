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
