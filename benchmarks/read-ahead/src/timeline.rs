//! `timeline [--detail] [--min-ms N] LOG...`: where each side `stat` spent its
//! time, from the files `run --trace-dir` writes.
//!
//! For every probe it reads, on the download's connection:
//!
//! - **queue**: probe start to the stat's frame leaving the writer task (credit
//!   reservation plus the writer queue; class (a) when it's most of the wait).
//! - **ahead**: the READ answers still owed when the stat went out, and how
//!   many of them the server sent before the stat's answer anyway.
//! - **overtaken**: READs sent AFTER the stat but answered before it (the server
//!   reordering the stat behind newer work, class (b)).
//! - **drain**: how long the bytes that arrived between the stat's send and the
//!   start of its answer would take at the download's own rate. Close to the
//!   wait means the stat was simply behind that many bytes (class (c)); a wait
//!   far above it means the wire sat idle meanwhile (the server held it, or
//!   the link stalled).
//! - **route**: the answer's last byte to the probe seeing it (client-side
//!   scheduling, class (d)).
//!
//! `--detail` also prints every request on the connection in send order.
//! `--nas FILE --nas-offset-ms N` adds each server `smbd` thread's share of
//! time in `D` during the wait (from `server-sample.sh`, N = the server's clock
//! minus ours), and `--ping FILE --tz-offset-min N` the worst `ping
//! --apple-time` round trip to the server meanwhile (N = our UTC offset), which
//! tells a link stall from a server one. The `class` column is the diagnosis's
//! (a)–(e), in `results/stat-stall-diagnosis.md`.

use std::collections::BTreeMap;

#[derive(Default, Clone, Debug)]
struct Req {
    cmd: String,
    sent: Option<f64>,
    /// When the frame carrying its answer started and finished arriving.
    frame_start: Option<f64>,
    frame_end: Option<f64>,
    routed: Option<f64>,
    frame_len: u64,
    credits: u32,
}

#[derive(Default, Debug)]
struct Probe {
    start: f64,
    end: Option<f64>,
    /// The stat compound's first MessageId (its CREATE).
    msg: Option<u64>,
    stuck: Option<String>,
}

/// Microseconds to milliseconds, relative to `t0`.
fn ms(t: f64, t0: f64) -> f64 {
    (t - t0) / 1000.0
}

fn field<'a>(msg: &'a str, key: &str) -> Option<&'a str> {
    let i = msg.find(key)? + key.len();
    let rest = &msg[i..];
    let end = rest.find([',', ' ', ']']).unwrap_or(rest.len());
    Some(&rest[..end])
}

/// Samples of the server's `smbd` processes (`server-sample.sh`): (Unix µs on
/// the server's clock, (thread id, state letter) per thread).
struct NasSamples(Vec<(f64, Vec<(u32, u8)>)>);

impl NasSamples {
    fn load(path: &str) -> NasSamples {
        let text = std::fs::read_to_string(path).unwrap();
        let rows = text
            .lines()
            .filter(|l| !l.starts_with('#'))
            .filter_map(|l| {
                let mut it = l.split(' ');
                let ns: f64 = it.next()?.parse().ok()?;
                let states = it
                    .filter_map(|f| {
                        let mut p = f.split(':');
                        Some((p.next()?.parse().ok()?, *p.next()?.as_bytes().first()?))
                    })
                    .collect();
                Some((ns / 1000.0, states))
            })
            .collect();
        NasSamples(rows)
    }

    /// Per `smbd` thread, the share of samples in `[from, to)` (Unix µs,
    /// client clock) it spent in `D`: `tid:D%` for each, and the sample count.
    fn d_share(&self, from: f64, to: f64, offset_us: f64) -> String {
        let rows: Vec<&Vec<(u32, u8)>> = self
            .0
            .iter()
            .filter(|(t, _)| t - offset_us >= from && t - offset_us < to)
            .map(|(_, s)| s)
            .collect();
        if rows.is_empty() {
            return "-".to_string();
        }
        let mut per: BTreeMap<u32, (usize, usize)> = BTreeMap::new();
        for row in &rows {
            for &(tid, s) in row.iter() {
                let e = per.entry(tid).or_default();
                e.1 += 1;
                if s == b'D' {
                    e.0 += 1;
                }
            }
        }
        per.iter().map(|(tid, (d, n))| format!("{tid}:{}", d * 100 / n)).collect::<Vec<_>>().join(" ")
            + &format!(" ({} samples)", rows.len())
    }
}

/// A `ping --apple-time` log to the server from the client: (Unix µs the echo
/// was sent, round trip ms, icmp_seq). Tells a link stall from a server one:
/// ICMP is answered by the server's kernel, never by `smbd`.
struct Pings(Vec<(f64, f64, u64)>);

