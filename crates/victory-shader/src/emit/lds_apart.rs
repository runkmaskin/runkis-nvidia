//! LDS that both halves of a console wave read and write inside one lane-branch region.
//!
//! A console wave64 program on a host of 32-lane waves runs each console wave as two host
//! waves. The console runs the wave's 64 lanes in step, so their LDS accesses happen in
//! program order; the two host waves do not keep that order. The lowering puts a group
//! barrier before every LDS access in code every lane runs, and around each lane-branch
//! region (the blocks only some lanes run, where a barrier could wait for threads that never
//! come: `lds_meetings` in [`super::lower`]). Inside one region nothing orders the halves, so a
//! region that both reads and writes LDS was refused.
//!
//! It needs no order when the two halves never touch an LDS dword in ways that conflict
//! there: then any interleaving of the halves gives what the console's lockstep gives.
//! [`Halves::apart`] proves that for one region by running the program's address arithmetic
//! for every thread of the group:
//!
//! - Each LDS address, and each lane condition on the way to the access, is evaluated per
//!   thread as a constant plus scalars times constants (modulo 2^32). The scalars are the
//!   program's inputs, loads, and arithmetic of them, which are the same in both halves of a
//!   console wave. A loop join that adds one constant each round is its value on entry plus
//!   the step times a count of rounds, which is each thread's own (the halves need not be in
//!   the same round) and below the loop limit. Lane masks, scalars a vector instruction or a
//!   mask makes, loaded and LDS values, and other joins of different values are not known. An
//!   address that depends on one refuses the region; a condition that depends on one is taken
//!   to hold.
//! - A thread runs an access unless its conditions (the block's reach and the instruction's
//!   guard) are false for it. `x == y` is false where `x - y` is a constant the scalar terms
//!   cannot make zero: `64 * group + t == 0` for t = 1..63. The access's own conditions hold
//!   where its address is worked out: an instruction under a guard they rule out (the other
//!   arm of an `if`) did not run, and a join takes no value from a way in they rule out.
//! - Two accesses by the two halves of one console wave conflict when they may touch one
//!   dword and one of them writes, unless both are the same commuting atomic (`ds_add_u32`
//!   and `ds_add_rtn_u32`, `or`, `max` ...). For those the order of the halves changes only
//!   which old value each lane gets back, as the order of the console's waves already does.
//!   The distance of two addresses is a constant, plus the scalars that do not cancel and the
//!   counts of rounds times their factors: it can only take the constant's value modulo the
//!   largest power of two dividing every factor, within what the terms' ranges give (group ids
//!   below 65536, counts below the loop limit, any other scalar any 32-bit value).
//! - The title's own barriers in the region split it, where they wait for the group
//!   (`VICTORY_GROUP_SYNC` unset or 2): accesses a different number of barriers into the
//!   region are ordered by them. A block that paths reach after different numbers of
//!   barriers counts as any number.
//!
//! shortcut: conflicts between threads of different console waves are left as they are; the
//! console runs those waves in no fixed order either.
//! shortcut: the host's barriers are taken to be run by every thread of the group in the same
//! order, as elsewhere in the translation.
//! `VICTORY_LDS_APART=0` refuses such regions as before.

use std::cell::RefCell;
use std::collections::HashMap;

use crate::gfx10::{Fields, NULL, Operand};
use crate::ir::{EXEC, Inst, Program, Reg, VCC, Value};
use crate::native::ComputeIo;
use crate::structure::{Atom, Bool, LaneExit, Structured};

type Why = &'static str;

/// `VICTORY_LDS_APART=0`: regions where both halves read and write LDS are refused as before.
pub(super) fn on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_LDS_APART").is_none_or(|v| v != "0"))
}

const UNKNOWN_ADDRESS: Why = "an LDS address not known per thread";
const SCALAR_APART: Why = "LDS addresses of the two halves that differ by a scalar";
const SHARED: Why = "an LDS dword both halves of a console wave touch in one region";
const GDS: Why = "GDS";

/// Deeper than this, a value counts as not known: the evaluation recurses.
const DEPTH: u32 = 400;
/// Values one thread works out at most (loops are followed more than once); past it the
/// rest are not known.
const STEPS: u32 = 50_000;

/// A 32-bit value: `k` plus each scalar times its factor, modulo 2^32. The terms are sorted
/// by value and have no zero factor, so equal forms are equal values.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Form {
    k: u32,
    terms: Vec<(Value, u32)>,
}

impl Form {
    fn konst(k: u32) -> Self {
        Form { k, terms: Vec::new() }
    }

    fn scalar(v: Value) -> Self {
        Form { k: 0, terms: vec![(v, 1)] }
    }

    fn value(&self) -> Option<u32> {
        self.terms.is_empty().then_some(self.k)
    }

    fn add(&self, o: &Form) -> Form {
        let mut terms = self.terms.clone();
        for &(v, c) in &o.terms {
            match terms.binary_search_by_key(&v, |t| t.0) {
                Ok(i) => {
                    terms[i].1 = terms[i].1.wrapping_add(c);
                    if terms[i].1 == 0 {
                        terms.remove(i);
                    }
                }
                Err(i) => terms.insert(i, (v, c)),
            }
        }
        Form { k: self.k.wrapping_add(o.k), terms }
    }

    fn scale(&self, c: u32) -> Form {
        Form {
            k: self.k.wrapping_mul(c),
            terms: self.terms.iter().map(|&(v, x)| (v, x.wrapping_mul(c))).filter(|t| t.1 != 0).collect(),
        }
    }

    fn sub(&self, o: &Form) -> Form {
        self.add(&o.scale(u32::MAX))
    }

    /// Whether some values of the scalars make it zero: `k + sum(c * s) = 0 (mod 2^32)` has a
    /// solution exactly when the largest power of two dividing every factor divides `k`.
    fn may_be_zero(&self) -> bool {
        match self.terms.iter().map(|t| t.1.trailing_zeros()).min() {
            None => self.k == 0,
            Some(z) => self.k.trailing_zeros() >= z,
        }
    }
}

