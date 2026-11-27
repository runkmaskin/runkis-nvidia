//! A console wave64 compute program on a host of 32-lane waves: a wave reduction inside a
//! per-lane branch, with that branch taken wave-wide ([`crate::structure::structure_wide`])
//! so that the two host waves can exchange their halves inside it. Lowered, validated by
//! `dxil.dll` when present, and run on the host GPU when it takes waves of 32.

use std::collections::HashSet;

use super::host::{Dispatch, Host, Root, View};
use super::{HostShader, lower, validator};
use crate::native::{
    ComputeIo, HwStage, NativeProgram, ResourceType, RootParam, RootType, StageIo, UsedResource,
};
use crate::{ShaderError, io, ir, resource, structure};

const S_ENDPGM: u32 = 0xBF81_0000;

/// 64 threads in one console wave. Each thread t holds the bit `1 << (t >> 1)`; the threads
/// below s0 (a root constant) OR theirs over the wave inside `if (t < s0) { ... }` and store
/// the result to dword 0 of the raw UAV.
/// `skip`: the branch past the body when no thread is below s0: `s_cbranch_execz`, or
/// `s_cbranch_scc0` on the SCC `s_and_saveexec` sets ("some lane is left").
fn program(skip: u32) -> NativeProgram {
    let mut words = vec![
        0x7E0A_0281, // v_mov_b32 v5, 1
        0x2C0C_0081, // v_lshrrev_b32 v6, 1, v0
        0x340E_0B06, // v_lshlrev_b32 v7, v6, v5
        0x7D88_0000, // v_cmp_gt_u32 vcc, s0, v0
        0xBE8C_246A, // s_and_saveexec_b64 s[12:13], vcc
        skip | 23,   // the branch to the restore of EXEC
    ];
    words.extend_from_slice(&super::wave::WAVE_OR_V7_TO_S7);
    words.extend_from_slice(&[
        0x7E00_0207, // v_mov_b32 v0, s7
        0x38 << 26 | 0x1C << 18,
        0x80 << 24 | 2 << 16, // buffer_store_dword v0, off, s[8:11], 0
        0xBEFE_040C, // s_mov_b64 exec, s[12:13]
    ]);
    native(&words)
}

/// A wave64 compute program of 64 threads (one console wave; the thread index in v0) with 4
/// root constants in s0..s3 and a raw UAV's descriptor in s[8:11], then `body`.
fn native(body: &[u32]) -> NativeProgram {
    native_of(body, true, 64)
}

/// The same for a console of 32-lane waves when `wave64` is not set, with `threads` threads.
fn native_of(body: &[u32], wave64: bool, threads: u32) -> NativeProgram {
    let mut words = vec![
        0xBE85_0380, // s_mov_b32 s5, 0
        0x3D << 26 | 0x02 << 18 | 8 << 6 | 2,
        125 << 25, // s_load_dwordx4 s[8:11], s[4:5], 0
    ];
    words.extend_from_slice(body);
    words.push(S_ENDPGM);
    NativeProgram {
        stage: HwStage::Cs,
        wave64,
        vgprs: 16,
        layout: vec![
            RootParam {
                root: RootType::Constants,
                table_offset_dwords: 0,
                sgprs: 4,
                base_register: 0,
                user_data: 0,
                slot: 0,
            },
            RootParam {
                root: RootType::Table,
                table_offset_dwords: 0,
                sgprs: 1,
                base_register: 0,
                user_data: 4,
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
            user_data: 4,
            slot: 1,
        }],
        io: StageIo::Cs(ComputeIo {
            rsrc2: 5 << 1, // 5 user SGPRs: s0..s4
            threads: [threads, 1, 1],
        }),
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    }
}

/// `program()` for a host of 32-lane waves, with the branches closing `wide` taken wave-wide.
fn lowered(native: &NativeProgram, p: &ir::Program, wide: &HashSet<usize>) -> Result<HostShader, ShaderError> {
    lowered_on(native, p, wide, 32)
}

/// [`lowered`] for a host whose waves have at most `host_wave` lanes.
fn lowered_on(
    native: &NativeProgram,
    p: &ir::Program,
    wide: &HashSet<usize>,
    host_wave: u32,
) -> Result<HostShader, ShaderError> {
    lowered_counted(native, p, wide, host_wave, false)
}

fn lowered_counted(
    native: &NativeProgram,
    p: &ir::Program,
    wide: &HashSet<usize>,
    host_wave: u32,
    counted: bool,
) -> Result<HostShader, ShaderError> {
    let st = structure::structure_wide(p, wide).expect("structure");
    let res = resource::track(p, &native.layout, native.stage, Some(&st)).expect("resource::track");
    let io = io::raise(p, native).expect("io::raise");
    let was = lower::TEST_HOST_WAVE.replace(Some(host_wave));
    let lowered = lower::lower_counted(&lower::Parts {
        native,
        p,
        st: &st,
        io: &io,
        res: &res,
        inputs: Vec::new(),
        outputs: Vec::new(),
        prims: Vec::new(),
        mesh_stride: 0,
    }, counted);
    lower::TEST_HOST_WAVE.set(was);
    let lowered = lowered?;
    Ok(HostShader {
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
    })
}

#[test]
fn a_sum_of_halves_over_the_wave_is_the_consoles_tree_bit_for_bit() {
    // thread t holds the 16-bit float with bits 0x3C00 + 37 t (1.0 and up, with odd
    // mantissas, so that additions round); all of them store the wave's sum
    let mut words = vec![
        0x340C_0085, // v_lshlrev_b32 v6, 5, v0
        0x340E_0082, // v_lshlrev_b32 v7, 2, v0
        0x4A0E_0F06, // v_add_nc_u32 v7, v6, v7
        0x4A0E_0F00, // v_add_nc_u32 v7, v0, v7
        0x4A0E_0EFF,
        0x0000_3C00, // v_add_nc_u32 v7, 0x3c00, v7
    ];
    // (the wave OR's words with v_add_f16 for v_or_b32: opcode 50 for 28)
    words.extend(super::wave::WAVE_OR_V7_TO_S7.iter().map(|&w| if w >> 25 == 28 { w & 0x01FF_FFFF | 50 << 25 } else { w }));
    words.extend_from_slice(&[
        0x7E00_0207, // v_mov_b32 v0, s7
        0x38 << 26 | 0x1C << 18,
        0x80 << 24 | 2 << 16, // buffer_store_dword v0, off, s[8:11], 0
    ]);
    let native = native(&words);
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    assert_eq!(p.blocks.iter().flat_map(|b| &b.insts).filter(|i| i.name() == "v_add_f16").count(), 6);
    let shader = lowered(&native, &p, &HashSet::new()).expect("lower");
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        let mut copy = shader.dxil.clone();
        v.validate(&mut copy).expect("DXIL validation failed");
    }
    // the console's sum: lanes 2k and 2k+1, then pairs, ... each addition rounded to 16 bits
    // (to nearest, ties to even); values here are normal numbers
    let wide = |h: u16| f32::from_bits((u32::from(h >> 10) + 112) << 23 | u32::from(h & 0x3FF) << 13);
    let narrow = |f: f32| {
        let b = f.to_bits();
        let h = ((b >> 23) - 112) << 10 | (b >> 13 & 0x3FF);
        let rest = b & 0x1FFF;
        (h + u32::from(rest > 0x1000 || rest == 0x1000 && h & 1 == 1)) as u16
    };
    let add = |a: u16, b: u16| narrow(wide(a) + wide(b));
    let terms: Vec<u16> = (0..64u16).map(|t| 0x3C00 + 37 * t).collect();
    let mut tree = terms.clone();
    while tree.len() > 1 {
        tree = tree.chunks(2).map(|c| add(c[0], c[1])).collect();
    }
    let in_a_row = terms.iter().copied().reduce(add).unwrap();
    assert_ne!(tree[0], in_a_row, "these terms tell the tree from another order");
    let _one = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let Ok(host) = Host::new(false) else {
        eprintln!("no SM 6.6 hardware device; not run");
        return;
    };
    let (root, pso) = match host.compute_pipeline(&shader) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("the hardware device does not take this program (waves of 32?): {e}; not run");
            return;
        }
    };
    let ud = vec![0u32; shader.user_data_dwords().max(1) as usize];
    let out = host
        .dispatch(
            &root,
            &pso,
            &Dispatch {
                roots: vec![(0, Root::Constants(ud))],
                views: &[View::RawUav(1)],
                samplers: &[],
                groups: [1, 1, 1],
            },
        )
        .unwrap_or_else(|e| panic!("dispatch failed: {e}"));
    // (the high half is lane 0's term's: zero)
    assert_eq!(out[0][0], u32::from(tree[0]), "tree {:#x}, in a row {in_a_row:#x}", tree[0]);
    eprintln!("ran on the hardware device: the sum {:#x} of the console's tree", tree[0]);
}

