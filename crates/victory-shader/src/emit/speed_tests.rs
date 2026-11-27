//! How long each stage of a translation takes on large synthetic programs:
//! wave32 compute code the way the console compiler lays it out (VALU runs,
//! lane branches with and without an else, uniform and per-lane loops,
//! texture samples, buffer loads), thousands of instructions long. No title
//! files and no GPU.
//!
//! `cargo test --release -p victory-shader translation_speed -- --nocapture --ignored`
//!
//! With `VICTORY_SPEED_OUT=<dir>` each program's DXIL is written there too,
//! so two builds of the translator can be compared byte for byte.

use std::time::{Duration, Instant};

use super::lower;
use crate::native::{
    ComputeIo, HwStage, NativeProgram, ResourceType, RootParam, RootType, StageIo, UsedResource,
};
use crate::{gfx10, io, ir, resource, structure};

/// A small deterministic generator (xorshift).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 32) as u32
    }

    fn below(&mut self, n: u32) -> u32 {
        self.next() % n
    }
}

/// Machine words with the name each instruction should decode as.
#[derive(Default)]
struct Asm {
    words: Vec<u32>,
    names: Vec<(usize, &'static str)>,
    /// Lane masks of 64 bits (register pairs).
    wave64: bool,
}

const VCC_LO: u32 = 106;
const EXEC_LO: u32 = 126;
/// The work registers the VALU runs read and write.
const WORK: std::ops::RangeInclusive<u32> = 20..=31;

impl Asm {
    fn put(&mut self, name: &'static str, words: &[u32]) {
        self.names.push((self.words.len() * 4, name));
        self.words.extend_from_slice(words);
    }

    /// The word index of the next instruction.
    fn here(&self) -> usize {
        self.words.len()
    }

    /// A SOPP branch to be aimed later with [`Self::aim`].
    fn branch(&mut self, name: &'static str, op: u32) -> usize {
        let at = self.here();
        self.put(name, &[0xBF80_0000 | op << 16]);
        at
    }

    fn aim(&mut self, at: usize, target: usize) {
        let delta = target as i64 - (at as i64 + 1);
        self.words[at] = self.words[at] & 0xFFFF_0000 | u32::from(delta as i16 as u16);
    }

    fn sop1(&mut self, name: &'static str, op: u32, dst: u32, src: u32) {
        self.put(name, &[0xBE80_0000 | dst << 16 | op << 8 | src]);
    }

    fn sop2(&mut self, name: &'static str, op: u32, dst: u32, a: u32, b: u32) {
        self.put(name, &[0x8000_0000 | op << 23 | dst << 16 | b << 8 | a]);
    }

    fn sopc(&mut self, name: &'static str, op: u32, a: u32, b: u32) {
        self.put(name, &[0xBF00_0000 | op << 16 | b << 8 | a]);
    }

    /// `src0`: 0..=255 scalars and constants, 256.. VGPRs; `src1` a VGPR.
    fn vop2(&mut self, name: &'static str, op: u32, dst: u32, src0: u32, vsrc1: u32) {
        self.put(name, &[op << 25 | dst << 17 | vsrc1 << 9 | src0]);
    }

    fn vop1(&mut self, name: &'static str, op: u32, dst: u32, src0: u32) {
        self.put(name, &[0x7E00_0000 | dst << 17 | op << 9 | src0]);
    }

    fn vopc(&mut self, name: &'static str, op: u32, src0: u32, vsrc1: u32) {
        self.put(name, &[0x7C00_0000 | op << 17 | vsrc1 << 9 | src0]);
    }

    fn vop3(&mut self, name: &'static str, op: u32, dst: u32, s: [u32; 3]) {
        self.put(name, &[0x35 << 26 | op << 16 | dst, s[0] | s[1] << 9 | s[2] << 18]);
    }

    /// `op saved, vcc` (`s_and_saveexec_*`, `s_or_saveexec_*`) at the wave's width.
    fn saveexec(&mut self, or: bool, saved: u32, src: u32) {
        match (self.wave64, or) {
            (false, false) => self.sop1("s_and_saveexec_b32", 0x3C, saved, src),
            (false, true) => self.sop1("s_or_saveexec_b32", 0x3D, saved, src),
            (true, false) => self.sop1("s_and_saveexec_b64", 0x24, saved, src),
            (true, true) => self.sop1("s_or_saveexec_b64", 0x25, saved, src),
        }
    }

