//! Scalar ALU instructions on the host GPU against the RDNA2 rules: each
//! program runs one SOP1, SOP2, SOPC or SOPK instruction on eight root
//! constants and stores what it left in s16, s17 and SCC. SCC is set from s7
//! beforehand, so an instruction that leaves SCC alone shows it.

use super::host::{Dispatch, Host, Root, View};
use super::{HostShader, lower, validator};
use crate::native::{
    ComputeIo, HwStage, NativeProgram, ResourceType, RootParam, RootType, StageIo, UsedResource,
};
use crate::{io, ir, resource, structure};

const fn sop1(op: u32, dst: u32, src: u32) -> u32 {
    0xBE80_0000 | dst << 16 | op << 8 | src
}

const fn sop2(op: u32, dst: u32, src0: u32, src1: u32) -> u32 {
    0x8000_0000 | op << 23 | dst << 16 | src1 << 8 | src0
}

const fn sopc(op: u32, src0: u32, src1: u32) -> u32 {
    0xBF00_0000 | op << 16 | src1 << 8 | src0
}

const fn sopk(op: u32, dst: u32, imm: u16) -> u32 {
    0xB000_0000 | op << 23 | dst << 16 | imm as u32
}

/// `buffer_store_dword vN, off, s[12:15], 0 offset:off`.
const fn store(v: u32, off: u32) -> [u32; 2] {
    [0x38 << 26 | 0x1C << 18 | off, 0x80 << 24 | 3 << 16 | v << 8]
}

/// The program around `op`, or why the translator refused it. Without
/// `scc`, s18 is 0 and SCC is not read.
fn shader(op: &[u32], scc: bool) -> Result<HostShader, String> {
    let mut words = vec![
        sop1(3, 9, 128), // s_mov_b32 s9, 0 (table address high)
        0x3D << 26 | 0x02 << 18 | 12 << 6 | 4,
        125 << 25,          // s_load_dwordx4 s[12:15], s[8:9], 0
        sopc(7, 7, 128),    // s_cmp_lg_u32 s7, 0
        sop1(3, 16, 5),     // s_mov_b32 s16, s5
        sop1(3, 17, 6),     // s_mov_b32 s17, s6
    ];
    words.extend_from_slice(op);
    words.push(if scc {
        sop2(0x0a, 18, 129, 128) // s_cselect_b32 s18, 1, 0
    } else {
        sop1(3, 18, 128) // s_mov_b32 s18, 0
    });
    for v in 0..3 {
        words.push(0x7E00_0200 | v << 17 | (16 + v)); // v_mov_b32 vN, s(16 + N)
    }
    for v in 0..3 {
        words.extend(store(v, 4 * v));
    }
    words.push(0xBF81_0000);
    lowered(&words, 1, false)
}

