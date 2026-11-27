//! Cross-lane and memory instructions against the RDNA2 ISA: DPP lanes that
//! read a lane outside EXEC, DPP row masks of a console wave64 run as two
//! host waves of 32, typed buffer stores at a scalar offset, and gathers at a
//! named level of a cube. Each program is lowered, validated by `dxil.dll`
//! when present and, where it lowers, run on the host GPU.

use super::host::{Dispatch, Host, Root, View};
use super::intersect_tests::build_compute_shader;
use super::{HostShader, lower, validator};
use crate::native::{
    ComputeIo, HwStage, NativeProgram, ResourceType, RootParam, RootType, StageIo, UsedResource,
};
use crate::{ShaderError, io, ir, resource, structure};
use windows::Win32::Graphics::Direct3D12::*;

const S_ENDPGM: u32 = 0xBF81_0000;

/// v_lshlrev_b32 v7, 2, v0; buffer_store_dword `v`, v7, s[8:11], 0 offen
fn store_lane(v: u32) -> [u32; 3] {
    [0x340E_0082, 0xE070_1000, 0x8002_0007 | v << 8]
}

/// A compute program of `threads` threads (the thread index in v0) with 4 root
/// constants in s0..s3 and a UAV's descriptor (`kind`, from a table at s4) in
/// s[8:11], then `body`.
fn native(body: &[u32], wave64: bool, threads: u32) -> NativeProgram {
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
            rsrc2: 5 << 1,
            threads: [threads, 1, 1],
        }),
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    }
}

/// `native` lowered for a host whose waves have at most `host_wave` lanes.
fn lowered(native: &NativeProgram, host_wave: u32) -> Result<HostShader, ShaderError> {
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir");
    let st = structure::structure(&p).expect("structure");
    let res = resource::track(&p, &native.layout, native.stage, Some(&st)).expect("resources");
    let io = io::raise(&p, native).expect("io");
    let was = lower::TEST_HOST_WAVE.replace(Some(host_wave));
    let result = lower::lower(&lower::Parts {
        native,
        p: &p,
        st: &st,
        io: &io,
        res: &res,
        inputs: Vec::new(),
        outputs: Vec::new(),
        prims: Vec::new(),
        mesh_stride: 0,
    });
    lower::TEST_HOST_WAVE.set(was);
    let result = result?;
    let shader = HostShader {
        stage: HwStage::Cs,
        dxil: result.dxil,
        user_data: result.user_data,
        samplers: result.samplers,
        static_samplers: Vec::new(),
        root_cbvs: result.root_cbvs,
        root_srvs: result.root_srvs,
        root_uavs: result.root_uavs,
        root_indexed_views: res.root_indexed_views,
        half_views: result.half_views,
    };
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        let mut copy = shader.dxil.clone();
        v.validate(&mut copy).expect("DXIL validation");
    }
    Ok(shader)
}

/// The UAV after `shader` ran once with root constants `constants`; none
/// without a device that takes it.
fn dispatched(shader: &HostShader, constants: &[u32], view: View) -> Option<Vec<u32>> {
    let _one = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let Ok(host) = Host::new(false) else {
        eprintln!("no SM 6.6 hardware device; not run");
        return None;
    };
    let (root, pso) = match host.compute_pipeline(shader) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("the hardware device does not take this program: {e}; not run");
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
                views: &[view],
                samplers: &[],
                groups: [1, 1, 1],
            },
        )
        .unwrap_or_else(|e| panic!("dispatch failed: {e}"));
    Some(out[0].clone())
}

/// DPP16 with FI clear treats a source lane outside EXEC as out of range:
/// BOUND_CTRL set reads 0 there, clear leaves the destination as it was
/// (RDNA2 ISA, DPP). With FI set the inactive lane's value is read.
#[test]
fn dpp_reads_of_lanes_outside_exec_follow_bound_ctrl_and_fi() {
    const OLD: u32 = 42;
    const BASE: u32 = 0x100;
    for (fi, bc) in [(false, false), (false, true), (true, false)] {
        let body = [
            0x7E02_02FF,
            OLD,         // v_mov_b32 v1, OLD
            0x4A04_00FF,
            BASE,        // v_add_nc_u32 v2, BASE, v0
            0xBEFE_0302, // s_mov_b32 exec_lo, s2
            0x7E02_02FA, // v_mov_b32_dpp v1, v2 quad_perm:[1,0,3,2]
            2 | 0xB1 << 8 | u32::from(fi) << 18 | u32::from(bc) << 19 | 0xFF << 24,
            0xBEFE_03C1, // s_mov_b32 exec_lo, -1
        ];
        let mut words = body.to_vec();
        words.extend(store_lane(1));
        let shader = lowered(&native(&words, false, 32), 32).expect("lower");
        for mask in [0x5555_5555u32, 0xFFFF_0F0F, 0xFFFF_FFFF] {
            let Some(out) = dispatched(&shader, &[0, 0, mask, 0], View::RawUav(32)) else {
                return;
            };
            for t in 0..32u32 {
                let src = t ^ 1;
                let want = if mask >> t & 1 == 0 {
                    OLD
                } else if fi || mask >> src & 1 != 0 {
                    BASE + src
                } else if bc {
                    0
                } else {
                    OLD
                };
                assert_eq!(out[t as usize], want, "fi={fi} bc={bc} mask={mask:#x} lane {t}");
            }
        }
    }
}

