//! `victory warm`: host pipelines made ahead of a run (`known_pipelines`), in
//! processes of their own: a compiler that crashes or hangs costs one pipeline.
//!
//! Pipelines are made once per host description: first each recorded
//! pipeline is named by the hash of what the driver is given (the translated
//! shaders' code, the root signature, the state; `known::names`, which
//! compiles nothing), and of the pipelines of one name only one is made.
//! What is done is written down by that name and the driver, not by the
//! translator: after a change of the translator only the pipelines whose
//! code changed are made again. `VICTORY_WARM_BY_KEY=1`: every recorded
//! pipeline is made, and what is done is kept by the translator, as before.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use victory_hle::known_pipelines as known;

/// Pipelines a process makes before another takes its place: the compiler's
/// and the translations' memory goes with it.
const AT_ONCE: usize = 40;
/// Pipelines a process names before another takes its place. Naming reads
/// the translations and makes a root signature: a few milliseconds each, so
/// a process's start (some 0.3 s) is spread over many.
const NAMES_AT_ONCE: usize = 1000;
/// A pipeline that takes longer is taken for one the compiler hangs on.
const LONGEST: Duration = Duration::from_secs(180);

/// As the runs count one that takes long.
const SLOW_MS: f64 = 1000.0;

/// The name of each recorded pipeline, by translator: `fingerprint key
/// name`, or `-` for one that gets no host pipeline. Only added to.
const PLAN_FILE: &str = "warm-plan.txt";
/// The title pipelines the latest runs drew with, the latest first, each in
/// the order it was first drawn with (`library`, with the pipeline book on).
const RECENT_FILE: &str = "recent-pipelines.txt";
/// The start of a line of `warm-done.txt` that names a host description
/// made for a driver (`= driver name`), not a recorded pipeline made by a
/// translator (`fingerprint key`).
const BY_NAME: &str = "=";

/// `VICTORY_WARM_BY_KEY=1`: as before the names (see the module's notes).
fn by_key() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_WARM_BY_KEY").is_some_and(|v| v == "1"))
}

/// What `victory warm` is asked to do.
pub struct Options {
    pub jobs: Option<usize>,
    pub limit: Option<usize>,
    pub again: bool,
    pub used: bool,
    pub at_once: Option<usize>,
    pub plan: bool,
    pub from: Option<usize>,
    pub first: Option<PathBuf>,
    pub these: Option<String>,
    pub names: bool,
}

fn folder() -> Result<PathBuf> {
    let cfg = victory_core::Config::load()?;
    Ok(cfg.cache_subdir("shaders")?.with_file_name("pipelines"))
}

/// Processes at once: one per logical core, and no more than leave 2 GB of
/// the memory free at 0.7 GB each. Measured on a 7800X3D (16 logical cores,
/// 31 GB): 600 pipelines in 20 s with 6 processes, 23 s with 10 and 15 s
/// with 16 (the parts differ; per second of compiling, 16 did 35 % more than
/// 10); the processes together held at most 5 GB with 10, 3.3 GB with 16.
/// `VICTORY_WARM_BY_KEY=1`: two thirds of the cores, as before.
fn default_jobs() -> usize {
    let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
    if by_key() {
        return (cores * 2 / 3).max(1);
    }
    let by_cores = cores;
    match known::free_memory_mb() {
        Some(mb) => by_cores.min((mb.saturating_sub(2048) / 700) as usize),
        None => by_cores,
    }
    .max(1)
}