/// Where a value is made.
#[derive(Debug, Clone, Copy)]
enum Def {
    Inst(usize, usize),
    Phi(usize, usize),
    Input(Reg),
}

/// How an access uses LDS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Read,
    Write,
    /// A commuting atomic, by its class: any two of one class may run in either order.
    Atomic(&'static str),
}

/// One LDS access of a region: its block and index, how it uses LDS, its address register,
/// its byte ranges (offset from the address, length; a byte or half covers the dword around
/// it), and how many of the region's barriers come before it (none: not one number).
struct Access {
    b: usize,
    i: usize,
    kind: Kind,
    addr: Value,
    ranges: Vec<(u32, u32)>,
    depth: Option<u32>,
}

/// A console wave64 compute program's values, evaluated thread by thread ([`Halves::apart`]).
pub(super) struct Halves<'a> {
    p: &'a Program,
    st: &'a Structured,
    /// Per block: every lane runs it ([`super::lower`]'s `all_lane_blocks`).
    all: &'a [bool],
    /// Per block: the loops around it, by number.
    loops: Vec<Vec<usize>>,
    /// Per block: its place in reverse postorder.
    index: Vec<usize>,
    threads: [u32; 3],
    /// Thread id components in v0, v1, v2.
    ids: u8,
    def: HashMap<Value, Def>,
    /// The group id inputs: below 65536 (D3D12's largest dispatch).
    group_ids: Vec<Value>,
    /// Scalars by value, the same in every thread (`None`: not known).
    scalars: RefCell<HashMap<Value, Option<Form>>>,
    /// SCC values made only of known scalars the halves share.
    sccs: RefCell<HashMap<Value, bool>>,
    /// Loop joins that count rounds ([`Self::rounds`]): as a term, the rounds since the loop
    /// was entered, below the loop limit. Each thread, and each half, has its own count.
    counted: RefCell<std::collections::HashSet<Value>>,
    /// Whether conditions that hold decide another one ([`Thread::guard`]).
    implied: RefCell<HashMap<(Vec<Bool>, Bool), Option<bool>>>,
}

impl<'a> Halves<'a> {
    pub(super) fn new(p: &'a Program, st: &'a Structured, all: &'a [bool], cs: &ComputeIo) -> Self {
        let mut def = HashMap::new();
        for &(reg, v) in &p.inputs {
            def.insert(v, Def::Input(reg));
        }
        // the loops (natural loops of the back edges) around each block, and the blocks' order
        let order = crate::ir::reverse_postorder(&p.blocks);
        let mut index = vec![usize::MAX; p.blocks.len()];
        for (k, &b) in order.iter().enumerate() {
            index[b] = k;
        }
        let idom = crate::ir::idoms(&p.blocks, &order, &index);
        let dominates = |a: usize, mut b: usize| loop {
            if a == b {
                break true;
            }
            if idom[b] == usize::MAX || idom[b] == b {
                break false;
            }
            b = idom[b];
        };
        let mut loops: Vec<Vec<usize>> = vec![Vec::new(); p.blocks.len()];
        let mut n = 0;
        for &b in &order {
            for t in p.blocks[b].exit.successors() {
                if !dominates(t, b) {
                    continue;
                }
                let mut body = vec![false; p.blocks.len()];
                body[t] = true;
                let mut stack = vec![b];
                while let Some(x) = stack.pop() {
                    if !body[x] {
                        body[x] = true;
                        stack.extend(p.blocks[x].preds.iter().copied());
                    }
                }
                for (x, inside) in body.into_iter().enumerate() {
                    if inside {
                        loops[x].push(n);
                    }
                }
                n += 1;
            }
        }
        let ids: Vec<u8> = cs.system_sgprs().0.into_iter().flatten().collect();
        let group_ids =
            p.inputs.iter().filter(|(reg, _)| matches!(reg, Reg::S(c) if ids.contains(c))).map(|&(_, v)| v).collect();
        for (b, block) in p.blocks.iter().enumerate() {
            for (k, phi) in block.phis.iter().enumerate() {
                def.insert(phi.value, Def::Phi(b, k));
            }
            for (i, inst) in block.insts.iter().enumerate() {
                for &(_, v) in &inst.writes {
                    def.insert(v, Def::Inst(b, i));
                }
            }
        }
        Halves {
            p,
            st,
            all,
            loops,
            index,
            threads: cs.threads,
            ids: cs.thread_id_vgprs(),
            def,
            group_ids,
            scalars: RefCell::new(HashMap::new()),
            sccs: RefCell::new(HashMap::new()),
            counted: RefCell::new(std::collections::HashSet::new()),
            implied: RefCell::new(HashMap::new()),
        }
    }

    /// Whether a term's value is a count of loop rounds, each thread's own.
    fn is_count(&self, v: Value) -> bool {
        self.counted.borrow().contains(&v)
    }

    /// Whether a form holds a count of loop rounds.
    fn counts(&self, f: &Form) -> bool {
        f.terms.iter().any(|t| self.is_count(t.0))
    }

    /// The values a term can take, as integers.
    fn range(&self, v: Value) -> (i128, i128) {
        if self.is_count(v) {
            (0, i128::from(super::loop_limit()))
        } else if self.group_ids.contains(&v) {
            (0, 65535)
        } else {
            (0, i128::from(u32::MAX))
        }
    }

    /// A loop join of `inits` (the ways in from outside the loop, all one value) and of rounds
    /// that add one constant to it (each the join itself plus that constant): the value on
    /// entry plus the rounds since the loop was entered times the step. `None` otherwise.
    fn rounds(&self, v: Value, args: &[Option<Form>]) -> Option<Form> {
        let own = Form::scalar(v);
        let (mut init, mut step): (Option<Form>, Option<u32>) = (None, None);
        for f in args {
            let f = f.as_ref()?;
            if f.terms.iter().any(|t| t.0 == v) {
                let delta = f.sub(&own);
                let d = delta.value()?;
                // (rounds of different steps would not keep the count below the limit)
                if d != 0 && step.is_some_and(|s| s != d) {
                    return None;
                }
                if d != 0 {
                    step = Some(d);
                }
            } else if init.as_ref().is_some_and(|i| i != f) {
                return None;
            } else {
                init = Some(f.clone());
            }
        }
        let init = init?;
        let Some(step) = step else { return Some(init) };
        self.counted.borrow_mut().insert(v);
        Some(init.add(&own.scale(step)))
    }

