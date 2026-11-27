//! An offline model of what the title's fences wait for behind the relay,
//! before and after `VICTORY_MERGE_SPREAD` and `VICTORY_FETCH_IN_FLIGHT`
//! (`cargo test --release -p victory-gpu fence_latency_offline -- --nocapture`).
//!
//! Not a measurement of the game: a frame is made up as the 7800X3D log of
//! 2026-11-27 describes it (17 flights, some 22 MB in 340 pieces of 64 KiB,
//! the card busy 1-4 ms of a 35 ms frame, the relay's fetch and merge rates
//! from that log), and the relay's landings are played out on a clock. The
//! segment cuts and parts are the layer's own (`segment_ends`,
//! `segment_parts`, `fetch_parts_of`); the rates are the model's.

use super::{fetch_parts_of, segment_ends, segment_parts};

/// The model's rates, from the log: the fetch's fixed cost (a list sent on
/// the copy queue, its fence, the wake), its bytes over the bus, the merge
/// (7.7 ms a frame for about 20 MB), copies in the PC's memory and the rest
/// of a landing, and the card copying a piece out inside its flight.
const FETCH_US: f64 = 450.0;
const BUS_US_PER_MB: f64 = 90.0;
const MERGE_US_PER_MB: f64 = 370.0;
const PC_US: f64 = 250.0;
const REST_US: f64 = 60.0;
const OUT_US_PER_MB: f64 = 45.0;
const PIECE_MB: f64 = 0.0625;

struct Flight {
    done: f64,
    pieces: usize,
    /// Tells right behind it.
    tells: usize,
}

/// One made-up frame of fixed seed `seed`.
fn frame(seed: u64) -> Vec<Flight> {
    let mut s = seed | 1;
    let mut next = move |n: u64| {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        s % n
    };
    let mut flights = Vec::new();
    let mut card = 0.0f64;
    let mut sent: Vec<f64> = (0..17).map(|_| next(30_000) as f64).collect();
    sent.sort_by(f64::total_cmp);
    // A few heavy flights (the skinning) and many light ones: 340 pieces.
    let weights: Vec<u64> = (0..17).map(|i| if i % 6 == 2 { 40 + next(30) } else { 1 + next(12) }).collect();
    let sum: u64 = weights.iter().sum();
    for (i, at) in sent.into_iter().enumerate() {
        card = card.max(at) + 60.0 + next(180) as f64;
        flights.push(Flight { done: card, pieces: (weights[i] * 340 / sum) as usize, tells: usize::from(next(2) == 0) });
    }
    flights
}

#[derive(Default)]
struct Result {
    /// Card done to told, over the tells: sum, most, count; the relay's
    /// time landing.
    sum: f64,
    most: f64,
    tells: f64,
    busy: f64,
}