impl Pings {
    /// `day_us`: Unix µs of the local midnight the log's times count from.
    fn load(path: &str, day_us: f64) -> Pings {
        let text = std::fs::read_to_string(path).unwrap();
        let rows = text
            .lines()
            .filter_map(|l| {
                let (clock, rest) = l.split_once(' ')?;
                let mut hms = clock.split(':');
                let h: f64 = hms.next()?.parse().ok()?;
                let m: f64 = hms.next()?.parse().ok()?;
                let s: f64 = hms.next()?.parse().ok()?;
                let rtt: f64 = field(rest, "time=")?.parse().ok()?;
                let seq: u64 = field(rest, "icmp_seq=")?.parse().ok()?;
                let received = day_us + ((h * 60.0 + m) * 60.0 + s) * 1e6;
                Some((received - rtt * 1000.0, rtt, seq))
            })
            .collect();
        Pings(rows)
    }

    /// The worst round trip among echoes sent in `[from, to)`, and how many
    /// sequence numbers in that span never came back.
    fn worst(&self, from: f64, to: f64) -> String {
        let inside: Vec<&(f64, f64, u64)> = self.0.iter().filter(|p| p.0 >= from && p.0 < to).collect();
        let (Some(lo), Some(hi)) = (inside.iter().map(|p| p.2).min(), inside.iter().map(|p| p.2).max()) else {
            return "-".to_string();
        };
        let lost = (hi - lo + 1) as usize - inside.len();
        format!("{:.0} ms, {} lost of {}", inside.iter().map(|p| p.1).fold(0.0, f64::max), lost, hi - lo + 1)
    }
}

pub(crate) fn run(args: &[String]) {
    let detail = args.iter().any(|a| a == "--detail");
    let min_ms: f64 = crate::arg(args, "--min-ms", "0").parse().unwrap();
    let nas = args.iter().any(|a| a == "--nas").then(|| NasSamples::load(&crate::arg(args, "--nas", "")));
    // The server's clock minus ours.
    let offset_us: f64 = crate::arg(args, "--nas-offset-ms", "0").parse::<f64>().unwrap() * 1000.0;
    // The client's UTC offset, for the ping log's local clock times.
    let tz_min: f64 = crate::arg(args, "--tz-offset-min", "0").parse().unwrap();
    let ping_path = args.iter().any(|a| a == "--ping").then(|| crate::arg(args, "--ping", ""));
    let files: Vec<&String> = {
        let mut skip = false;
        args.iter()
            .filter(|a| {
                if skip {
                    skip = false;
                    return false;
                }
                if ["--min-ms", "--nas", "--nas-offset-ms", "--ping", "--tz-offset-min"].contains(&a.as_str()) {
                    skip = true;
                    return false;
                }
                !a.starts_with("--")
            })
            .collect()
    };
    for f in files {
        one(f, detail, min_ms, nas.as_ref(), offset_us, ping_path.as_deref(), tz_min);
    }
}