/// "Some lane is left" of a mask operation as a number: an upscaler's programs wait for
/// other groups in a loop that ends when every lane saw its flag, or after enough rounds:
/// both tests become -1 or 0 and SCC of `timeout | ~waiting` ends it.
#[test]
fn some_lane_of_a_mask_is_a_number_the_whole_wave_agrees_on() {
    let native = native(&[
        0xD765_0001,
        0x0001_00C1, // v_mbcnt_lo_u32_b32 v1, -1, 0
        0xD766_0001,
        0x0002_02C1, // v_mbcnt_hi_u32_b32 v1, -1, v1
        0x7D84_0200, // v_cmp_eq_u32 vcc, s0, v1
        0x8792_7E6A, // s_and_b64 s[18:19], vcc, exec
        0x8592_80C1, // s_cselect_b64 s[18:19], -1, 0
        0xBF09_8501, // s_cmp_ge_u32 s1, 5
        0x8594_80C1, // s_cselect_b64 s[20:21], -1, 0
        0x8B92_1214, // s_orn2_b64 s[18:19], s[20:21], s[18:19]
        0x8506_8081, // s_cselect_b32 s6, 1, 0
        0x7E04_0206, // v_mov_b32 v2, s6
        0xE070_0000,
        0x8002_0200, // buffer_store_dword v2, off, s[8:11], 0
    ]);
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let shader = on_64_lanes(&native, &p);
    // (lane s0 waits, if there is one; s1 rounds)
    for (wait, rounds, done) in [(0, 0, 0), (37, 4, 0), (63, 4, 0), (64, 0, 1), (37, 5, 1), (64, 9, 1)] {
        let Some(out) = dispatched(&shader, &[wait, rounds]) else {
            return;
        };
        assert_eq!(out, done, "lane {wait} after {rounds} rounds");
    }
}

/// `native` for a host of 64-lane waves.
fn on_64_lanes(native: &NativeProgram, p: &ir::Program) -> HostShader {
    let st = structure::structure(p).expect("structure");
    let res = resource::track(p, &native.layout, native.stage, Some(&st)).expect("resource::track");
    let io = io::raise(p, native).expect("io::raise");
    let was = lower::TEST_HOST_WAVE.replace(Some(64));
    let lowered = lower::lower(&lower::Parts {
        native,
        p,
        st: &st,
        io: &io,
        res: &res,
        inputs: Vec::new(),
        outputs: Vec::new(),
        prims: Vec::new(),
        mesh_stride: 0,
    });
    lower::TEST_HOST_WAVE.set(was);
    let lowered = lowered.expect("lower");
    assert_eq!(lowered.wave_size, Some(64));
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
        half_views: Vec::new(),
    };
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        let mut copy = shader.dxil.clone();
        v.validate(&mut copy).expect("DXIL validation failed");
    }
    shader
}

/// Dword 0 of the UAV after `shader` ran with root constants `constants`; none without a
/// device that takes it.
fn dispatched(shader: &HostShader, constants: &[u32]) -> Option<u32> {
    let _one = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let Ok(host) = Host::new(false) else {
        eprintln!("no SM 6.6 hardware device; not run");
        return None;
    };
    let (root, pso) = match host.compute_pipeline(shader) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("the hardware device does not take this program (waves of 64?): {e}; not run");
            return None;
        }
    };
    let mut ud = vec![0u32; shader.user_data_dwords().max(1) as usize];
    for u in &shader.user_data {
        if u.root == RootType::Constants {
            ud[u.index as usize] = constants.get(u.dword as usize).copied().unwrap_or(0);
        }
    }
    let out = host
        .dispatch(
            &root,
            &pso,
            &Dispatch {
                roots: vec![(0, Root::Constants(ud))],
                views: &[View::RawUav(1)],
                samplers: &[],
                groups: [1, 1, 1],
            },
        )
        .unwrap_or_else(|e| panic!("dispatch failed: {e}"));
    Some(out[0][0])
}

/// The same sum inside `if (t >= s0) { ... }` on a host of 64-lane waves: the lanes below s0
/// are elsewhere, and count as +0 in the console's tree.
#[test]
fn a_sum_of_halves_where_not_every_lane_runs_on_a_wave_of_64() {
    let mut words = vec![
        0x340C_0085, // v_lshlrev_b32 v6, 5, v0
        0x340E_0082, // v_lshlrev_b32 v7, 2, v0
        0x4A0E_0F06, // v_add_nc_u32 v7, v6, v7
        0x4A0E_0F00, // v_add_nc_u32 v7, v0, v7
        0x4A0E_0EFF,
        0x0000_3C00, // v_add_nc_u32 v7, 0x3c00, v7
        0x7D86_0000, // v_cmp_le_u32 vcc, s0, v0
        0xBE8C_246A, // s_and_saveexec_b64 s[12:13], vcc
        0xBF88_0000 | 23, // s_cbranch_execz to the restore of EXEC
    ];
    words.extend(super::wave::WAVE_OR_V7_TO_S7.iter().map(|&w| if w >> 25 == 28 { w & 0x01FF_FFFF | 50 << 25 } else { w }));
    words.extend_from_slice(&[
        0x7E00_0207, // v_mov_b32 v0, s7
        0x38 << 26 | 0x1C << 18,
        0x80 << 24 | 2 << 16, // buffer_store_dword v0, off, s[8:11], 0
        0xBEFE_040C, // s_mov_b64 exec, s[12:13]
    ]);
    let native = native(&words);
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let shader = on_64_lanes(&native, &p);
    let wide = |h: u16| match h {
        0 => 0.0,
        h => f32::from_bits((u32::from(h >> 10) + 112) << 23 | u32::from(h & 0x3FF) << 13),
    };
    let narrow = |f: f32| {
        if f == 0.0 {
            return 0;
        }
        let b = f.to_bits();
        let h = ((b >> 23) - 112) << 10 | (b >> 13 & 0x3FF);
        let rest = b & 0x1FFF;
        (h + u32::from(rest > 0x1000 || rest == 0x1000 && h & 1 == 1)) as u16
    };
    for from in [0u32, 1, 5, 31, 32, 37, 63] {
        let mut tree: Vec<u16> = (0..64u16).map(|t| if u32::from(t) < from { 0 } else { 0x3C00 + 37 * t }).collect();
        while tree.len() > 1 {
            tree = tree.chunks(2).map(|c| narrow(wide(c[0]) + wide(c[1]))).collect();
        }
        let Some(out) = dispatched(&shader, &[from]) else {
            return;
        };
        assert_eq!(out, u32::from(tree[0]), "lanes from {from}");
    }
}

