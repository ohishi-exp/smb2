# rt: the async runtime underneath the client

Everything that needs a running reactor (spawning, timers, sockets) goes through this module, on tokio or smol, or on a
single-threaded wasm32 host (Cloudflare Workers). Crate-private. The public face is three Cargo features, `tokio`
(default), `smol`, and `wasm`, and the crate docs' "Async runtime" section.

## Files

- `mod.rs`: `backend()`, `spawn` / `TaskHandle`, `sleep` / `sleep_until` / `Sleep`, `timeout` / `timeout_at` /
  `Elapsed`, `yield_now`, and `Instant`
- `net.rs`: `resolve`, `TcpStream` (split into `ReadHalf` / `WriteHalf`), `UdpSocket` (the KDC client's). Only with
  `tokio` or `smol`: the wasm build has no sockets
- `wasm.rs`: the wasm32 backend's `spawn` (`spawn_local` inside an `Abortable`) and `Sleep` (`setTimeout`, re-armed
  until the clock passes the deadline)
- `mod.rs` also has `std_time`: std's `Instant` / `SystemTime`, or `web-time`'s on wasm32
- `tests.rs`: each contract scenario is one async fn, run once under `#[tokio::test]` and once under `smol::block_on`
- The end-to-end proof lives elsewhere: `client/smol_runtime_tests.rs` (real loopback sockets, no tokio runtime) and
  `tests/smol_integration.rs` (Docker Samba, smol-only build)

## How the backend is picked

Per call: a tokio runtime in reach (`Handle::try_current`) wins, anything else runs on smol. With one feature on
there's nothing to decide. With both on, which feature unification makes common (one crate in a consumer's graph asks
for `smol`), each consumer still gets the runtime it's running on. A connection's socket, tasks, and timers are all
created from the same context, so one connection never mixes the two.

- **smol means "any executor".** smol's reactor and global executor run on threads of their own (`SMOL_THREADS`, one
  by default), so the smol backend also works under `futures::executor::block_on`, `pollster`, and so on.
- A tokio-only build called outside a tokio runtime panics with a message naming the `smol` feature. Tokio's own
  panic only names tokio, which is what sent issue #1's reporter looking in the wrong place.
- **On wasm32 with `wasm` on, it's always the wasm backend.** Tokio's and smol's reactors don't build there, so there is
  nothing to decide. `wasm` on any other target does nothing (it only pulls in wasm32-only dependencies), so
  `--all-features` and docs.rs are unaffected.

## Gotchas

- ❌ **Nothing outside this module may call tokio's runtime APIs** (`tokio::spawn`, `tokio::time::{sleep, timeout}`,
  `tokio::net`, `tokio::io`). They compile fine in a default build and panic on a smol user. `tokio::sync` is the
  exception and stays everywhere: channels, semaphores, and `Notify` work on any executor. The guard is the smol-only
  build (`just clippy`, CI): it leaves tokio's `time`, `net`, `io-util`, and `rt` features off, so a stray call fails
  to compile. Tests are exempt; they drive the far end of sockets with tokio on purpose.
- ❌ **`TaskHandle` drop detaches; only `abort()` stops the task.** That's tokio's contract, and the connection's four
  background tasks rely on it (parked in `Inner`, aborted by `Inner::drop`). smol's `Task` cancels on drop, so the
  smol arm detaches explicitly. Never hold a raw `smol::Task` in the crate: dropping one silently stops a writer or
  receiver task. Pinned by `a_dropped_handle_leaves_the_task_running` on both backends.
- **`WriteHalf` shuts the write side down on drop, on both backends.** Tokio's `OwnedWriteHalf` does that itself;
  smol's halves are clones of one socket, so the smol arm calls `shutdown(Write)`. The socket-lifetime guarantees in
  `client/CLAUDE.md` § Socket lifetime assume it. Pinned on smol by `client/smol_runtime_tests.rs`.
- **`timeout` takes its deadline when called, not when first polled**, and polls the future before the timer, so a
  finished future wins at a passed deadline. Both are tokio's semantics; the tests pin them.
