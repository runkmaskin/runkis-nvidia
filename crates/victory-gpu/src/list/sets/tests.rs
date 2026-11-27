//! The sets left out change nothing a draw sees: a model of what a D3D12
//! list holds, given every set (as before) and given only those
//! [`HostSets`] lets through, is the same at every draw of fixed-seed call
//! streams; on the host GPU the same streams draw the same picture, byte
//! for byte. `bench_redundant_sets` (ignored; `cargo test --release -p
//! victory-gpu --lib -- --ignored --nocapture bench_redundant_sets`) times
//! a frame's worth of draws recorded both ways, and each kind of call.

use super::*;
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::ID3DBlob;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::System::Threading::{CreateEventW, INFINITE, WaitForSingleObject};
use windows::core::{Interface, PCSTR, s};

/// A fixed-seed generator (xorshift64*).
struct Seeded(u64);

impl Seeded {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    /// True `percent` times in a hundred.
    fn odds(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }
}

/// A call on a host list, as the draw code makes them.
#[derive(Clone, Debug)]
enum Call {
    /// A side's root signature (`State::bind` when the pipeline's differs).
    RootSignature(bool),
    Vertex(Vec<D3D12_VERTEX_BUFFER_VIEW>),
    /// One slot (`State::rectangles`).
    VertexAt(u32, D3D12_VERTEX_BUFFER_VIEW),
    Index(D3D12_INDEX_BUFFER_VIEW),
    Constants(bool, u32, Vec<u32>),
    View(bool, u32, RootView, u64),
    Table(bool, u32, u64),
    /// A word of a root parameter set by other code (`State::dispatch`'s
    /// thread count).
    Word(bool, u32, u32),
    /// A new host list (a cut, a reset).
    Fresh,
    Draw,
}

/// A root argument as the model holds it.
#[derive(Clone, Debug, PartialEq)]
enum Arg {
    Constants(Vec<u32>),
    Value(u8, u64),
}

/// What a D3D12 list holds.
#[derive(Clone, Debug, PartialEq, Default)]
struct Model {
    vertex: [Option<Vertex>; 32],
    index: Option<(u64, u32, i32)>,
    roots: [[Option<Arg>; 8]; 2],
}

impl Model {
    fn apply(&mut self, call: &Call) {
        match call {
            Call::RootSignature(c) => self.roots[usize::from(*c)] = Default::default(),
            Call::Vertex(views) => {
                for (h, v) in self.vertex.iter_mut().zip(views) {
                    *h = Some(vertex(v));
                }
            }
            Call::VertexAt(slot, v) => self.vertex[*slot as usize] = Some(vertex(v)),
            Call::Index(v) => self.index = Some((v.BufferLocation, v.SizeInBytes, v.Format.0)),
            Call::Constants(c, p, values) => self.roots[usize::from(*c)][*p as usize] = Some(Arg::Constants(values.clone())),
            Call::View(c, p, kind, gpu) => self.roots[usize::from(*c)][*p as usize] = Some(Arg::Value(*kind as u8, *gpu)),
            Call::Table(c, p, handle) => self.roots[usize::from(*c)][*p as usize] = Some(Arg::Value(3, *handle)),
            Call::Word(c, p, value) => {
                let slot = &mut self.roots[usize::from(*c)][*p as usize];
                let mut words = match slot.take() {
                    Some(Arg::Constants(w)) => w,
                    _ => vec![0; 4],
                };
                words[0] = *value;
                *slot = Some(Arg::Constants(words));
            }
            Call::Fresh => *self = Model::default(),
            Call::Draw => {}
        }
    }
}

/// Whether `sets` lets `call` through to the list, as the draw code asks
/// it (and tells it of the calls it makes anyway).
fn through(sets: &mut HostSets, call: &Call) -> bool {
    match call {
        Call::RootSignature(c) => {
            sets.root_signature_set(*c);
            true
        }
        Call::Vertex(views) => sets.want_vertex(views),
        Call::VertexAt(slot, v) => {
            sets.note_vertex_at(*slot, v);
            true
        }
        Call::Index(v) => sets.want_index(v),
        Call::Constants(c, p, values) => sets.want_constants(*c, *p, values),
        Call::View(c, p, kind, gpu) => sets.want_view(*c, *p, *kind, *gpu),
        Call::Table(c, p, handle) => sets.want_table(*c, *p, *handle),
        Call::Word(c, p, _) => {
            sets.forget_root(*c, *p);
            true
        }
        Call::Fresh => {
            sets.forget();
            true
        }
        Call::Draw => true,
    }
}

fn view(at: u64, size: u32, stride: u32) -> D3D12_VERTEX_BUFFER_VIEW {
    D3D12_VERTEX_BUFFER_VIEW { BufferLocation: at, SizeInBytes: size, StrideInBytes: stride }
}