/// The console's 16-bit wave sum where the lanes at or past s0 have left the program (a
/// thread past the end of the data): on the console they add +0, and after each step of the
/// tree they hold their run's partial sum all the same. On a host of 64-lane waves (AMD) the
/// lanes that are here read each partner run's sum from a lane of it that is here.
#[test]
fn a_sum_of_halves_where_lanes_have_left_is_the_consoles_tree_bit_for_bit() {
    let mut words = vec![
        0x7DA8_0000, // v_cmpx_gt_u32 exec, s0, v0: the lanes below s0 stay
        0, // s_cbranch_execz to the end (filled in below)
        0x340C_0085, // v_lshlrev_b32 v6, 5, v0
        0x340E_0082, // v_lshlrev_b32 v7, 2, v0
        0x4A0E_0F06, // v_add_nc_u32 v7, v6, v7
        0x4A0E_0F00, // v_add_nc_u32 v7, v0, v7
        0x4A0E_0EFF,
        0x0000_3C00, // v_add_nc_u32 v7, 0x3c00, v7
    ];
    words.extend(super::wave::WAVE_OR_V7_TO_S7.iter().map(|&w| if w >> 25 == 28 { w & 0x01FF_FFFF | 50 << 25 } else { w }));
    words.extend_from_slice(&[
        0x7E00_0207, // v_mov_b32 v0, s7
        0x38 << 26 | 0x1C << 18,
        0x80 << 24 | 2 << 16, // buffer_store_dword v0, off, s[8:11], 0
    ]);
    // past every word after it, to the program's end
    words[1] = 0xBF88_0000 | (words.len() as u32 - 2);
    let native = native(&words);
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let shader = lowered_on(&native, &p, &HashSet::new(), 64).expect("lower");
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        let mut copy = shader.dxil.clone();
        v.validate(&mut copy).expect("DXIL validation failed");
    }
    let wide = |h: u16| {
        if h == 0 {
            0.0
        } else {
            f32::from_bits((u32::from(h >> 10) + 112) << 23 | u32::from(h & 0x3FF) << 13)
        }
    };
    let narrow = |f: f32| {
        if f == 0.0 {
            return 0;
        }
        let b = f.to_bits();
        let h = ((b >> 23) - 112) << 10 | (b >> 13 & 0x3FF);
        let rest = b & 0x1FFF;
        (h + u32::from(rest > 0x1000 || rest == 0x1000 && h & 1 == 1)) as u16
    };
    let add = |a: u16, b: u16| narrow(wide(a) + wide(b));
    let _one = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let mut ran = false;
    for stay in [64u32, 63, 40, 33, 32, 17, 5, 1] {
        let terms: Vec<u16> = (0..64u32)
            .map(|t| if t < stay { (0x3C00 + 37 * t) as u16 } else { 0 })
            .collect();
        let mut tree = terms.clone();
        while tree.len() > 1 {
            tree = tree.chunks(2).map(|c| add(c[0], c[1])).collect();
        }
        let Some(got) = on_the_card(&shader, [stay, 0]) else {
            return;
        };
        ran = true;
        // (the high half is lane 0's term's: zero)
        assert_eq!(got, u32::from(tree[0]), "{stay} lanes stay: got {got:#x}, the console's tree {:#x}", tree[0]);
    }
    if ran {
        eprintln!("ran on the hardware device: the console's tree with lanes gone, in every case");
    }
}

/// One `v_add_f16` whose exact result lies between two halves: 1 + 3/1024 (0x3C03) plus 2
/// (0x4000) is 3 + 1.5 steps of that range, which the console rounds up to 0x4202. The
/// host's `f32tof16` may give 0x4201 (toward zero); with `VICTORY_F16_NEAREST=1` the
/// translation gives the console's whatever the host does.
#[test]
fn a_16_bit_sum_rounds_to_nearest_when_asked_to() {
    let words = [
        0x7E0E_02FF,
        0x0000_3C03, // v_mov_b32 v7, 0x3c03
        0x7E0C_02FF,
        0x0000_4000, // v_mov_b32 v6, 0x4000
        0x6400_0D07, // v_add_f16 v0, v7, v6
        0x38 << 26 | 0x1C << 18,
        0x80 << 24 | 2 << 16, // buffer_store_dword v0, off, s[8:11], 0
    ];
    let native = native(&words);
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    assert!(p.blocks.iter().flat_map(|b| &b.insts).any(|i| i.name() == "v_add_f16"));
    let _one = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    for nearest in [false, true] {
        let was = lower::TEST_F16_NEAREST.replace(Some(nearest));
        let shader = lowered(&native, &p, &HashSet::new());
        lower::TEST_F16_NEAREST.set(was);
        let shader = shader.expect("lower");
        if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
            let mut copy = shader.dxil.clone();
            v.validate(&mut copy).expect("DXIL validation failed");
        }
        let Ok(host) = Host::new(false) else {
            eprintln!("no SM 6.6 hardware device; not run");
            return;
        };
        let Ok((root, pso)) = host.compute_pipeline(&shader) else {
            eprintln!("the hardware device does not take this program; not run");
            return;
        };
        let ud = vec![0u32; shader.user_data_dwords().max(1) as usize];
        let out = host
            .dispatch(
                &root,
                &pso,
                &Dispatch {
                    roots: vec![(0, Root::Constants(ud))],
                    views: &[View::RawUav(1)],
                    samplers: &[],
                    groups: [1, 1, 1],
                },
            )
            .unwrap_or_else(|e| panic!("dispatch failed: {e}"));
        let got = out[0][0] & 0xFFFF;
        eprintln!("v_add_f16 0x3c03 + 0x4000 on the hardware device, nearest {nearest}: {got:#x}");
        if nearest {
            assert_eq!(got, 0x4202);
        } else {
            assert!(got == 0x4201 || got == 0x4202, "{got:#x}");
        }
    }
}

#[test]
fn a_wave_or_inside_a_lane_branch_needs_the_branch_wave_wide() {
    for (skip, name) in [(0xBF88_0000, "s_cbranch_execz"), (0xBF84_0000, "s_cbranch_scc0")] {
        a_wave_or_inside(skip, name);
    }
}

fn a_wave_or_inside(skip: u32, name: &str) {
    let native = program(skip);
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let branch = (0..p.blocks.len())
        .find(|&b| p.blocks[b].insts.last().is_some_and(|i| i.name() == name))
        .expect("the branch");
    // per lane, the two host waves cannot meet inside the branch
    lower::take_needs();
    let per_lane = lowered(&native, &p, &HashSet::new());
    assert!(
        matches!(&per_lane, Err(ShaderError::Unsupported { what, .. }) if *what == "a wave reduction where not every lane runs"),
        "{:?}",
        per_lane.err()
    );
    // ... and the lowering names the block, around which that branch is the one to widen
    let needs = lower::take_needs();
    let st = structure::structure(&p).expect("structure");
    assert_eq!(lower::branches_around(&st, &needs), Some(vec![branch]));
    // (taken, past the body, when every lane's bit is clear)
    assert_eq!(st.wave_any[branch], Some(false));
    // wave-wide, every lane runs the reduction (EXEC stays a guard on what it does there)
    let wide = HashSet::from([branch]);
    let st = structure::structure_wide(&p, &wide).expect("structure");
    let inside = match p.blocks[branch].exit {
        ir::Exit::Branch { not_taken, .. } => not_taken,
        _ => unreachable!(),
    };
    assert!(st.guards[inside].iter().any(Option::is_some));
    let shader = lowered(&native, &p, &wide).expect("lower");
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        let mut copy = shader.dxil.clone();
        v.validate(&mut copy).expect("DXIL validation failed");
    }
    // On a GPU that takes waves of 32 (two host waves for the console's one): the OR is of
    // both halves, whichever half's lanes take the branch.
    let _one = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let Ok(host) = Host::new(false) else {
        eprintln!("no SM 6.6 hardware device; not run");
        return;
    };
    let (root, pso) = match host.compute_pipeline(&shader) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("the hardware device does not take this program (waves of 32?): {e}; not run");
            return;
        }
    };
    // (threads below s0 take the branch; thread t gives bit t >> 1)
    for (below, expected) in [(64u32, u32::MAX), (40, 0x000F_FFFF), (33, 0x0001_FFFF), (10, 0x1F), (1, 1)] {
        let mut ud = vec![0u32; shader.user_data_dwords().max(1) as usize];
        for u in &shader.user_data {
            if let (RootType::Constants, 0, 0) = (u.root, u.slot, u.dword) {
                ud[u.index as usize] = below;
            }
        }
        let out = host
            .dispatch(
                &root,
                &pso,
                &Dispatch {
                    roots: vec![(0, Root::Constants(ud))],
                    views: &[View::RawUav(1)],
                    samplers: &[],
                    groups: [1, 1, 1],
                },
            )
            .unwrap_or_else(|e| panic!("dispatch failed: {e}"));
        assert_eq!(out[0][0], expected, "{name}: {below} threads in the branch");
    }
    eprintln!("ran on the hardware device ({name}): the OR of both halves in every case");
}

