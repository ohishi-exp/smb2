# Why a side `stat` sometimes outlasts a whole download on the QNAP (2026-09-24)

Issue #10, item 1. On the QNAP TS-464 over Wi-Fi, about a third of unloaded 32 MiB downloads had one side `stat` that
waited at least 80% of the ~600 ms transfer, where a 4 MiB window at ~55 MB/s should bound it near 80 ms.

**Verdict: the server holds the `stat`; the download doesn't.** Every slow `stat` but one is class (b). The `smbd`
serving that connection sits blocked in the kernel (state `D`) for the whole wait while the download's READs keep
flowing, including READs sent long after the `stat`. The same stalls hit a `stat` with no download running at all, and
a connection that only sends ECHOs never stalls, so it's the metadata operation itself that blocks on this server.
The server's load sets how often it happens: with the photo app paused, the rate of `stat`s over 300 ms fell about
5× (2.9% to 0.6%) and no download had a bad run (0 of 20, against 10 of 60). The stalls didn't go away, though, and
the longest got no shorter (5.7 s), so the photo app amplifies them without being their only cause.
The read-ahead window does exactly what it should: every `stat` that wasn't held waited at most 109 ms (127 ms on the
idle NAS's slower link), behind at most 4 MiB. One slow `stat` in 400 was class (d), a client-side scheduling delay
worth a small, separate fix.

## Rig

- Same server and client as `self-tuning.md` § Real NAS validation: QNAP TS-464 (QuTS hero, ZFS, Samba 4.20), an M3
  Max laptop on Wi-Fi only, ~3.5 ms ping. The NAS was busy with its own photo library's indexing the whole time (CPU
  0% idle, load 9–14), exactly as in the original observation.
- The Wi-Fi changed under us: the first batch ran at ~7–9 MB/s (the laptop reported MCS 3), the rest at 23–70 MB/s,
  the original regime. Both show the same thing.
- The client machine was loaded too (load average 20–36 from other work), which matters for class (d) only.
- No sudo and no pcap on the client, no root on the NAS. The timeline is built from the client's `TRACE` log, a sampler
  on the NAS that needs no root, and pings.

## Method

1. **Client timeline.** `run --trace-dir` (new) keeps smb2's `TRACE` lines in memory with microsecond stamps and the
   worker thread, and writes one file per run. Two `TRACE` lines gained fields for this (library behavior unchanged):
   the writer task's `send:` line now names the frame's `msg_id`, and `recv: routed` carries the credits granted and
   the frame length. From those, `timeline` (new) rebuilds every request on the download's connection: when it left
   the writer task, when its answer started and finished arriving (`tcp: receiving` / `tcp: received`), and when it was
   routed. Each probe logs its start and end, and a probe still waiting after 100 ms logs `Connection::diagnostics()`:
   every outstanding request with `sent_age`, credits available, the send queue, and the frame arriving. Every run
   also records `credit_waits`, `credit_starvations`, and `scheduling_stalls` before and after.
2. **Server side.** `server-sample.sh` (new) runs on the NAS over ssh and records, every ~20 ms, the state letter of
   every thread of the `smbd` processes serving the bench's connections. The clocks agree to ~4 ms (best of 20 round
   trips over one ssh session).
3. **The link.** `ping -i 0.05 --apple-time` from the laptop to the NAS throughout, and in the controls also to the
   router and from a wired machine (a Raspberry Pi) to the NAS. ICMP is answered by the NAS's kernel, never by `smbd`,
   so a stall pings see is the link, and one they don't see is the server.
4. **Controls.** `probe --secs N` (new): a `stat` and an ECHO every 20 ms on each of two connections, and no download.
   `--a-echo-only` makes one connection send only ECHOs. Also run against a share on the NAS's SSD pool alongside the
   HDD-pool share the bench uses, at the same time.
5. **Runs.** `run --load-writers 0 --variants adaptive:shipping,adaptive:ref --sizes 33554432`: 8 runs on the slow
   link, then 10 and 20 runs per variant on the fast one (60 downloads, 400 side `stat`s).

### The classes, as `timeline` assigns them

For each probe: `queue` is probe start to its frame leaving the writer; `wire` is from there to the first byte of its
answer; `route` is the answer's last byte to the probe seeing it. `drain` is how long the bytes that arrived during
`wire` take at the download's own rate. `overtaken` counts READs sent after the `stat` whose answers arrived before it.

- **(a) Credits or the send queue:** `queue` is at least half the wait.
- **(d) Client scheduling:** `route` (plus routing itself) is at least half the wait.
- **(b) The server held it:** READs sent after it were answered first and `wire` exceeds the drain of the READs ahead
  of it by 100 ms or more, or the wait exceeds what arrived meanwhile by 100 ms and the pings were fine.