pub fn run(o: Options) -> Result<()> {
    let dir = folder()?;
    if let Some(these) = &o.these {
        return if o.names { name(&dir, these) } else { make(&dir, these) };
    }
    let began = Instant::now();
    victory_hle::known_pipelines::use_translations(dir.with_file_name("shaders"));
    let fingerprint = known::fingerprint();
    let driver = known::driver_fingerprint();
    let done_file = dir.join("warm-done.txt");
    let bad_file = dir.join("warm-bad.txt");
    let keys = known::keys(&dir);
    let listed = std::fs::read_to_string(&done_file).unwrap_or_default();
    let mut done: HashSet<&str> = HashSet::new();
    let mut done_names: HashSet<&str> = HashSet::new();
    for line in listed.lines() {
        match line.split_once(' ') {
            Some((BY_NAME, rest)) => {
                if let Some((of, name)) = rest.split_once(' ')
                    && of == driver
                {
                    done_names.insert(name);
                }
            }
            Some((of, key)) if of == fingerprint => {
                done.insert(key);
            }
            _ => {}
        }
    }
    let bad = std::fs::read_to_string(&bad_file).unwrap_or_default();
    let bad: HashSet<&str> = bad.lines().filter_map(|l| l.split(' ').next()).collect();
    let early = std::fs::read_to_string(dir.join("early-pipelines.txt")).unwrap_or_default();
    let early: HashSet<&str> = early.lines().collect();
    let recent = std::fs::read_to_string(dir.join(RECENT_FILE)).unwrap_or_default();
    let recent: HashMap<&str, usize> = recent.lines().enumerate().map(|(n, k)| (k, n)).collect();
    let first = o.first.as_ref().and_then(|f| std::fs::read_to_string(f).ok()).unwrap_or_default();
    let first: HashMap<&str, usize> = first.lines().enumerate().map(|(n, k)| (k, n)).collect();
    // Those asked for first (`--first`); then those the latest runs drew with,
    // in the order they were drawn with; then those earlier runs drew with;
    // the others in no order (by key).
    let rank = |key: &str| {
        let at = |list: &HashMap<&str, usize>| list.get(key).copied().unwrap_or(usize::MAX);
        (at(&first), at(&recent), !early.contains(key))
    };
    let mut order: Vec<usize> = (0..keys.len())
        .filter(|&n| {
            let key = keys[n].as_str();
            !bad.contains(key) && (!o.used || early.contains(key))
        })
        .collect();
    order.sort_by_key(|&n| (rank(&keys[n]), &keys[n]));
    let jobs = o.jobs.unwrap_or_else(default_jobs).max(1);
    let at_once = o.at_once.unwrap_or(AT_ONCE).max(1);

    let mut all: Vec<usize>;
    // The name each one made is written down by (none: by its key).
    let named: HashMap<usize, String>;
    if by_key() {
        all = order.into_iter().filter(|&n| o.again || !done.contains(keys[n].as_str())).collect();
        named = HashMap::new();
        println!(
            "{} pipelines known, {} made before ({fingerprint}), {} bad; {} to make, {jobs} processes",
            keys.len(),
            done.len(),
            bad.len(),
            all.len()
        );
    } else {
        // Named a part at a time: with a limit, only as many as it takes.
        let wanted = o.limit.map_or(usize::MAX, |l| l.saturating_add(o.from.unwrap_or(0)));
        let step = if wanted == usize::MAX { order.len().max(1) } else { (wanted * 2).max(NAMES_AT_ONCE * jobs) };
        let mut plan: HashMap<usize, Option<String>> = HashMap::new();
        let mut pick = Pick { done: done_names.into_iter().map(str::to_owned).collect(), again: o.again, ..Pick::default() };
        let mut looked = 0;
        for part in order.chunks(step) {
            let names = self::plan(&dir, &keys, part, &fingerprint, jobs)?;
            pick.take(part, &names, |n| done.contains(keys[n].as_str()));
            looked += part.len();
            plan.extend(names);
            if pick.all.len() >= wanted {
                break;
            }
        }
        let order = &order[..looked];
        (all, named) = (std::mem::take(&mut pick.all), std::mem::take(&mut pick.named));
        let Pick { none, same, before, seen, .. } = &pick;
        println!(
            "{} pipelines known, {before} made before ({driver}), {} bad; {} to make, {jobs} processes \
             ({} looked at: {none} get no host pipeline, {same} are the same as another to the driver)",
            keys.len(),
            bad.len(),
            all.len(),
            order.len(),
        );
        if o.plan {
            let used: HashSet<&str> = order
                .iter()
                .filter(|&&n| early.contains(keys[n].as_str()))
                .filter_map(|n| plan.get(n)?.as_deref())
                .collect();
            println!(
                "{} host descriptions in all; {} of them drawn with by earlier runs ({} recorded pipelines drawn with)",
                seen.len(),
                used.len(),
                order.iter().filter(|&&n| early.contains(keys[n].as_str())).count()
            );
            return Ok(());
        }
    }
    if let Some(from) = o.from {
        all.drain(..from.min(all.len()));
    }
    all.truncate(o.limit.unwrap_or(usize::MAX));
    // Before the first play nothing is recorded and the folder may not exist yet: nothing to do is no error.
    if all.is_empty() {
        println!("nothing to make: pipelines are recorded while the title runs (VICTORY_PIPELINE_BOOK=1)");
        return Ok(());
    }
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    // A whole warm-up (no limit) ends when its slowest process does: those
    // known to take long go first, a few to a process, so that none of them
    // is left to run alone at the end (a tenth of the recorded pipelines took
    // over a second each, some over a minute). With a limit, the order is
    // what was asked for.
    let slow = if by_key() || o.limit.is_some() { HashMap::new() } else { slow_ms(&dir) };
    let chunks = chunked(&all, |n| slow.get(keys[n].as_str()).copied(), at_once);
    let chunks: Vec<&[usize]> = chunks.iter().map(Vec::as_slice).collect();
    let next = AtomicUsize::new(0);
    let state = Mutex::new(Stats::default());
    let done_out = Mutex::new(std::fs::OpenOptions::new().create(true).append(true).open(&done_file)?);
    // (Read back by the runs: these are made beside the others, not before.)
    let slow_out = Mutex::new(std::fs::OpenOptions::new().create(true).append(true).open(dir.join("slow-pipelines.txt"))?);
    let bad_out = Mutex::new(std::fs::OpenOptions::new().create(true).append(true).open(&bad_file)?);
    // `VICTORY_WARM_TIMES=<file>`: each pipeline's milliseconds (all, translations), for measurements.
    let times_out = std::env::var_os("VICTORY_WARM_TIMES")
        .and_then(|f| std::fs::OpenOptions::new().create(true).append(true).open(f).ok())
        .map(Mutex::new);
    std::thread::scope(|s| {
        for _ in 0..jobs.min(chunks.len()) {
            s.spawn(|| {
                while let Some(&chunk) = chunks.get(next.fetch_add(1, Ordering::Relaxed)) {
                    let mut rest = chunk;
                    while !rest.is_empty() {
                        let (made, ended, start_ms) = child(&["warm"], rest, |words| {
                            let ["made", n, ms, there, more @ ..] = words else { return false };
                            let (Ok(n), Ok(ms)) = (n.parse::<usize>(), ms.parse::<f64>()) else { return false };
                            let translate_ms = more.first().and_then(|t| t.parse().ok()).unwrap_or(0.0);
                            if ms >= SLOW_MS {
                                let _ = writeln!(lock(&slow_out), "\n{} {ms:.0}", keys[n]);
                            }
                            if let Some(out) = &times_out {
                                let _ = writeln!(lock(out), "{} {ms:.2} {translate_ms:.2} {there}", keys[n]);
                            }
                            let mut state = lock(&state);
                            state.times.push((ms, translate_ms));
                            state.refused += usize::from(*there != "1");
                            let count = state.times.len();
                            // (Often enough for the launcher's progress bar.)
                            if count.is_multiple_of(250) {
                                println!("{count} of {}, {:.0} s", all.len(), began.elapsed().as_secs_f64());
                            }
                            drop(state);
                            let _ = match named.get(&n) {
                                Some(name) => writeln!(lock(&done_out), "{BY_NAME} {driver} {name}"),
                                None => writeln!(lock(&done_out), "{fingerprint} {}", keys[n]),
                            };
                            true
                        });
                        lock(&state).starts.extend(start_ms);
                        rest = &rest[made.min(rest.len())..];
                        match (ended, rest.split_first()) {
                            (Some(why), Some((&n, after))) => {
                                // The one it was at.
                                println!("pipeline {n} {}: {why}", keys[n]);
                                let _ = writeln!(lock(&bad_out), "{} {why}", keys[n]);
                                lock(&state).crashed += 1;
                                rest = after;
                            }
                            _ => break,
                        }
                    }
                }
            });
        }
    });
    let stats = state.into_inner().unwrap_or_else(|e| e.into_inner());
    stats.report(began.elapsed(), jobs);
    Ok(())
}