/// A stream of `draws` draws shaped like a frame's: state that mostly
/// stays from draw to draw, pipelines (root signatures) changing now and
/// then, a cut or two.
fn stream(seed: u64, draws: usize) -> Vec<Call> {
    let mut r = Seeded(seed);
    let mut calls = Vec::new();
    let vertex_pool: Vec<Vec<D3D12_VERTEX_BUFFER_VIEW>> = (0..6)
        .map(|i| (0..1 + i % 3).map(|k| view(0x1000_0000 + i * 0x10000 + k * 0x100, 4096, 16 + 4 * k as u32)).collect())
        .collect();
    let index_pool: Vec<D3D12_INDEX_BUFFER_VIEW> = (0..4)
        .map(|i| D3D12_INDEX_BUFFER_VIEW {
            BufferLocation: 0x2000_0000 + i * 0x1000,
            SizeInBytes: 1024 + i as u32,
            Format: if i % 2 == 0 { DXGI_FORMAT_R16_UINT } else { DXGI_FORMAT_R32_UINT },
        })
        .collect();
    let mut constants = [[0u32; 4]; 2];
    for _ in 0..draws {
        let compute = r.odds(15);
        if r.odds(1) {
            calls.push(Call::Fresh);
        }
        if r.odds(8) {
            calls.push(Call::RootSignature(compute));
        }
        if !compute {
            if r.odds(30) {
                calls.push(Call::Vertex(vertex_pool[r.below(6) as usize].clone()));
            } else {
                // The same as the draw before (the code sets them every draw).
                let last = calls.iter().rev().find_map(|c| match c {
                    Call::Vertex(v) => Some(v.clone()),
                    _ => None,
                });
                calls.push(Call::Vertex(last.unwrap_or_else(|| vertex_pool[0].clone())));
            }
            if r.odds(3) {
                calls.push(Call::VertexAt(r.below(3) as u32, view(0x3000_0000 + r.below(4) * 0x100, 512, 24)));
            }
            calls.push(Call::Index(index_pool[if r.odds(25) { r.below(4) as usize } else { 0 }]));
        }
        // User data of each stage: a word or two change now and then.
        for param in [0u32, 1] {
            let side = &mut constants[usize::from(compute)];
            if r.odds(35) {
                side[r.below(4) as usize] = r.below(3) as u32;
            }
            calls.push(Call::Constants(compute, param, side.to_vec()));
        }
        calls.push(Call::View(compute, 2, RootView::Constants, 0x4000_0000 + r.below(3) * 256));
        calls.push(Call::View(compute, 3, if r.odds(50) { RootView::Shader } else { RootView::Unordered }, 0x5000_0000 + r.below(2) * 256));
        calls.push(Call::Table(compute, 4 + r.below(2) as u32, 0x6000_0000 + r.below(3) * 32));
        if compute && r.odds(10) {
            calls.push(Call::Word(true, 0, r.below(5) as u32));
        }
        calls.push(Call::Draw);
    }
    calls
}

/// Every set and only the needed ones leave the list holding the same at
/// every draw (fixed seeds), and many are left out.
#[test]
fn left_out_sets_change_nothing_a_draw_sees() {
    for seed in [1u64, 7, 42, 1234, 0xdead_beef] {
        let calls = stream(seed, 7000);
        let (mut every, mut needed) = (Model::default(), Model::default());
        let mut sets = HostSets::enabled();
        let (mut made, mut left) = (0usize, 0usize);
        let mut seen = (blake3::Hasher::new(), blake3::Hasher::new());
        for call in &calls {
            every.apply(call);
            if through(&mut sets, call) {
                needed.apply(call);
                made += 1;
            } else {
                left += 1;
            }
            if let Call::Draw = call {
                assert_eq!(every, needed, "seed {seed}");
                seen.0.update(format!("{every:?}").as_bytes());
                seen.1.update(format!("{needed:?}").as_bytes());
            }
        }
        assert_eq!(seen.0.finalize(), seen.1.finalize());
        assert!(left > made / 4, "seed {seed}: {left} left out of {}", made + left);
    }
}

/// Off (the default), every set of a stream is made, as before.
#[test]
fn off_every_set_is_made() {
    if on() {
        return; // VICTORY_SET_CACHE=1
    }
    let mut sets = HostSets::default();
    for call in stream(11, 2000) {
        assert!(through(&mut sets, &call), "{call:?}");
    }
    assert_eq!(sets.counts, [[0; KINDS]; 2]);
}