    /// A mask operation (`or`, `xor`, `andn2`) at the wave's width.
    fn mask_op(&mut self, op: &str, dst: u32, a: u32, b: u32) {
        let (name, code) = match (op, self.wave64) {
            ("or", false) => ("s_or_b32", 0x10),
            ("or", true) => ("s_or_b64", 0x11),
            ("xor", false) => ("s_xor_b32", 0x12),
            ("xor", true) => ("s_xor_b64", 0x13),
            ("andn2", false) => ("s_andn2_b32", 0x14),
            ("andn2", true) => ("s_andn2_b64", 0x15),
            ("mov", false) => return self.sop1("s_mov_b32", 0x03, dst, a),
            _ => return self.sop1("s_mov_b64", 0x04, dst, a),
        };
        self.sop2(name, code, dst, a, b);
    }

    fn wait_vm(&mut self) {
        self.put("s_waitcnt", &[0xBF8C_3F70]);
    }

    /// `image_sample_lz v[12:15], v[coord:coord+1], s[rsrc:rsrc+7], s[8:11]`.
    fn sample(&mut self, coord: u32, rsrc: u32) {
        self.put("image_sample_lz", &[0xF09C_0F08, 2 << 21 | (rsrc / 4) << 16 | 12 << 8 | coord]);
        self.wait_vm();
    }

    /// `buffer_load_dword vdst, v16, s[40:43], 0 offen offset:byte`.
    fn load(&mut self, dst: u32, byte: u32) {
        self.put("buffer_load_dword", &[0xE030_1000 | byte, 0x800A_0010 | dst << 8]);
        self.wait_vm();
    }