    /// Whether `v` is a lane mask (its bits are per lane, not one number).
    fn is_mask(&self, v: Value) -> bool {
        self.st.masks.contains_key(&v) || self.st.highs.contains_key(&v)
    }

    /// The value of scalar `v`, the same in both halves of a console wave; `None` if it is not
    /// known to be.
    fn scalar(&self, v: Value, depth: u32) -> Option<Form> {
        if let Some(f) = self.scalars.borrow().get(&v) {
            return f.clone();
        }
        if depth > DEPTH {
            return None;
        }
        // (a join that leads back to itself is not known while it is worked out)
        self.scalars.borrow_mut().insert(v, None);
        let f = if self.is_mask(v) { None } else { self.scalar_of(v, depth + 1) };
        self.scalars.borrow_mut().insert(v, f.clone());
        f
    }

    fn scalar_of(&self, v: Value, depth: u32) -> Option<Form> {
        match *self.def.get(&v)? {
            // user data and the group's ids
            Def::Input(Reg::S(c)) => (c < VCC).then(|| Form::scalar(v)),
            Def::Input(_) => None,
            Def::Phi(b, k) => {
                let phi = &self.p.blocks[b].phis[k];
                // (the join as a term of its own while the ways back into it are worked out;
                // the values made of that term are forgotten after: the term becomes a count)
                self.scalars.borrow_mut().insert(v, Some(Form::scalar(v)));
                let args: Vec<Option<Form>> =
                    phi.args.iter().filter(|&&a| a != v).map(|&a| self.scalar(a, depth)).collect();
                self.scalars.borrow_mut().retain(|_, f| !holds_term(f, v));
                self.scalars.borrow_mut().insert(v, None);
                if args.is_empty() || args.iter().any(Option::is_none) {
                    return None;
                }
                if args.windows(2).all(|w| w[0] == w[1]) && !holds_term(&args[0], v) {
                    return args.into_iter().next().flatten();
                }
                // Where every lane runs the join, both halves of a console wave are in the same
                // round of any loop around a region there (the halves meet around the region):
                // the join is a scalar they share. Elsewhere it is one only as a count of rounds.
                if self.all[b] && args.iter().flatten().all(|f| !self.counts(f)) {
                    Some(Form::scalar(v))
                } else {
                    self.rounds(v, &args)
                }
            }
            Def::Inst(b, i) => {
                let inst = &self.p.blocks[b].insts[i];
                if !self.reads_known(inst, depth) {
                    return None;
                }
                let exact = match &inst.fields {
                    Fields::Sop { .. } => self.salu(inst, v, depth),
                    _ => None,
                };
                // Any other result of known scalars is one the halves share, unless it depends
                // on a count of rounds (the halves may be in different rounds).
                exact.or_else(|| self.reads_shared(inst, depth).then(|| Form::scalar(v)))
            }
        }
    }

    /// Whether every register `inst` reads is a known scalar (a SALU or SMEM instruction).
    fn reads_known(&self, inst: &Inst, depth: u32) -> bool {
        matches!(inst.fields, Fields::Sop { .. } | Fields::Smem { .. })
            && inst.reads.iter().all(|&(reg, x)| match reg {
                Reg::S(c) => c != EXEC && c != EXEC + 1 && self.scalar(x, depth).is_some(),
                Reg::Scc => self.scc_known(x, depth),
                Reg::V(_) => false,
            })
    }

    /// Whether `inst` reads only known scalars without a count of rounds in them.
    fn reads_shared(&self, inst: &Inst, depth: u32) -> bool {
        self.reads_known(inst, depth)
            && inst.reads.iter().all(|&(reg, x)| match reg {
                Reg::S(_) => self.scalar(x, depth).is_some_and(|f| !self.counts(&f)),
                _ => true,
            })
    }

    /// Whether SCC value `v` is made of known scalars the halves share.
    fn scc_known(&self, v: Value, depth: u32) -> bool {
        if let Some(&k) = self.sccs.borrow().get(&v) {
            return k;
        }
        self.sccs.borrow_mut().insert(v, false);
        let k = depth <= DEPTH
            && match self.def.get(&v) {
                Some(&Def::Inst(b, i)) => self.reads_shared(&self.p.blocks[b].insts[i], depth + 1),
                Some(&Def::Phi(b, k)) => {
                    self.p.blocks[b].phis[k].args.iter().all(|&a| a == v || self.scc_known(a, depth + 1))
                }
                _ => false,
            };
        self.sccs.borrow_mut().insert(v, k);
        k
    }

    /// A SALU operand as a form.
    fn salu_operand(&self, inst: &Inst, op: &Operand, depth: u32) -> Option<Form> {
        match *op {
            Operand::Scalar { code: NULL, .. } => Some(Form::konst(0)),
            Operand::Scalar { code, dwords: 1 } => self.scalar(inst.read(Reg::S(code))?, depth),
            Operand::Const(c) => inline(c).map(Form::konst),
            Operand::Literal(x) => Some(Form::konst(x)),
            _ => None,
        }
    }