/// A set is left out only when the list holds the same: after a root
/// signature, a new list or a word set elsewhere it is made again.
#[test]
fn sets_are_made_again_after_what_clears_them() {
    let mut s = HostSets::enabled();
    assert!(s.want_constants(false, 0, &[1, 2]));
    assert!(!s.want_constants(false, 0, &[1, 2]));
    assert!(s.want_constants(false, 0, &[1, 3]));
    // The other side is its own.
    assert!(s.want_constants(true, 0, &[1, 3]));
    s.root_signature_set(false);
    assert!(s.want_constants(false, 0, &[1, 3]));
    assert!(!s.want_constants(true, 0, &[1, 3]));
    s.forget_root(true, 0);
    assert!(s.want_constants(true, 0, &[1, 3]));
    // A view and a table at one parameter are not the same.
    assert!(s.want_view(false, 1, RootView::Constants, 256));
    assert!(s.want_view(false, 1, RootView::Shader, 256));
    assert!(s.want_table(false, 1, 256));
    assert!(!s.want_table(false, 1, 256));
    // Fewer vertex buffers than the list holds: held if those are.
    let views = [view(1, 2, 3), view(4, 5, 6)];
    assert!(s.want_vertex(&views));
    assert!(!s.want_vertex(&views[..1]));
    s.note_vertex_at(1, &view(9, 9, 9));
    assert!(s.want_vertex(&views));
    let index = D3D12_INDEX_BUFFER_VIEW { BufferLocation: 1, SizeInBytes: 2, Format: DXGI_FORMAT_R16_UINT };
    assert!(s.want_index(&index));
    assert!(!s.want_index(&index));
    s.forget();
    assert!(s.want_index(&index));
    assert!(s.want_vertex(&views[..1]));
}

// On the host GPU.

const SHADERS: &str = r#"
#define RS_A "RootFlags(ALLOW_INPUT_ASSEMBLER_INPUT_LAYOUT), RootConstants(num32BitConstants=4, b0), CBV(b1)"
#define RS_B "RootFlags(ALLOW_INPUT_ASSEMBLER_INPUT_LAYOUT), RootConstants(num32BitConstants=4, b0), CBV(b1), RootConstants(num32BitConstants=1, b2)"
cbuffer Place : register(b0) { float2 at; float2 size; };
cbuffer Tint : register(b1) { float4 tint; };
cbuffer More : register(b2) { float more; };
struct V { float4 p : SV_Position; float4 c : COLOR; };
[RootSignature(RS_A)]
V vs_a(float2 p : POSITION, float4 c : COLOR) { V v; v.p = float4(at + p * size, 0.5, 1); v.c = c * tint; return v; }
[RootSignature(RS_B)]
V vs_b(float2 p : POSITION, float4 c : COLOR) { V v; v.p = float4(at + p * size * 0.5, 0.5, 1); v.c = c * tint * more; return v; }
float4 ps(V v) : SV_Target { return v.c; }
"#;

fn compile(entry: &str, target: &str) -> Vec<u8> {
    let entry = std::ffi::CString::new(entry).unwrap();
    let target = std::ffi::CString::new(target).unwrap();
    let mut code: Option<ID3DBlob> = None;
    // SAFETY: the source and names outlive the call.
    unsafe {
        D3DCompile(
            SHADERS.as_ptr().cast(),
            SHADERS.len(),
            s!("sets"),
            None,
            None,
            PCSTR(entry.as_ptr().cast()),
            PCSTR(target.as_ptr().cast()),
            0,
            0,
            &mut code,
            None,
        )
        .unwrap();
        let code = code.unwrap();
        std::slice::from_raw_parts(code.GetBufferPointer().cast::<u8>(), code.GetBufferSize()).to_vec()
    }
}

const SIZE: u32 = 128;
const FORMAT: DXGI_FORMAT = DXGI_FORMAT_R16G16B16A16_FLOAT;

/// Two pipelines with their own root signatures, a target, buffers, and a
/// queue to run lists on.
struct Gpu {
    device: ID3D12Device,
    queue: ID3D12CommandQueue,
    allocator: ID3D12CommandAllocator,
    list: Rec,
    fence: ID3D12Fence,
    ran: u64,
    roots: [ID3D12RootSignature; 2],
    pipelines: [ID3D12PipelineState; 2],
    target: ID3D12Resource,
    rtv: D3D12_CPU_DESCRIPTOR_HANDLE,
    back: ID3D12Resource,
    /// Vertex buffers (corners, colours, other corners), index buffers
    /// (a quad, a triangle) and tints, in one upload buffer.
    data: ID3D12Resource,
}

const CORNERS: u64 = 0;
const COLOURS: u64 = 256;
const HALVES: u64 = 512;
const QUAD: u64 = 768;
const TRIANGLE: u64 = 1024;
const TINTS: u64 = 1280;