    /// `buffer_store_dword vsrc, v16, s[40:43], 0 offen offset:byte`.
    fn store(&mut self, src: u32, byte: u32) {
        self.put("buffer_store_dword", &[0xE070_1000 | byte, 0x800A_0010 | src << 8]);
    }
}

fn work(rng: &mut Rng) -> u32 {
    WORK.start() + rng.below(WORK.end() - WORK.start() + 1)
}

/// A VALU source: mostly work registers, then the coordinates and samples,
/// now and then an inline constant.
fn source(rng: &mut Rng) -> u32 {
    match rng.below(10) {
        0 => [240, 242, 244, 129][rng.below(4) as usize],
        1 => 256 + [2, 3][rng.below(2) as usize],
        2 => 256 + 12 + rng.below(4),
        _ => 256 + work(rng),
    }
}

fn alu(a: &mut Asm, rng: &mut Rng, n: u32) {
    for _ in 0..n {
        let dst = work(rng);
        let (s0, s1) = (source(rng), work(rng));
        match rng.below(16) {
            0 => a.vop2("v_add_f32", 0x03, dst, s0, s1),
            1 => a.vop2("v_sub_f32", 0x04, dst, s0, s1),
            2 | 3 => a.vop2("v_mul_f32", 0x08, dst, s0, s1),
            4 => a.vop2("v_min_f32", 0x0F, dst, s0, s1),
            5 => a.vop2("v_max_f32", 0x10, dst, s0, s1),
            6 => a.vop2("v_and_b32", 0x1B, dst, s0, s1),
            7 => a.vop2("v_xor_b32", 0x1D, dst, s0, s1),
            8 => a.vop2("v_add_nc_u32", 0x25, dst, s0, s1),
            9 => a.vop1("v_rcp_f32", 0x2A, dst, 256 + s1),
            10 => a.vop1("v_sqrt_f32", 0x33, dst, 256 + s1),
            11 => a.vop1("v_fract_f32", 0x20, dst, 256 + s1),
            12 => a.vop1("v_floor_f32", 0x24, dst, 256 + s1),
            13 => a.vop1("v_cvt_f32_u32", 0x06, dst, 256 + s1),
            _ => {
                let s2 = source(rng);
                a.vop3("v_fma_f32", 0x14B, dst, [s0, 256 + s1, s2]);
            }
        }
    }
}

/// `v_cmp_gt_f32 vcc_lo, a, b` of two work registers.
fn compare(a: &mut Asm, rng: &mut Rng) {
    let (x, y) = (work(rng), work(rng));
    a.vopc("v_cmp_gt_f32", 0x04, 256 + x, y);
}

/// Code of about `budget` instructions at nesting `depth`.
fn region(a: &mut Asm, rng: &mut Rng, mut budget: i64, depth: u32) {
    while budget > 0 {
        let start = a.names.len();
        let pick = rng.below(100);
        match pick {
            0..45 => {
                let n = 4 + rng.below(9);
                alu(a, rng, n);
            }
            45..60 if depth < 3 => {
                // if (lanes): saved EXEC in s48 + depth (a pair from s48 on)
                let saved = 48 + 2 * depth;
                compare(a, rng);
                a.saveexec(false, saved, VCC_LO);
                let skip = a.branch("s_cbranch_execz", 8);
                region(a, rng, budget.min(40), depth + 1);
                let end = a.here();
                a.aim(skip, end);
                a.mask_op("or", EXEC_LO, EXEC_LO, saved);
            }
            60..68 if depth < 3 => {
                // if / else, as the console compiler lays it out
                let saved = 48 + 2 * depth;
                compare(a, rng);
                a.saveexec(false, saved, VCC_LO);
                a.mask_op("xor", saved, EXEC_LO, saved);
                let to_else = a.branch("s_cbranch_execz", 8);
                region(a, rng, budget.min(30), depth + 1);
                let other = a.here();
                a.aim(to_else, other);
                a.saveexec(true, saved, saved);
                a.mask_op("xor", EXEC_LO, EXEC_LO, saved);
                let to_end = a.branch("s_cbranch_execz", 8);
                region(a, rng, budget.min(30), depth + 1);
                let end = a.here();
                a.aim(to_end, end);
                a.mask_op("or", EXEC_LO, EXEC_LO, saved);
            }
            68..75 if depth < 2 => {
                // a loop of 4 iterations, counter in s56 + depth
                let counter = 56 + depth;
                a.sop1("s_mov_b32", 0x03, counter, 132);
                let top = a.here();
                region(a, rng, budget.min(40), depth + 1);
                a.sop2("s_add_i32", 0x02, counter, counter, 0xC1);
                a.sopc("s_cmp_lg_u32", 0x07, counter, 0x80);
                let back = a.branch("s_cbranch_scc1", 5);
                a.aim(back, top);
            }
            75..80 if depth < 2 => {
                // a loop each lane leaves on its own
                let saved = 60 + 2 * depth;
                a.mask_op("mov", saved, EXEC_LO, 0);
                let top = a.here();
                region(a, rng, budget.min(30), depth + 2);
                compare(a, rng);
                a.mask_op("andn2", EXEC_LO, EXEC_LO, VCC_LO);
                let back = a.branch("s_cbranch_execnz", 9);
                a.aim(back, top);
                a.mask_op("mov", EXEC_LO, saved, 0);
            }
            80..92 => {
                let coord = 20 + 2 * rng.below(6);
                a.sample(coord, if rng.below(2) == 0 { 24 } else { 32 });
                // fold the sample into the work registers
                for k in 0..4 {
                    let dst = work(rng);
                    a.vop2("v_add_f32", 0x03, dst, 256 + 12 + k, dst);
                }
            }
            92..100 => {
                let dst = work(rng);
                a.load(dst, 4 * rng.below(64));
            }
            _ => continue,
        }
        budget -= (a.names.len() - start) as i64;
    }
}

/// A program of roughly `size` instructions.
fn program(seed: u64, size: i64, wave64: bool) -> Vec<u32> {
    let mut a = Asm { wave64, ..Asm::default() };
    let mut rng = Rng(seed);
    a.sop1("s_mov_b32", 0x03, 4, 0); // s4 = s0
    a.sop1("s_mov_b32", 0x03, 5, 0x80);
    a.sop1("s_mov_b32", 0x03, 6, 1); // s6 = s1
    a.sop1("s_mov_b32", 0x03, 7, 0x80);
    a.put("s_load_dwordx8", &[0xF40C_0602, 0xFA00_0000]); // s[24:31], s[4:5], 0
    a.put("s_load_dwordx8", &[0xF40C_0802, 0xFA00_0020]); // s[32:39], s[4:5], 0x20
    a.put("s_load_dwordx4", &[0xF408_0203, 0xFA00_0000]); // s[8:11], s[6:7], 0
    a.put("s_load_dwordx4", &[0xF408_0A02, 0xFA00_0040]); // s[40:43], s[4:5], 0x40
    a.vop1("v_cvt_f32_u32", 0x06, 2, 256);
    a.vop1("v_mov_b32", 0x01, 3, 258);
    a.vop2("v_lshlrev_b32", 0x1A, 16, 130, 0);
    for r in (12..=15).chain(WORK) {
        a.vop1("v_mov_b32", 0x01, r, 256 + 2 + (r & 1));
    }
    a.put("s_waitcnt", &[0xBF8C_C07F]);
    region(&mut a, &mut rng, size, 0);
    for (k, r) in WORK.enumerate() {
        a.store(r, 4 * k as u32);
    }
    a.put("s_endpgm", &[0xBF81_0000]);
    let code: Vec<u8> = a.words.iter().flat_map(|w| w.to_le_bytes()).collect();
    // Every instruction is the one meant.
    let decoded = gfx10::decode(&code).expect("decodes");
    assert_eq!(decoded.len(), a.names.len());
    for (inst, &(offset, name)) in decoded.iter().zip(&a.names) {
        assert_eq!((inst.offset, inst.name()), (offset, Some(name)));
    }
    a.words
}

fn native(words: &[u32], wave64: bool) -> NativeProgram {
    let table = |user_data: u8| RootParam {
        root: RootType::Table,
        table_offset_dwords: 0,
        sgprs: 1,
        base_register: 0,
        user_data,
        slot: user_data,
    };
    let range = |kind, table_offset_dwords, dwords, user_data| UsedResource {
        kind,
        root: RootType::Table,
        table_offset_dwords,
        lower_bound: 0,
        stride: 0,
        dwords,
        user_data,
        slot: user_data,
    };
    NativeProgram {
        stage: HwStage::Cs,
        wave64,
        vgprs: 64,
        layout: vec![table(0), table(1)],
        used: vec![
            range(ResourceType::Srv, 0, 8, 0),
            range(ResourceType::Srv, 8, 8, 0),
            range(ResourceType::Uav, 16, 4, 0),
            range(ResourceType::Sampler, 0, 4, 1),
        ],
        io: StageIo::Cs(ComputeIo {
            rsrc2: 2 << 1,
            threads: [64, 1, 1],
        }),
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    }
}

/// FNV-1a, to print a short name for a DXIL module.
fn fnv(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xCBF2_9CE4_8422_2325, |h, &b| (h ^ u64::from(b)).wrapping_mul(0x100_0000_01B3))
}