    /// The 32-bit SALU arithmetic `v` is made by, worked out; `None` for any other.
    fn salu(&self, inst: &Inst, v: Value, depth: u32) -> Option<Form> {
        let Fields::Sop { dst: Some(Operand::Scalar { code, dwords: 1 }), src, imm } = &inst.fields else {
            return None;
        };
        if inst.write(Reg::S(*code)) != Some(v) {
            return None;
        }
        let s = |n: usize| src.get(n).and_then(|op| self.salu_operand(inst, op, depth));
        let k = |n: usize| s(n).and_then(|f| f.value());
        let simm = imm.map(|i| i as i16 as i32 as u32);
        Some(match inst.name() {
            "s_mov_b32" => s(0)?,
            "s_movk_i32" => Form::konst(simm?),
            "s_add_u32" | "s_add_i32" => s(0)?.add(&s(1)?),
            "s_sub_u32" | "s_sub_i32" => s(0)?.sub(&s(1)?),
            "s_addk_i32" => self.scalar(inst.read(Reg::S(*code))?, depth)?.add(&Form::konst(simm?)),
            "s_mulk_i32" => self.scalar(inst.read(Reg::S(*code))?, depth)?.scale(simm?),
            "s_mul_i32" => match (k(0), k(1)) {
                (Some(a), _) => s(1)?.scale(a),
                (_, Some(b)) => s(0)?.scale(b),
                _ => return None,
            },
            "s_lshl_b32" => s(0)?.scale(1u32 << (k(1)? & 31)),
            "s_lshl1_add_u32" => s(0)?.scale(2).add(&s(1)?),
            "s_lshl2_add_u32" => s(0)?.scale(4).add(&s(1)?),
            "s_lshl3_add_u32" => s(0)?.scale(8).add(&s(1)?),
            "s_lshl4_add_u32" => s(0)?.scale(16).add(&s(1)?),
            name => Form::konst(bits(name, &[k(0)?, k(1)?])?),
        })
    }

    /// Whether every LDS access of `region` (blocks only some lanes run, joined by the edges
    /// between them) leaves the two halves of each console wave apart: no dword that both
    /// halves touch there, one of them writing, between two of the region's barriers.
    /// `waits`: the title's barriers in the region wait for the group. The error names the
    /// instruction's offset.
    pub(super) fn apart(&self, region: &[usize], waits: bool) -> Result<(), (usize, Why)> {
        let n = self.p.blocks.len();
        let mut inside = vec![false; n];
        for &b in region {
            inside[b] = true;
        }
        let succ = |b: usize| -> Vec<usize> {
            match self.st.exits[b] {
                LaneExit::Jump(t) => vec![t],
                LaneExit::Branch { taken, not_taken, .. } => vec![taken, not_taken],
                LaneExit::End | LaneExit::Indirect => Vec::new(),
            }
        };
        // The title's barriers that wait, by block: their positions.
        let barriers: Vec<Vec<usize>> = (0..n)
            .map(|b| {
                if !waits || !inside[b] {
                    return Vec::new();
                }
                let sections = super::wave::sections(self.p, b);
                let in_section = |k: usize| sections.iter().any(|(s, sec)| k > *s && k < sec.end);
                let insts = &self.p.blocks[b].insts;
                (0..insts.len()).filter(|&k| insts[k].name() == "s_barrier" && !in_section(k)).collect()
            })
            .collect();
        // Barriers passed since the region was entered, at each block's start: unset, one
        // number, or several (None).
        let mut depth: Vec<Option<Option<u32>>> = vec![None; n];
        let mut work = Vec::new();
        for b in 0..n {
            if !inside[b] {
                for t in succ(b) {
                    if inside[t] && depth[t].is_none() {
                        depth[t] = Some(Some(0));
                        work.push(t);
                    }
                }
            }
        }
        while let Some(b) = work.pop() {
            let out = depth[b].flatten().map(|d| d + barriers[b].len() as u32);
            for t in succ(b) {
                if !inside[t] {
                    continue;
                }
                let next = match depth[t] {
                    None => Some(out),
                    Some(d) if d != out && d.is_some() => Some(None),
                    _ => None,
                };
                if let Some(next) = next {
                    depth[t] = Some(next);
                    work.push(t);
                }
            }
        }
        let mut accesses = Vec::new();
        for &b in region {
            for (i, inst) in self.p.blocks[b].insts.iter().enumerate() {
                let Fields::Ds { addr, offset0, offset1, gds, .. } = &inst.fields else { continue };
                let name = inst.name();
                if name.starts_with("ds_swizzle") || name.contains("permute") {
                    continue;
                }
                let offset = inst.machine.offset;
                if *gds {
                    return Err((offset, GDS));
                }
                let Some(Operand::Vector { reg, .. }) = addr else { return Err((offset, UNKNOWN_ADDRESS)) };
                let addr = inst.read(Reg::V(*reg)).ok_or((offset, UNKNOWN_ADDRESS))?;
                let at = barriers[b].iter().filter(|&&k| k < i).count() as u32;
                accesses.push(Access {
                    b,
                    i,
                    kind: kind(name),
                    addr,
                    ranges: ranges(name, *offset0, *offset1),
                    depth: depth[b].flatten().map(|d| d + at),
                });
            }
        }
        let group: u32 = self.threads.iter().product();
        // (thread, start, length, access) of each half
        type Touch = (u32, Form, u32, usize);
        for wave in 0..group.div_ceil(64) {
            let mut halves: [Vec<Touch>; 2] = [Vec::new(), Vec::new()];
            for lane in 0..64 {
                let t = 64 * wave + lane;
                if t >= group {
                    break;
                }
                // (one evaluation per block and guard: an address is only needed where its
                // access runs, so the access's conditions hold where it is worked out)
                let mut plain = Thread::new(self, t, Vec::new(), None);
                let mut by_guard: HashMap<(Bool, Option<Bool>), Thread> = HashMap::new();
                for (a, acc) in accesses.iter().enumerate() {
                    let (reach, guard) = (self.st.reach[acc.b], self.st.guards[acc.b][acc.i]);
                    let runs = plain.may(reach) && guard.is_none_or(|g| plain.may(g));
                    if !runs {
                        continue;
                    }
                    let holds = [Some(reach), guard].into_iter().flatten().collect();
                    let th = by_guard.entry((reach, guard)).or_insert_with(|| Thread::new(self, t, holds, Some(acc.b)));
                    let offset = self.p.blocks[acc.b].insts[acc.i].machine.offset;
                    let base = th.value(acc.addr, 0).ok_or((offset, UNKNOWN_ADDRESS))?;
                    for &(off, len) in &acc.ranges {
                        halves[(lane / 32) as usize].push((t, base.add(&Form::konst(off)), len, a));
                    }
                }
            }
            for x in &halves[0] {
                for y in &halves[1] {
                    let (p, q) = (&accesses[x.3], &accesses[y.3]);
                    let ordered = matches!((p.depth, q.depth), (Some(d), Some(e)) if d != e);
                    let commute = match (p.kind, q.kind) {
                        (Kind::Read, Kind::Read) => true,
                        (Kind::Atomic(c), Kind::Atomic(d)) => c == d,
                        _ => false,
                    };
                    if ordered || commute {
                        continue;
                    }
                    if self.may_meet(&x.1, x.2, &y.1, y.2) {
                        let offset = self.p.blocks[q.b].insts[q.i].machine.offset;
                        let shared = |f: &Form| f.terms.iter().any(|t| !self.is_count(t.0));
                        let apart = if shared(&x.1) || shared(&y.1) { SCALAR_APART } else { SHARED };
                        return Err((offset, apart));
                    }
                }
            }
        }
        Ok(())
    }