impl Gpu {
    fn new() -> Option<Gpu> {
        let host = crate::host::host()?;
        let device = host.device.clone();
        // SAFETY: plain object creation with valid descriptions.
        unsafe {
            let queue: ID3D12CommandQueue = device
                .CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC { Type: D3D12_COMMAND_LIST_TYPE_DIRECT, ..Default::default() })
                .ok()?;
            let allocator: ID3D12CommandAllocator = device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).ok()?;
            let list: ID3D12GraphicsCommandList =
                device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None).ok()?;
            list.Close().ok()?;
            let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE).ok()?;
            let ps = compile("ps", "ps_5_1");
            let inputs = [
                D3D12_INPUT_ELEMENT_DESC {
                    SemanticName: s!("POSITION"),
                    Format: DXGI_FORMAT_R32G32_FLOAT,
                    InputSlot: 0,
                    InputSlotClass: D3D12_INPUT_CLASSIFICATION_PER_VERTEX_DATA,
                    ..Default::default()
                },
                D3D12_INPUT_ELEMENT_DESC {
                    SemanticName: s!("COLOR"),
                    Format: DXGI_FORMAT_R32G32B32A32_FLOAT,
                    InputSlot: 1,
                    InputSlotClass: D3D12_INPUT_CLASSIFICATION_PER_VERTEX_DATA,
                    ..Default::default()
                },
            ];
            let mut roots = Vec::new();
            let mut pipelines = Vec::new();
            for entry in ["vs_a", "vs_b"] {
                let vs = compile(entry, "vs_5_1");
                let root: ID3D12RootSignature = device.CreateRootSignature(0, &vs).ok()?;
                let mut desc = D3D12_GRAPHICS_PIPELINE_STATE_DESC {
                    pRootSignature: std::mem::transmute_copy(&root),
                    VS: D3D12_SHADER_BYTECODE { pShaderBytecode: vs.as_ptr().cast(), BytecodeLength: vs.len() },
                    PS: D3D12_SHADER_BYTECODE { pShaderBytecode: ps.as_ptr().cast(), BytecodeLength: ps.len() },
                    SampleMask: u32::MAX,
                    RasterizerState: D3D12_RASTERIZER_DESC {
                        FillMode: D3D12_FILL_MODE_SOLID,
                        CullMode: D3D12_CULL_MODE_NONE,
                        DepthClipEnable: true.into(),
                        ..Default::default()
                    },
                    InputLayout: D3D12_INPUT_LAYOUT_DESC { pInputElementDescs: inputs.as_ptr(), NumElements: 2 },
                    PrimitiveTopologyType: D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
                    NumRenderTargets: 1,
                    SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    ..Default::default()
                };
                // Added up: a draw with anything wrong changes the sum.
                let blend = &mut desc.BlendState.RenderTarget[0];
                blend.BlendEnable = true.into();
                blend.SrcBlend = D3D12_BLEND_ONE;
                blend.DestBlend = D3D12_BLEND_ONE;
                blend.BlendOp = D3D12_BLEND_OP_ADD;
                blend.SrcBlendAlpha = D3D12_BLEND_ONE;
                blend.DestBlendAlpha = D3D12_BLEND_ONE;
                blend.BlendOpAlpha = D3D12_BLEND_OP_ADD;
                blend.RenderTargetWriteMask = D3D12_COLOR_WRITE_ENABLE_ALL.0 as u8;
                desc.RTVFormats[0] = FORMAT;
                pipelines.push(device.CreateGraphicsPipelineState::<ID3D12PipelineState>(&desc).ok()?);
                roots.push(root);
            }
            let texture = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
                Width: u64::from(SIZE),
                Height: SIZE,
                DepthOrArraySize: 1,
                MipLevels: 1,
                Format: FORMAT,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Flags: D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
                ..Default::default()
            };
            let mut target: Option<ID3D12Resource> = None;
            device
                .CreateCommittedResource(
                    &D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() },
                    D3D12_HEAP_FLAG_NONE,
                    &texture,
                    D3D12_RESOURCE_STATE_RENDER_TARGET,
                    None,
                    &mut target,
                )
                .ok()?;
            let target = target?;
            let rtvs: ID3D12DescriptorHeap = device
                .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: D3D12_DESCRIPTOR_HEAP_TYPE_RTV,
                    NumDescriptors: 1,
                    ..Default::default()
                })
                .ok()?;
            let rtv = rtvs.GetCPUDescriptorHandleForHeapStart();
            device.CreateRenderTargetView(&target, None, rtv);
            // (The heap lives as long as the device holds the view's memory:
            // kept by leaking one reference.)
            std::mem::forget(rtvs);
            let buffer = |heap: D3D12_HEAP_TYPE, bytes: u64, state: D3D12_RESOURCE_STATES| -> Option<ID3D12Resource> {
                let mut made: Option<ID3D12Resource> = None;
                device
                    .CreateCommittedResource(
                        &D3D12_HEAP_PROPERTIES { Type: heap, ..Default::default() },
                        D3D12_HEAP_FLAG_NONE,
                        &D3D12_RESOURCE_DESC {
                            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                            Width: bytes,
                            Height: 1,
                            DepthOrArraySize: 1,
                            MipLevels: 1,
                            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                            ..Default::default()
                        },
                        state,
                        None,
                        &mut made,
                    )
                    .ok()?;
                made
            };
            let back = buffer(D3D12_HEAP_TYPE_READBACK, u64::from(SIZE * SIZE) * 8, D3D12_RESOURCE_STATE_COPY_DEST)?;
            let data = buffer(D3D12_HEAP_TYPE_UPLOAD, 4096, D3D12_RESOURCE_STATE_GENERIC_READ)?;
            let mut at = std::ptr::null_mut();
            data.Map(0, None, Some(&mut at)).ok()?;
            let at = at.cast::<u8>();
            let put = |offset: u64, words: &[f32]| {
                std::ptr::copy_nonoverlapping(words.as_ptr().cast::<u8>(), at.add(offset as usize), words.len() * 4);
            };
            put(CORNERS, &[0.0, 0.0, 1.0, 0.0, 0.0, 1.0, 1.0, 1.0]);
            put(HALVES, &[0.0, 0.0, 0.5, 0.0, 0.0, 0.5, 0.5, 0.5]);
            put(
                COLOURS,
                &[0.01, 0.0, 0.0, 1.0, 0.0, 0.02, 0.0, 1.0, 0.0, 0.0, 0.03, 1.0, 0.01, 0.01, 0.01, 1.0],
            );
            let indices: [u16; 6] = [0, 1, 2, 2, 1, 3];
            std::ptr::copy_nonoverlapping(indices.as_ptr().cast::<u8>(), at.add(QUAD as usize), 12);
            let triangle: [u16; 3] = [0, 1, 3];
            std::ptr::copy_nonoverlapping(triangle.as_ptr().cast::<u8>(), at.add(TRIANGLE as usize), 6);
            for (k, tint) in [[1.0f32, 1.0, 1.0, 1.0], [0.5, 1.0, 2.0, 1.0], [2.0, 0.25, 1.0, 1.0]].iter().enumerate() {
                put(TINTS + 256 * k as u64, tint);
            }
            Some(Gpu {
                device,
                queue,
                allocator,
                list: Rec::new(list),
                fence,
                ran: 0,
                roots: [roots[0].clone(), roots[1].clone()],
                pipelines: [pipelines[0].clone(), pipelines[1].clone()],
                target,
                rtv,
                back,
                data,
            })
        }
    }

    /// Opens the list onto a cleared target.
    fn open(&self) {
        // SAFETY: the list has run (every run is waited for).
        unsafe {
            self.allocator.Reset().unwrap();
            self.list.Reset(&self.allocator, None).unwrap();
            self.list.ClearRenderTargetView(self.rtv, &[0.0; 4], None);
            self.list.OMSetRenderTargets(1, Some(&self.rtv), false, None);
            self.list.RSSetViewports(&[D3D12_VIEWPORT {
                Width: SIZE as f32,
                Height: SIZE as f32,
                MaxDepth: 1.0,
                ..Default::default()
            }]);
            self.list.RSSetScissorRects(&[windows::Win32::Foundation::RECT {
                left: 0,
                top: 0,
                right: SIZE as i32,
                bottom: SIZE as i32,
            }]);
            self.list.IASetPrimitiveTopology(windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
        }
    }

    /// Closes the list, runs it and gives back the target's texels.
    fn run(&mut self) -> Vec<u8> {
        // SAFETY: an open list and our resources; the run is waited for.
        unsafe {
            let l = &self.list;
            l.ResourceBarrier(&[crate::host::transition(
                &self.target,
                D3D12_RESOURCE_STATE_RENDER_TARGET,
                D3D12_RESOURCE_STATE_COPY_SOURCE,
            )]);
            let footprint = D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                Offset: 0,
                Footprint: D3D12_SUBRESOURCE_FOOTPRINT { Format: FORMAT, Width: SIZE, Height: SIZE, Depth: 1, RowPitch: SIZE * 8 },
            };
            let to = D3D12_TEXTURE_COPY_LOCATION {
                pResource: std::mem::transmute_copy(&self.back),
                Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint: footprint },
            };
            let from = D3D12_TEXTURE_COPY_LOCATION {
                pResource: std::mem::transmute_copy(&self.target),
                Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: 0 },
            };
            l.CopyTextureRegion(&to, 0, 0, 0, &from, None);
            l.ResourceBarrier(&[crate::host::transition(
                &self.target,
                D3D12_RESOURCE_STATE_COPY_SOURCE,
                D3D12_RESOURCE_STATE_RENDER_TARGET,
            )]);
            l.Close().unwrap();
            self.queue.ExecuteCommandLists(&[Some(l.cast::<ID3D12CommandList>().unwrap())]);
            self.ran += 1;
            self.queue.Signal(&self.fence, self.ran).unwrap();
            let event = CreateEventW(None, false, false, None).unwrap();
            self.fence.SetEventOnCompletion(self.ran, event).unwrap();
            WaitForSingleObject(event, INFINITE);
            let _ = windows::Win32::Foundation::CloseHandle(event);
            let mut data = std::ptr::null_mut();
            self.back.Map(0, None, Some(&mut data)).unwrap();
            let texels = std::slice::from_raw_parts(data.cast::<u8>(), (SIZE * SIZE * 8) as usize).to_vec();
            self.back.Unmap(0, None);
            texels
        }
    }

    /// Records `draws` draws of a fixed-seed stream: every set made
    /// (`sets` none, as before) or through `sets` (`blind`: as a cache that left out too much would:
    /// the tint is set at the first draw only). The calls made.
    fn record(&self, seed: u64, draws: usize, mut sets: Option<&mut HostSets>, blind: bool) -> usize {
        let mut r = Seeded(seed);
        // SAFETY: our upload buffer.
        let gpu = unsafe { self.data.GetGPUVirtualAddress() };
        let l = &self.list;
        let mut calls = 0;
        let mut pipeline = usize::MAX;
        let mut place = [0f32; 4];
        let (mut corners, mut index, mut tint) = (CORNERS, QUAD, 0u64);
        for i in 0..draws {
            // A pipeline (and its root signature) now and then.
            if pipeline == usize::MAX || r.odds(6) {
                let p = r.below(2) as usize;
                if p != pipeline {
                    pipeline = p;
                    // SAFETY: an open list and live objects.
                    unsafe {
                        l.SetGraphicsRootSignature(&self.roots[p]);
                        l.SetPipelineState(&self.pipelines[p]);
                    }
                    calls += 2;
                    if let Some(s) = sets.as_deref_mut() {
                        s.root_signature_set(false);
                    }
                }
            }
            if r.odds(20) {
                corners = if r.odds(50) { CORNERS } else { HALVES };
            }
            if r.odds(15) {
                index = if r.odds(70) { QUAD } else { TRIANGLE };
            }
            if r.odds(25) {
                tint = r.below(3);
            }
            if r.odds(40) {
                // Another place on a grid (some draws fall on others).
                let cell = r.below(64) as f32;
                place = [-1.0 + (cell % 8.0) * 0.25, -1.0 + (cell / 8.0).floor() * 0.25, 0.25, 0.25];
            }
            let views = [
                view(gpu + corners, 32, 8),
                view(gpu + COLOURS, 64, 16),
            ];
            let ib = D3D12_INDEX_BUFFER_VIEW { BufferLocation: gpu + index, SizeInBytes: 12, Format: DXGI_FORMAT_R16_UINT };
            let words = place.map(f32::to_bits);
            let more = [(1.0f32 + (i % 3) as f32 * 0.5).to_bits()];
            let count = if index == QUAD { 6 } else { 3 };
            // SAFETY: an open list; the views name our live buffer.
            unsafe {
                match sets.as_deref_mut() {
                    // As the code was: everything, every draw.
                    None => {
                        l.IASetVertexBuffers(0, Some(&views));
                        l.IASetIndexBuffer(Some(&ib));
                        l.SetGraphicsRoot32BitConstants(0, 4, words.as_ptr().cast(), 0);
                        l.SetGraphicsRootConstantBufferView(1, gpu + TINTS + 256 * tint);
                        calls += 4;
                        if pipeline == 1 {
                            l.SetGraphicsRoot32BitConstants(2, 1, more.as_ptr().cast(), 0);
                            calls += 1;
                        }
                    }
                    Some(s) => {
                        let before = l.calls();
                        s.vertex_buffers(l, &views);
                        s.index_buffer(l, &ib);
                        s.constants(l, false, 0, &words);
                        if !blind || i == 0 {
                            s.view(l, false, 1, RootView::Constants, gpu + TINTS + 256 * tint);
                        }
                        if pipeline == 1 {
                            s.constants(l, false, 2, &more);
                        }
                        calls += (l.calls() - before) as usize;
                    }
                }
                l.DrawIndexedInstanced(count, 1, 0, 0, 0);
            }
            calls += 1;
        }
        calls
    }
}

