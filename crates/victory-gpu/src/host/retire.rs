//! Retired host objects kept apart per thread (`VICTORY_RETIRE_APART=1`, off).
//!
//! 20,000 descriptor slots a frame give up the resource they showed, on the
//! title's threads, the record helpers and the views helpers at once, and
//! each was pushed under one lock beside one counter: the lock's word, the
//! queue's tail and the counter went from core to core at every push
//! (`host::retire_resource`, 1.8 % of the list threads' time, measured in
//! game). Here each thread pushes into a queue of its own (16, by the order
//! threads first retire), on a cache line of its own, and counts there.
//!
//! What is dropped when stays what the one queue made it: an object goes
//! only once the submissions it waits for have finished. A queue's front is
//! taken while it waits for no later submission than those, as the one
//! queue's was; one thread that overtakes another holds back its own queue
//! only, so an object can go a little earlier than with one queue, never
//! before its submissions finished.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Queues: more than the threads that retire at once.
const SHARDS: usize = 16;

/// One thread's queue, and the objects pushed into it since the last take.
#[repr(align(128))]
struct Shard<T>(Mutex<(VecDeque<(u64, T)>, u64)>);

pub(crate) struct Graves<T> {
    shards: [Shard<T>; SHARDS],
}

/// The queue the next thread to retire gets.
static NEXT: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    static MINE: usize = NEXT.fetch_add(1, Ordering::Relaxed) % SHARDS;
}

impl<T> Graves<T> {
    pub(crate) const fn new() -> Self {
        Self { shards: [const { Shard(Mutex::new((VecDeque::new(), 0))) }; SHARDS] }
    }

    /// `object` waits for the submissions up to `at`.
    pub(crate) fn push(&self, at: u64, object: T) {
        let shard = &self.shards[MINE.with(|m| *m)];
        let mut queue = shard.0.lock().unwrap_or_else(|e| e.into_inner());
        queue.0.push_back((at, object));
        queue.1 += 1;
    }

    /// Into `out`, the objects that wait for no submission after
    /// `finished`, from the front of each queue; and the objects pushed
    /// since the last take.
    pub(crate) fn take(&self, finished: u64, out: &mut Vec<T>) -> u64 {
        let mut pushed = 0;
        for shard in &self.shards {
            let mut queue = shard.0.lock().unwrap_or_else(|e| e.into_inner());
            pushed += std::mem::take(&mut queue.1);
            let n = queue.0.iter().take_while(|(at, _)| *at <= finished).count();
            if n > 0 {
                out.extend(queue.0.drain(..n).map(|(_, object)| object));
            }
        }
        pushed
    }
}

#[cfg(test)]
mod tests {
    use super::Graves;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    /// A fixed-seed source of numbers (xorshift64*).
    struct Seeded(u64);