/// Runs `shader` on the hardware device with the root constants `s0`, `s1` and gives dword 0
/// of the UAV; None (and a line) without a device that takes it.
fn on_the_card(shader: &HostShader, constants: [u32; 2]) -> Option<u32> {
    let Ok(host) = Host::new(false) else {
        eprintln!("no SM 6.6 hardware device; not run");
        return None;
    };
    let Ok((root, pso)) = host.compute_pipeline(shader) else {
        eprintln!("the hardware device does not take this program; not run");
        return None;
    };
    let mut ud = vec![0u32; shader.user_data_dwords().max(1) as usize];
    for u in &shader.user_data {
        if let (RootType::Constants, 0, d @ 0..=1) = (u.root, u.slot, u.dword) {
            ud[u.index as usize] = constants[d as usize];
        }
    }
    let out = host
        .dispatch(
            &root,
            &pso,
            &Dispatch {
                roots: vec![(0, Root::Constants(ud))],
                views: &[View::RawUav(1)],
                samplers: &[],
                groups: [1, 1, 1],
            },
        )
        .unwrap_or_else(|e| panic!("dispatch failed: {e}"));
    Some(out[0][0])
}

/// A 32-lane console wave. A register holds a lane mask on one path (the lanes below s0)
/// and a number on the other, and is read as a number after the join, as the trace programs
/// keep their lanes' masks and their node numbers on one stack
/// (`deferred_tiled:CS_LaserLightShadowRaytracing`, s46 at 0x104, 0x75c and 0x188). The
/// mask arrives as the ballot of its lanes.
#[test]
fn a_lane_mask_joined_with_a_number_is_read_as_the_number_it_is() {
    let words = [
        0x7D88_0000, // v_cmp_gt_u32 vcc_lo, s0, v0
        0xBE94_036A, // s_mov_b32 s20, vcc_lo
        0xBF06_8001, // s_cmp_eq_u32 s1, 0
        0xBF85_0001, // s_cbranch_scc1 over the next
        0xBE94_0385, // s_mov_b32 s20, 5
        0x7E00_0214, // v_mov_b32 v0, s20
        0x38 << 26 | 0x1C << 18,
        0x80 << 24 | 2 << 16, // buffer_store_dword v0, off, s[8:11], 0
    ];
    let native = native_of(&words, false, 32);
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let shader = lowered(&native, &p, &HashSet::new()).expect("lower");
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        let mut copy = shader.dxil.clone();
        v.validate(&mut copy).expect("DXIL validation failed");
    }
    let _one = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    for (below, other, expected) in [(32u32, 0u32, u32::MAX), (10, 0, 0x3FF), (0, 0, 0), (10, 1, 5)] {
        let Some(got) = on_the_card(&shader, [below, other]) else { return };
        assert_eq!(got, expected, "{below} lanes in the mask, s1 = {other}");
    }
    eprintln!("ran on the hardware device: the mask's bits, or the number");
}

/// A 32-lane console wave. `if (t < s0) { ... SCC = (s1 == 7) }` and a branch on SCC after
/// the join: the skip brings the SCC of `s_and_saveexec` ("some lane is left") along
/// (`rtdirectionalshadows:CS_RTShadowTrace32HQ_TiledFarOnly`, 0x360-0x39c). On the console
/// the skip is taken by the wave, so with some lane inside every lane sees the inner test.
/// A lane that skipped on its own would see "some lane is left" instead: refused per lane,
/// right with the branch taken wave-wide. Lane 31 stores what it saw.
#[test]
fn scc_of_a_mask_operation_joined_with_a_scalar_test_needs_the_skip_wave_wide() {
    let words = [
        0x7D88_0000, // v_cmp_gt_u32 vcc_lo, s0, v0
        0xBE8D_3C6A, // s_and_saveexec_b32 s13, vcc_lo
        0xBF88_0001, // s_cbranch_execz over the next
        0xBF06_8701, // s_cmp_eq_u32 s1, 7
        0xBEFE_030D, // s_mov_b32 exec_lo, s13
        0xBF85_0002, // s_cbranch_scc1 to "seven"
        0x7E0A_0281, // v_mov_b32 v5, 1
        0xBF82_0001, // s_branch over the next
        0x7E0A_0282, // seven: v_mov_b32 v5, 2
        0x7DA4_009F, // v_cmpx_eq_u32 exec_lo, 31, v0
        0x38 << 26 | 0x1C << 18,
        0x80 << 24 | 2 << 16 | 5 << 8, // buffer_store_dword v5, off, s[8:11], 0
    ];
    let native = native_of(&words, false, 32);
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let branch = (0..p.blocks.len())
        .find(|&b| p.blocks[b].insts.last().is_some_and(|i| i.name() == "s_cbranch_execz"))
        .expect("the skip");
    lower::take_needs();
    let per_lane = lowered(&native, &p, &HashSet::new());
    assert!(
        matches!(&per_lane, Err(ShaderError::Unsupported { what, .. }) if *what == "a lane mask read as data"),
        "{:?}",
        per_lane.err()
    );
    let st = structure::structure(&p).expect("structure");
    assert_eq!(lower::branches_around(&st, &lower::take_needs()), Some(vec![branch]));
    let shader = lowered(&native, &p, &HashSet::from([branch])).expect("lower");
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        let mut copy = shader.dxil.clone();
        v.validate(&mut copy).expect("DXIL validation failed");
    }
    let _one = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    // (lanes below s0 inside, s1) -> what lane 31 stores: 2 if SCC was set at the branch
    for (below, s1, expected) in [(0u32, 7u32, 1u32), (10, 7, 2), (10, 3, 1), (32, 3, 1), (32, 7, 2)] {
        let Some(got) = on_the_card(&shader, [below, s1]) else { return };
        assert_eq!(got, expected, "{below} lanes inside, s1 = {s1}");
    }
    eprintln!("ran on the hardware device: the inner test's SCC for every lane once some lane is inside");
}

/// A dispatch counted in threads: of the last group only the lanes below the
/// count store (after DexNom/VIctory bc3cd4b0).
#[test]
fn the_lanes_past_a_count_of_threads_do_not_run() {
    // Each lane that runs adds 1 to counter[local x].
    let native = native(&[
        0x3402_0082, // v_lshlrev_b32 v1, 2, v0
        0x7e04_0281, // v_mov_b32 v2, 1
        0xe0c8_1000, 0x8002_0201, // buffer_atomic_add v2, v1, s[8:11], 0 offen
    ]);
    let p = ir::build(&native.code, native.wave64, native.vgprs).unwrap();
    let shader = lowered_counted(&native, &p, &HashSet::new(), 64, true).unwrap();
    let limit = shader
        .user_data
        .iter()
        .find(|u| u.root == RootType::Other(super::USER_DISPATCH_THREADS_X))
        .unwrap();
    assert_eq!((limit.slot, limit.dword), (0, 64));
    let _one = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let Ok(host) = Host::new(true) else { return };
    let Ok((root, pso)) = host.compute_pipeline(&shader) else { return };
    for count in [0u32, 1, 7, 63, 64, 65, 74, 77, 127, 128] {
        let mut constants = vec![0; shader.user_data_dwords() as usize];
        constants[limit.index as usize] = count;
        let result = host
            .dispatch(&root, &pso, &Dispatch {
                roots: vec![(0, Root::Constants(constants))],
                views: &[View::RawUav(68)],
                samplers: &[],
                groups: [2, 1, 1],
            })
            .unwrap();
        let active = if count == 0 { 128 } else { count };
        for (i, &word) in result[0].iter().enumerate().take(64) {
            let i = i as u32;
            assert_eq!(word, u32::from(i < active) + u32::from(i + 64 < active), "count {count}, word {i}");
        }
    }
}