- **(e) The link stalled:** the same excess, with pings to the NAS 150 ms or worse in that span.
- **(c) Head-of-line:** everything else: the answer came right after the bytes ahead of it.

## Results

### Per-class counts (fast link, 60 downloads, 400 side `stat`s)

- **(a) credits:** 0. `credit_waits` and `credit_starvations` were 0 in all 68 traced runs, 504–980 credits were
  available whenever a probe was stuck, the send queue was empty, and none of 139 stuck-probe snapshots had
  `sent_age: None`. Every slow `stat` reached the wire within 0.3 ms of being issued; across all 400 the longest
  wait to reach it was 19 ms, on a `stat` that took 27 ms in total.
- **(b) server held it:** 30 of the 31 probes over 150 ms (the 28 over 300 ms included), 0.17–2.9 s.
- **(c) head-of-line:** 364 probes: p50 55 ms, p90 80 ms, max 109 ms, never more than 4 MiB ahead. This is the
  window working as designed.
- **(d) client scheduling:** 1 probe (427 ms, of which 333 ms after its answer was routed).
- **(e) link:** 0. During every held `stat`, pings to the NAS stayed under 80 ms with no loss.

Bad runs by the issue's measure (one `stat` ≥ 80% of the download's wall time): 10 of 60, five per variant
(`shipping` and `ref` alike, as before). All ten are class (b). The rate is lower than the 30–40% of the first pass,
which fits a stall rate that follows the NAS's own load.

The slow-link batch (8 downloads at 7–9 MB/s) shows the same class (b) shape: one `stat` answered only after 594 ms
while five READs sent after it came back first, and another answered 456 ms after the download's own CLOSE, with
the wire idle in between.

### Good runs against bad runs

Nothing about the download differs. Good and bad runs have the same window (peak 4 MiB), the same rate, no credit
waits, and the same ~6 probes per download. In a bad run, one of those probes happened to coincide with a server stall.
A stall can also land on the download's own open: in the worst run the download's CREATE and the first probe both
waited ~600 ms, which is also why some downloads take 1.2 s instead of 0.6 s.

### The controls: no download, same stalls

- **A `stat` alone stalls.** One connection, 60 s: 1,185 `stat`s, p50 6 ms, p99 507 ms, max 1,085 ms, 24 over
  300 ms. Across six such runs, 1.7–5.1% of `stat`s took over 300 ms and the longest took 4.4 s. At ~6 probes per
  download, that alone predicts the bad-run rates above.
- **Not the link.** During the stalls, pings from the laptop to the router and to the NAS lost nothing and peaked at
  94 ms, and pings from the wired Pi to the NAS never exceeded 1.9 ms, while SMB requests waited up to 2.7 s.
- **Not the server's CPU.** The NAS-side sampler (a shell loop forking `date` and `cut` every 20 ms) never paused
  100 ms or more, so user space on the NAS kept running.
- **The `smbd` blocks in the kernel.** In one 2.4 s stall, both bench `smbd` processes were in `D` for all ~85 samples
  of it, and went back to `S`/`R` as it ended. `smbd` is single-threaded there, so an ECHO on that connection waits as
  long as the `stat`: across the controls, ECHO and `stat` on the same connection stalled together every time.
- **The file operation causes it.** With `--a-echo-only`, the ECHO-only connection had 1 request over 300 ms in 2,250
  (390 ms), while the `stat` connection beside it had 38 in 705.
- **Not the disk pool the file lives on.** A `stat` on a share on the SSD pool stalled just as often as one on the HDD
  pool, at the same time: 21 of 1,207 against 24 of 1,046 over 300 ms, max 2.6 s against 3.2 s.

### Annotated timelines

**Class (b), the worst run** (`adaptive:shipping`, 32 MiB at 61 MB/s, wall 1,168 ms, one `stat` 2,334 ms). Times are
ms from the download's start.

```
   0.1  CREATE (download open) sent; probe 0's stat sent
 597.4  probe 0's stat answered (597 ms: smbd in D 94% of that span; the download's open waited too)
 606.2  download open answered; READs 748..804 (8 x 512 KiB = 4 MiB) sent at 606
 619.6  probe 1's stat sent (msg 812). 8 READs (4 MiB) ahead of it: ~70 ms of drain at 61 MB/s
 613-690  those 8 READs arrive, back to back
 652-1154  56 more READs, ALL sent after msg 812, are answered first (28 MiB, wire busy)
1168     download done (CLOSE answered)
1168-2953  wire idle; pings to the NAS 44 ms worst, none lost; this connection's smbd in D in 98% of samples
2953.3  msg 812 answered, all four parts in one 488-byte frame; routed and seen by the probe within 0.1 ms
```