/// Pipelines known to take long go this many to a process.
const SLOW_AT_ONCE: usize = 4;

/// The longest time each title pipeline took to make, in ms, as the runs
/// and warm-ups wrote down (`slow-pipelines.txt`: those over a second).
fn slow_ms(dir: &Path) -> HashMap<String, u64> {
    let listed = std::fs::read_to_string(dir.join("slow-pipelines.txt")).unwrap_or_default();
    let mut slow: HashMap<String, u64> = HashMap::new();
    for (key, ms) in listed.lines().filter_map(|l| l.split_once(' ')) {
        if let Ok(ms) = ms.trim().parse::<f64>() {
            let known = slow.entry(key.to_owned()).or_default();
            *known = (*known).max(ms as u64);
        }
    }
    slow
}

/// `all` in the parts processes are given: those `slow` knows, the slowest
/// first, [`SLOW_AT_ONCE`] at a time; then the others, `at_once` at a time,
/// in their order.
fn chunked(all: &[usize], slow: impl Fn(usize) -> Option<u64>, at_once: usize) -> Vec<Vec<usize>> {
    let (mut long, rest): (Vec<usize>, Vec<usize>) = all.iter().partition(|&&n| slow(n).is_some());
    long.sort_by_key(|&n| std::cmp::Reverse(slow(n)));
    let mut chunks: Vec<Vec<usize>> = long.chunks(SLOW_AT_ONCE.min(at_once)).map(<[usize]>::to_vec).collect();
    chunks.extend(rest.chunks(at_once).map(<[usize]>::to_vec));
    chunks
}