    /// Whether bytes `[x, x + lx)` of one thread and `[y, y + ly)` of a thread of the other
    /// half may overlap, modulo 2^32. The scalars they share cancel; counts of rounds are each
    /// thread's own. What is left of `y - x` is a constant plus terms times factors: every value
    /// it takes is the constant modulo the largest power of two dividing every factor, and lies
    /// within the terms' ranges.
    fn may_meet(&self, x: &Form, lx: u32, y: &Form, ly: u32) -> bool {
        let mut d = Form::konst(y.k.wrapping_sub(x.k));
        let mut own: Vec<(Value, u32)> = Vec::new();
        for &(v, c) in &y.terms {
            if self.is_count(v) {
                own.push((v, c));
            } else {
                d = d.add(&Form { k: 0, terms: vec![(v, c)] });
            }
        }
        for &(v, c) in &x.terms {
            if self.is_count(v) {
                own.push((v, c.wrapping_neg()));
            } else {
                d = d.sub(&Form { k: 0, terms: vec![(v, c)] });
            }
        }
        let terms: Vec<(Value, u32)> = d.terms.iter().copied().chain(own).filter(|t| t.1 != 0).collect();
        // the window of distances that overlap: (-ly, lx)
        let (wlo, whi) = (1 - i128::from(ly), i128::from(lx) - 1);
        let c = i128::from(d.k as i32);
        if terms.is_empty() {
            return (wlo..=whi).contains(&c);
        }
        let g = 1i128 << terms.iter().map(|t| t.1.trailing_zeros()).min().unwrap_or(0);
        let (mut lo, mut hi) = (c, c);
        for &(v, f) in &terms {
            let f = i128::from(f as i32);
            let (a, b) = self.range(v);
            lo += (f * a).min(f * b);
            hi += (f * a).max(f * b);
        }
        // the distance modulo 2^32: the window moved by every multiple of 2^32 the range meets
        let wrap = 1i128 << 32;
        let first = (lo - whi).div_euclid(wrap);
        let last = (hi - wlo).div_euclid(wrap) + 1;
        if last - first > 4 {
            return true;
        }
        (first..=last).any(|m| {
            let (from, to) = (lo.max(wlo + m * wrap), hi.min(whi + m * wrap));
            // the first value from `from` on that is `c` modulo `g`
            from <= to && from + (c - from).rem_euclid(g) <= to
        })
    }
}

/// Whether a known value has term `v`.
fn holds_term(f: &Option<Form>, v: Value) -> bool {
    f.as_ref().is_some_and(|f| f.terms.iter().any(|t| t.0 == v))
}

/// How an LDS instruction uses LDS.
fn kind(name: &str) -> Kind {
    if name.starts_with("ds_read") {
        return Kind::Read;
    }
    if name.starts_with("ds_write") && !name.starts_with("ds_wrxchg") {
        return Kind::Write;
    }
    let op = name.trim_start_matches("ds_").replace("_rtn", "");
    match op.as_str() {
        // adding and subtracting commute with each other
        "add_u32" | "sub_u32" => Kind::Atomic("add"),
        "add_u64" | "sub_u64" => Kind::Atomic("add64"),
        "and_b32" => Kind::Atomic("and"),
        "or_b32" => Kind::Atomic("or"),
        "xor_b32" => Kind::Atomic("xor"),
        "min_i32" => Kind::Atomic("min_i32"),
        "max_i32" => Kind::Atomic("max_i32"),
        "min_u32" => Kind::Atomic("min_u32"),
        "max_u32" => Kind::Atomic("max_u32"),
        "min_f32" => Kind::Atomic("min_f32"),
        "max_f32" => Kind::Atomic("max_f32"),
        // exchanges, compare-and-swaps, wrapping increments: their order matters
        _ => Kind::Write,
    }
}

/// The byte ranges an LDS instruction touches, from its address: (offset, length). As the
/// lowering takes them (`ds` in [`super::lower`]): read2/write2 scale their two offsets by the
/// element size (x64 for st64), the others take both offset bytes as one. A byte or a half
/// covers the dword around it: three bytes before it to four after.
fn ranges(name: &str, offset0: u8, offset1: u8) -> Vec<(u32, u32)> {
    let w = match name.rsplit('_').next() {
        Some("b64" | "u64" | "i64" | "f64") => 2,
        Some("b96") => 3,
        Some("b128") => 4,
        _ => 1,
    };
    let base = name.trim_end_matches("_hi").trim_end_matches("_d16");
    if matches!(base, "ds_read_u8" | "ds_read_i8" | "ds_write_b8" | "ds_read_u16" | "ds_read_i16" | "ds_write_b16") {
        let at = u32::from(offset0) | u32::from(offset1) << 8;
        return vec![(at.wrapping_sub(3), 7)];
    }
    let two = name.starts_with("ds_read2") || name.starts_with("ds_write2") || name.starts_with("ds_wrxchg2");
    if two {
        let scale = 4 * w * if name.contains("st64") { 64 } else { 1 };
        vec![(u32::from(offset0) * scale, 4 * w), (u32::from(offset1) * scale, 4 * w)]
    } else {
        vec![(u32::from(offset0) | u32::from(offset1) << 8, 4 * w)]
    }
}