/// A block-wide prefix sum of a group of several console waves, as the ray-tracing structure
/// builder's scans are made: each console wave sums its lanes (inside each half with
/// `ds_swizzle`, then the lower half's total read with `v_readlane` and added to the upper
/// half), its last lane writes the wave's total to LDS, and after `s_barrier` each wave adds
/// the totals of the waves before it in a loop. The loop starts under `v_cmpx` (wave 0 does
/// not enter it), leaves through `v_cmpx` / `s_andn2` / `s_cbranch_scc1`, and carries its sum
/// in a scalar with `v_readfirstlane`. Thread t adds 5t + s0; it stores its inclusive sum.
/// `lane_varying`: the loop's sum also adds the lane's own number, so the first-lane read
/// takes a value that differs between the lanes. `write_after`: each wave writes its total to
/// LDS again after the loop, so LDS may still change where the loop reads it.
fn block_scan(threads: u32, lane_varying: bool, write_after: bool) -> NativeProgram {
    let mut w: Vec<u32> = vec![
        0x340A_0082, // v_lshlrev_b32 v5, 2, v0
        0x4A0A_0B00, // v_add_nc_u32 v5, v0, v5
        0x4A0A_0A00, // v_add_nc_u32 v5, s0, v5: the thread's term
        0xD765_0001,
        0x0001_00C1, // v_mbcnt_lo_u32_b32 v1, -1, 0
        0xD766_0001,
        0x0002_02C1, // v_mbcnt_hi_u32_b32 v1, -1, v1: the lane's number
        0x7E0C_0305, // v_mov_b32 v6, v5
    ];
    // in each half: lanes with bit k of their number set add the last lane below that bit's block
    for k in [1u32, 2, 4, 8, 16] {
        let swizzle = (0x1F ^ (2 * k - 1)) | (k - 1) << 5;
        w.extend_from_slice(&[
            0xD8D4_0000 | swizzle,
            0x0200_0006,      // ds_swizzle_b32 v2, v6 (and_mask, or_mask)
            0xBF8C_C07F,      // s_waitcnt lgkmcnt(0)
            0x3606_0200 | (0x80 + k), // v_and_b32 v3, k, v1
            0x7D8A_0680,      // v_cmp_ne_u32 vcc, 0, v3
            0x0204_0480,      // v_cndmask_b32 v2, 0, v2, vcc
            0x4A0C_0D02,      // v_add_nc_u32 v6, v2, v6
        ]);
    }
    w.extend_from_slice(&[
        0xD760_000C,
        0x0001_3F06, // v_readlane_b32 s12, v6, 31: the lower half's total
        0x7D82_029F, // v_cmp_lt_u32 vcc, 31, v1
        0xD501_0002,
        0x01A8_1880, // v_cndmask_b32 v2, 0, s12, vcc
        0x4A0C_0D02, // v_add_nc_u32 v6, v2, v6: the sum over the console wave's lanes so far
        0x2C0E_0086, // v_lshrrev_b32 v7, 6, v0: the console wave's number
        0xBE80_047E, // s_mov_b64 s[0:1], exec
        0x7DA4_02BF, // v_cmpx_eq_u32 exec, 63, v1
        0x3406_0E82, // v_lshlrev_b32 v3, 2, v7
        0xD834_0000,
        0x0000_0603, // ds_write_b32 v3, v6: the wave's total
        0xBEFE_0400, // s_mov_b64 exec, s[0:1]
        0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
        0xBF8A_0000, // s_barrier
        0x7E10_0280, // v_mov_b32 v8, 0
        0xBE80_047E, // s_mov_b64 s[0:1], exec
        0x7DA2_0E80, // v_cmpx_lt_u32 exec, 0, v7
    ]);
    let skip = w.len();
    w.extend_from_slice(&[
        0,           // s_cbranch_execz done (filled in below)
        0xBE86_047E, // s_mov_b64 s[6:7], exec: the lanes still in the loop
        0xBE92_0380, // s_mov_b32 s18, 0: the wave whose total is next
        0xBE91_0380, // s_mov_b32 s17, 0: the sum of the totals so far
        0xBF82_0002, // s_branch body
    ]);
    let latch = w.len();
    w.extend_from_slice(&[
        0x87FE_0614, // latch: s_and_b64 exec, s[20:21], s[6:7]
        0x7E22_0508, // v_readfirstlane_b32 s17, v8
        0x8F10_8212, // body: s_lshl_b32 s16, s18, 2
        0x8112_8112, // s_add_i32 s18, s18, 1
        0x7E12_0210, // v_mov_b32 v9, s16
        0xD8D8_0000,
        0x0900_0009, // ds_read_b32 v9, v9
        0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
        0x4A10_1211, // v_add_nc_u32 v8, s17, v9
    ]);
    if lane_varying {
        w.push(0x4A10_1101); // v_add_nc_u32 v8, v1, v8
    }
    w.extend_from_slice(&[
        0xBE94_047E, // s_mov_b64 s[20:21], exec
        0x7DA4_0E12, // v_cmpx_eq_u32 exec, s18, v7: the lanes whose waves are all summed
        0x8A86_7E06, // s_andn2_b64 s[6:7], s[6:7], exec
    ]);
    let back = w.len();
    w.push(0xBF85_0000 | (latch as i32 - back as i32 - 1) as u16 as u32); // s_cbranch_scc1 latch
    let done = w.len();
    w[skip] = 0xBF88_0000 | (done - skip - 1) as u32; // s_cbranch_execz done
    w.extend_from_slice(&[
        0xBEFE_0400, // done: s_mov_b64 exec, s[0:1]
        0x4A0C_0D08, // v_add_nc_u32 v6, v8, v6
    ]);
    if write_after {
        w.extend_from_slice(&[
            0xD834_0000,
            0x0000_0603, // ds_write_b32 v3, v6
        ]);
    }
    w.extend_from_slice(&[
        0x340E_0082, // v_lshlrev_b32 v7, 2, v0
        0xE070_1000,
        0x8002_0607, // buffer_store_dword v6, v7, s[8:11], 0 offen
    ]);
    let mut native = native_of(&w, true, threads);
    if let StageIo::Cs(cs) = &mut native.io {
        cs.rsrc2 |= 1 << 15; // 512 bytes of LDS
    }
    native
}

/// The whole UAV of `dwords` dwords after `shader` ran once with root constant s0 = `s0`;
/// None (and a line) without a device that takes it.
fn dispatched_all(shader: &HostShader, s0: u32, dwords: u32) -> Option<Vec<u32>> {
    let _one = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let Ok(host) = Host::new(false) else {
        eprintln!("no SM 6.6 hardware device; not run");
        return None;
    };
    let (root, pso) = match host.compute_pipeline(shader) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("the hardware device does not take this program (waves of 32?): {e}; not run");
            return None;
        }
    };
    let mut ud = vec![0u32; shader.user_data_dwords().max(1) as usize];
    for u in &shader.user_data {
        if let (RootType::Constants, 0, 0) = (u.root, u.slot, u.dword) {
            ud[u.index as usize] = s0;
        }
    }
    let out = host
        .dispatch(
            &root,
            &pso,
            &Dispatch {
                roots: vec![(0, Root::Constants(ud))],
                views: &[View::RawUav(dwords)],
                samplers: &[],
                groups: [1, 1, 1],
            },
        )
        .unwrap_or_else(|e| panic!("dispatch failed: {e}"));
    Some(out[0].clone())
}