/// Of the pipelines in an order, one of each host description not made
/// before: the first.
#[derive(Default)]
struct Pick {
    /// Those to make, in order, and the name of each.
    all: Vec<usize>,
    named: HashMap<usize, String>,
    /// The names met so far, and those made before.
    seen: HashSet<String>,
    done: HashSet<String>,
    /// Make those made before too.
    again: bool,
    /// Those with no host pipeline, the same as one met before, and made before.
    none: usize,
    same: usize,
    before: usize,
}

impl Pick {
    /// Takes the next part of the order, named `names`; `done_key`: whether
    /// one was made by its key (by an earlier warm-up of this translator).
    fn take(&mut self, part: &[usize], names: &HashMap<usize, Option<String>>, done_key: impl Fn(usize) -> bool) {
        for (&n, name) in names {
            if let Some(name) = name
                && done_key(n)
            {
                self.done.insert(name.clone());
            }
        }
        for &n in part {
            let Some(Some(name)) = names.get(&n) else {
                self.none += 1;
                continue;
            };
            if !self.seen.insert(name.clone()) {
                self.same += 1;
            } else if !self.again && self.done.contains(name) {
                self.before += 1;
            } else {
                self.all.push(n);
                self.named.insert(n, name.clone());
            }
        }
    }
}

/// What the processes told.
#[derive(Default)]
struct Stats {
    /// Each pipeline's milliseconds: all, and of them its translations.
    times: Vec<(f64, f64)>,
    /// Milliseconds from each process's start to its first word.
    starts: Vec<f64>,
    refused: usize,
    crashed: usize,
}