fn one(path: &str, detail: bool, min_ms: f64, nas: Option<&NasSamples>, offset_us: f64, ping_path: Option<&str>, tz_min: f64) {
    let text = std::fs::read_to_string(path).unwrap();
    let mut reqs: BTreeMap<u64, Req> = BTreeMap::new();
    let mut probes: BTreeMap<u32, Probe> = BTreeMap::new();
    let mut header = Vec::new();
    let (mut dl_start, mut dl_end) = (None, None);
    // The frame currently arriving: (start, end, len).
    let mut frame: (f64, Option<f64>, u64) = (0.0, None, 0);
    // A probe whose stat hasn't been matched to a send yet.
    let mut pending_probe: Option<u32> = None;
    let mut credit_lines = 0;
    let mut epoch = 0.0f64;
    for line in text.lines() {
        if let Some(h) = line.strip_prefix("# ") {
            if let Some(e) = h.strip_prefix("epoch_unix_us=") {
                epoch = e.parse().unwrap();
            } else {
                header.push(h.to_string());
            }
            continue;
        }
        let mut parts = line.splitn(5, ' ');
        let t: f64 = match parts.next().and_then(|s| s.parse().ok()) {
            Some(t) => t,
            None => continue,
        };
        let _thread = parts.next();
        let _level = parts.next();
        let target = parts.next().unwrap_or("");
        let msg = parts.next().unwrap_or("");
        if target == "bench" {
            if msg.starts_with("download start") {
                dl_start = Some(t);
            } else if msg.starts_with("download end") {
                dl_end = Some(t);
            } else if let Some(rest) = msg.strip_prefix("probe start seq=") {
                let seq: u32 = rest.parse().unwrap();
                probes.insert(seq, Probe { start: t, ..Default::default() });
                pending_probe = Some(seq);
            } else if msg.starts_with("probe end seq=") {
                let seq: u32 = field(msg, "seq=").unwrap().parse().unwrap();
                probes.get_mut(&seq).unwrap().end = Some(t);
            } else if msg.starts_with("probe stuck seq=") {
                let seq: u32 = field(msg, "seq=").unwrap().parse().unwrap();
                probes.get_mut(&seq).unwrap().stuck = Some(msg.to_string());
            }
            continue;
        }
        if msg.starts_with("credits:") {
            credit_lines += 1;
        } else if msg.starts_with("send: cmd=") {
            let cmd = field(msg, "cmd=").unwrap().to_string();
            let Some(id) = field(msg, "msg_id=").and_then(|s| s.parse::<u64>().ok()) else { continue };
            let bytes: u64 = msg.split(", ").nth(2).and_then(|s| s.split(' ').next()).and_then(|s| s.parse().ok()).unwrap_or(0);
            // A stat is the only 4-op compound here: CREATE + 2 QUERY_INFO + CLOSE, 456 bytes.
            if cmd == "Create" && bytes > 300 {
                if let Some(seq) = pending_probe.take() {
                    probes.get_mut(&seq).unwrap().msg = Some(id);
                }
            }
            let r = reqs.entry(id).or_default();
            r.cmd = cmd;
            r.sent = Some(t);
        } else if let Some(l) = msg.strip_prefix("tcp: receiving frame, len=") {
            frame = (t, None, l.parse().unwrap());
        } else if msg.starts_with("tcp: received frame") {
            frame.1 = Some(t);
        } else if msg.starts_with("recv: routed msg_id=") {
            let id: u64 = field(msg, "msg_id=").unwrap().parse().unwrap();
            let r = reqs.entry(id).or_default();
            if r.cmd.is_empty() {
                r.cmd = field(msg, "cmd=").unwrap_or("?").to_string();
            }
            r.frame_start = Some(frame.0);
            r.frame_end = frame.1;
            r.frame_len = frame.2;
            r.routed = Some(t);
            r.credits = field(msg, "credits_granted=").and_then(|s| s.parse().ok()).unwrap_or(0);
        }
    }
    let t0 = dl_start.unwrap_or(0.0);
    let t_end = dl_end.unwrap_or(f64::MAX);

    // The download's own rate: READ bytes over the span they arrived in.
    let reads: Vec<&Req> = reqs.values().filter(|r| r.cmd == "Read" && r.frame_start.is_some()).collect();
    let read_bytes: u64 = reads.iter().map(|r| r.frame_len).sum();
    let first = reads.iter().filter_map(|r| r.frame_start).fold(f64::MAX, f64::min);
    let last = reads.iter().filter_map(|r| r.frame_end).fold(0.0, f64::max);
    let rate = read_bytes as f64 / ((last - first) / 1e6); // bytes/s

    println!("\n## {path}");
    for h in &header {
        println!("# {h}");
    }
    println!(
        "download {:.0} ms, READ bytes {} at {:.1} MB/s over the arrivals, credit log lines {}",
        (t_end - t0) / 1000.0,
        read_bytes,
        rate / 1e6,
        credit_lines
    );
    let pings = ping_path.map(|p| {
        let tz_us = tz_min * 60e6;
        let day = 86_400e6;
        Pings::load(p, ((epoch + tz_us) / day).floor() * day - tz_us)
    });
    println!("| probe | class | start | lat ms | queue ms | wire→ans start ms | ans start→routed ms | route→probe ms | READs ahead (answered first) | bytes ahead KiB | arrived meanwhile KiB | drain ms | overtaken | idle wire ms | smbd D % | ping worst |");
    println!("|---|---|---:|---:|---:|---:|---:|---:|---|---:|---:|---:|---:|---:|---|---|");
    for (seq, p) in &probes {
        let (Some(end), Some(id)) = (p.end, p.msg) else { continue };
        if (end - p.start) / 1000.0 < min_ms {
            continue;
        }
        let Some(r) = reqs.get(&id) else { continue };
        let (Some(sent), Some(fs)) = (r.sent, r.frame_start) else { continue };
        // The stat is four requests (CREATE, two QUERY_INFOs, CLOSE); a server
        // may split their answers, and the probe needs all four.
        let parts: Vec<&Req> = (id..id + 4).filter_map(|i| reqs.get(&i)).collect();
        let Some(routed) = parts.iter().filter_map(|q| q.routed).reduce(f64::max) else { continue };
        let fe = parts.iter().filter_map(|q| q.frame_end).fold(fs, f64::max);
        // READs owed when the stat went out.
        let ahead: Vec<(&u64, &Req)> = reqs
            .iter()
            .filter(|(_, q)| q.cmd == "Read" && q.sent.is_some_and(|s| s < sent) && q.routed.is_none_or(|x| x > sent))
            .collect();
        let ahead_first = ahead.iter().filter(|(_, q)| q.frame_start.is_some_and(|x| x < fs)).count();
        let ahead_bytes: u64 = ahead.iter().filter(|(_, q)| q.frame_start.is_some_and(|x| x < fs)).map(|(_, q)| q.frame_len).sum();
        let overtaken = reqs
            .values()
            .filter(|q| q.cmd == "Read" && q.sent.is_some_and(|s| s > sent) && q.frame_start.is_some_and(|x| x < fs))
            .count();
        // Bytes whose frames arrived (fully or partly) between send and the answer's start,
        // and the idle wire time in that span: gaps between consecutive frames.
        let mut frames: Vec<(f64, f64, u64)> = reqs
            .values()
            .filter_map(|q| Some((q.frame_start?, q.frame_end?, q.frame_len)))
            .collect();
        frames.sort_by(|a, b| a.0.total_cmp(&b.0));
        frames.dedup_by(|a, b| a.0 == b.0);
        let mut meanwhile = 0u64;
        let mut idle = 0.0;
        let mut cursor = sent;
        for &(s, e, len) in &frames {
            if e <= sent || s >= fs {
                continue;
            }
            let span = (e - s).max(1.0);
            let overlap = (e.min(fs) - s.max(sent)).max(0.0);
            meanwhile += (len as f64 * overlap / span) as u64;
            if s > cursor {
                idle += s - cursor;
            }
            cursor = cursor.max(e);
        }
        if fs > cursor {
            idle += fs - cursor;
        }
        let drain_ms = meanwhile as f64 / rate * 1000.0;
        let d_share = nas.map_or("-".to_string(), |n| n.d_share(epoch + sent, epoch + fs, offset_us));
        let ping = pings.as_ref().map_or("-".to_string(), |p| p.worst(epoch + sent, epoch + fs));
        let ping_bad = pings.as_ref().is_some_and(|p| {
            let w = p.0.iter().filter(|x| x.0 >= epoch + sent && x.0 < epoch + fs).map(|x| x.1).fold(0.0, f64::max);
            w >= 150.0
        });
        // What the wait is mostly made of (see the module doc and the classes
        // in `results/stat-stall-diagnosis.md`).
        let lat = (end - p.start) / 1000.0;
        let wire = (fs - sent) / 1000.0;
        let ahead_drain = ahead_bytes as f64 / rate * 1000.0;
        let class = if (sent - p.start) / 1000.0 >= lat / 2.0 {
            "a"
        } else if (end - fs) / 1000.0 >= lat / 2.0 {
            "d"
        } else if overtaken > 0 && wire - ahead_drain >= 100.0 {
            "b"
        } else if wire - drain_ms >= 100.0 {
            if ping_bad { "e" } else { "b" }
        } else {
            "c"
        };
        println!(
            "| {seq}{} | {class} | {:.0} | {:.1} | {:.1} | {:.1} | {:.1} | {:.1} | {} ({}) | {} | {} | {:.0} | {} | {:.1} | {d_share} | {ping} |",
            if p.stuck.is_some() { "*" } else { "" },
            ms(p.start, t0),
            (end - p.start) / 1000.0,
            (sent - p.start) / 1000.0,
            (fs - sent) / 1000.0,
            (routed - fs) / 1000.0,
            (end - routed.max(fe)) / 1000.0,
            ahead.len(),
            ahead_first,
            ahead_bytes / 1024,
            meanwhile / 1024,
            drain_ms,
            overtaken,
            idle / 1000.0
        );
    }
    if detail {
        println!("\n| msg | cmd | sent ms | ans start ms | ans end ms | routed ms | frame KiB | credits |");
        println!("|---:|---|---:|---:|---:|---:|---:|---:|");
        let mut by_send: Vec<(&u64, &Req)> = reqs.iter().filter(|(_, r)| r.sent.is_some()).collect();
        by_send.sort_by(|a, b| a.1.sent.unwrap().total_cmp(&b.1.sent.unwrap()));
        let f = |x: Option<f64>| x.map_or("-".to_string(), |x| format!("{:.1}", ms(x, t0)));
        for (id, r) in by_send {
            println!(
                "| {id} | {} | {} | {} | {} | {} | {} | {} |",
                r.cmd,
                f(r.sent),
                f(r.frame_start),
                f(r.frame_end),
                f(r.routed),
                r.frame_len / 1024,
                r.credits
            );
        }
    }
}