/// The loop over the waves before this one runs a different number of rounds in each
/// console wave, so the two host waves of one cannot meet in it; but the value the first-lane
/// read takes there is the same in every lane that holds it (scalars, and an LDS read at a
/// scalar address once no thread writes LDS any more), so each host wave's own first lane
/// gives the console's. Exact on a host of 32-lane waves, against the sum on the CPU.
#[test]
fn a_block_prefix_sum_over_console_waves_runs_in_waves_of_32() {
    for threads in [256u32, 512] {
        let native = block_scan(threads, false, false);
        let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
        assert_eq!(p.blocks.iter().flat_map(|b| &b.insts).filter(|i| i.name() == "v_readfirstlane_b32").count(), 1);
        let shader = lowered(&native, &p, &HashSet::new()).expect("lower");
        if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
            let mut copy = shader.dxil.clone();
            v.validate(&mut copy).expect("DXIL validation failed");
        }
        for s0 in [0u32, 7, 0x7FFF_FFF0] {
            let mut sum = 0u32;
            let want: Vec<u32> = (0..threads)
                .map(|t| {
                    sum = sum.wrapping_add((5 * t).wrapping_add(s0));
                    sum
                })
                .collect();
            let Some(out) = dispatched_all(&shader, s0, threads) else {
                return;
            };
            for t in 0..threads as usize {
                assert_eq!(out[t], want[t], "{threads} threads, s0 = {s0}: thread {t}");
            }
        }
        eprintln!("ran on the hardware device: the block prefix sum of {threads} threads");
    }
}

/// The same loop where the value read is the lane's own: the console takes the first lane in
/// EXEC, which may be in the other host wave, and the two cannot meet in the loop. Refused.
#[test]
fn a_first_lane_read_of_a_lane_value_in_a_loop_per_console_wave_is_refused() {
    let native = block_scan(256, true, false);
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let r = lowered(&native, &p, &HashSet::new());
    assert!(
        matches!(&r, Err(ShaderError::Unsupported { what, .. }) if *what == "a first-lane read where not every lane runs"),
        "{:?}",
        r.err()
    );
}

/// The same loop where a thread may still write LDS after the barrier: another console wave
/// may change what the loop reads between the reads of its two host waves. Refused.
#[test]
fn a_first_lane_read_of_lds_that_may_still_change_is_refused() {
    let native = block_scan(256, false, true);
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let r = lowered(&native, &p, &HashSet::new());
    assert!(
        matches!(&r, Err(ShaderError::Unsupported { what, .. }) if *what == "a first-lane read where not every lane runs"),
        "{:?}",
        r.err()
    );
}
/// An LDS instruction (DS encoding): opcode, address VGPR, data VGPRs, destination VGPR,
/// offset (offset1 in the high byte).
fn ds(op: u32, addr: u32, data0: u32, data1: u32, vdst: u32, offset: u32) -> [u32; 2] {
    [0xD800_0000 | op << 18 | offset, vdst << 24 | data1 << 16 | data0 << 8 | addr]
}

const DS_ADD_U32: u32 = 0;
const DS_MAX_U32: u32 = 8;
const DS_WRITE_B32: u32 = 13;
const DS_WRITE2_B32: u32 = 14;
const DS_ADD_RTN_U32: u32 = 32;
const DS_SWIZZLE_B32: u32 = 53;
const DS_READ_B32: u32 = 54;
const DS_READ2_B32: u32 = 55;

/// `native` lowered for a host of 32-lane waves as the last try of a program that does not
/// translate otherwise (LDS halves apart, [`lower::with_lds_apart`]), and the wave size it asks
/// of the host (64: refused there).
fn lowered_wave(native: &NativeProgram) -> (HostShader, Option<u32>) {
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let st = structure::structure(&p).expect("structure");
    let res = resource::track(&p, &native.layout, native.stage, Some(&st)).expect("resource::track");
    let io = io::raise(&p, native).expect("io::raise");
    let was = lower::TEST_HOST_WAVE.replace(Some(32));
    let lowered = lower::with_lds_apart(|| lower::lower(&lower::Parts {
        native,
        p: &p,
        st: &st,
        io: &io,
        res: &res,
        inputs: Vec::new(),
        outputs: Vec::new(),
        prims: Vec::new(),
        mesh_stride: 0,
    }));
    lower::TEST_HOST_WAVE.set(was);
    let lowered = lowered.expect("lower");
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
    }
    (shader, lowered.wave_size)
}

/// The whole UAV of `dwords` dwords after `shader` ran once with root constants s0, s1; None
/// (and a line) without a device that takes it.
fn all_on_the_card(shader: &HostShader, constants: [u32; 2], dwords: u32) -> Option<Vec<u32>> {
    let _one = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let Ok(host) = Host::new(false) else {
        eprintln!("no SM 6.6 hardware device; not run");
        return None;
    };
    let (root, pso) = match host.compute_pipeline(shader) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("the hardware device does not take this program (waves of 32?): {e}; not run");
            return None;
        }
    };
    let mut ud = vec![0u32; shader.user_data_dwords().max(1) as usize];
    for u in &shader.user_data {
        if let (RootType::Constants, 0, d @ 0..=1) = (u.root, u.slot, u.dword) {
            ud[u.index as usize] = constants[d as usize];
        }
    }
    let out = host
        .dispatch(
            &root,
            &pso,
            &Dispatch {
                roots: vec![(0, Root::Constants(ud))],
                views: &[View::RawUav(dwords)],
                samplers: &[],
                groups: [1, 1, 1],
            },
        )
        .unwrap_or_else(|e| panic!("dispatch failed: {e}"));
    Some(out[0].clone())
}

/// A mip level made in LDS, as the title's transmission prefilter makes it
/// (`CS_AlphTransmissionPrefilterMip`): 256 threads (four console waves), all of the body
/// inside `if (t < s0)` (a bounds test). Each thread writes its texel 3t + 1 to LDS dword t,
/// the group meets at the title's `s_barrier`, and the first 64 threads each store the sum
/// of texels 4t..4t+3. The barrier stands in a block not every lane runs, so the halves of a
/// console wave cannot meet around each access there. `barrier` false: an `s_nop` instead.
fn mip_level(barrier: bool) -> NativeProgram {
    let mut w = vec![
        0x7D88_0000, // v_cmp_gt_u32 vcc, s0, v0: in bounds
        0xBE8C_246A, // s_and_saveexec_b64 s[12:13], vcc
        0,           // s_cbranch_execz end (filled in below)
        0x340A_0081, // v_lshlrev_b32 v5, 1, v0
        0x4A0A_0B00, // v_add_nc_u32 v5, v0, v5
        0x4A0A_0A81, // v_add_nc_u32 v5, 1, v5: the texel
        0x340C_0082, // v_lshlrev_b32 v6, 2, v0
    ];
    // (a quad swap, as the title's filter combines its quads: a lane read makes the program
    // one that needs the console's lanes; the sums over each quad stay the same)
    w.extend(ds(DS_SWIZZLE_B32, 5, 0, 0, 5, 0x80B1)); // ds_swizzle_b32 v5, v5 quad_perm:[1,0,3,2]
    w.extend(ds(DS_WRITE_B32, 6, 5, 0, 0, 0)); // ds_write_b32 v6, v5
    w.push(if barrier { 0xBF8A_0000 } else { 0xBF80_0000 }); // s_barrier, or s_nop
    w.push(0x7DA8_00C0); // v_cmpx_gt_u32 exec, 64, v0
    let skip = w.len();
    w.push(0); // s_cbranch_execz end (filled in below)
    w.push(0x340E_0084); // v_lshlrev_b32 v7, 4, v0
    w.extend(ds(DS_READ2_B32, 7, 0, 0, 8, 1 << 8)); // ds_read2_b32 v[8:9], v7 offset1:1
    w.extend(ds(DS_READ2_B32, 7, 0, 0, 10, 2 | 3 << 8)); // ds_read2_b32 v[10:11], v7 offset0:2 offset1:3
    w.extend_from_slice(&[
        0x4A10_1109, // v_add_nc_u32 v8, v9, v8
        0x4A10_110A, // v_add_nc_u32 v8, v10, v8
        0x4A10_110B, // v_add_nc_u32 v8, v11, v8
        0x340E_0082, // v_lshlrev_b32 v7, 2, v0
        0xE070_1000,
        0x8002_0807, // buffer_store_dword v8, v7, s[8:11], 0 offen
    ]);
    let end = w.len();
    w[2] = 0xBF88_0000 | (end - 3) as u32;
    w[skip] = 0xBF88_0000 | (end - skip - 1) as u32;
    w.push(0xBEFE_040C); // end: s_mov_b64 exec, s[12:13]
    let mut native = native_of(&w, true, 256);
    if let StageIo::Cs(cs) = &mut native.io {
        cs.rsrc2 |= 2 << 15; // 1 KiB of LDS
    }
    native
}