    impl Seeded {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n.max(1)
        }
    }

    /// The one queue, as `host::bury` takes from it.
    fn old_take(queue: &mut VecDeque<(u64, u32)>, finished: u64) -> Vec<u32> {
        let n = queue.iter().take_while(|(at, _)| *at <= finished).count();
        queue.drain(..n).map(|(_, o)| o).collect()
    }

    /// Fixed seeds; pushes from 1-12 threads (each with its own queue)
    /// that read the submission count at different moments, so that they
    /// overtake each other, and takes between: nothing is taken before its
    /// submission finished, everything the one queue would have let go by
    /// a take is gone by the same take, and all of it goes in the end.
    #[test]
    fn apart_graves_let_go_what_the_one_queue_did_and_never_sooner() {
        for seed in [3u64, 11, 0xfeed, 0x1234_5678, 99] {
            for threads in [1usize, 2, 5, 12] {
                let mut rng = Seeded(seed);
                let graves: Arc<Graves<u32>> = Arc::new(Graves::new());
                let mut one: VecDeque<(u64, u32)> = VecDeque::new();
                // Each thread's own view of the count, a little behind.
                let mut seen = vec![0u64; threads];
                let (mut started, mut finished) = (0u64, 0u64);
                let (mut gone_old, mut gone_new) = (std::collections::BTreeSet::new(), std::collections::BTreeSet::new());
                let mut pushed = 0u64;
                let mut counted = 0u64;
                let mut at_of = Vec::new();
                // Pushes run on real threads, one at a time, to get each
                // its own queue.
                let workers: Vec<_> = (0..threads)
                    .map(|_| {
                        let (send, get) = std::sync::mpsc::channel::<Option<(u64, u32)>>();
                        let (done, wait) = std::sync::mpsc::channel::<()>();
                        let graves = graves.clone();
                        let handle = std::thread::spawn(move || {
                            while let Ok(Some((at, o))) = get.recv() {
                                graves.push(at, o);
                                let _ = done.send(());
                            }
                        });
                        (send, wait, handle)
                    })
                    .collect();
                for step in 0..3000u32 {
                    match rng.below(10) {
                        0 => started += 1,
                        1 => {
                            finished = (finished + rng.below(3)).min(started);
                            let mut out = Vec::new();
                            counted += graves.take(finished, &mut out);
                            for o in out {
                                assert!(at_of[o as usize] <= finished, "seed {seed}: {o} taken before its submission finished");
                                gone_new.insert(o);
                            }
                            gone_old.extend(old_take(&mut one, finished));
                            assert!(gone_old.is_subset(&gone_new), "seed {seed} threads {threads} step {step}");
                        }
                        _ => {
                            let t = rng.below(threads as u64) as usize;
                            if rng.below(3) != 0 {
                                seen[t] = started;
                            }
                            let o = at_of.len() as u32;
                            at_of.push(seen[t]);
                            one.push_back((seen[t], o));
                            workers[t].0.send(Some((seen[t], o))).unwrap();
                            workers[t].1.recv().unwrap();
                            pushed += 1;
                        }
                    }
                }
                let mut out = Vec::new();
                counted += graves.take(u64::MAX, &mut out);
                gone_new.extend(out);
                gone_old.extend(old_take(&mut one, u64::MAX));
                assert_eq!(gone_new.len() as u64, pushed);
                assert_eq!(gone_old, gone_new);
                assert_eq!(counted, pushed);
                for (send, _, handle) in workers {
                    send.send(None).unwrap();
                    handle.join().unwrap();
                }
            }
        }
    }

    /// An object that checks, when dropped, that its submission finished.
    struct Probe {
        at: u64,
        finished: Arc<AtomicU64>,
        early: Arc<AtomicU64>,
        dropped: Arc<AtomicU64>,
    }

    impl Drop for Probe {
        fn drop(&mut self) {
            if self.finished.load(Ordering::Acquire) < self.at {
                self.early.fetch_add(1, Ordering::Relaxed);
            }
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Eight threads retire while another starts and finishes submissions
    /// and buries, all at once: nothing is dropped before its submission
    /// finished, and everything is dropped in the end.
    #[test]
    fn apart_graves_hold_while_threads_race() {
        let graves: Arc<Graves<Probe>> = Arc::new(Graves::new());
        let started = Arc::new(AtomicU64::new(0));
        let finished = Arc::new(AtomicU64::new(0));
        let (early, dropped) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        const EACH: u64 = 20_000;
        let workers: Vec<_> = (0..8)
            .map(|_| {
                let (graves, started, finished, early, dropped) =
                    (graves.clone(), started.clone(), finished.clone(), early.clone(), dropped.clone());
                std::thread::spawn(move || {
                    for _ in 0..EACH {
                        // As `host::retire_resource`: the submissions begun
                        // now are those it may be named by.
                        let at = started.load(Ordering::Acquire) + 1;
                        graves.push(at, Probe { at, finished: finished.clone(), early: early.clone(), dropped: dropped.clone() });
                    }
                })
            })
            .collect();
        let burier = {
            let (graves, started, finished, stop) = (graves.clone(), started.clone(), finished.clone(), stop.clone());
            std::thread::spawn(move || {
                let mut out = Vec::new();
                while !stop.load(Ordering::Relaxed) {
                    let n = started.fetch_add(1, Ordering::AcqRel) + 1;
                    std::thread::yield_now();
                    finished.store(n.saturating_sub(1), Ordering::Release);
                    graves.take(finished.load(Ordering::Acquire), &mut out);
                    out.clear();
                }
            })
        };
        for w in workers {
            w.join().unwrap();
        }
        stop.store(true, Ordering::Relaxed);
        burier.join().unwrap();
        finished.store(u64::MAX, Ordering::Release);
        let mut out = Vec::new();
        graves.take(u64::MAX, &mut out);
        drop(out);
        assert_eq!(early.load(Ordering::Relaxed), 0, "dropped before their submission finished");
        assert_eq!(dropped.load(Ordering::Relaxed), 8 * EACH);
    }

    /// Offline: 8 threads retire 20,000 objects each, into the one queue
    /// with its counter (as `host::retire_resource` does) and into queues
    /// kept apart. Run with
    /// `cargo test --release -p victory-gpu --lib -- --ignored --nocapture bench_retire_apart`.
    #[test]
    #[ignore]
    fn bench_retire_apart() {
        const EACH: u64 = 20_000;
        const THREADS: usize = 8;
        let run = |push: Arc<dyn Fn(u64) + Send + Sync>| {
            let begun = std::time::Instant::now();
            let threads: Vec<_> = (0..THREADS)
                .map(|_| {
                    let push = push.clone();
                    std::thread::spawn(move || {
                        for i in 0..EACH {
                            push(i);
                        }
                    })
                })
                .collect();
            for t in threads {
                t.join().unwrap();
            }
            begun.elapsed().as_secs_f64() * 1e3
        };
        let best = |make: &dyn Fn() -> Arc<dyn Fn(u64) + Send + Sync>| (0..10).map(|_| run(make())).fold(f64::MAX, f64::min);
        let one = best(&|| {
            let queue: Arc<(Mutex<VecDeque<(u64, u64)>>, AtomicU64)> = Arc::new((Mutex::new(VecDeque::new()), AtomicU64::new(0)));
            Arc::new(move |i| {
                queue.1.fetch_add(1, Ordering::Relaxed);
                queue.0.lock().unwrap().push_back((i, i));
            })
        });
        let apart = best(&|| {
            let graves: Arc<Graves<u64>> = Arc::new(Graves::new());
            Arc::new(move |i| graves.push(i, i))
        });
        println!(
            "{THREADS} threads retiring {EACH} each: {one:.2} ms into one queue, {apart:.2} ms into queues apart ({:.0} and {:.0} ns a push)",
            one * 1e6 / (EACH as f64 * THREADS as f64),
            apart * 1e6 / (EACH as f64 * THREADS as f64)
        );
    }
}
