# Downloads checking what's on its way against what's arrived

2026-09-24, vdavid/smb2#10 item 3. What changed: a download now raises its drained estimate of what's still on its way
to at least the READs whose answers haven't come off the wire, less what the link carried since the latest answer,
on answers to its first flight (`Window` § Closing the loop in `crates/smb2/src/client/read_ahead.rs`).

## Docker, slow start after idle on

`run.sh` with its default (the container's TCP restarts slow start after an idle period), `VARIANTS=adaptive`,
`SIZES=8388608`, `LOADS=0`, 10 runs per link. Every 8 MiB run ends with the cancel probe and a 500 ms pause, so each
download starts after an idle spell on a connection that carries the last run's rate. "Before" is the parent commit
built from a `git archive` copy (`BIN=`); both ran back to back on the same machine.

Medians of 10 (wall ms, side `stat` p50 / max ms, `stat` after cancel ms, peak in flight):

- **+60 ms, 3 MB/s**: before 3,119 ms, 162 / 408, 183, 2 MiB; after 3,118 ms, 162 / 407, 183, 2 MiB.
- **+200 ms, 3 MB/s**: before 3,537 ms, 345 / 730, 365, 2 MiB; after 3,541 ms, 345 / 729, 366, 2 MiB.
- **+60 ms, 10 MB/s**: before 1,069 ms, 143 / 371, 219, 4 MiB; after 1,074 ms, 142 / 350, 192, 3 MiB.
- **+200 ms, 10 MB/s**: before 1,487 ms, 361 / 619, 384, 4 MiB; after 1,493 ms, 225 / 619, 274, 4 MiB.

Throughput is within 0.5% everywhere. The gain shows where slow start takes the most round trips to reach the link's
rate (+200 ms at 10 MB/s: the median side `stat` 38% shorter, the `stat` after a cancel 29% shorter). At 3 MB/s the
target is under one chunk and the first flight is one or two READs, so there's little surplus to take back.

## Simulator

- `a_download_that_starts_in_slow_start_leaves_no_standing_queue` (+60 / +200 ms at 3, 10, and 30 MB/s): before, the
  queue after the ramp stood at 868 KB against a 270 KB target (+60 ms, 3 MB/s), 2.6 MB against 690 KB (+200 ms,
  3 MB/s), and 2.8 MB against 900 KB (+60 ms, 10 MB/s). After, every case stays within the target plus one chunk.
- `compare_headroom_candidates`: every `shipping` row identical before and after.
- Correcting on every answer instead (what uploads do) is the rejected alternative: through 150 ms server freezes
  every second at 20 MB/s it ran 7% slower (idle 407 → 1,014 ms) with the median `stat` at 56 ms instead of 137, and
  5% slower through 60 ms jitter. The open-loop surplus had been covering the next stall, so whether to trade it for
  the shorter queue is a tuning question for the grid (its `dstall` and `djitter` groups), not part of this fix.