/// The least of `reps` runs of `f`, and its last result.
fn timed<T>(reps: u32, mut f: impl FnMut() -> T) -> (Duration, T) {
    let mut best = Duration::MAX;
    let mut out = None;
    for _ in 0..reps {
        let start = Instant::now();
        let r = f();
        best = best.min(start.elapsed());
        out = Some(r);
    }
    (best, out.expect("one run"))
}

#[test]
#[ignore = "a measurement: run it in release with --ignored --nocapture"]
fn translation_speed() {
    let out_dir = std::env::var_os("VICTORY_SPEED_OUT").map(std::path::PathBuf::from);
    let ms = |d: Duration| d.as_secs_f64() * 1e3;
    println!(
        "{:<8} {:>6} {:>6} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8}  dxil",
        "program", "insts", "blocks", "decode", "ir", "struct", "track", "io", "lower", "opt", "write", "total"
    );
    // The four sizes are the measurement; the others (`mix`, `w64`: a console
    // wave64 program on a host of 32-lane waves, as on NVIDIA) widen what the
    // byte-for-byte comparison covers.
    let mut programs: Vec<(String, u64, i64, bool)> = [
        ("small", 1, 400),
        ("medium", 2, 2_000),
        ("large", 3, 6_000),
        ("huge", 4, 15_000),
    ]
    .into_iter()
    .map(|(label, seed, size)| (label.to_owned(), seed, size, false))
    .collect();
    for n in 0..8 {
        programs.push((format!("mix{n}"), 10 + n, 300 + 500 * n as i64, false));
    }
    for n in 0..4 {
        programs.push((format!("w64-{n}"), 20 + n, 300 + 900 * n as i64, true));
    }
    // (`VICTORY_SPEED_ONLY=<text>`: the programs whose name holds it)
    let only = std::env::var("VICTORY_SPEED_ONLY").unwrap_or_default();
    for (label, seed, size, wave64) in programs.into_iter().filter(|p| p.0.contains(&only)) {
        let words = program(seed, size, wave64);
        let native = native(&words, wave64);
        lower::TEST_HOST_WAVE.set(wave64.then_some(32));
        // (`VICTORY_SPEED_REPS`: more runs, for a profiler)
        let reps = std::env::var("VICTORY_SPEED_REPS")
            .ok()
            .and_then(|n| n.parse().ok())
            .unwrap_or(if size > 10_000 { 2 } else { 5 });
        let (decode, insts) = timed(reps, || gfx10::decode(&native.code).expect("decode").len());
        let (build, p) = timed(reps, || ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build"));
        let (structure, st) = timed(reps, || structure::structure(&p).expect("structure"));
        let (track, res) =
            timed(reps, || resource::track(&p, &native.layout, native.stage, Some(&st)).expect("track"));
        let (raise, io) = timed(reps, || io::raise(&p, &native).expect("io::raise"));
        // The lowering, with the clean-up and the writing apart (the least of each).
        let [mut own, mut opt, mut write] = [Duration::MAX; 3];
        let mut dxil = Vec::new();
        for _ in 0..reps {
            super::CLOCK.set([Duration::ZERO; 2]);
            let start = Instant::now();
            dxil = lower::lower(&lower::Parts {
                native: &native,
                p: &p,
                st: &st,
                io: &io,
                res: &res,
                inputs: Vec::new(),
                outputs: Vec::new(),
                prims: Vec::new(),
                mesh_stride: 0,
            })
            .expect("lower")
            .dxil;
            let all = start.elapsed();
            let [o, w] = super::CLOCK.get();
            own = own.min(all.saturating_sub(o + w));
            opt = opt.min(o);
            write = write.min(w);
        }
        lower::TEST_HOST_WAVE.set(None);
        let total = build + structure + track + raise + own + opt + write;
        println!(
            "{label:<8} {insts:>6} {:>6} {:>8.2} {:>8.2} {:>8.2} {:>8.2} {:>8.2} {:>8.2} {:>8.2} {:>8.2} {:>8.2}  {} B {:016x}",
            p.blocks.len(),
            ms(decode),
            ms(build),
            ms(structure),
            ms(track),
            ms(raise),
            ms(own),
            ms(opt),
            ms(write),
            ms(total),
            dxil.len(),
            fnv(&dxil),
        );
        if let Some(dir) = &out_dir {
            std::fs::create_dir_all(dir).expect("output folder");
            std::fs::write(dir.join(format!("{label}.dxil")), &dxil).expect("write the DXIL");
        }
    }
    println!("(milliseconds, the best of several runs; ir includes decode, lower excludes opt and write)");
}
/// The whole translation of each `mix` program, on 1 to 16 threads at once
/// (`victory shaders --jobs`): translations a second, and how far the
/// threads keep each other back (the allocator's locks, say).
#[test]
#[ignore = "a measurement: run it in release with --ignored --nocapture"]
fn translation_threads() {
    let programs: Vec<NativeProgram> =
        (0..8).map(|n| native(&program(10 + n, 300 + 500 * n as i64, false), false)).collect();
    let translate = |native: &NativeProgram| {
        let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
        let st = structure::structure(&p).expect("structure");
        let res = resource::track(&p, &native.layout, native.stage, Some(&st)).expect("track");
        let io = io::raise(&p, native).expect("io::raise");
        lower::lower(&lower::Parts {
            native,
            p: &p,
            st: &st,
            io: &io,
            res: &res,
            inputs: Vec::new(),
            outputs: Vec::new(),
            prims: Vec::new(),
            mesh_stride: 0,
        })
        .expect("lower")
        .dxil
        .len()
    };
    const JOBS: usize = 400;
    let mut one = 0.0;
    for threads in [1, 2, 4, 8, 16] {
        let next = std::sync::atomic::AtomicUsize::new(0);
        let start = Instant::now();
        std::thread::scope(|s| {
            for _ in 0..threads {
                s.spawn(|| {
                    loop {
                        let n = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        if n >= JOBS {
                            break;
                        }
                        std::hint::black_box(translate(&programs[n % programs.len()]));
                    }
                });
            }
        });
        let rate = JOBS as f64 / start.elapsed().as_secs_f64();
        if threads == 1 {
            one = rate;
        }
        println!("{threads:>2} threads: {rate:>7.0} translations/s, {:.2}x one thread", rate / one);
    }
}