/// An inline constant's bits.
fn inline(code: u8) -> Option<u32> {
    Some(match code {
        128..=192 => u32::from(code - 128),
        193..=208 => (192 - i32::from(code)) as u32,
        240 => 0x3F00_0000,
        241 => 0xBF00_0000,
        242 => 0x3F80_0000,
        243 => 0xBF80_0000,
        244 => 0x4000_0000,
        245 => 0xC000_0000,
        246 => 0x4080_0000,
        247 => 0xC080_0000,
        248 => 0x3E22_F983,
        _ => return None,
    })
}

/// Bit operations of constants, by the VALU or SALU name; `None` for the others.
fn bits(name: &str, s: &[u32]) -> Option<u32> {
    let a = |n: usize| s.get(n).copied();
    let op = name.trim_start_matches("v_").trim_start_matches("s_");
    Some(match op {
        "and_b32" => a(0)? & a(1)?,
        "or_b32" => a(0)? | a(1)?,
        "xor_b32" => a(0)? ^ a(1)?,
        "not_b32" => !a(0)?,
        "lshr_b32" => a(0)? >> (a(1)? & 31),
        "lshrrev_b32" => a(1)? >> (a(0)? & 31),
        "ashr_i32" => ((a(0)? as i32) >> (a(1)? & 31)) as u32,
        "ashrrev_i32" => ((a(1)? as i32) >> (a(0)? & 31)) as u32,
        "bfe_u32" if name.starts_with("v_") => {
            let (off, width) = (a(1)? & 31, a(2)? & 31);
            if width == 0 { 0 } else { (a(0)? >> off) & (u32::MAX >> (32 - width)) }
        }
        "bfe_u32" => {
            let (off, width) = (a(1)? & 31, a(1)? >> 16 & 0x7F);
            match width {
                0 => 0,
                32.. => a(0)? >> off,
                w => (a(0)? >> off) & ((1 << w) - 1),
            }
        }
        "bfi_b32" => a(0)? & a(1)? | !a(0)? & a(2)?,
        "and_or_b32" => a(0)? & a(1)? | a(2)?,
        "or3_b32" => a(0)? | a(1)? | a(2)?,
        "lshl_or_b32" => a(0)?.wrapping_shl(a(1)? & 31) | a(2)?,
        "min_u32" => a(0)?.min(a(1)?),
        "max_u32" => a(0)?.max(a(1)?),
        "min_i32" => (a(0)? as i32).min(a(1)? as i32) as u32,
        "max_i32" => (a(0)? as i32).max(a(1)? as i32) as u32,
        "mul_u32_u24" => (a(0)? & 0xFF_FFFF).wrapping_mul(a(1)? & 0xFF_FFFF),
        "mul_hi_u32" => ((u64::from(a(0)?) * u64::from(a(1)?)) >> 32) as u32,
        _ => return None,
    })
}

/// One thread's values ([`Halves::apart`]).
struct Thread<'h, 'a> {
    h: &'h Halves<'a>,
    /// Flat index in the group, and its x, y, z.
    flat: u32,
    id: [u32; 3],
    memo: HashMap<Value, Option<Form>>,
    /// Per lane condition: whether it can hold, whether it can fail.
    tri: HashMap<Bool, (bool, bool)>,
    /// Conditions taken to hold: the reach and guard of the access whose address is wanted,
    /// and its block.
    holds: Vec<Bool>,
    at: Option<usize>,
    /// The values and conditions worked out without holds (ways back into a loop).
    plain: (HashMap<Value, Option<Form>>, HashMap<Bool, (bool, bool)>),
    /// Values worked out so far: past a bound the rest are not known.
    steps: u32,
}

impl<'h, 'a> Thread<'h, 'a> {
    fn new(h: &'h Halves<'a>, flat: u32, holds: Vec<Bool>, at: Option<usize>) -> Self {
        let [x, y, _] = h.threads;
        let id = [flat % x, flat / x % y, flat / (x * y)];
        let tri = holds.iter().map(|&g| (g, (true, false))).collect();
        Thread { h, flat, id, memo: HashMap::new(), tri, holds, at, plain: Default::default(), steps: 0 }
    }

    /// Whether no lane at the access came into join `j` from `q`: the conditions taken to
    /// hold rule out reaching `q`. Only where `q`, `j` and the access lie in the same loops and
    /// `q` comes before `j` (no back edge): their conditions then speak of one round.
    fn not_from(&self, q: usize, j: usize) -> bool {
        let Some(at) = self.at else { return false };
        let h = self.h;
        if self.holds.is_empty() || h.index[q] >= h.index[j] || h.loops[q] != h.loops[j] || h.loops[j] != h.loops[at] {
            return false;
        }
        self.decide(h.st.reach[q]) == Some(false)
    }

    /// What the conditions taken to hold say of `g`, whatever the thread: that it holds, that
    /// it fails, or nothing.
    fn decide(&self, g: Bool) -> Option<bool> {
        if self.holds.is_empty() {
            return None;
        }
        let key = (self.holds.clone(), g);
        if let Some(&known) = self.h.implied.borrow().get(&key) {
            return known;
        }
        let with = |value: bool| {
            let mut all: Vec<(Bool, bool)> = self.holds.iter().map(|&h| (h, true)).collect();
            all.push((g, value));
            self.h.st.bools.meet(&all)
        };
        let known = match (with(true), with(false)) {
            (false, _) => Some(false),
            (_, false) => Some(true),
            _ => None,
        };
        self.h.implied.borrow_mut().insert(key, known);
        known
    }

    /// [`Self::both`] for the guard of an instruction the value comes from: also decided where
    /// the conditions taken to hold decide it, whatever the thread (as `if (c) {..} else {..}`
    /// decides the guards of both arms).
    fn guard(&mut self, g: Bool, depth: u32) -> (bool, bool) {
        match self.decide(g) {
            Some(true) => (true, false),
            Some(false) => (false, true),
            None => self.both(g, depth),
        }
    }