impl Stats {
    fn report(mut self, took: Duration, jobs: usize) {
        self.times.sort_by(|a, b| a.0.total_cmp(&b.0));
        let times = &self.times;
        let at = |q: f64| times.get((times.len() as f64 * q) as usize).map_or(0.0, |t| t.0);
        let sum: f64 = times.iter().map(|t| t.0).sum();
        let translations: f64 = times.iter().map(|t| t.1).sum();
        // How much of the time the slowest hundredth and tenth took.
        let share = |part: f64| {
            let n = ((times.len() as f64 * part).ceil() as usize).min(times.len());
            times[times.len() - n..].iter().map(|t| t.0).sum::<f64>() / sum.max(1e-9) * 100.0
        };
        println!(
            "{} made in {:.0} s ({} have no host pipeline, {} ended their process); ms each: median {:.1}, p90 {:.1}, p99 {:.1}, most {:.0}, sum {:.0} s",
            times.len(),
            took.as_secs_f64(),
            self.refused,
            self.crashed,
            at(0.5),
            at(0.9),
            at(0.99),
            times.last().map_or(0.0, |t| t.0),
            sum / 1e3
        );
        self.starts.sort_by(f64::total_cmp);
        let starts: f64 = self.starts.iter().sum();
        println!(
            "of the sum: translations {:.0} s, the driver {:.0} s; the slowest 1 % took {:.0} % of it, the slowest 10 % {:.0} %; \
             {} processes started in {:.0} ms each (median), {:.0} s in all; {:.0} % of {jobs} processes' time was making",
            translations / 1e3,
            (sum - translations) / 1e3,
            share(0.01),
            share(0.1),
            self.starts.len(),
            self.starts.get(self.starts.len() / 2).copied().unwrap_or(0.0),
            starts / 1e3,
            sum / 1e3 / (took.as_secs_f64() * jobs as f64).max(1e-9) * 100.0
        );
    }
}