/// The same stream draws the same picture with every set made and with
/// the redundant ones left out.
#[test]
fn left_out_sets_draw_the_same_picture() {
    let Some(mut gpu) = Gpu::new() else {
        eprintln!("no host GPU");
        return;
    };
    for seed in [3u64, 99] {
        gpu.open();
        let every = gpu.record(seed, 3000, None, false);
        let before = gpu.run();
        gpu.open();
        let mut sets = HostSets::enabled();
        let needed = gpu.record(seed, 3000, Some(&mut sets), false);
        let after = gpu.run();
        assert!(before.iter().any(|&b| b != 0), "nothing drawn");
        assert!(before == after, "seed {seed}: the pictures differ");
        assert!(needed < every * 3 / 4, "seed {seed}: {needed} calls against {every}");
        // The picture tells: a cache that leaves out sets that change
        // something draws another one.
        gpu.open();
        let mut blind = HostSets::enabled();
        gpu.record(seed, 3000, Some(&mut blind), true);
        assert!(gpu.run() != before, "seed {seed}: the picture does not show a wrong cache");
    }
}

/// A frame's worth of draws (6,500, the state shaped as in
/// [`Gpu::record`]) recorded with every set and through [`HostSets`]; then
/// what each kind of call costs the driver.
#[test]
#[ignore = "benchmark; see the module notes"]
fn bench_redundant_sets() {
    let Some(gpu) = Gpu::new() else {
        eprintln!("no host GPU");
        return;
    };
    const DRAWS: usize = 6500;
    const RUNS: usize = 15;
    let mut times = [Vec::new(), Vec::new()];
    let mut calls = [0, 0];
    for run in 0..RUNS + 2 {
        for (k, cached) in [false, true].into_iter().enumerate() {
            gpu.open();
            let mut sets = HostSets::enabled();
            let began = std::time::Instant::now();
            calls[k] = gpu.record(5, DRAWS, cached.then_some(&mut sets), false);
            // SAFETY: an open list.
            unsafe { gpu.list.Close().unwrap() };
            let took = began.elapsed().as_secs_f64() * 1e6;
            if run >= 2 {
                times[k].push(took);
            }
        }
    }
    let median = |v: &mut Vec<f64>| {
        v.sort_by(f64::total_cmp);
        v[v.len() / 2]
    };
    let (every, needed) = (median(&mut times[0]), median(&mut times[1]));
    eprintln!("{DRAWS} draws a list, recording medians of {RUNS} (driver and our bookkeeping):");
    eprintln!(
        "  every set:      {every:8.0} us, {:.3} us a draw, {} calls",
        every / DRAWS as f64,
        calls[0]
    );
    eprintln!(
        "  through HostSets: {needed:8.0} us, {:.3} us a draw, {} calls ({:+.1} %)",
        needed / DRAWS as f64,
        calls[1],
        (needed / every - 1.0) * 100.0
    );
    // What one call of each kind costs, recorded 6,500 times.
    // SAFETY: an open list; the views and objects are live.
    unsafe {
        let gpu_va = gpu.data.GetGPUVirtualAddress();
        let views = [view(gpu_va + CORNERS, 32, 8), view(gpu_va + COLOURS, 64, 16)];
        let ib = D3D12_INDEX_BUFFER_VIEW { BufferLocation: gpu_va + QUAD, SizeInBytes: 12, Format: DXGI_FORMAT_R16_UINT };
        let words = [1u32, 2, 3, 4];
        let kinds: [(&str, &dyn Fn(&Rec, usize)); 8] = [
            ("IASetVertexBuffers (2)", &|l, _| l.IASetVertexBuffers(0, Some(&views))),
            ("IASetIndexBuffer", &|l, _| l.IASetIndexBuffer(Some(&ib))),
            ("SetGraphicsRoot32BitConstants (4)", &|l, _| l.SetGraphicsRoot32BitConstants(0, 4, words.as_ptr().cast(), 0)),
            ("SetGraphicsRootConstantBufferView", &|l, i| l.SetGraphicsRootConstantBufferView(1, gpu_va + TINTS + 256 * (i as u64 % 3))),
            ("SetPipelineState (two in turn)", &|l, i| l.SetPipelineState(&gpu.pipelines[i % 2])),
            ("SetGraphicsRootSignature (two in turn)", &|l, i| l.SetGraphicsRootSignature(&gpu.roots[i % 2])),
            ("ResourceBarrier (1 UAV)", &|l, _| {
                l.ResourceBarrier(&[D3D12_RESOURCE_BARRIER {
                    Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
                    ..Default::default()
                }])
            }),
            ("DrawIndexedInstanced", &|l, _| l.DrawIndexedInstanced(6, 1, 0, 0, 0)),
        ];
        eprintln!("one call of each kind, ns (median of {RUNS} lists of {DRAWS}):");
        for (name, call) in &kinds {
            let mut t = Vec::new();
            for _ in 0..RUNS {
                gpu.open();
                gpu.list.SetGraphicsRootSignature(&gpu.roots[0]);
                gpu.list.SetPipelineState(&gpu.pipelines[0]);
                gpu.list.IASetVertexBuffers(0, Some(&views));
                gpu.list.IASetIndexBuffer(Some(&ib));
                let began = std::time::Instant::now();
                for i in 0..DRAWS {
                    call(&gpu.list, i);
                }
                t.push(began.elapsed().as_secs_f64() * 1e9 / DRAWS as f64);
                gpu.list.Close().unwrap();
            }
            eprintln!("  {name:<40} {:7.1}", median(&mut t));
        }
        // What leaving one out costs: the compare.
        let mut s = HostSets::enabled();
        let mut t = Vec::new();
        for _ in 0..RUNS {
            let began = std::time::Instant::now();
            for _ in 0..DRAWS {
                std::hint::black_box(s.want_vertex(std::hint::black_box(&views)));
                std::hint::black_box(s.want_constants(false, 0, std::hint::black_box(&words)));
            }
            t.push(began.elapsed().as_secs_f64() * 1e9 / DRAWS as f64 / 2.0);
        }
        eprintln!("  {:<40} {:7.1}", "a set left out (the compare)", median(&mut t));
    }
}