    /// Whether `b` may hold in this thread.
    fn may(&mut self, b: Bool) -> bool {
        self.both(b, 0).0
    }

    /// Whether `b` may hold, and whether it may fail, in this thread.
    fn both(&mut self, b: Bool, depth: u32) -> (bool, bool) {
        if b == Bool::TRUE {
            return (true, false);
        }
        if b == Bool::FALSE {
            return (false, true);
        }
        if let Some(&r) = self.tri.get(&b) {
            return r;
        }
        let Some((atom, lo, hi)) = self.h.st.bools.node(b) else { return (true, true) };
        let r = match self.atom(atom, depth) {
            Some(true) => self.both(hi, depth),
            Some(false) => self.both(lo, depth),
            None => {
                let (l, h) = (self.both(lo, depth), self.both(hi, depth));
                (l.0 || h.0, l.1 || h.1)
            }
        };
        self.tri.insert(b, r);
        r
    }

    /// The atom's value in this thread, if known.
    fn atom(&mut self, atom: Atom, depth: u32) -> Option<bool> {
        match atom {
            Atom::Constant(m) => Some(m >> (self.flat % 64) & 1 == 1),
            Atom::Lane(v) => self.compare(v, depth),
            _ => None,
        }
    }

    /// The lane's bit of the compare that writes `v`.
    fn compare(&mut self, v: Value, depth: u32) -> Option<bool> {
        let Some(&Def::Inst(b, i)) = self.h.def.get(&v) else { return None };
        let inst = &self.h.p.blocks[b].insts[i];
        let name = inst.name();
        let rest = name.strip_prefix("v_cmpx_").or_else(|| name.strip_prefix("v_cmp_"))?;
        let (op, ty) = rest.split_once('_')?;
        if !matches!(ty, "u32" | "i32") {
            return None;
        }
        let Fields::Valu(val) = &inst.fields else { return None };
        if val.dpp.is_some() || val.neg != 0 || val.abs != 0 || val.clamp {
            return None;
        }
        let x = self.source(inst, val, 0, depth)?;
        let y = self.source(inst, val, 1, depth)?;
        if let (Some(a), Some(b)) = (x.value(), y.value()) {
            let (sa, sb) = (a as i32, b as i32);
            let signed = ty == "i32";
            return Some(match op {
                "f" => false,
                "t" => true,
                "eq" => a == b,
                "ne" => a != b,
                "lt" => if signed { sa < sb } else { a < b },
                "le" => if signed { sa <= sb } else { a <= b },
                "gt" => if signed { sa > sb } else { a > b },
                "ge" => if signed { sa >= sb } else { a >= b },
                _ => return None,
            });
        }
        match op {
            "eq" if !x.sub(&y).may_be_zero() => Some(false),
            "ne" if !x.sub(&y).may_be_zero() => Some(true),
            _ => None,
        }
    }

    /// Source `n` of a VALU instruction, through an SDWA select.
    fn source(&mut self, inst: &Inst, val: &crate::gfx10::Valu, n: usize, depth: u32) -> Option<Form> {
        let f = match *val.src.get(n)? {
            Operand::Vector { reg, dwords: 1 } => self.value(inst.read(Reg::V(reg))?, depth + 1)?,
            Operand::Scalar { code: NULL, .. } => Form::konst(0),
            Operand::Scalar { code, dwords: 1 } => self.h.scalar(inst.read(Reg::S(code))?, depth + 1)?,
            Operand::Const(c) => Form::konst(inline(c)?),
            Operand::Literal(x) => Form::konst(x),
            _ => return None,
        };
        let Some(sdwa) = val.sdwa else { return Some(f) };
        let sel = *sdwa.src_sel.get(n).unwrap_or(&6);
        if sel == 6 {
            return Some(f);
        }
        let x = f.value()?;
        let sext = sdwa.sext >> n & 1 == 1;
        Some(Form::konst(match sel {
            0..=3 => {
                let b = x >> (8 * u32::from(sel)) & 0xFF;
                if sext { b as u8 as i8 as i32 as u32 } else { b }
            }
            4 | 5 => {
                let h = x >> (16 * u32::from(sel - 4)) & 0xFFFF;
                if sext { h as u16 as i16 as i32 as u32 } else { h }
            }
            _ => return None,
        }))
    }

    /// The value of VGPR value `v` in this thread, if known.
    fn value(&mut self, v: Value, depth: u32) -> Option<Form> {
        if let Some(f) = self.memo.get(&v) {
            return f.clone();
        }
        self.steps += 1;
        if depth > DEPTH || self.steps > STEPS {
            return None;
        }
        // (a join that leads back to itself is not known while it is worked out)
        self.memo.insert(v, None);
        let f = self.value_of(v, depth + 1);
        self.memo.insert(v, f.clone());
        f
    }