READs keep flowing while the `smbd` is blocked. QNAP's Samba has `server kernel smbd support = yes` and `ksmbd`
kernel threads running, so the data path likely doesn't need the `smbd` main loop at all (unverified; Samba's own aio
threads would need the main loop to send). Either way, a READ doesn't wait for the blocked operation and the `stat`
does.

**Class (d)** (`adaptive:ref`, 32 MiB at 42 MB/s, one `stat` 427 ms):

```
 386    stat sent (msg 2530); 7 READs (3 MiB) ahead of it
 480    stat answered right behind them (94 ms, as the window intends); routed on worker w14
 480-811  w14 keeps running the receiver task: it routes 27 READ frames (13.5 MiB), verifying and routing each
          512 KiB frame taking 4-20 ms on the loaded client; the other workers pick up nothing
 811    the last READ answer is routed; the receiver has nothing left to read
 813    the probe task finally runs, on w14 (333 ms after its answer was routed)
```

All 400 probes resumed on the worker that routed their answer, normally within 2 ms; only three waited more than
5 ms (6, 7, and 333 ms).

## Evidence for each conclusion

- **Not credits (a):** zero credit waits anywhere, hundreds of credits free at every stuck snapshot, `sent_age`
  always set and equal to the request's age, every slow `stat` on the wire within 0.3 ms.
- **Not the window or chunk size (c):** the READs ahead of a held `stat` never exceeded 4 MiB (the controller's cap);
  unheld `stat`s waited at most 109 ms; no compound READ was involved (all streamed, `MaxReadSize` never used).
- **The server (b):** READs sent after the `stat` answered first (up to 56 of them, 28 MiB); `stat`s answered long
  after the download finished with the wire idle; the connection's own `smbd` in `D` for 64–100% of each held wait
  while the other bench connections' `smbd`s showed 0%; the same stalls with no download at all; none on an ECHO-only
  connection.
- **Not the link (e):** no ping loss and no ping over 94 ms during any stall, from the laptop or the wired Pi.
- **Client scheduling (d), once:** the answer was routed, and the probe task ran 333 ms later on the same worker,
  2 ms after the receiver on that worker went idle. It fits tokio's LIFO slot: a task the receiver wakes goes into the
  receiver's worker's LIFO slot, which other workers can't steal from, and runs only once the receiver yields. Tokio's
  coop budget makes it yield after 128 socket reads, and at several reads per 512 KiB frame that is tens of frames: on
  a client slow enough that the socket never drains, most of a download.

## With the photo app paused

David paused every job queue of the NAS's photo app, and the same controls and a traced download batch ran again on
the quieter server.

**Conditions.** The NAS's 1-minute load fell from ~10 to ~2.2 within 3 minutes, `top` showed no photo-app or ML
process using CPU (the processes stayed up, idle), the CPU was 60–90% idle, and the HDD pool went from ~50–100
reads/s to none. During the runs the load sat at 2–5 (a blocked `smbd` counts toward it too). The laptop was on a
better Wi-Fi spot: MCS 8, 432 Mbit/s transmit rate, −67 dBm, pings 3.4 ms average (worst 7.6–28 ms per run, none
lost); downloads ran at 40–45 MB/s. The wired Pi's pings to the NAS stayed under 1.8 ms, none lost.

**Controls, no download (three 60 s runs, a `stat` and an ECHO on each of two connections):**

- **`stat`s over 300 ms:** 67 of 10,597 (0.63%; per connection-run 0.34–1.03%), against 254 of 8,632 (2.9%) across
  the five comparable busy runs. Max 5.7 s, against 4.4 s busy.
- **Same shape as busy:** every stall hit the ECHO on the same connection too, often both connections at once, with no
  ping loss and no ping over 28 ms.
- **ECHO-only connection:** max 23.7 ms over 2,245 ECHOs, none over 100 ms, while the `stat` connection beside it had
  3 of 2,123 over 300 ms (max 1.2 s). Busy: 1 of 2,250 against 38 of 705.
- **`D` state:** 9.8% and 31.0% of the bench `smbd`s' samples in two runs (the sampler missed the processes in the
  third), against 58.7% busy. In the ECHO-only run, the ECHO connection's `smbd` was in `D` for 1.0% of samples and
  the `stat` connection's for 4.8%.

**Traced downloads (10 runs each of `adaptive:shipping` and `adaptive:ref`, 32 MiB, 192 side `stat`s):**

- **Bad runs:** 0 of 20, against 10 of 60 busy. The longest `stat` took 200 ms (busy: 2.9 s).
- **Classes:** 191 class (c), max 127 ms behind 4 MiB (4 MiB takes ~95 ms at 44 MB/s). One class (b) at 200 ms: the
  run's first probe, sent with the download's open, answered with the wire idle and no `smbd` in `D` in its 7 samples.