/// The region of the bounds test both writes and reads LDS, but the title's barrier lies
/// between the writes and the reads: the halves of a console wave are ordered by it, and
/// within each side of it they touch no dword the other half does. Runs in waves of 32,
/// exact against the console's sums; with threads out of bounds too (s0 = 200: whole host
/// waves of the last console wave skip the body).
#[test]
fn a_mip_level_in_lds_behind_a_bounds_test_runs_in_waves_of_32() {
    let native = mip_level(true);
    let (shader, wave) = lowered_wave(&native);
    assert_eq!(wave, Some(32), "refused for 32-lane waves");
    for s0 in [256u32, 200, 129] {
        let Some(out) = all_on_the_card(&shader, [s0, 0], 256) else {
            return;
        };
        // texels 4t..4t+3, all in bounds
        for t in 0..(s0 / 4).min(64) {
            let want: u32 = (0..4).map(|k| 3 * (4 * t + k) + 1).sum();
            assert_eq!(out[t as usize], want, "s0 = {s0}: thread {t}");
        }
        assert!(out[64..].iter().all(|&x| x == 0), "s0 = {s0}: only the first 64 threads store");
    }
    eprintln!("ran on the hardware device: a mip level through LDS behind a bounds test");
}

/// Without the barrier the first 64 threads read texels other threads wrote in the same
/// region: half 0 of console wave 0 reads what half 1 writes. Refused for 32-lane waves.
#[test]
fn a_mip_level_in_lds_without_its_barrier_is_refused() {
    let (_, wave) = lowered_wave(&mip_level(false));
    assert_eq!(wave, Some(64));
}

/// What the lanes of [`counters`] read in their region.
#[derive(Clone, Copy, PartialEq)]
enum Read {
    /// LDS dword 2, which only the first thread wrote before.
    Base,
    /// Dword 0, the counter the lanes add to in the same region.
    Counter,
    /// The dword at byte s1 + 8: its distance from the counters is not known.
    Scalar,
    /// Dword 2, with `ds_max_u32` on the slot counter beside the adds.
    Mixed,
}

/// Slots handed out through LDS counters, as the title's particle visibility program
/// (`GpuParticleUpdate:CS_Visibility`) does: one console wave of 64 threads, no barrier.
/// The group's first thread (64 * s1 + t == 0) zeroes two counters and writes the base 7;
/// each thread below s0 reads the base, takes a slot with `ds_add_rtn_u32` and adds its number
/// to the second counter, and stores base + slot; the first thread then reads both counters,
/// writes their sum, reads it back, and stores the three at dwords 64, 65, 66.
fn counters(read: Read) -> NativeProgram {
    let mut w = vec![
        0xD746_0001,
        0x0401_0C01, // v_lshl_add_u32 v1, s1, 6, v0: the thread's number in the dispatch
        0x7D84_0280, // v_cmp_eq_u32 vcc, 0, v1
        0xBE8C_246A, // s_and_saveexec_b64 s[12:13], vcc
        0,           // s_cbranch_execz (filled in below)
        0x7E04_0280, // v_mov_b32 v2, 0
        0x7E06_0287, // v_mov_b32 v3, 7
    ];
    w.extend(ds(DS_WRITE2_B32, 2, 2, 2, 0, 1 << 8)); // ds_write2_b32 v2, v2, v2 offset1:1
    w.extend(ds(DS_WRITE_B32, 2, 3, 0, 0, 8)); // ds_write_b32 v2, v3 offset:8
    w[4] = 0xBF88_0000 | (w.len() - 5) as u32;
    w.extend_from_slice(&[
        0xBEFE_040C, // s_mov_b64 exec, s[12:13]
        0x7D88_0000, // v_cmp_gt_u32 vcc, s0, v0
        0xBE8E_246A, // s_and_saveexec_b64 s[14:15], vcc
    ]);
    let skip = w.len();
    w.extend_from_slice(&[
        0,           // s_cbranch_execz (filled in below)
        0x7E04_0280, // v_mov_b32 v2, 0
        0x7E08_0281, // v_mov_b32 v4, 1
        0x7E18_0201, // v_mov_b32 v12, s1
    ]);
    w.extend(match read {
        Read::Base | Read::Mixed => ds(DS_READ_B32, 2, 0, 0, 5, 8), // ds_read_b32 v5, v2 offset:8
        Read::Counter => ds(DS_READ_B32, 2, 0, 0, 5, 0),            // ds_read_b32 v5, v2
        Read::Scalar => ds(DS_READ_B32, 12, 0, 0, 5, 8),            // ds_read_b32 v5, v12 offset:8
    });
    w.extend(ds(DS_ADD_RTN_U32, 2, 4, 0, 6, 0)); // ds_add_rtn_u32 v6, v2, v4: the slot
    w.extend(if read == Read::Mixed {
        ds(DS_MAX_U32, 2, 0, 0, 0, 0) // ds_max_u32 v2, v0
    } else {
        ds(DS_ADD_U32, 2, 0, 0, 0, 4) // ds_add_u32 v2, v0 offset:4
    });
    w.extend_from_slice(&[
        0x4A0C_0D05, // v_add_nc_u32 v6, v5, v6
        0x340E_0082, // v_lshlrev_b32 v7, 2, v0
        0xE070_1000,
        0x8002_0607, // buffer_store_dword v6, v7, s[8:11], 0 offen
    ]);
    w[skip] = 0xBF88_0000 | (w.len() - skip - 1) as u32;
    w.extend_from_slice(&[
        0xBEFE_040E, // s_mov_b64 exec, s[14:15]
        0x7D84_0280, // v_cmp_eq_u32 vcc, 0, v1
        0xBE8C_246A, // s_and_saveexec_b64 s[12:13], vcc
    ]);
    let skip = w.len();
    w.extend_from_slice(&[0, 0x7E04_0280]); // s_cbranch_execz (filled in below); v_mov_b32 v2, 0
    w.extend(ds(DS_READ2_B32, 2, 0, 0, 8, 1 << 8)); // ds_read2_b32 v[8:9], v2 offset1:1
    w.push(0x4A14_1308); // v_add_nc_u32 v10, v8, v9
    w.extend(ds(DS_WRITE_B32, 2, 10, 0, 0, 12)); // ds_write_b32 v2, v10 offset:12
    w.extend(ds(DS_READ_B32, 2, 0, 0, 11, 12)); // ds_read_b32 v11, v2 offset:12
    w.extend_from_slice(&[
        0xE070_1100,
        0x8002_0802, // buffer_store_dword v8, v2, s[8:11], 0 offen offset:256
        0xE070_1104,
        0x8002_0902, // buffer_store_dword v9, v2, s[8:11], 0 offen offset:260
        0xE070_1108,
        0x8002_0B02, // buffer_store_dword v11, v2, s[8:11], 0 offen offset:264
    ]);
    w[skip] = 0xBF88_0000 | (w.len() - skip - 1) as u32;
    w.push(0xBEFE_040C); // s_mov_b64 exec, s[12:13]
    let mut native = native_of(&w, true, 64);
    if let StageIo::Cs(cs) = &mut native.io {
        cs.rsrc2 |= 1 << 15; // 512 bytes of LDS
    }
    native
}

/// The lanes' region reads only the base, which no one writes there, and adds to the counters
/// only with commuting atomics; the first thread's region (64 * s1 + t is 0 for no other
/// thread, whatever s1 is) is in one half. Runs in waves of 32: every thread below s0 gets a
/// slot of its own (the order may differ from the console's lane order, as it may between
/// console waves), and the counters are the console's.
#[test]
fn slots_from_lds_counters_in_one_console_wave_run_in_waves_of_32() {
    let (shader, wave) = lowered_wave(&counters(Read::Base));
    assert_eq!(wave, Some(32), "refused for 32-lane waves");
    let mut ran = false;
    for below in [64u32, 63, 40, 33, 32, 17, 1] {
        let Some(out) = all_on_the_card(&shader, [below, 0], 67) else {
            return;
        };
        ran = true;
        let mut slots: Vec<u32> = out[..below as usize].to_vec();
        slots.sort_unstable();
        assert_eq!(slots, (7..7 + below).collect::<Vec<_>>(), "{below} threads: a slot each");
        assert!(out[below as usize..64].iter().all(|&x| x == 0), "{below} threads: only they store");
        let sum: u32 = (0..below).sum();
        assert_eq!(out[64..67], [below, sum, below + sum], "{below} threads: the counters");
    }
    if ran {
        eprintln!("ran on the hardware device: a slot each and the console's counters");
    }
}