    fn value_of(&mut self, v: Value, depth: u32) -> Option<Form> {
        match *self.h.def.get(&v)? {
            Def::Input(Reg::V(c)) if c < self.h.ids => Some(Form::konst(self.id[usize::from(c)])),
            Def::Input(Reg::S(_)) => self.h.scalar(v, depth),
            Def::Input(_) => None,
            Def::Phi(b, k) => {
                let phi = &self.h.p.blocks[b].phis[k];
                if matches!(phi.reg, Reg::S(_)) {
                    return self.h.scalar(v, depth);
                }
                // A join is known where every way in brings the same value, or as a count of
                // rounds (the join as a term of its own while the ways back into it are worked
                // out; the values made of that term are forgotten after: it becomes a count).
                self.memo.insert(v, Some(Form::scalar(v)));
                let preds = &self.h.p.blocks[b].preds;
                let mut args: Vec<Option<Form>> = Vec::new();
                for (&a, &q) in phi.args.iter().zip(preds) {
                    if a == v || self.not_from(q, b) {
                        continue;
                    }
                    if self.holds.is_empty() || self.h.index[q] < self.h.index[b] {
                        args.push(self.value(a, depth));
                        continue;
                    }
                    // A way back: the value of an earlier round, worked out without the
                    // conditions taken to hold (those speak of this round) and what they gave.
                    let holds = std::mem::take(&mut self.holds);
                    std::mem::swap(&mut self.memo, &mut self.plain.0);
                    std::mem::swap(&mut self.tri, &mut self.plain.1);
                    self.memo.insert(v, Some(Form::scalar(v)));
                    args.push(self.value(a, depth));
                    self.memo.retain(|_, f| !holds_term(f, v));
                    self.memo.remove(&v);
                    std::mem::swap(&mut self.memo, &mut self.plain.0);
                    std::mem::swap(&mut self.tri, &mut self.plain.1);
                    self.holds = holds;
                }
                self.memo.retain(|_, f| !holds_term(f, v));
                self.memo.insert(v, None);
                if args.is_empty() || args.iter().any(Option::is_none) {
                    return None;
                }
                if args.windows(2).all(|w| w[0] == w[1]) && !holds_term(&args[0], v) {
                    return args.into_iter().next().flatten();
                }
                self.h.rounds(v, &args)
            }
            Def::Inst(b, i) => {
                let inst = &self.h.p.blocks[b].insts[i];
                if inst.writes.iter().any(|&(r, w)| w == v && matches!(r, Reg::S(_))) {
                    return self.h.scalar(v, depth);
                }
                self.valu(b, i, v, depth)
            }
        }
    }

    /// The VGPR `v` a VALU instruction writes, in this thread.
    fn valu(&mut self, b: usize, i: usize, v: Value, depth: u32) -> Option<Form> {
        let inst = &self.h.p.blocks[b].insts[i];
        let Fields::Valu(val) = &inst.fields else { return None };
        let Some(Operand::Vector { reg, dwords: 1 }) = val.dst else { return None };
        if inst.write(Reg::V(reg)) != Some(v)
            || val.dpp.is_some()
            || val.neg != 0
            || val.abs != 0
            || val.opsel != 0
            || val.clamp
            || val.omod != 0
            || val.sdwa.is_some_and(|s| s.dst_sel != 6)
        {
            return None;
        }
        let new = self.arith(inst, val, depth);
        match self.h.st.guards[b][i] {
            None => new,
            Some(g) => {
                // lanes outside EXEC keep the old value
                let old = || inst.read(Reg::V(reg));
                match self.guard(g, depth) {
                    (true, false) => new,
                    (false, true) => self.value(old()?, depth),
                    _ => {
                        let old = self.value(old()?, depth)?;
                        (new? == old).then_some(old)
                    }
                }
            }
        }
    }

    /// What a VALU instruction computes from its sources, where that is known.
    fn arith(&mut self, inst: &Inst, val: &crate::gfx10::Valu, depth: u32) -> Option<Form> {
        let name = inst.name();
        let s = |th: &mut Self, n: usize| th.source(inst, val, n, depth);
        let shift = |f: &Form| f.value().map(|k| 1u32 << (k & 31));
        Some(match name {
            "v_mov_b32" => s(self, 0)?,
            "v_add_nc_u32" | "v_add_u32" => s(self, 0)?.add(&s(self, 1)?),
            "v_sub_nc_u32" | "v_sub_u32" => s(self, 0)?.sub(&s(self, 1)?),
            "v_subrev_nc_u32" | "v_subrev_u32" => s(self, 1)?.sub(&s(self, 0)?),
            "v_add3_u32" => s(self, 0)?.add(&s(self, 1)?).add(&s(self, 2)?),
            "v_lshlrev_b32" => {
                let by = shift(&s(self, 0)?)?;
                s(self, 1)?.scale(by)
            }
            "v_lshl_add_u32" => {
                let by = shift(&s(self, 1)?)?;
                s(self, 0)?.scale(by).add(&s(self, 2)?)
            }
            "v_add_lshl_u32" => {
                let by = shift(&s(self, 2)?)?;
                s(self, 0)?.add(&s(self, 1)?).scale(by)
            }
            "v_mul_lo_u32" => {
                let (a, b) = (s(self, 0)?, s(self, 1)?);
                match (a.value(), b.value()) {
                    (Some(k), _) => b.scale(k),
                    (_, Some(k)) => a.scale(k),
                    _ => return None,
                }
            }
            "v_mad_u32_u24" => {
                let (a, b) = (s(self, 0)?.value()?, s(self, 1)?.value()?);
                Form::konst((a & 0xFF_FFFF).wrapping_mul(b & 0xFF_FFFF)).add(&s(self, 2)?)
            }
            "v_cndmask_b32" => {
                let (x, y) = (s(self, 0)?, s(self, 1)?);
                if x == y {
                    return Some(x);
                }
                let Some(Operand::Scalar { code, .. }) = val.src.get(2) else { return None };
                let low = inst.read(Reg::S(*code))?;
                let high = inst.read(Reg::S(code + 1));
                let m = self.h.st.mask_bits(low, high)?;
                match self.guard(m, depth) {
                    (true, false) => y,
                    (false, true) => x,
                    _ => return None,
                }
            }
            _ => {
                let n = val.src.len().min(3);
                let mut ks = Vec::with_capacity(n);
                for k in 0..n {
                    ks.push(s(self, k)?.value()?);
                }
                Form::konst(bits(name, &ks)?)
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::Form;
    use crate::ir::Value;

    #[test]
    fn a_scalar_term_makes_zero_only_what_its_factor_divides() {
        let g = Value(7);
        // 64 * group + t
        let at = |t: u32| Form::scalar(g).scale(64).add(&Form::konst(t));
        assert!(at(0).may_be_zero());
        assert!((1..64).all(|t| !at(t).may_be_zero()));
        assert!(at(128).may_be_zero());
        // equal scalar parts cancel
        let d = at(40).sub(&at(8));
        assert_eq!(d.value(), Some(32));
        assert!(Form::scalar(g).add(&Form::konst(5)).may_be_zero());
    }
}