/// The whole program `words` in a group of `threads`, lowered and validated.
fn lowered(words: &[u32], threads: u32, wave64: bool) -> Result<HostShader, String> {
    let native = NativeProgram {
        stage: HwStage::Cs,
        wave64,
        vgprs: 8,
        layout: vec![
            RootParam {
                root: RootType::Constants,
                table_offset_dwords: 0,
                sgprs: 8,
                base_register: 0,
                user_data: 0,
                slot: 0,
            },
            RootParam {
                root: RootType::Table,
                table_offset_dwords: 0,
                sgprs: 1,
                base_register: 0,
                user_data: 8,
                slot: 1,
            },
        ],
        used: vec![UsedResource {
            kind: ResourceType::Uav,
            root: RootType::Table,
            table_offset_dwords: 0,
            lower_bound: 0,
            stride: 0,
            dwords: 4,
            user_data: 8,
            slot: 1,
        }],
        io: StageIo::Cs(ComputeIo {
            rsrc2: 9 << 1,
            threads: [threads, 1, 1],
        }),
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    };
    let p = ir::build(&native.code, native.wave64, native.vgprs).map_err(|e| format!("{e:?}"))?;
    let st = structure::structure(&p).map_err(|e| format!("{e:?}"))?;
    let res = resource::track(&p, &native.layout, native.stage, Some(&st))
        .map_err(|e| format!("{e:?}"))?;
    let io = io::raise(&p, &native).map_err(|e| format!("{e:?}"))?;
    let lowered = lower::lower(&lower::Parts {
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
    .map_err(|e| format!("{e:?}"))?;
    let shader = HostShader {
        stage: HwStage::Cs,
        dxil: lowered.dxil,
        user_data: lowered.user_data,
        samplers: lowered.samplers,
        static_samplers: Vec::new(),
        root_cbvs: lowered.root_cbvs,
        root_srvs: lowered.root_srvs,
        root_uavs: lowered.root_uavs,
        root_indexed_views: res.root_indexed_views,
        half_views: lowered.half_views,
    };
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        let mut copy = shader.dxil.clone();
        v.validate(&mut copy).expect("DXIL validation failed");
        assert_eq!(copy[4..20], shader.dxil[4..20], "container hash mismatch");
    }
    Ok(shader)
}

/// The first `n` dwords the program stores, for each input.
fn run(host: &Host, shader: &HostShader, cases: &[[u32; 8]], n: u32) -> Vec<Vec<u32>> {
    let (root, pso) = host.compute_pipeline(shader).expect("compute_pipeline");
    cases
        .iter()
        .map(|inputs| {
            let mut ud = vec![0u32; shader.user_data_dwords().max(1) as usize];
            for u in &shader.user_data {
                ud[u.index as usize] = match (u.root, u.slot) {
                    (RootType::Constants, 0) => inputs[u.dword as usize],
                    (RootType::Table, 1) => 0,
                    other => panic!("unexpected user data {other:?}"),
                };
            }
            let out = host
                .dispatch(
                    &root,
                    &pso,
                    &Dispatch {
                        roots: vec![(0, Root::Constants(ud))],
                        views: &[View::RawUav(n)],
                        samplers: &[],
                        groups: [1, 1, 1],
                    },
                )
                .expect("dispatch");
            out[0][..n as usize].to_vec()
        })
        .collect()
}

/// (s16, s17, SCC) by the RDNA2 rules for instruction `name` (immediate `k`
/// for SOPK) on inputs `x`: s[0:1] and s[2:3] are the 64-bit sources, s16 and
/// s17 hold s5 and s6 before, SCC is s7 != 0.
fn rdna2(name: &str, x: [u32; 8], k: u16) -> (u32, u32, bool) {
    let (a, b, c) = (x[0], x[1], x[2]);
    let wa = u64::from(x[0]) | u64::from(x[1]) << 32;
    let wb = u64::from(x[2]) | u64::from(x[3]) << 32;
    let old = u64::from(x[5]) | u64::from(x[6]) << 32;
    let scc = x[7] != 0;
    let kx = k as i16 as i32 as u32;
    let one = |r: u32, s: bool| (r, x[6], s);
    let nz = |r: u32| (r, x[6], r != 0);
    let wide = |r: u64, s: bool| (r as u32, (r >> 32) as u32, s);
    let wnz = |r: u64| (r as u32, (r >> 32) as u32, r != 0);
    let cmp = |s: bool| (x[5], x[6], s);
    let mask32 = |w: u32| if w >= 32 { u32::MAX } else { (1u32 << w) - 1 };
    let mask64 = |w: u32| if w >= 64 { u64::MAX } else { (1u64 << w) - 1 };
    let sext32 = |r: u32, w: u32| {
        if w == 0 || w >= 32 { r } else { ((r << (32 - w)) as i32 >> (32 - w)) as u32 }
    };
    let sext64 = |r: u64, w: u32| {
        if w == 0 || w >= 64 { r } else { ((r << (64 - w)) as i64 >> (64 - w)) as u64 }
    };
    let wqm = |v: u64, n: u32| {
        (0..n / 4).fold(0u64, |r, i| if v >> (4 * i) & 0xF != 0 { r | 0xF << (4 * i) } else { r })
    };
    let quad = |v: u64, n: u32| {
        (0..n / 4).fold(0u64, |r, i| if v >> (4 * i) & 0xF != 0 { r | 1 << i } else { r })
    };
    let flbit_signed = |v: u64, n: u32| -> u32 {
        let top = v >> (n - 1) & 1 != 0;
        let all = if n == 64 { u64::MAX } else { (1u64 << n) - 1 };
        if v == 0 || v == all {
            u32::MAX
        } else if top {
            ((!v & all) << (64 - n)).leading_zeros()
        } else {
            (v << (64 - n)).leading_zeros()
        }
    };
    let first = |v: u64, none: bool| if none { u32::MAX } else { v.trailing_zeros() };
    match name {
        "s_add_u32" => one(a.wrapping_add(b), u64::from(a) + u64::from(b) > 0xFFFF_FFFF),
        "s_sub_u32" => one(a.wrapping_sub(b), b > a),
        "s_add_i32" => one(a.wrapping_add(b), (a as i32).checked_add(b as i32).is_none()),
        "s_sub_i32" => one(a.wrapping_sub(b), (a as i32).checked_sub(b as i32).is_none()),
        "s_addc_u32" => {
            let r = u64::from(a) + u64::from(b) + u64::from(scc);
            one(r as u32, r > 0xFFFF_FFFF)
        }
        "s_subb_u32" => one(
            a.wrapping_sub(b).wrapping_sub(u32::from(scc)),
            u64::from(b) + u64::from(scc) > u64::from(a),
        ),
        "s_cselect_b32" => one(if scc { a } else { b }, scc),
        "s_and_b32" => nz(a & b),
        "s_or_b32" => nz(a | b),
        "s_xor_b32" => nz(a ^ b),
        "s_andn2_b32" => nz(a & !b),
        "s_orn2_b32" => nz(a | !b),
        "s_nand_b32" => nz(!(a & b)),
        "s_nor_b32" => nz(!(a | b)),
        "s_xnor_b32" => nz(!(a ^ b)),
        "s_lshl_b32" => nz(a << (b & 31)),
        "s_lshr_b32" => nz(a >> (b & 31)),
        "s_ashr_i32" => nz(((a as i32) >> (b & 31)) as u32),
        "s_bfm_b32" => one(mask32(a & 31) << (b & 31), scc),
        "s_mul_i32" => one(a.wrapping_mul(b), scc),
        "s_bfe_u32" => nz((a >> (b & 31)) & mask32(b >> 16 & 0x7F)),
        "s_bfe_i32" => {
            let w = b >> 16 & 0x7F;
            nz(sext32(((a as i32 >> (b & 31)) as u32) & mask32(w), w))
        }
        "s_absdiff_i32" => nz((a as i32).wrapping_sub(b as i32).wrapping_abs() as u32),
        "s_pack_ll_b32_b16" => one(a & 0xFFFF | b << 16, scc),
        "s_pack_lh_b32_b16" => one(a & 0xFFFF | b & 0xFFFF_0000, scc),
        "s_pack_hh_b32_b16" => one(a >> 16 | b & 0xFFFF_0000, scc),
        "s_mul_hi_u32" => one(((u64::from(a) * u64::from(b)) >> 32) as u32, scc),
        "s_mul_hi_i32" => one(((i64::from(a as i32) * i64::from(b as i32)) >> 32) as u32, scc),
        "s_cselect_b64" => wide(if scc { wa } else { wb }, scc),
        "s_and_b64" => wnz(wa & wb),
        "s_or_b64" => wnz(wa | wb),
        "s_xor_b64" => wnz(wa ^ wb),
        "s_andn2_b64" => wnz(wa & !wb),
        "s_orn2_b64" => wnz(wa | !wb),
        "s_nand_b64" => wnz(!(wa & wb)),
        "s_nor_b64" => wnz(!(wa | wb)),
        "s_xnor_b64" => wnz(!(wa ^ wb)),
        "s_lshl_b64" => wnz(wa << (c & 63)),
        "s_lshr_b64" => wnz(wa >> (c & 63)),
        "s_ashr_i64" => wnz(((wa as i64) >> (c & 63)) as u64),
        "s_bfm_b64" => wide(mask64(a & 63) << (b & 63), scc),
        "s_bfe_u64" => wnz((wa >> (c & 63)) & mask64(c >> 16 & 0x7F)),
        "s_bfe_i64" => {
            let w = c >> 16 & 0x7F;
            wnz(sext64(((wa as i64 >> (c & 63)) as u64) & mask64(w), w))
        }
        "s_mov_b32" => one(a, scc),
        "s_cmov_b32" => one(if scc { a } else { x[5] }, scc),
        "s_not_b32" => nz(!a),
        "s_wqm_b32" => nz(wqm(u64::from(a), 32) as u32),
        "s_brev_b32" => one(a.reverse_bits(), scc),
        "s_bcnt0_i32_b32" => nz(a.count_zeros()),
        "s_bcnt1_i32_b32" => nz(a.count_ones()),
        "s_ff0_i32_b32" => one(first(u64::from(!a), a == u32::MAX), scc),
        "s_ff1_i32_b32" => one(first(u64::from(a), a == 0), scc),
        "s_flbit_i32_b32" => one(if a == 0 { u32::MAX } else { a.leading_zeros() }, scc),
        "s_flbit_i32" => one(flbit_signed(u64::from(a), 32), scc),
        "s_sext_i32_i8" => one(a as u8 as i8 as i32 as u32, scc),
        "s_sext_i32_i16" => one(a as u16 as i16 as i32 as u32, scc),
        "s_bitset0_b32" => one(x[5] & !(1 << (a & 31)), scc),
        "s_bitset1_b32" => one(x[5] | 1 << (a & 31), scc),
        "s_quadmask_b32" => nz(quad(u64::from(a), 32) as u32),
        "s_abs_i32" => nz((a as i32).wrapping_abs() as u32),
        "s_mov_b64" => wide(wa, scc),
        "s_cmov_b64" => wide(if scc { wa } else { old }, scc),
        "s_not_b64" => wnz(!wa),
        "s_wqm_b64" => wnz(wqm(wa, 64)),
        "s_brev_b64" => wide(wa.reverse_bits(), scc),
        "s_bcnt0_i32_b64" => nz(wa.count_zeros()),
        "s_bcnt1_i32_b64" => nz(wa.count_ones()),
        "s_ff0_i32_b64" => one(first(!wa, wa == u64::MAX), scc),
        "s_ff1_i32_b64" => one(first(wa, wa == 0), scc),
        "s_flbit_i32_b64" => one(if wa == 0 { u32::MAX } else { wa.leading_zeros() }, scc),
        "s_flbit_i32_i64" => one(flbit_signed(wa, 64), scc),
        "s_bitset0_b64" => wide(old & !(1 << (a & 63)), scc),
        "s_bitset1_b64" => wide(old | 1 << (a & 63), scc),
        "s_quadmask_b64" => wnz(quad(wa, 64)),
        "s_bitreplicate_b64_b32" => wide(
            (0..32).fold(0u64, |r, i| if a >> i & 1 != 0 { r | 3 << (2 * i) } else { r }),
            scc,
        ),
        "s_cmp_eq_i32" | "s_cmp_eq_u32" => cmp(a == b),
        "s_cmp_lg_i32" | "s_cmp_lg_u32" => cmp(a != b),
        "s_cmp_gt_i32" => cmp(a as i32 > b as i32),
        "s_cmp_ge_i32" => cmp(a as i32 >= b as i32),
        "s_cmp_lt_i32" => cmp((a as i32) < b as i32),
        "s_cmp_le_i32" => cmp(a as i32 <= b as i32),
        "s_cmp_gt_u32" => cmp(a > b),
        "s_cmp_ge_u32" => cmp(a >= b),
        "s_cmp_lt_u32" => cmp(a < b),
        "s_cmp_le_u32" => cmp(a <= b),
        "s_bitcmp0_b32" => cmp(a >> (b & 31) & 1 == 0),
        "s_bitcmp1_b32" => cmp(a >> (b & 31) & 1 == 1),
        "s_bitcmp0_b64" => cmp(wa >> (c & 63) & 1 == 0),
        "s_bitcmp1_b64" => cmp(wa >> (c & 63) & 1 == 1),
        "s_cmp_eq_u64" => cmp(wa == wb),
        "s_cmp_lg_u64" => cmp(wa != wb),
        "s_movk_i32" => one(kx, scc),
        "s_cmovk_i32" => one(if scc { kx } else { x[5] }, scc),
        "s_cmpk_eq_i32" => cmp(a == kx),
        "s_cmpk_lg_i32" => cmp(a != kx),
        "s_cmpk_gt_i32" => cmp(a as i32 > kx as i32),
        "s_cmpk_ge_i32" => cmp(a as i32 >= kx as i32),
        "s_cmpk_lt_i32" => cmp((a as i32) < kx as i32),
        "s_cmpk_le_i32" => cmp(a as i32 <= kx as i32),
        "s_cmpk_eq_u32" => cmp(a == u32::from(k)),
        "s_cmpk_lg_u32" => cmp(a != u32::from(k)),
        "s_cmpk_gt_u32" => cmp(a > u32::from(k)),
        "s_cmpk_ge_u32" => cmp(a >= u32::from(k)),
        "s_cmpk_lt_u32" => cmp(a < u32::from(k)),
        "s_cmpk_le_u32" => cmp(a <= u32::from(k)),
        "s_addk_i32" => one(
            x[5].wrapping_add(kx),
            (x[5] as i32).checked_add(kx as i32).is_none(),
        ),
        "s_mulk_i32" => one(x[5].wrapping_mul(kx), scc),
        _ => panic!("no reference for {name}"),
    }
}

const CASES: [[u32; 8]; 10] = [
    [0, 0, 0, 0, 0, 0, 0, 0],
    [u32::MAX, 1, 0, 0, 0x5555_5555, 0xAAAA_AAAA, 0, 1],
    [0x8000_0000, 0x7FFF_FFFF, 0x1F, 0x8000_0000, 7, 9, 0, 0],
    [0x1234_5678, 0x0008_0004, 0x0010_0024, 0x9ABC_DEF0, 0xDEAD_BEEF, 0xCAFE_F00D, 0, 1],
    [0xFFFF_FFF0, 0x0020_0000, 0x0040_0000, u32::MAX, 1, 2, 0, 0],
    [0x0000_8000, 0x0000_0021, 0x0000_003F, 1, 0x7FFF_FFFF, 3, 0, 1],
    [0x7FFF_FFFF, u32::MAX, 0x0018_0028, 0x7FFF_FFFF, 0x8000_0000, 0, 0, 0],
    [0xF0F0_0F0F, 0x000C_001C, 0x0021_0021, 0x0F0F_F0F0, 0x8000_0001, 0x1234, 0, 1],
    [5, 5, 5, 5, 0xFFFF_8001, 7, 0, 0],
    [0xFFFF_8001, 0x8001, 0x40, 0, 0x7FFF_FFFF, 0, 0, 1],
];

/// The instructions checked: (name, words, SOPK immediate).
fn programs() -> Vec<(&'static str, Vec<u32>, u16)> {
    let mut all = Vec::new();
    // SOP2: 32-bit forms on s0, s1; 64-bit on s[0:1], s[2:3] (shifts and
    // field extracts by s2); s_bfm_b64 of s0, s1.
    for (op, name) in [
        (0x00, "s_add_u32"),
        (0x01, "s_sub_u32"),
        (0x02, "s_add_i32"),
        (0x03, "s_sub_i32"),
        (0x04, "s_addc_u32"),
        (0x05, "s_subb_u32"),
        (0x0a, "s_cselect_b32"),
        (0x0e, "s_and_b32"),
        (0x10, "s_or_b32"),
        (0x12, "s_xor_b32"),
        (0x14, "s_andn2_b32"),
        (0x16, "s_orn2_b32"),
        (0x18, "s_nand_b32"),
        (0x1a, "s_nor_b32"),
        (0x1c, "s_xnor_b32"),
        (0x1e, "s_lshl_b32"),
        (0x20, "s_lshr_b32"),
        (0x22, "s_ashr_i32"),
        (0x24, "s_bfm_b32"),
        (0x25, "s_bfm_b64"),
        (0x26, "s_mul_i32"),
        (0x27, "s_bfe_u32"),
        (0x28, "s_bfe_i32"),
        (0x2c, "s_absdiff_i32"),
        (0x32, "s_pack_ll_b32_b16"),
        (0x33, "s_pack_lh_b32_b16"),
        (0x34, "s_pack_hh_b32_b16"),
        (0x35, "s_mul_hi_u32"),
        (0x36, "s_mul_hi_i32"),
    ] {
        all.push((name, vec![sop2(op, 16, 0, 1)], 0));
    }
    for (op, name) in [
        (0x0b, "s_cselect_b64"),
        (0x0f, "s_and_b64"),
        (0x11, "s_or_b64"),
        (0x13, "s_xor_b64"),
        (0x15, "s_andn2_b64"),
        (0x17, "s_orn2_b64"),
        (0x19, "s_nand_b64"),
        (0x1b, "s_nor_b64"),
        (0x1d, "s_xnor_b64"),
        (0x1f, "s_lshl_b64"),
        (0x21, "s_lshr_b64"),
        (0x23, "s_ashr_i64"),
        (0x29, "s_bfe_u64"),
        (0x2a, "s_bfe_i64"),
    ] {
        all.push((name, vec![sop2(op, 16, 0, 2)], 0));
    }
    // SOP1 of s0 (s[0:1] for the 64-bit sources).
    for (op, name) in [
        (0x03, "s_mov_b32"),
        (0x04, "s_mov_b64"),
        (0x05, "s_cmov_b32"),
        (0x06, "s_cmov_b64"),
        (0x07, "s_not_b32"),
        (0x08, "s_not_b64"),
        (0x09, "s_wqm_b32"),
        (0x0a, "s_wqm_b64"),
        (0x0b, "s_brev_b32"),
        (0x0c, "s_brev_b64"),
        (0x0d, "s_bcnt0_i32_b32"),
        (0x0e, "s_bcnt0_i32_b64"),
        (0x0f, "s_bcnt1_i32_b32"),
        (0x10, "s_bcnt1_i32_b64"),
        (0x11, "s_ff0_i32_b32"),
        (0x12, "s_ff0_i32_b64"),
        (0x13, "s_ff1_i32_b32"),
        (0x14, "s_ff1_i32_b64"),
        (0x15, "s_flbit_i32_b32"),
        (0x16, "s_flbit_i32_b64"),
        (0x17, "s_flbit_i32"),
        (0x18, "s_flbit_i32_i64"),
        (0x19, "s_sext_i32_i8"),
        (0x1a, "s_sext_i32_i16"),
        (0x1b, "s_bitset0_b32"),
        (0x1c, "s_bitset0_b64"),
        (0x1d, "s_bitset1_b32"),
        (0x1e, "s_bitset1_b64"),
        (0x2c, "s_quadmask_b32"),
        (0x2d, "s_quadmask_b64"),
        (0x34, "s_abs_i32"),
        (0x3b, "s_bitreplicate_b64_b32"),
    ] {
        all.push((name, vec![sop1(op, 16, 0)], 0));
    }
    // SOPC of s0, s1; the 64-bit bit test of s[0:1] by s2, the 64-bit
    // compares of s[0:1] and s[2:3].
    for (op, name) in [
        (0x00, "s_cmp_eq_i32"),
        (0x01, "s_cmp_lg_i32"),
        (0x02, "s_cmp_gt_i32"),
        (0x03, "s_cmp_ge_i32"),
        (0x04, "s_cmp_lt_i32"),
        (0x05, "s_cmp_le_i32"),
        (0x06, "s_cmp_eq_u32"),
        (0x07, "s_cmp_lg_u32"),
        (0x08, "s_cmp_gt_u32"),
        (0x09, "s_cmp_ge_u32"),
        (0x0a, "s_cmp_lt_u32"),
        (0x0b, "s_cmp_le_u32"),
        (0x0c, "s_bitcmp0_b32"),
        (0x0d, "s_bitcmp1_b32"),
    ] {
        all.push((name, vec![sopc(op, 0, 1)], 0));
    }
    for (op, name) in [
        (0x0e, "s_bitcmp0_b64"),
        (0x0f, "s_bitcmp1_b64"),
        (0x12, "s_cmp_eq_u64"),
        (0x13, "s_cmp_lg_u64"),
    ] {
        all.push((name, vec![sopc(op, 0, 2)], 0));
    }
    // SOPK: the compares read s0; the others write s16 (holding s5).
    for k in [0x8001u16, 0x7FFF, 0x0005, 0xFFFF] {
        for (op, name) in [
            (0x00, "s_movk_i32"),
            (0x02, "s_cmovk_i32"),
            (0x0f, "s_addk_i32"),
            (0x10, "s_mulk_i32"),
        ] {
            all.push((name, vec![sopk(op, 16, k)], k));
        }
        for (op, name) in [
            (0x03, "s_cmpk_eq_i32"),
            (0x04, "s_cmpk_lg_i32"),
            (0x05, "s_cmpk_gt_i32"),
            (0x06, "s_cmpk_ge_i32"),
            (0x07, "s_cmpk_lt_i32"),
            (0x08, "s_cmpk_le_i32"),
            (0x09, "s_cmpk_eq_u32"),
            (0x0a, "s_cmpk_lg_u32"),
            (0x0b, "s_cmpk_gt_u32"),
            (0x0c, "s_cmpk_ge_u32"),
            (0x0d, "s_cmpk_lt_u32"),
            (0x0e, "s_cmpk_le_u32"),
        ] {
            all.push((name, vec![sopk(op, 0, k)], k));
        }
    }
    all
}

/// Every scalar ALU instruction the translator takes gives on the GPU what
/// the console gives: its result dwords and SCC (set, or left alone).
/// Those it refuses are listed, not failed.
#[test]
fn scalar_alu_results_and_scc_match_rdna2() {
    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let host = match Host::new(false).or_else(|_| Host::new(true)) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("no SM 6.6 device ({e}); skipped");
            return;
        }
    };
    let (mut wrong, mut refused) = (Vec::new(), Vec::new());
    for (name, words, k) in programs() {
        // With SCC read, and where that is refused, the result alone.
        for scc in [true, false] {
            let built = std::panic::catch_unwind(|| shader(&words, scc))
                .unwrap_or_else(|_| Err("panicked".into()));
            let shader = match built {
                Ok(s) => s,
                Err(why) => {
                    refused.push(format!("{name} (SCC read: {scc}): {why}"));
                    continue;
                }
            };
            for (x, got) in CASES.iter().zip(run(&host, &shader, &CASES, 3)) {
                let (lo, hi, s) = rdna2(name, *x, k);
                let want = [lo, hi, u32::from(s && scc)];
                if got != want {
                    wrong.push(format!(
                        "{name} k={k:#x} in {x:08x?}: got {got:08x?}, want {want:08x?}"
                    ));
                }
            }
            break;
        }
    }
    eprintln!("refused ({}):\n  {}", refused.len(), refused.join("\n  "));
    assert!(wrong.is_empty(), "{} wrong:\n{}", wrong.len(), wrong.join("\n"));
}