/// A plain read of the counter the other half adds to in the same region, an address whose
/// distance from the counters is a scalar, and two kinds of atomics on one dword: each needs
/// the console's order of the halves. Refused for 32-lane waves.
#[test]
fn lds_counters_the_halves_share_otherwise_are_refused() {
    for read in [Read::Counter, Read::Scalar, Read::Mixed] {
        let (_, wave) = lowered_wave(&counters(read));
        assert_eq!(wave, Some(64), "read {}", read as u8);
    }
}
/// A strided loop over LDS records inside `if (t < s0)`, as the title's decal trail packing
/// (`vfx_proj_decal_trail:CS_PackProjDecalTrailVertexBuffer`) walks them: one console wave of
/// 64 threads, no barrier. LDS dword 2 + i holds record i's flag (i & 1, written before by
/// every thread); in two rounds thread t takes records i = t and t + 64. A record whose flag
/// is 0 stores 1000 to UAV dword i (and the arm then reuses its address register); one whose
/// flag is set takes a slot from the counter at dword 0 and writes 100 + slot over its flag,
/// at the address made before the branch. After the region every thread stores its records'
/// LDS dwords at UAV dwords 256 + i. `step`: the records' stride, 64 (or 32: thread t then
/// takes t and t + 32, which the other half takes too).
fn strided_records(step: u32) -> NativeProgram {
    let mut w = vec![
        0x3610_0081, // v_and_b32 v8, 1, v0
        0x3412_0082, // v_lshlrev_b32 v9, 2, v0
    ];
    w.extend(ds(DS_WRITE2_B32, 9, 8, 8, 0, 2 | 66 << 8)); // ds_write2_b32 v9, v8, v8 offset0:2 offset1:66
    w.push(0x7E14_0280); // v_mov_b32 v10, 0
    w.extend(ds(DS_WRITE_B32, 10, 10, 0, 0, 0)); // ds_write_b32 v10, v10: the counter
    w.extend_from_slice(&[
        0x7D88_0000, // v_cmp_gt_u32 vcc, s0, v0
        0xBE8C_246A, // s_and_saveexec_b64 s[12:13], vcc
    ]);
    let skip = w.len();
    w.extend_from_slice(&[
        0,           // s_cbranch_execz end (filled in below)
        0xBE94_0380, // s_mov_b32 s20, 0: the round
        0x7E02_0300, // v_mov_b32 v1, v0: the record
    ]);
    let top = w.len();
    w.push(0x3404_0282); // loop: v_lshlrev_b32 v2, 2, v1
    w.extend(ds(DS_READ_B32, 2, 0, 0, 3, 8)); // ds_read_b32 v3, v2 offset:8: the flag
    w.extend_from_slice(&[
        0x7D84_0680, // v_cmp_eq_u32 vcc, 0, v3
        0xBE90_246A, // s_and_saveexec_b64 s[16:17], vcc
    ]);
    let then = w.len();
    w.extend_from_slice(&[
        0, // s_cbranch_execz else (filled in below)
        0x7E08_02FF,
        1000, // v_mov_b32 v4, 1000
        0xE070_1000,
        0x8002_0402, // buffer_store_dword v4, v2, s[8:11], 0 offen
        0x7E04_0280, // v_mov_b32 v2, 0
    ]);
    w[then] = 0xBF88_0000 | (w.len() - then - 1) as u32;
    w.push(0x8AFE_7E10); // else: s_andn2_b64 exec, s[16:17], exec
    let other = w.len();
    w.extend_from_slice(&[
        0,           // s_cbranch_execz join (filled in below)
        0x7E0A_0280, // v_mov_b32 v5, 0
        0x7E0C_0281, // v_mov_b32 v6, 1
    ]);
    w.extend(ds(DS_ADD_RTN_U32, 5, 6, 0, 7, 0)); // ds_add_rtn_u32 v7, v5, v6: the slot
    w.extend_from_slice(&[0x4A0E_0EFF, 100]); // v_add_nc_u32 v7, 100, v7
    w.extend(ds(DS_WRITE_B32, 2, 7, 0, 0, 8)); // ds_write_b32 v2, v7 offset:8
    w[other] = 0xBF88_0000 | (w.len() - other - 1) as u32;
    w.extend_from_slice(&[
        0xBEFE_0410, // join: s_mov_b64 exec, s[16:17]
        0x4A02_0280 | (128 + step), // v_add_nc_u32 v1, step, v1
        0x8014_8114, // s_add_u32 s20, s20, 1
        0xBF0A_8214, // s_cmp_lt_u32 s20, 2
    ]);
    let back = w.len();
    w.push(0xBF85_0000 | (top as i32 - back as i32 - 1) as u16 as u32); // s_cbranch_scc1 loop
    w[skip] = 0xBF88_0000 | (w.len() - skip - 1) as u32;
    w.push(0xBEFE_040C); // end: s_mov_b64 exec, s[12:13]
    w.extend(ds(DS_READ2_B32, 9, 0, 0, 11, 2 | 66 << 8)); // ds_read2_b32 v[11:12], v9 offset0:2 offset1:66
    w.extend_from_slice(&[
        0xE070_1400,
        0x8002_0B09, // buffer_store_dword v11, v9, s[8:11], 0 offen offset:1024
        0xE070_1500,
        0x8002_0C09, // buffer_store_dword v12, v9, s[8:11], 0 offen offset:1280
    ]);
    let mut native = native_of(&w, true, 64);
    if let StageIo::Cs(cs) = &mut native.io {
        cs.rsrc2 |= 2 << 15; // 1 KiB of LDS
    }
    native
}

/// Each half touches only its own records (i = t + 64 k, the round k each thread's own), and
/// the counter only through commuting atomics; the flag read and the record write lie apart
/// from the counter whatever the round (the rounds are below the loop limit). The record's
/// address in the arm that takes a slot is the one made before the branch: the lanes there
/// did not run the other arm, which overwrote it. Runs in waves of 32, exact against the
/// console: a distinct slot per flagged record, the others untouched.
#[test]
fn records_walked_in_rounds_behind_a_bounds_test_run_in_waves_of_32() {
    let (shader, wave) = lowered_wave(&strided_records(64));
    assert_eq!(wave, Some(32), "refused for 32-lane waves");
    let mut ran = false;
    for s0 in [64u32, 63, 40, 33, 32, 31, 1] {
        let Some(out) = all_on_the_card(&shader, [s0, 0], 384) else {
            return;
        };
        ran = true;
        let mut slots = Vec::new();
        for i in 0..128u32 {
            let taken = i % 64 < s0;
            let flag = i & 1;
            assert_eq!(out[i as usize], if taken && flag == 0 { 1000 } else { 0 }, "s0 = {s0}: record {i}'s store");
            let last = out[256 + i as usize];
            if taken && flag == 1 {
                slots.push(last.wrapping_sub(100));
            } else {
                assert_eq!(last, flag, "s0 = {s0}: record {i} untouched");
            }
        }
        slots.sort_unstable();
        assert_eq!(slots, (0..slots.len() as u32).collect::<Vec<_>>(), "s0 = {s0}: a slot each");
    }
    if ran {
        eprintln!("ran on the hardware device: records in rounds, a slot each");
    }
}
/// With a stride of 32 a thread's second record is the first one of a thread in the other
/// half: both halves read and write it in the region. Refused for 32-lane waves.
#[test]
fn records_the_halves_share_across_rounds_are_refused() {
    let (_, wave) = lowered_wave(&strided_records(32));
    assert_eq!(wave, Some(64));
}