/// Names `order`'s pipelines that have no name for this translator yet, in
/// processes of their own, and writes the names down; every name of
/// `order`'s (`None`: no host pipeline).
fn plan(
    dir: &Path,
    keys: &[String],
    order: &[usize],
    fingerprint: &str,
    jobs: usize,
) -> Result<HashMap<usize, Option<String>>> {
    let file = dir.join(PLAN_FILE);
    let listed = std::fs::read_to_string(&file).unwrap_or_default();
    let mut by_key: HashMap<&str, Option<&str>> = HashMap::new();
    for line in listed.lines() {
        let mut words = line.split(' ');
        if let (Some(of), Some(key), Some(name)) = (words.next(), words.next(), words.next())
            && of == fingerprint
        {
            by_key.insert(key, (name != "-").then_some(name));
        }
    }
    let mut names: HashMap<usize, Option<String>> = HashMap::new();
    let mut missing = Vec::new();
    for &n in order {
        match by_key.get(keys[n].as_str()) {
            Some(name) => {
                names.insert(n, name.map(str::to_owned));
            }
            None => missing.push(n),
        }
    }
    if missing.is_empty() {
        return Ok(names);
    }
    let began = Instant::now();
    println!("{} pipelines to name, {jobs} processes", missing.len());
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let out = Mutex::new(std::fs::OpenOptions::new().create(true).append(true).open(&file)?);
    let found = Mutex::new(Vec::new());
    let chunks: Vec<&[usize]> = missing.chunks(NAMES_AT_ONCE).collect();
    let next = AtomicUsize::new(0);
    std::thread::scope(|s| {
        for _ in 0..jobs.min(chunks.len()) {
            s.spawn(|| {
                while let Some(&chunk) = chunks.get(next.fetch_add(1, Ordering::Relaxed)) {
                    let mut rest = chunk;
                    while !rest.is_empty() {
                        let (told, ended, _) = child(&["warm", "--names"], rest, |words| {
                            let ["named", n, name] = words else { return false };
                            let Ok(n) = n.parse::<usize>() else { return false };
                            let name = (*name != "-").then(|| (*name).to_owned());
                            let _ = writeln!(lock(&out), "{fingerprint} {} {}", keys[n], name.as_deref().unwrap_or("-"));
                            let mut found = lock(&found);
                            found.push((n, name));
                            if found.len().is_multiple_of(5000) {
                                println!("{} of {} named, {:.0} s", found.len(), missing.len(), began.elapsed().as_secs_f64());
                            }
                            true
                        });
                        rest = &rest[told.min(rest.len())..];
                        match (ended, rest.split_first()) {
                            // The one it was at is not named: it is made, and
                            // its own process ends as it may.
                            (Some(_), Some((_, after))) => rest = after,
                            _ => break,
                        }
                    }
                }
            });
        }
    });
    let found = found.into_inner().unwrap_or_else(|e| e.into_inner());
    println!("{} of {} named in {:.0} s", found.len(), missing.len(), began.elapsed().as_secs_f64());
    let told: HashSet<usize> = found.iter().map(|f| f.0).collect();
    names.extend(found);
    // Those whose process ended: made as they are, by key.
    for n in missing.into_iter().filter(|n| !told.contains(n)) {
        names.insert(n, Some(format!("key-{}", keys[n])));
    }
    Ok(names)
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Has a process (`victory <what> --these …`) do `these`; `each` is given
/// each line's words and says whether it told of one of them. How many it
/// told of, how it ended if not well, and the milliseconds from its start to
/// its first line.
fn child(what: &[&str], these: &[usize], mut each: impl FnMut(&[&str]) -> bool) -> (usize, Option<String>, Option<f64>) {
    let list: Vec<String> = these.iter().map(usize::to_string).collect();
    let began = Instant::now();
    let spawned = std::env::current_exe().and_then(|exe| {
        use std::os::windows::process::CommandExt;
        const IDLE_PRIORITY_CLASS: u32 = 0x40;
        Command::new(exe)
            .args(what)
            .args(["--these", &list.join(",")])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .creation_flags(IDLE_PRIORITY_CLASS)
            .spawn()
    });
    let Ok(mut process) = spawned else { return (these.len(), None, None) };
    let Some(out) = process.stdout.take() else { return (these.len(), None, None) };
    let process = Mutex::new(process);
    let (heard, over) = (Mutex::new(Instant::now()), AtomicBool::new(false));
    let hung = AtomicBool::new(false);
    let mut told = 0;
    let mut first = None;
    std::thread::scope(|s| {
        s.spawn(|| {
            while !over.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(500));
                if lock(&heard).elapsed() > LONGEST {
                    hung.store(true, Ordering::Relaxed);
                    let _ = lock(&process).kill();
                    return;
                }
            }
        });
        for line in BufReader::new(out).lines().map_while(Result::ok) {
            *lock(&heard) = Instant::now();
            first.get_or_insert_with(|| began.elapsed().as_secs_f64() * 1e3);
            let words: Vec<&str> = line.split(' ').collect();
            if each(&words) {
                told += 1;
            }
        }
        over.store(true, Ordering::Relaxed);
    });
    let status = lock(&process).wait();
    let well = status.as_ref().is_ok_and(|s| s.success());
    if well {
        return (these.len(), None, first);
    }
    let why = if hung.load(Ordering::Relaxed) {
        format!("hung for {} s", LONGEST.as_secs())
    } else {
        status.map_or_else(|e| e.to_string(), |s| format!("{s}"))
    };
    (told, Some(why), first)
}

fn numbers(these: &str) -> Result<Vec<usize>> {
    these.split(',').map(|n| n.parse().context("--these")).collect()
}