/// Scalar loops and branches: a loop closed on SCC, one left by a break on
/// SCC, one counted down to zero, an if/else whose join branches on the SCC
/// from before it, and `s_cbranch_vccz` on a mask made from SCC. The SCC the
/// program ends with comes from inside the loop.
#[test]
fn scalar_loops_and_branches_on_scc_match_rdna2() {
    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let Ok(host) = Host::new(false).or_else(|_| Host::new(true)) else {
        eprintln!("no SM 6.6 device; skipped");
        return;
    };
    type Want = fn(&[u32; 8]) -> [u32; 3];
    let programs: [(&str, Vec<u32>, Want); 5] = [
        (
            "do { s16++; s17 += s1 } while (s16 < s0)",
            vec![
                sop1(3, 16, 128),
                sop1(3, 17, 128),
                sop2(0, 16, 16, 129),
                sop2(0, 17, 17, 1),
                sopc(9, 16, 0),
                0xBF84_FFFC, // s_cbranch_scc0 -4
            ],
            |x| {
                let n = x[0].max(1);
                [n, x[1].wrapping_mul(n), 1]
            },
        ),
        (
            "for (s16 = 0; s16 != s0; s16++) s17 += s1",
            vec![
                sop1(3, 16, 128),
                sopc(6, 16, 0),
                0xBF85_0003, // s_cbranch_scc1 +3
                sop2(0, 16, 16, 129),
                sop2(0, 17, 17, 1),
                0xBF82_FFFB, // s_branch -5
            ],
            |x| [x[0], x[6].wrapping_add(x[1].wrapping_mul(x[0])), 1],
        ),
        (
            "s16 = s0; do { s17 += s1 } while (--s16)",
            vec![
                sop1(3, 16, 0),
                sop2(0, 17, 17, 1),
                sop2(1, 16, 16, 129),
                sopc(7, 16, 128),
                0xBF85_FFFC, // s_cbranch_scc1 -4
            ],
            |x| [0, x[6].wrapping_add(x[1].wrapping_mul(x[0])), 0],
        ),
        (
            "if (s0 > s1) s17 = s3 else s17 = s2; if (SCC) s16 = s4",
            vec![
                sopc(8, 0, 1),
                0xBF85_0002, // s_cbranch_scc1 +2
                sop1(3, 17, 2),
                0xBF82_0001, // s_branch +1
                sop1(3, 17, 3),
                0xBF84_0001, // s_cbranch_scc0 +1
                sop1(3, 16, 4),
            ],
            |x| {
                let g = x[0] > x[1];
                [if g { x[4] } else { x[5] }, if g { x[3] } else { x[2] }, u32::from(g)]
            },
        ),
        (
            "vcc = s0 > s1 ? -1 : 0; if (vcc) s16 = s4",
            vec![
                sopc(8, 0, 1),
                sop2(0x0a, 106, 193, 128), // s_cselect_b32 vcc_lo, -1, 0
                0xBF86_0001,               // s_cbranch_vccz +1
                sop1(3, 16, 4),
            ],
            |x| [if x[0] > x[1] { x[4] } else { x[5] }, x[6], 1],
        ),
    ];
    let cases: [[u32; 8]; 5] = [
        [0, 3, 1, 2, 10, 11, 12, 0],
        [1, 3, 1, 2, 10, 11, 12, 1],
        [5, 3, 1, 2, 10, 11, 12, 0],
        [9, 0x1000_0001, 1, 2, 10, 11, 12, 1],
        [3, 7, 4, 5, 6, 7, 8, 1],
    ];
    let mut wrong = Vec::new();
    for (what, words, want) in programs {
        let shader = shader(&words, true).unwrap_or_else(|e| panic!("{what}: {e}"));
        for (x, got) in cases.iter().zip(run(&host, &shader, &cases, 3)) {
            let mut w = want(x);
            // The vccz program ends with the compare's SCC.
            if what.starts_with("vcc") {
                w[2] = u32::from(x[0] > x[1]);
            }
            // The countdown runs once from 0 too, and wraps.
            if what.starts_with("s16 = s0") && x[0] == 0 {
                continue;
            }
            if got != w {
                wrong.push(format!("{what} in {x:?}: got {got:x?}, want {w:x?}"));
            }
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

/// An inline float constant in a 64-bit scalar operand is the double: 1.0 is
/// 0x3FF0_0000_0000_0000 in `s_mov_b64` and `s_and_b64`, not the single's bits
/// with the sign above them (RDNA2 ISA, "Inline Constants": a 64-bit
/// operation takes the double-precision form).
#[test]
fn inline_float_constants_of_64_bit_scalar_operands_are_doubles() {
    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let Ok(host) = Host::new(false).or_else(|_| Host::new(true)) else {
        eprintln!("no SM 6.6 device; skipped");
        return;
    };
    let doubles = [
        (240, 0.5f64),
        (241, -0.5),
        (242, 1.0),
        (243, -1.0),
        (244, 2.0),
        (245, -2.0),
        (246, 4.0),
        (247, -4.0),
    ];
    let mut constants: Vec<(u32, u64)> = doubles.iter().map(|&(c, d)| (c, d.to_bits())).collect();
    constants.push((248, 0x3FC4_5F30_6DC9_C882)); // 1/(2π)
    let cases = [[u32::MAX, u32::MAX, 0, 0, 0, 0, 0, 0], [0x0F0F_0F0F, 0xF0F0_F0F0, 0, 0, 0, 0, 0, 0]];
    let mut wrong = Vec::new();
    for (c, bits) in constants {
        for (name, op) in [("s_mov_b64", sop1(4, 16, c)), ("s_and_b64", sop2(0x0f, 16, 0, c))] {
            let shader = shader(&[op], false).unwrap_or_else(|e| panic!("{name} {c}: {e}"));
            for (x, got) in cases.iter().zip(run(&host, &shader, &cases, 2)) {
                let a = u64::from(x[0]) | u64::from(x[1]) << 32;
                let want = if name == "s_mov_b64" { bits } else { a & bits };
                let want = vec![want as u32, (want >> 32) as u32];
                if got != want {
                    wrong.push(format!("{name} s[16:17], {c}: got {got:08x?}, want {want:08x?}"));
                }
            }
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}