- **`D` state over the whole batch:** 4.4% of samples, against 10.4% in the busy 40-run batch.

**What it changes.** The photo app's load multiplies the stall rate about 5× and is what made the stalls show up in
a third of the downloads. It isn't the whole cause: a quiet NAS still blocks a `stat`'s `smbd` in the kernel for up
to several seconds, just rarely enough (~0.6% of `stat`s) that 20 downloads with ~10 probes each missed it. An ECHO
still never stalls, so it's still the file operation.

## What's still unknown

- **What `smbd` waits on in the kernel.** Without root, `/proc/<pid>/stack` and `wchan` are hidden. The leading
  suspect is ZFS throttling the writes Samba makes on every open and close: its lock directory (`locking.tdb`,
  `smbXsrv_open_global.tdb`, 20+ MB each) lives on the SSD pool that also holds the photo app's containers and
  database, and in one 60 s run most long stalls spanned one of that pool's ~5-second write bursts (transaction group
  syncs). Suggestive, not proven: some stalls didn't. Root on the NAS during a stall (`cat /proc/<pid>/stack`) would
  settle it.
- **What still stalls a quiet NAS.** With the photo app paused, stalls fell ~5× but kept their length (up to 5.7 s).
  The NAS still runs other services (media servers, databases, the system pool's periodic writes), and none was
  paused, so it's open whether a truly idle QNAP stalls at all. Docker Samba never has. That makes the per-open write
  path (the ZFS suspect above) more likely than anything specific to the photo app.
- **Why READs flow while `smbd` is blocked:** QNAP's kernel data path (`ksmbd` threads, `server kernel smbd support`)
  is the likely reason, but that's inferred from config and process names only.
- **Class (d)'s mechanism** is inferred from one occurrence and tokio's documented behavior, not reproduced in
  isolation.

## Suggested fix direction

1. **Item 1 needs no change to the read-ahead.** The window bounds what it controls (109 ms worst, 4 MiB ahead). A
   smaller cap would only shorten class (c), which isn't the problem. A dedicated transfer session (Cmdr #133) wouldn't
   help either: the `stat` stalls on a connection with no transfer on it.
2. **Treat it as server latency in Cmdr.** On this NAS under its own load, any metadata operation (listing, `stat`,
   open) can take 0.3–3 s, 2–5% of the time, with or without a transfer (~0.6% with the photo app paused, still up to
   5.7 s). The UI shouldn't block on one, and nothing
   should read it as a dead connection. The crate already doesn't: stalls stay far below the 5 s keepalive threshold
   and the 30 s response deadline. Note that an ECHO on the stalled connection waits too.
3. **Make the bench tell the two apart.** `probe_max` on a real NAS mostly measures the server's tail. Report the
   head-of-line wait separately (for example `timeline`'s class (c) max, or `probe_max` next to a `probe` control from
   the same session), so a later tuning pass isn't judged by server stalls.
4. **Optional, small: have the receiver task yield after routing a frame** (`rt` gets a `yield_now`), so a task it
   wakes doesn't wait behind the rest of a download on a loaded client. Worth a test that reproduces it first (a
   multi-thread runtime, a receiver that never drains the socket, a woken task timed). Rare here (1 in 400, on a
   client at load 20–36), but it's a latency a consumer can't work around.

## Reproducing

Credentials come from the maintainer's notes and are never pasted here.

```sh
cd benchmarks/read-ahead && cargo build --release
export SMB_BENCH_SHARE=<share> SMB_BENCH_USER=<user> SMB_BENCH_PASS=<pass>
B=./target/release/read-ahead-bench; H=<host>:445
$B run --prep --addr $H --rtt-ms real --load-writers 0 --runs 1 --variants compound --sizes 65536,33554432 --out /dev/null
# Server-side sampler and a ping log alongside the traced downloads
ssh <nas> "SECS=240 CLIENT=<client IP prefix> sh -s" < server-sample.sh > nas-sample.txt &
ping -i 0.05 -c 4800 --apple-time <host> > ping-nas.txt &
$B run --addr $H --rtt-ms real --load-writers 0 --runs 20 --variants adaptive:shipping,adaptive:ref \
  --sizes 33554432 --out dl.csv --trace-dir traces
wait
$B timeline --min-ms 150 --nas nas-sample.txt --nas-offset-ms <server clock minus ours> \
  --ping ping-nas.txt --tz-offset-min <UTC offset> traces/*.log
# Controls: no download; then one connection sending only ECHOs
$B probe --addr $H --secs 60
$B probe --addr $H --secs 60 --a-echo-only
```

Then delete `bench/`, `load/`, and `up/` from the share, and from its `@Recycle/`.