/// Tells whoever asked; ends this process when they are gone.
fn tell(out: &mut std::io::Stdout, line: std::fmt::Arguments) {
    if writeln!(out, "{line}").and_then(|()| out.flush()).is_err() {
        std::process::exit(1);
    }
}

/// What such a process does.
fn make(dir: &Path, these: &str) -> Result<()> {
    let these = numbers(these)?;
    let cfg = victory_core::Config::load()?;
    victory_hle::known_pipelines::use_translations(cfg.cache_subdir("shaders")?);
    let mut out = std::io::stdout();
    known::make(dir, &these, |n, came| match came {
        None => tell(&mut out, format_args!("at {n}")),
        Some(m) => tell(&mut out, format_args!("made {n} {:.2} {} {:.2}", m.ms, u8::from(m.there), m.translate_ms)),
    });
    // Without what a run's end does (nothing to save, the host's threads).
    let _ = out.flush();
    // (Measurements: `VICTORY_WARM_LINGER_MS`, a wait before the end, for the
    // driver's own writes to its cache.)
    if let Some(ms) = std::env::var("VICTORY_WARM_LINGER_MS").ok().and_then(|v| v.parse().ok()) {
        std::thread::sleep(Duration::from_millis(ms));
    }
    std::process::exit(0)
}

/// What a process that names does.
fn name(dir: &Path, these: &str) -> Result<()> {
    let these = numbers(these)?;
    let cfg = victory_core::Config::load()?;
    victory_hle::known_pipelines::use_translations(cfg.cache_subdir("shaders")?);
    let mut out = std::io::stdout();
    known::names(dir, &these, |n, name| tell(&mut out, format_args!("named {n} {}", name.as_deref().unwrap_or("-"))));
    let _ = out.flush();
    std::process::exit(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[(usize, Option<&str>)]) -> HashMap<usize, Option<String>> {
        list.iter().map(|&(n, name)| (n, name.map(str::to_owned))).collect()
    }

    /// One of each host description is made, the first in the order; those
    /// with none and those made before (by name, or by key in an earlier
    /// warm-up of the same translator) are left.
    #[test]
    fn one_of_each_description_is_made() {
        let mut pick = Pick { done: ["old".to_owned()].into(), ..Pick::default() };
        let part = [5, 1, 4, 2, 3, 0, 6];
        let named = names(&[(0, Some("a")), (1, Some("b")), (2, Some("a")), (3, None), (4, Some("old")), (5, Some("c")), (6, Some("d"))]);
        pick.take(&part, &named, |n| n == 6);
        assert_eq!(pick.all, [5, 1, 2]);
        assert_eq!(pick.named[&2], "a");
        assert_eq!((pick.none, pick.same, pick.before), (1, 1, 2));
        // The next part: what the first held is not made again.
        pick.take(&[7, 8], &names(&[(7, Some("a")), (8, Some("e"))]), |_| false);
        assert_eq!(pick.all, [5, 1, 2, 8]);
        assert_eq!(pick.same, 2);
    }

    #[test]
    fn slow_ones_go_first_a_few_at_a_time() {
        let slow = |n: usize| [(3, 5000), (7, 1200), (9, 60000)].iter().find(|s| s.0 == n).map(|s| s.1);
        let all: Vec<usize> = (0..12).collect();
        let chunks = chunked(&all, slow, 5);
        assert_eq!(chunks, [vec![9, 3, 7], vec![0, 1, 2, 4, 5], vec![6, 8, 10, 11]]);
        // Nothing known: the order, as before.
        assert_eq!(chunked(&all, |_| None, 40), std::slice::from_ref(&all));
    }

    #[test]
    fn again_makes_those_made_before() {
        let mut pick = Pick { done: ["a".to_owned()].into(), again: true, ..Pick::default() };
        pick.take(&[0, 1], &names(&[(0, Some("a")), (1, Some("a"))]), |_| true);
        assert_eq!(pick.all, [0]);
        assert_eq!((pick.same, pick.before), (1, 0));
    }
}