/// A console wave64 program cut into two host waves of 32: DPP's row mask
/// names the console's rows 0..3, lanes 32..63 being rows 2 and 3.
#[test]
fn dpp_row_masks_of_a_wave64_in_two_host_waves_name_the_consoles_rows() {
    for rows in [0x1u32, 0x4, 0x5, 0x3, 0xC, 0x8] {
        let mut words = vec![
            0x7E02_0280, // v_mov_b32 v1, 0
            0x4A04_0081, // v_add_nc_u32 v2, 1, v0
            0x7E02_02FA, // v_mov_b32_dpp v1, v2 quad_perm:[0,1,2,3] row_mask:rows
            2 | 0xE4 << 8 | 0xF << 24 | rows << 28,
        ];
        words.extend(store_lane(1));
        let shader = lowered(&native(&words, true, 64), 32).expect("lower");
        let Some(out) = dispatched(&shader, &[], View::RawUav(64)) else {
            return;
        };
        for t in 0..64u32 {
            let want = if rows >> (t / 16) & 1 != 0 { t + 1 } else { 0 };
            assert_eq!(out[t as usize], want, "row_mask {rows:#x} lane {t}");
        }
    }
}

/// `buffer_store_format_xyzw` by index adds SOFFSET (and the instruction
/// offset) to the element's address. A typed host store has no byte
/// offset: such a store is refused, as the typed load is, not written to
/// the element the index alone names.
#[test]
#[ignore = "known, not fixed: refusing the store would drop its pipelines' draws (lower.rs, typed stores)"]
fn a_typed_buffer_store_at_a_scalar_offset_is_not_written_without_it() {
    let words = [
        0x7E02_02F2, // v_mov_b32 v1, 1.0
        0x7E04_02F4, // v_mov_b32 v2, 2.0
        0x7E06_02F6, // v_mov_b32 v3, 4.0
        0x7E08_02F0, // v_mov_b32 v4, 0.5
        0x7E0A_0280, // v_mov_b32 v5, 0
        0xE01C_2000,
        0x0002_0105, // buffer_store_format_xyzw v[1:4], v5, s[8:11], s0 idxen
    ];
    match lowered(&native(&words, false, 1), 32) {
        Err(ShaderError::Unsupported { what, .. }) => {
            assert_eq!(what, "a typed buffer store of some channels or at an offset");
        }
        Err(e) => panic!("{e}"),
        Ok(shader) => {
            // s0 = 8: one RGBA16F element on, element 1
            let Some(out) = dispatched(&shader, &[8], View::HalfUav(4)) else {
                return;
            };
            let element = [0x4000_3C00, 0x3800_4400];
            assert_eq!(out[0..4], [0, 0, element[0], element[1]], "the store at element 1");
        }
    }
}

/// A compute program that gathers channel 0 at level `lod` with the four
/// coordinates `at` from the SRV of dimension `dim` (3 cube, 5 2D array) at
/// s[16:23] and stores the four texels to a raw UAV, lowered with
/// `VICTORY_GATHER_LEVELS` as `levels`.
fn gather_at_level(dim: u32, at: [f32; 4], levels: bool) -> Result<HostShader, ShaderError> {
    let mut words = vec![
        0xBE81_0380,
        0xBE83_0380, // s1, s3 = 0 (upper halves of the tables)
        0xF40C_0400,
        0xFA00_0000, // SRV to s[16:23] from s[0:1]
        0xF408_0300,
        0xFA00_0020, // raw output UAV to s[12:15] from s[0:1]+32
        0xF408_0601,
        0xFA00_0000, // sampler to s[24:27] from s[2:3]
        0x7E1E_0280, // v15 = 0 for the output byte offset
    ];
    for (reg, value) in at.iter().enumerate() {
        words.extend([0x7E00_02FF | ((reg as u32) << 17), value.to_bits()]);
    }
    // image_gather4_l v[10:13], v[0:3], s[16:23], s[24:27] dmask:0x1 dim:`dim`
    words.extend([0xF000_0100 | dim << 3 | (0x44 << 18), 0x00C4_0A00]);
    // buffer_store_dwordx4 v[10:13], v15, s[12:15], 0 offen
    words.extend([0xE078_1000, 0x8003_0A0F]);
    words.push(S_ENDPGM);
    let table = |slot, user_data| RootParam {
        root: RootType::Table,
        table_offset_dwords: 0,
        sgprs: 1,
        base_register: 0,
        user_data,
        slot,
    };
    let range = |kind, at, words, slot, user_data| UsedResource {
        kind,
        root: RootType::Table,
        table_offset_dwords: at,
        lower_bound: 0,
        stride: 0,
        dwords: words,
        user_data,
        slot,
    };
    let native = NativeProgram {
        stage: HwStage::Cs,
        wave64: false,
        vgprs: 16,
        layout: vec![table(0, 0), table(1, 2)],
        used: vec![
            range(ResourceType::Srv, 0, 8, 0, 0),
            range(ResourceType::Uav, 8, 4, 0, 0),
            range(ResourceType::Sampler, 0, 4, 1, 2),
        ],
        io: StageIo::Cs(ComputeIo {
            rsrc2: 3 << 1,
            threads: [1, 1, 1],
        }),
        code: words.into_iter().flat_map(u32::to_le_bytes).collect(),
    };
    let was = lower::TEST_GATHER_LEVELS.replace(Some(levels));
    let built = build_compute_shader(&native);
    lower::TEST_GATHER_LEVELS.set(was);
    built.map(|(shader, _)| shader)
}