/// Plays the relay over `flights` (each one's card time widened by its copy
/// inside the flight with `in_flight`), with `parts` fetch parts, segments
/// told one by one with `spread`.
fn play(flights: &[Flight], parts: usize, spread: bool, in_flight: bool, out: &mut Result) {
    let done: Vec<f64> = {
        let mut late = 0.0;
        flights
            .iter()
            .map(|f| {
                if in_flight {
                    late += f.pieces as f64 * PIECE_MB * OUT_US_PER_MB;
                }
                f.done + late
            })
            .collect()
    };
    let (mut relay, mut next) = (0.0f64, 0usize);
    while next < flights.len() {
        let begun = relay.max(done[next]);
        let landed = (next..flights.len()).take_while(|&i| done[i] <= begun).count();
        let mine = &flights[next..next + landed];
        // The line as the layer sees it: a flight, its tells.
        let line: Vec<Option<u64>> =
            mine.iter().enumerate().flat_map(|(i, f)| std::iter::once(Some(i as u64 + 1)).chain(std::iter::repeat_n(None, f.tells))).collect();
        let ends = if spread { segment_ends(&line) } else { vec![line.len()] };
        // Pieces, in the line's order, and where each segment's end.
        let titles: Vec<(usize, usize)> = mine.iter().flat_map(|f| (0..f.pieces).map(|_| (0usize, 64 << 10))).collect();
        let mut seg_pieces = Vec::new();
        let (mut place, mut count) = (0usize, 0usize);
        let mut fi = 0;
        for &end in &ends {
            while place < end {
                if line[place].is_some() {
                    count += mine[fi].pieces;
                    fi += 1;
                }
                place += 1;
            }
            seg_pieces.push(count);
        }
        // Titles of distinct blocks, so that jobs are cut as for a frame.
        let titles: Vec<(usize, usize)> = titles.iter().enumerate().map(|(i, t)| (i << 16, t.1)).collect();
        let (jobs, cuts) = if spread {
            segment_parts(&titles, &seg_pieces, parts)
        } else {
            let jobs = crate::land::jobs(&titles);
            let cuts = fetch_parts_of(&jobs, &titles, parts);
            (jobs, cuts)
        };
        // When each part is in, and merged; then each segment is told.
        let mut clock = begun + 20.0 + titles.len() as f64 * 0.5;
        let fetched_from = clock + if in_flight { 0.0 } else { FETCH_US };
        let mut arrived = fetched_from;
        let mut merged_at = Vec::new();
        for part in &cuts {
            let pieces = jobs[part.start].start..jobs[part.end - 1].end;
            let mb = pieces.len() as f64 * PIECE_MB;
            if !in_flight {
                arrived += mb * BUS_US_PER_MB;
            }
            clock = clock.max(arrived) + mb * MERGE_US_PER_MB;
            merged_at.push((pieces.end, clock));
        }
        let told_at = |upto: usize, clock: &mut f64| {
            *clock = clock.max(merged_at.iter().find(|m| m.0 >= upto).map_or(fetched_from, |m| m.1));
            *clock += PC_US / ends.len() as f64 + REST_US;
            *clock
        };
        let mut clock = fetched_from;
        let mut from = 0;
        let mut fi = 0;
        for (s, &end) in ends.iter().enumerate() {
            let at = told_at(seg_pieces[s], &mut clock);
            let mut ahead = None;
            for item in &line[from..end] {
                match item {
                    Some(_) => {
                        ahead = Some(done[next + fi]);
                        fi += 1;
                    }
                    None => {
                        let waited = at - ahead.unwrap_or(begun);
                        out.sum += waited;
                        out.most = out.most.max(waited);
                        out.tells += 1.0;
                    }
                }
            }
            from = end;
        }
        out.busy += clock - begun;
        relay = clock;
        next += landed;
    }
}

/// Before and after, printed (offline, modelled), with the time the layer's
/// own cutting takes on such a frame.
#[test]
fn fence_latency_offline() {
    let frames: Vec<Vec<Flight>> = (0..200u64).map(|f| frame(0x5eed_0000 + f)).collect();
    let n = frames.len() as f64;
    println!("fence latency, offline model (17 flights, ~22 MB in ~340 pieces a frame, 200 frames):");
    let mut base = 0.0;
    for (name, parts, spread, in_flight) in [
        ("old, 1 part", 1, false, false),
        ("old, VICTORY_FETCH_PARTS=4 (default)", 4, false, false),
        ("VICTORY_MERGE_SPREAD=1", 4, true, false),
        ("VICTORY_FETCH_IN_FLIGHT=1", 4, false, true),
        ("both", 4, true, true),
    ] {
        let mut r = Result::default();
        for f in &frames {
            play(f, parts, spread, in_flight, &mut r);
        }
        let mean = r.sum / r.tells.max(1.0) / 1000.0;
        if parts == 4 && !spread && !in_flight {
            base = mean;
        }
        println!(
            "  {name:40} card done to told {mean:5.2} ms each (most {:5.2}), {:4.1} tells a frame; relay landing {:5.1} ms a frame{}",
            r.most / 1000.0,
            r.tells / n,
            r.busy / n / 1000.0,
            if base > 0.0 && (spread || in_flight) { format!("; {:+.0} % against the default", (mean / base - 1.0) * 100.0) } else { String::new() },
        );
        assert!(mean.is_finite() && r.tells > 0.0);
    }
    // What cutting a landing in segments and parts costs on the CPU.
    let line: Vec<Option<u64>> = (0..34).map(|i| (i % 2 == 0).then_some(i as u64)).collect();
    let titles: Vec<(usize, usize)> = (0..340).map(|i| (i << 16, 64 << 10)).collect();
    let started = std::time::Instant::now();
    for _ in 0..1000 {
        let ends = segment_ends(std::hint::black_box(&line));
        let seg: Vec<usize> = ends.iter().map(|&e| e * 10).collect();
        std::hint::black_box(segment_parts(&titles, &seg, 4));
    }
    println!("  cutting a landing of 17 flights in segments and parts: {:.1} us", started.elapsed().as_secs_f64() * 1e3);
}