/// What sending costs the submitting thread: a frame's 30 submissions of
/// four lists each (two ways: one `ExecuteCommandLists` and one signal per
/// submission, as `Host::submit` does without cuts; one call per list, as
/// parts joined by `VICTORY_JOIN_PARTS` go), the signals alone, and all 120
/// lists in one call. Each list holds 50 draws. Ignored; run with
/// `cargo test --release -p victory-gpu --lib -- --ignored --nocapture bench_submission_calls`.
#[test]
#[ignore = "benchmark; see the comment"]
fn bench_submission_calls() {
    let Some(gpu) = Gpu::new() else {
        eprintln!("no host GPU");
        return;
    };
    const SUBMISSIONS: usize = 30;
    const PER: usize = 4;
    const RUNS: usize = 15;
    // SAFETY: plain object creation; every run is waited for before the
    // lists are recorded again.
    unsafe {
        let lists: Vec<(ID3D12CommandAllocator, ID3D12GraphicsCommandList)> = (0..SUBMISSIONS * PER)
            .map(|_| {
                let a: ID3D12CommandAllocator = gpu.device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).unwrap();
                let l: ID3D12GraphicsCommandList =
                    gpu.device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &a, None).unwrap();
                l.OMSetRenderTargets(1, Some(&gpu.rtv), false, None);
                l.SetGraphicsRootSignature(&gpu.roots[0]);
                l.SetPipelineState(&gpu.pipelines[0]);
                l.IASetPrimitiveTopology(windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
                for _ in 0..50 {
                    l.DrawInstanced(3, 1, 0, 0);
                }
                l.Close().unwrap();
                (a, l)
            })
            .collect();
        let all: Vec<Option<ID3D12CommandList>> = lists.iter().map(|(_, l)| Some(l.cast().unwrap())).collect();
        let fence: ID3D12Fence = gpu.device.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap();
        let event = CreateEventW(None, false, false, None).unwrap();
        let mut value = 0u64;
        let wait = |value: u64| {
            fence.SetEventOnCompletion(value, event).unwrap();
            WaitForSingleObject(event, INFINITE);
        };
        let ways: [(&str, &dyn Fn(&mut u64)); 5] = [
            ("one call and a signal per submission", &|v| {
                for s in all.chunks(PER) {
                    gpu.queue.ExecuteCommandLists(s);
                    *v += 1;
                    gpu.queue.Signal(&fence, *v).unwrap();
                }
            }),
            ("one call per list, a signal per submission", &|v| {
                for s in all.chunks(PER) {
                    for l in s {
                        gpu.queue.ExecuteCommandLists(std::slice::from_ref(l));
                    }
                    *v += 1;
                    gpu.queue.Signal(&fence, *v).unwrap();
                }
            }),
            ("one call per submission, one signal", &|v| {
                for s in all.chunks(PER) {
                    gpu.queue.ExecuteCommandLists(s);
                }
                *v += 1;
                gpu.queue.Signal(&fence, *v).unwrap();
            }),
            ("30 signals alone", &|v| {
                for _ in 0..SUBMISSIONS {
                    *v += 1;
                    gpu.queue.Signal(&fence, *v).unwrap();
                }
            }),
            ("all lists in one call, one signal", &|v| {
                gpu.queue.ExecuteCommandLists(&all);
                *v += 1;
                gpu.queue.Signal(&fence, *v).unwrap();
            }),
        ];
        eprintln!("{SUBMISSIONS} submissions of {PER} lists, CPU time to send them (median of {RUNS}):");
        for (name, send) in &ways {
            let mut t = Vec::new();
            for run in 0..RUNS + 2 {
                let began = std::time::Instant::now();
                send(&mut value);
                let took = began.elapsed().as_secs_f64() * 1e6;
                wait(value);
                if run >= 2 {
                    t.push(took);
                }
            }
            t.sort_by(f64::total_cmp);
            eprintln!("  {name:<44} {:7.0} us", t[t.len() / 2]);
        }
        let _ = windows::Win32::Foundation::CloseHandle(event);
    }
}