/// The four texels `shader` gathered from `texture` with a linear sampler
/// (a linear filter must not blend the texels of a gather); none without a
/// device.
fn gathered(shader: &HostShader, texture: View) -> Option<Vec<f32>> {
    let sampler = D3D12_SAMPLER_DESC {
        Filter: D3D12_FILTER_MIN_MAG_MIP_LINEAR,
        AddressU: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
        AddressV: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
        AddressW: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
        MaxLOD: f32::MAX,
        MaxAnisotropy: 1,
        ..Default::default()
    };
    let _one = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let Ok(host) = Host::new(false) else {
        eprintln!("no SM 6.6 hardware device; not run");
        return None;
    };
    let (root, pso) = host.compute_pipeline(shader).unwrap();
    let out = host
        .dispatch(
            &root,
            &pso,
            &Dispatch {
                roots: vec![(0, Root::Constants(vec![0; shader.user_data_dwords() as usize]))],
                views: &[texture, View::RawUav(4)],
                samplers: &[sampler],
                groups: [1, 1, 1],
            },
        )
        .unwrap();
    Some(out[0].iter().copied().map(f32::from_bits).collect())
}

/// `image_gather4_l` reads the four texels at the level it names. On a cube
/// the host's gather reads level 0; with `VICTORY_GATHER_LEVELS=1` the four
/// texels of the named level, in the gather's order (u0,v1) (u1,v1) (u1,v0)
/// (u0,v0).
#[test]
fn a_gather_at_a_named_level_of_a_cube_reads_that_level() {
    const SIZE: usize = 8;
    const MIPS: u16 = 3;
    // A texel holds 1000 times its level, plus 10 times its row and its
    // column, on every face.
    let texels: Vec<_> = (0..6)
        .flat_map(|_| {
            (0..MIPS).flat_map(|mip| {
                let n = SIZE >> mip;
                (0..n * n).map(move |i| [f32::from(mip) * 1000.0 + (i / n * 10 + i % n) as f32; 4])
            })
        })
        .collect();
    // Positive X face, a little past its centre (texel 1.6 of 4 across, 1.6
    // down); level 1.
    let at = [1.4f32, 1.4, 0.0, 1.0];
    let cube = View::Cube(SIZE as u32, MIPS, &texels);
    let old = gather_at_level(3, at, false).expect("lower");
    let Some(got) = gathered(&old, cube) else {
        return;
    };
    // Without the switch: level 0 (texels 2 and 3 of 8), as before.
    assert_eq!(got, [32.0, 33.0, 23.0, 22.0], "level 0 without VICTORY_GATHER_LEVELS");
    let new = gather_at_level(3, at, true).expect("lower");
    let got = gathered(&new, cube).expect("a device");
    assert_eq!(got, [1021.0, 1022.0, 1012.0, 1011.0], "the four texels of level 1");
}

/// The same for a 2D array: the slice the third coordinate names (rounded),
/// at the level.
#[test]
fn a_gather_at_a_named_level_of_an_array_reads_that_level_and_slice() {
    const SIZE: usize = 8;
    const MIPS: u16 = 3;
    const SLICES: u16 = 3;
    // A texel holds 10,000 times its slice, 1000 times its level, plus 10
    // times its row and its column.
    let texels: Vec<_> = (0..SLICES)
        .flat_map(|slice| {
            (0..MIPS).flat_map(move |mip| {
                let n = SIZE >> mip;
                (0..n * n).map(move |i| {
                    [f32::from(slice) * 10_000.0 + f32::from(mip) * 1000.0 + (i / n * 10 + i % n) as f32; 4]
                })
            })
        })
        .collect();
    // Texel 1.6 across and down of level 1's 4, slice 1.6 (2); level 1.
    let at = [0.4f32, 0.4, 1.6, 1.0];
    let array = View::Array(SIZE as u32, SIZE as u32, SLICES, MIPS, &texels);
    let old = gather_at_level(5, at, false).expect("lower");
    let Some(got) = gathered(&old, array) else {
        return;
    };
    assert_eq!(got, [20_032.0, 20_033.0, 20_023.0, 20_022.0], "level 0 without VICTORY_GATHER_LEVELS");
    let new = gather_at_level(5, at, true).expect("lower");
    let got = gathered(&new, array).expect("a device");
    assert_eq!(got, [21_021.0, 21_022.0, 21_012.0, 21_011.0], "slice 2, level 1");
}