- **`Instant` is tokio's whenever the `tokio` feature is on**, because the timing tests (`download_tests`,
  `upload_tests`) run on tokio's paused clock, and tokio's `Instant` reads the std clock outside a runtime. In a
  smol-only build it's std's. Use `rt::Instant` everywhere, never `tokio::time::Instant` or `std::time::Instant` for
  anything a timer compares against.
- **The test suite needs the `tokio` feature.** It is written against tokio's paused clock, so a smol-only build is
  compile-checked (lib and examples), not tested. The smol behavior is tested under `--features smol` (both on) by
  the files above, and against real Samba by `tests/smol_integration.rs` on a smol-only build.
- **`yield_now` is a plain self-wake, with no backend switch.** It works on any executor (smol's own `yield_now` is
  the same thing), and tokio requeues a task that woke itself at the back of the local queue, behind the LIFO slot,
  which is the whole point: the receiver task yields once per routed frame so the caller it woke gets to run
  (`client/CLAUDE.md` § Connection internals). ❌ Don't swap in `tokio::task::yield_now`: it defers the wake until the
  worker next polls the I/O driver, measured at 5–13 µs a call against ~40 ns for the self-wake.
- `Sleep` boxes tokio's timer so `Sleep` is `Unpin` and races with `futures_util::future::select` after a plain
  `pin!`. One allocation per timer is noise next to the frame each one guards.

- ❌ **Nothing outside tests may call `std::time::Instant::now` or `SystemTime::now`.** On wasm32-unknown-unknown both
  build and panic on first use. Use `rt::Instant` for anything a timer compares against and `rt::std_time::{Instant,
  SystemTime}` for the rest (liveness bookkeeping, log throughput, DFS TTLs, the NTLM and Kerberos timestamps).
  `std_time` is std's own on every other target, so native behavior doesn't move. The wasm clippy build in CI catches a
  type mismatch, not a stray `std::time::Instant::now()` on its own, so grep for it.
- **wasm tasks are `Send` anyway.** `spawn` keeps its `Send` bound on every backend so one set of task bodies builds
  everywhere. The one wasm type that isn't `Send`, gloo's `TimeoutFuture`, sits in `send_wrapper::SendWrapper`, which
  panics if it is ever touched from another thread instead of misbehaving, and needs no `unsafe` here. A consumer's JS
  socket has the same problem one level up; see `transport/CLAUDE.md` § Transport factories.
- **The wasm `Sleep` re-arms until the clock agrees.** `setTimeout` takes a signed 32-bit millisecond delay and fires at
  once for anything bigger, and `Duration::MAX` means "never" here, so one timer can't express a far deadline. Firing
  is a reason to look at the clock, not a verdict. Measured in `wrangler dev` (workerd, 2026-09-30): 10 / 100 / 1000 ms
  timers advance `performance.now()` by 13 ms / 100 ms / 1.002 s, and the connection's 1 s keepalive tick and 20 s / 30 s
  stale-request warnings fire on schedule.
- **No wasm test harness.** The contract tests run on tokio and smol; the wasm backend is compile-checked (CI clippy)
  and exercised end to end by `examples/workers-probe` under `wrangler dev`.

## Decisions

- **Decision:** an internal backend switch, not a public `Runtime` trait. **Why:** the runtimes that fit this crate's
  design (`Send` tasks that migrate between threads, borrowed read buffers) are tokio and smol, and smol already
  covers "any executor". wasm is the third arm rather than a reason for the trait: it needs only a spawner and a timer,
  and the sockets it lacks come in through `ClientConfig::transport_factory`, a public seam that already existed in
  spirit (`Connection::from_transport`). The io_uring runtimes (glommio, monoio, compio) are thread-per-core with owned buffers and
  would need deeper changes than a trait. A public trait would also hand consumers the detach-versus-cancel trap
  above. If a real third runtime shows up, making this module's surface a public trait is additive.
- **Decision:** `tokio::sync` stays a hard dependency. **Why:** it's executor-independent, and swapping it for
  `async-channel` / `async-lock` would touch every hardened concurrency path in `client/` for no behavior change.
