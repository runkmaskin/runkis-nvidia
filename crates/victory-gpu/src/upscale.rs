//! LOCAL: `VICTORY_PRESENT_SCALE=1`: the presented frame scaled to the window here, sharp.
//!
//! A Series S title draws 1080p. Without this the swap chain has the
//! texture's size and Windows stretches it to the window, bilinear and
//! blurry on a 1440p screen. With it the swap chain has the window's client
//! size and one full-screen pass draws the frame into the back buffer: a
//! light contrast-adaptive sharpen of the source texels, then a Catmull-Rom
//! resample (4x4 taps) held inside its nearest 2x2 texels so edges do not
//! ring. The aspect is kept; bars are black. Where the window has the
//! texture's size the old copy is used.
//! `VICTORY_PRESENT_SHARPEN=<0..1>` (default 0.3; 0: none).

use std::sync::{Mutex, OnceLock};

use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::{D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST, ID3DBlob};
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::Interface;

/// Presents in flight must not find their descriptors rewritten.
const SLOTS: u32 = 16;

const SOURCE: &str = r#"
#define RS "RootFlags(0), RootConstants(num32BitConstants=8, b0), DescriptorTable(SRV(t0))"
cbuffer Place : register(b0) { float2 origin; float2 extent; float2 source; float sharp; float unused; };
Texture2D<float4> shown : register(t0);

[RootSignature(RS)]
float4 vs(uint id : SV_VertexID) : SV_Position {
    float2 p = float2((id << 1) & 2, id & 2);
    return float4(p.x * 2.0 - 1.0, 1.0 - p.y * 2.0, 0.0, 1.0);
}

float3 texel(int2 p) {
    int2 last = int2(source) - 1;
    return shown.Load(int3(clamp(p, int2(0, 0), last), 0)).rgb;
}

// Catmull-Rom weights of the taps at -1, 0, 1 and 2 for a fraction t.
float4 catmull_rom(float t) {
    float t2 = t * t;
    float t3 = t2 * t;
    return 0.5 * float4(-t3 + 2.0 * t2 - t, 3.0 * t3 - 5.0 * t2 + 2.0, -3.0 * t3 + 4.0 * t2 + t, t3 - t2);
}

[RootSignature(RS)]
float4 ps(float4 pos : SV_Position) : SV_Target {
    float2 at = (pos.xy - origin) / extent * source - 0.5;
    float2 cell = floor(at);
    float2 f = at - cell;
    int2 corner = int2(cell) - 2;
    float3 t[6][6];
    [unroll] for (int y = 0; y < 6; y++) {
        [unroll] for (int x = 0; x < 6; x++) {
            t[y][x] = texel(corner + int2(x, y));
        }
    }
    // The 4x4 taps sharpened against their four neighbours. The strength
    // falls to nothing where the neighbourhood already reaches black or
    // white, so the sharpen cannot push past the ends of the range. Written
    // as differences: a flat area stays exactly flat.
    float3 s[4][4];
    [unroll] for (int j = 0; j < 4; j++) {
        [unroll] for (int i = 0; i < 4; i++) {
            float3 c = t[j + 1][i + 1];
            float3 n = t[j][i + 1];
            float3 d = t[j + 2][i + 1];
            float3 w = t[j + 1][i];
            float3 e = t[j + 1][i + 2];
            float3 lo = min(c, min(min(n, d), min(w, e)));
            float3 hi = max(c, max(max(n, d), max(w, e)));
            float3 room = saturate(min(lo, 1.0 - hi) / max(hi, 1e-5));
            float3 k = sharp * 0.125 * sqrt(room);
            s[j][i] = c + k * ((c - n) + (c - d) + (c - w) + (c - e)) / (1.0 - 4.0 * k);
        }
    }
    float4 wx = catmull_rom(f.x);
    float4 wy = catmull_rom(f.y);
    float3 mid = s[1][1];
    float3 sum = 0.0;
    [unroll] for (int b = 0; b < 4; b++) {
        [unroll] for (int a = 0; a < 4; a++) {
            sum += (wx[a] * wy[b]) * (s[b][a] - mid);
        }
    }
    // Held inside the nearest four texels: Catmull-Rom's overshoot is the ring.
    float3 lo2 = min(min(s[1][1], s[1][2]), min(s[2][1], s[2][2]));
    float3 hi2 = max(max(s[1][1], s[1][2]), max(s[2][1], s[2][2]));
    return float4(clamp(mid + sum, lo2, hi2), 1.0);
}
"#;

/// `VICTORY_PRESENT_SCALE=1`.
pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_PRESENT_SCALE").is_some_and(|v| v == "1"))
}

/// `VICTORY_PRESENT_SHARPEN`, 0 to 1.
pub(crate) fn sharpness() -> f32 {
    static SHARP: OnceLock<f32> = OnceLock::new();
    *SHARP.get_or_init(|| parse_sharpness(std::env::var("VICTORY_PRESENT_SHARPEN").ok().as_deref()))
}

/// The sharpness a setting asks for: 0.3 if unset or unreadable. A comma is
/// read as the decimal point (a Swedish system writes one).
fn parse_sharpness(text: Option<&str>) -> f32 {
    text.and_then(|t| t.trim().replace(',', ".").parse::<f32>().ok())
        .filter(|v| v.is_finite())
        .map_or(0.3, |v| v.clamp(0.0, 1.0))
}

/// The swap chain's size: the window's client size when scaling, else the
/// texture's. A minimised window (no client area) keeps the `current` size,
/// so restoring it does not make the buffers twice.
pub(crate) fn target(texture: (u32, u32), client: Option<(u32, u32)>, current: (u32, u32), scaling: bool) -> (u32, u32) {
    if !scaling {
        return texture;
    }
    match client {
        Some((w, h)) if w > 0 && h > 0 => (w, h),
        _ => current,
    }
}

/// Whether the frame is copied as before: the back buffer has its size.
pub(crate) fn copies(texture: (u32, u32), back: (u32, u32)) -> bool {
    texture == back
}

/// Where the texture lands in the back buffer, aspect kept and centred:
/// left, top, width, height.
pub(crate) fn fit((sw, sh): (u32, u32), (dw, dh): (u32, u32)) -> [u32; 4] {
    let (sw, sh, dw, dh) = (u64::from(sw.max(1)), u64::from(sh.max(1)), u64::from(dw), u64::from(dh));
    let (w, h) = if dw * sh <= dh * sw {
        (dw, ((sh * dw + sw / 2) / sw).min(dh))
    } else {
        (((sw * dh + sh / 2) / sh).min(dw), dh)
    };
    [((dw - w) / 2) as u32, ((dh - h) / 2) as u32, w as u32, h as u32]
}

/// The format the presented texture is read as and the back buffer's view
/// is written as, for a texture of `format` presented into `swap`. `None`
/// for formats this pass does not read (integer and signed ones: the copy
/// only moved their bits). An sRGB texture is read as sRGB and written
/// through an sRGB view, so its bytes keep their meaning.
fn formats(format: DXGI_FORMAT, swap: DXGI_FORMAT) -> Option<(DXGI_FORMAT, DXGI_FORMAT)> {
    let read = match format.0 {
        27 | 28 => DXGI_FORMAT_R8G8B8A8_UNORM,
        29 => DXGI_FORMAT_R8G8B8A8_UNORM_SRGB,
        87 | 90 => DXGI_FORMAT_B8G8R8A8_UNORM,
        91 => DXGI_FORMAT_B8G8R8A8_UNORM_SRGB,
        23 | 24 => DXGI_FORMAT_R10G10B10A2_UNORM,
        9 | 10 => DXGI_FORMAT_R16G16B16A16_FLOAT,
        _ => return None,
    };
    let write = match (read, swap) {
        (DXGI_FORMAT_R8G8B8A8_UNORM_SRGB, DXGI_FORMAT_R8G8B8A8_UNORM) => DXGI_FORMAT_R8G8B8A8_UNORM_SRGB,
        (DXGI_FORMAT_B8G8R8A8_UNORM_SRGB, DXGI_FORMAT_B8G8R8A8_UNORM) => DXGI_FORMAT_B8G8R8A8_UNORM_SRGB,
        _ => swap,
    };
    Some((read, write))
}

struct Pass {
    /// The device it was made on (each test makes its own).
    device: ID3D12Device,
    root: ID3D12RootSignature,
    vs: ID3DBlob,
    ps: ID3DBlob,
    /// One pipeline for each back-buffer view format.
    pipelines: Vec<(i32, ID3D12PipelineState)>,
    views: ID3D12DescriptorHeap,
    /// Held: the back-buffer views are written into it.
    _targets: ID3D12DescriptorHeap,
    cpu: usize,
    gpu: u64,
    step: u32,
    target_cpu: usize,
    target_step: u32,
    next: u32,
}
// SAFETY: D3D12 objects are free-threaded; the pass is used under a lock.
unsafe impl Send for Pass {}

/// The pass: not tried yet, could not be made, or made.
static PASS: Mutex<Option<Option<Pass>>> = Mutex::new(None);

fn compile(entry: windows::core::PCSTR, profile: windows::core::PCSTR) -> windows::core::Result<ID3DBlob> {
    let mut code: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    // SAFETY: the source and names live through the call; the out pointers
    // are ours.
    let made = unsafe {
        D3DCompile(
            SOURCE.as_ptr().cast(),
            SOURCE.len(),
            windows::core::s!("present scale"),
            None,
            None,
            entry,
            profile,
            0,
            0,
            &mut code,
            Some(&mut errors),
        )
    };
    if let Err(e) = made {
        if let Some(errors) = errors {
            // SAFETY: the blob's own bytes.
            let text = unsafe { std::slice::from_raw_parts(errors.GetBufferPointer().cast::<u8>(), errors.GetBufferSize()) };
            tracing::warn!(errors = %String::from_utf8_lossy(text), "present scale: the shader did not compile");
        }
        return Err(e);
    }
    code.ok_or_else(windows::core::Error::empty)
}

fn bytes(blob: &ID3DBlob) -> &[u8] {
    // SAFETY: the blob's own bytes, which live as long as the blob.
    unsafe { std::slice::from_raw_parts(blob.GetBufferPointer().cast::<u8>(), blob.GetBufferSize()) }
}

fn make(device: &ID3D12Device) -> windows::core::Result<Pass> {
    let vs = compile(windows::core::s!("vs"), windows::core::s!("vs_5_1"))?;
    let ps = compile(windows::core::s!("ps"), windows::core::s!("ps_5_1"))?;
    // SAFETY: plain object creation and queries.
    unsafe {
        let root: ID3D12RootSignature = device.CreateRootSignature(0, bytes(&ps))?;
        let kind = D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV;
        let views: ID3D12DescriptorHeap = device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
            Type: kind,
            NumDescriptors: SLOTS,
            Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
            NodeMask: 0,
        })?;
        let targets: ID3D12DescriptorHeap = device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
            Type: D3D12_DESCRIPTOR_HEAP_TYPE_RTV,
            NumDescriptors: SLOTS,
            Flags: D3D12_DESCRIPTOR_HEAP_FLAG_NONE,
            NodeMask: 0,
        })?;
        Ok(Pass {
            cpu: views.GetCPUDescriptorHandleForHeapStart().ptr,
            gpu: views.GetGPUDescriptorHandleForHeapStart().ptr,
            step: device.GetDescriptorHandleIncrementSize(kind),
            target_cpu: targets.GetCPUDescriptorHandleForHeapStart().ptr,
            target_step: device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_RTV),
            device: device.clone(),
            root,
            vs,
            ps,
            pipelines: Vec::new(),
            views,
            _targets: targets,
            next: 0,
        })
    }
}

impl Pass {
    fn pipeline(&mut self, device: &ID3D12Device, format: DXGI_FORMAT) -> Option<ID3D12PipelineState> {
        if let Some((_, p)) = self.pipelines.iter().find(|(f, _)| *f == format.0) {
            return Some(p.clone());
        }
        let mut blend = D3D12_BLEND_DESC::default();
        blend.RenderTarget[0].RenderTargetWriteMask = D3D12_COLOR_WRITE_ENABLE_ALL.0 as u8;
        let mut formats = [DXGI_FORMAT_UNKNOWN; 8];
        formats[0] = format;
        let desc = D3D12_GRAPHICS_PIPELINE_STATE_DESC {
            // SAFETY: the same pointer, not counted; the root outlives the call.
            pRootSignature: unsafe { std::mem::transmute_copy(&self.root) },
            VS: D3D12_SHADER_BYTECODE { pShaderBytecode: bytes(&self.vs).as_ptr().cast(), BytecodeLength: bytes(&self.vs).len() },
            PS: D3D12_SHADER_BYTECODE { pShaderBytecode: bytes(&self.ps).as_ptr().cast(), BytecodeLength: bytes(&self.ps).len() },
            BlendState: blend,
            SampleMask: u32::MAX,
            RasterizerState: D3D12_RASTERIZER_DESC {
                FillMode: D3D12_FILL_MODE_SOLID,
                CullMode: D3D12_CULL_MODE_NONE,
                DepthClipEnable: true.into(),
                ..Default::default()
            },
            PrimitiveTopologyType: D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
            NumRenderTargets: 1,
            RTVFormats: formats,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            ..Default::default()
        };
        // SAFETY: a full description whose pointers live through the call.
        match unsafe { device.CreateGraphicsPipelineState::<ID3D12PipelineState>(&desc) } {
            Ok(p) => {
                self.pipelines.push((format.0, p.clone()));
                Some(p)
            }
            Err(e) => {
                tracing::warn!(error = %e, format = format.0, "present scale: no pipeline for this back-buffer format");
                None
            }
        }
    }
}

/// Runs `f` with the pass, made the first time.
fn with_pass<T>(device: &ID3D12Device, f: impl FnOnce(&mut Pass) -> Option<T>) -> Option<T> {
    let mut kept = PASS.lock().unwrap_or_else(|e| e.into_inner());
    if kept.as_ref().is_some_and(|k| k.as_ref().is_some_and(|p| p.device.as_raw() != device.as_raw())) {
        *kept = None;
    }
    if kept.is_none() {
        *kept = Some(match make(device) {
            Ok(p) => {
                tracing::info!(sharpen = sharpness(), "present scale: frames are scaled to the window");
                Some(p)
            }
            Err(e) => {
                tracing::warn!(error = %e, "present scale: the pass could not be made; the window stretches the frame");
                None
            }
        });
    }
    f(kept.as_mut()?.as_mut()?)
}

/// Whether a texture of `format`, shown into a swap chain of `swap`, can be
/// scaled here: the pass and its pipeline are made. When not, the swap
/// chain keeps the texture's size and the copy is used.
pub(crate) fn ready(device: &ID3D12Device, format: DXGI_FORMAT, swap: DXGI_FORMAT) -> bool {
    let Some((_, write)) = formats(format, swap) else {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| tracing::warn!(format = format.0, "present scale: not for this format; the frame is copied"));
        return false;
    };
    with_pass(device, |pass| pass.pipeline(device, write)).is_some()
}

/// Records, into `list`, `shown` scaled into `back` (of size `size` and
/// format `swap`), sharpened by `sharp`, with black bars. `shown` is in
/// the copy-source state and `back` in the copy-dest state; both are left
/// so. False if nothing was recorded.
pub(crate) fn record(
    device: &ID3D12Device,
    list: &ID3D12GraphicsCommandList,
    shown: &ID3D12Resource,
    back: &ID3D12Resource,
    size: (u32, u32),
    swap: DXGI_FORMAT,
    sharp: f32,
) -> bool {
    // SAFETY: a live resource; a plain query.
    let desc = unsafe { shown.GetDesc() };
    let Some((read, write)) = formats(desc.Format, swap) else { return false };
    let source = (desc.Width as u32, desc.Height);
    with_pass(device, |pass| {
        let pso = pass.pipeline(device, write)?;
        let slot = pass.next % SLOTS;
        pass.next = pass.next.wrapping_add(1);
        let view = D3D12_CPU_DESCRIPTOR_HANDLE { ptr: pass.cpu + (slot * pass.step) as usize };
        let table = D3D12_GPU_DESCRIPTOR_HANDLE { ptr: pass.gpu + u64::from(slot * pass.step) };
        let target = D3D12_CPU_DESCRIPTOR_HANDLE { ptr: pass.target_cpu + (slot * pass.target_step) as usize };
        let srv = D3D12_SHADER_RESOURCE_VIEW_DESC {
            Format: read,
            ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2D,
            Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
            Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                Texture2D: D3D12_TEX2D_SRV { MostDetailedMip: 0, MipLevels: 1, ..Default::default() },
            },
        };
        let rtv = D3D12_RENDER_TARGET_VIEW_DESC {
            Format: write,
            ViewDimension: D3D12_RTV_DIMENSION_TEXTURE2D,
            Anonymous: D3D12_RENDER_TARGET_VIEW_DESC_0 { Texture2D: D3D12_TEX2D_RTV { MipSlice: 0, PlaneSlice: 0 } },
        };
        let [x, y, w, h] = fit(source, size);
        let constants: [f32; 8] = [x as f32, y as f32, w as f32, h as f32, source.0 as f32, source.1 as f32, sharp, 0.0];
        let t = crate::host::transition;
        let read_state = D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE;
        // SAFETY: an open list; slots of our heaps; live resources and
        // objects (the pass's own live as long as the process; the caller
        // keeps `shown` and `back` until the list has run).
        unsafe {
            device.CreateShaderResourceView(shown, Some(&srv), view);
            device.CreateRenderTargetView(back, Some(&rtv), target);
            list.ResourceBarrier(&[
                t(shown, D3D12_RESOURCE_STATE_COPY_SOURCE, read_state),
                t(back, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_RENDER_TARGET),
            ]);
            list.OMSetRenderTargets(1, Some(&target), false, None);
            if (w, h) != size {
                list.ClearRenderTargetView(target, &[0.0, 0.0, 0.0, 1.0], None);
            }
            list.SetDescriptorHeaps(&[Some(pass.views.clone())]);
            list.SetGraphicsRootSignature(&pass.root);
            list.SetPipelineState(&pso);
            list.SetGraphicsRoot32BitConstants(0, 8, constants.as_ptr().cast(), 0);
            list.SetGraphicsRootDescriptorTable(1, table);
            list.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            list.RSSetViewports(&[D3D12_VIEWPORT {
                TopLeftX: 0.0,
                TopLeftY: 0.0,
                Width: size.0 as f32,
                Height: size.1 as f32,
                MinDepth: 0.0,
                MaxDepth: 1.0,
            }]);
            list.RSSetScissorRects(&[RECT { left: x as i32, top: y as i32, right: (x + w) as i32, bottom: (y + h) as i32 }]);
            list.DrawInstanced(3, 1, 0, 0);
            list.ResourceBarrier(&[
                t(shown, read_state, D3D12_RESOURCE_STATE_COPY_SOURCE),
                t(back, D3D12_RESOURCE_STATE_RENDER_TARGET, D3D12_RESOURCE_STATE_COPY_DEST),
            ]);
        }
        Some(())
    })
    .is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
    use windows::Win32::Graphics::Dxgi::*;
    use windows::Win32::System::Threading::{CreateEventW, INFINITE, WaitForSingleObject};

    #[test]
    fn the_swap_chain_follows_the_window_only_when_scaling() {
        assert_eq!(target((1920, 1080), Some((2560, 1440)), (1920, 1080), false), (1920, 1080), "off: the texture's size");
        assert_eq!(target((1920, 1080), Some((2560, 1440)), (1920, 1080), true), (2560, 1440));
        assert_eq!(target((1920, 1080), Some((0, 0)), (2560, 1440), true), (2560, 1440), "minimised: kept");
        assert_eq!(target((1920, 1080), None, (1920, 1080), true), (1920, 1080));
        // A window of the texture's size: the old copy.
        let back = target((64, 36), Some((64, 36)), (64, 36), true);
        assert!(copies((64, 36), back));
        assert!(!copies((64, 36), (128, 72)));
    }

    #[test]
    fn the_frame_keeps_its_aspect_with_bars() {
        assert_eq!(fit((1920, 1080), (2560, 1440)), [0, 0, 2560, 1440]);
        assert_eq!(fit((64, 36), (128, 96)), [0, 12, 128, 72], "bars above and below");
        assert_eq!(fit((64, 36), (200, 72)), [36, 0, 128, 72], "bars at the sides");
        assert_eq!(fit((64, 36), (100, 56)), [0, 0, 100, 56], "rounded to the nearest pixel");
        assert_eq!(fit((1920, 1080), (2560, 1600)), [0, 80, 2560, 1440]);
    }

    #[test]
    fn sharpness_reads_both_decimal_marks() {
        assert_eq!(parse_sharpness(None), 0.3);
        assert_eq!(parse_sharpness(Some("0.5")), 0.5);
        assert_eq!(parse_sharpness(Some("0,5")), 0.5);
        assert_eq!(parse_sharpness(Some("7")), 1.0);
        assert_eq!(parse_sharpness(Some("-1")), 0.0);
        assert_eq!(parse_sharpness(Some("x")), 0.3);
    }

    #[test]
    fn only_formats_with_a_meaning_to_filter_are_scaled() {
        let r = DXGI_FORMAT_R8G8B8A8_UNORM;
        assert_eq!(formats(DXGI_FORMAT_R8G8B8A8_TYPELESS, r), Some((r, r)));
        assert_eq!(
            formats(DXGI_FORMAT_R8G8B8A8_UNORM_SRGB, r),
            Some((DXGI_FORMAT_R8G8B8A8_UNORM_SRGB, DXGI_FORMAT_R8G8B8A8_UNORM_SRGB))
        );
        assert_eq!(formats(DXGI_FORMAT_R8G8B8A8_UINT, r), None);
        assert_eq!(
            formats(DXGI_FORMAT_R16G16B16A16_TYPELESS, DXGI_FORMAT_R16G16B16A16_FLOAT),
            Some((DXGI_FORMAT_R16G16B16A16_FLOAT, DXGI_FORMAT_R16G16B16A16_FLOAT))
        );
    }

    /// The card with the most memory of its own, else the software adapter.
    fn device() -> Option<ID3D12Device> {
        // SAFETY: plain DXGI queries and device creation.
        unsafe {
            let factory: IDXGIFactory4 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)).ok()?;
            let mut best: Option<(usize, IDXGIAdapter1)> = None;
            for i in 0.. {
                let Ok(adapter) = factory.EnumAdapters1(i) else { break };
                let Ok(desc) = adapter.GetDesc1() else { continue };
                if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0 {
                    continue;
                }
                if best.as_ref().is_none_or(|b| desc.DedicatedVideoMemory > b.0) {
                    best = Some((desc.DedicatedVideoMemory, adapter));
                }
            }
            let adapter: IDXGIAdapter = match best {
                Some((_, a)) => a.cast().ok()?,
                None => factory.EnumWarpAdapter().ok()?,
            };
            let mut device: Option<ID3D12Device> = None;
            D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_12_0, &mut device).ok()?;
            device
        }
    }

    fn texture(device: &ID3D12Device, (w, h): (u32, u32), flags: D3D12_RESOURCE_FLAGS, state: D3D12_RESOURCE_STATES) -> ID3D12Resource {
        let heap = D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Width: u64::from(w),
            Height: h,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Flags: flags,
            ..Default::default()
        };
        let mut out: Option<ID3D12Resource> = None;
        // SAFETY: plain creation.
        unsafe { device.CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &desc, state, None, &mut out) }.unwrap();
        out.unwrap()
    }

    fn buffer(device: &ID3D12Device, kind: D3D12_HEAP_TYPE, size: u64, state: D3D12_RESOURCE_STATES) -> ID3D12Resource {
        let heap = D3D12_HEAP_PROPERTIES { Type: kind, ..Default::default() };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: size,
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            ..Default::default()
        };
        let mut out: Option<ID3D12Resource> = None;
        // SAFETY: plain creation.
        unsafe { device.CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &desc, state, None, &mut out) }.unwrap();
        out.unwrap()
    }

    fn located(resource: &ID3D12Resource, footprint: Option<(u32, u32, u32)>) -> D3D12_TEXTURE_COPY_LOCATION {
        D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: the same pointer, not counted; used while it lives.
            pResource: unsafe { std::mem::transmute_copy(resource) },
            Type: if footprint.is_some() { D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT } else { D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX },
            Anonymous: match footprint {
                Some((w, h, pitch)) => D3D12_TEXTURE_COPY_LOCATION_0 {
                    PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                        Offset: 0,
                        Footprint: D3D12_SUBRESOURCE_FOOTPRINT { Format: DXGI_FORMAT_R8G8B8A8_UNORM, Width: w, Height: h, Depth: 1, RowPitch: pitch },
                    },
                },
                None => D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: 0 },
            },
        }
    }

    /// The GPU tests one at a time: the pass is one for the process, and a
    /// test on another device would make it again under a list in flight.
    static TURN: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct Rig {
        device: ID3D12Device,
        queue: ID3D12CommandQueue,
        allocator: ID3D12CommandAllocator,
        list: ID3D12GraphicsCommandList,
        fence: ID3D12Fence,
        done: u64,
    }

    impl Rig {
        fn new() -> Option<Rig> {
            let device = device()?;
            // SAFETY: plain creation of objects of our own.
            unsafe {
                let queue: ID3D12CommandQueue = device
                    .CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC { Type: D3D12_COMMAND_LIST_TYPE_DIRECT, ..Default::default() })
                    .ok()?;
                let allocator: ID3D12CommandAllocator = device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).ok()?;
                let list: ID3D12GraphicsCommandList = device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None).ok()?;
                list.Close().ok()?;
                let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE).ok()?;
                Some(Rig { device, queue, allocator, list, fence, done: 0 })
            }
        }

        fn begin(&self) {
            // SAFETY: the list's last run was waited for.
            unsafe {
                self.allocator.Reset().unwrap();
                self.list.Reset(&self.allocator, None).unwrap();
            }
        }

        fn run(&mut self) {
            self.done += 1;
            // SAFETY: our closed list; our fence and event.
            unsafe {
                self.list.Close().unwrap();
                self.queue.ExecuteCommandLists(&[Some(self.list.cast().unwrap())]);
                self.queue.Signal(&self.fence, self.done).unwrap();
                let event = CreateEventW(None, false, false, None).unwrap();
                self.fence.SetEventOnCompletion(self.done, event).unwrap();
                WaitForSingleObject(event, INFINITE);
                let _ = CloseHandle(event);
            }
        }

        /// The source of `pixels` (RGBA8), left in the copy-source state.
        fn source(&mut self, (w, h): (u32, u32), pixels: &[[u8; 4]]) -> ID3D12Resource {
            let pitch = (w * 4).next_multiple_of(256);
            let upload = buffer(&self.device, D3D12_HEAP_TYPE_UPLOAD, u64::from(pitch * h), D3D12_RESOURCE_STATE_GENERIC_READ);
            // SAFETY: a mapped upload buffer of that size.
            unsafe {
                let mut at = std::ptr::null_mut();
                upload.Map(0, None, Some(&mut at)).unwrap();
                for y in 0..h as usize {
                    let row = &pixels[y * w as usize..(y + 1) * w as usize];
                    std::ptr::copy_nonoverlapping(row.as_ptr().cast::<u8>(), at.cast::<u8>().add(y * pitch as usize), row.len() * 4);
                }
                upload.Unmap(0, None);
            }
            let tex = texture(&self.device, (w, h), D3D12_RESOURCE_FLAG_NONE, D3D12_RESOURCE_STATE_COPY_DEST);
            self.begin();
            // SAFETY: an open list and live resources.
            unsafe {
                self.list.CopyTextureRegion(&located(&tex, None), 0, 0, 0, &located(&upload, Some((w, h, pitch))), None);
                self.list.ResourceBarrier(&[crate::host::transition(&tex, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_COPY_SOURCE)]);
            }
            self.run();
            tex
        }

        /// `shown` scaled to `size` by the pass, read back.
        fn scaled(&mut self, shown: &ID3D12Resource, size: (u32, u32), sharp: f32) -> Vec<[u8; 4]> {
            let back = texture(&self.device, size, D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET, D3D12_RESOURCE_STATE_COPY_DEST);
            let pitch = (size.0 * 4).next_multiple_of(256);
            let read = buffer(&self.device, D3D12_HEAP_TYPE_READBACK, u64::from(pitch * size.1), D3D12_RESOURCE_STATE_COPY_DEST);
            self.begin();
            assert!(record(&self.device, &self.list, shown, &back, size, DXGI_FORMAT_R8G8B8A8_UNORM, sharp), "recorded");
            // SAFETY: an open list and live resources.
            unsafe {
                self.list.ResourceBarrier(&[crate::host::transition(&back, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_COPY_SOURCE)]);
                self.list.CopyTextureRegion(&located(&read, Some((size.0, size.1, pitch))), 0, 0, 0, &located(&back, None), None);
            }
            self.run();
            let mut out = Vec::with_capacity((size.0 * size.1) as usize);
            // SAFETY: a mapped readback buffer of that size, written by the run waited for.
            unsafe {
                let mut at = std::ptr::null_mut();
                read.Map(0, None, Some(&mut at)).unwrap();
                for y in 0..size.1 as usize {
                    let row = at.cast::<[u8; 4]>().byte_add(y * pitch as usize);
                    out.extend_from_slice(std::slice::from_raw_parts(row, size.0 as usize));
                }
                read.Unmap(0, None);
            }
            out
        }
    }

    const A: [u8; 4] = [200, 40, 90, 255];
    const B: [u8; 4] = [30, 160, 220, 255];

    /// 64x36: left half `A`, right half `B`, with an 8x8 checkerboard of
    /// grey levels in the middle of the left half.
    fn pattern() -> Vec<[u8; 4]> {
        let mut p = Vec::new();
        for y in 0..36u32 {
            for x in 0..64u32 {
                p.push(if (12..20).contains(&x) && (14..22).contains(&y) {
                    if (x + y) % 2 == 0 { [180, 180, 180, 255] } else { [70, 70, 70, 255] }
                } else if x < 32 {
                    A
                } else {
                    B
                });
            }
        }
        p
    }

    fn mean(pixels: &[[u8; 4]], w: u32, [x0, y0, rw, rh]: [u32; 4]) -> [f64; 3] {
        let mut sum = [0.0f64; 3];
        for y in y0..y0 + rh {
            for x in x0..x0 + rw {
                let p = pixels[(y * w + x) as usize];
                for c in 0..3 {
                    sum[c] += f64::from(p[c]);
                }
            }
        }
        sum.map(|s| s / f64::from(rw * rh))
    }

    /// The pass on the card: flat areas stay exactly flat, the mean is
    /// kept, bars are black.
    #[test]
    fn gpu_scaling_keeps_flat_areas_the_mean_and_black_bars() {
        let _turn = TURN.lock().unwrap_or_else(|e| e.into_inner());
        let Some(mut rig) = Rig::new() else {
            eprintln!("no D3D12 device: skipped");
            return;
        };
        let input = pattern();
        let shown = rig.source((64, 36), &input);
        let in_mean = mean(&input, 64, [0, 0, 64, 36]);
        for size in [(128, 72), (100, 56), (128, 96), (200, 72), (64, 36)] {
            for sharp in [0.0, 0.3, 1.0] {
                let out = rig.scaled(&shown, size, sharp);
                let rect = fit((64, 36), size);
                let [x0, y0, w, h] = rect;
                for y in 0..size.1 {
                    for x in 0..size.0 {
                        let p = out[(y * size.0 + x) as usize];
                        let inside = (x0..x0 + w).contains(&x) && (y0..y0 + h).contains(&y);
                        if !inside {
                            assert_eq!(&p[..3], &[0, 0, 0], "{size:?}: a bar at {x},{y}");
                            continue;
                        }
                        // The source texel this pixel's centre falls on;
                        // flat if four texels from the edges between areas.
                        let sx = (f64::from(x - x0) + 0.5) / f64::from(w) * 64.0;
                        let sy = (f64::from(y - y0) + 0.5) / f64::from(h) * 36.0;
                        let near = |a: f64, b: f64| (a - b).abs() < 4.0;
                        let in_checker = (12.0 - 4.0..20.0 + 4.0).contains(&sx) && (14.0 - 4.0..22.0 + 4.0).contains(&sy);
                        if in_checker || near(sx, 32.0) {
                            continue;
                        }
                        let want = if sx < 32.0 { A } else { B };
                        assert_eq!(&p[..3], &want[..3], "{size:?} sharpen {sharp}: flat at {x},{y} (source {sx:.1},{sy:.1})");
                    }
                }
                let out_mean = mean(&out, size.0, rect);
                for c in 0..3 {
                    assert!(
                        (out_mean[c] - in_mean[c]).abs() < 1.5,
                        "{size:?} sharpen {sharp}: channel {c} mean {} against {}",
                        out_mean[c],
                        in_mean[c]
                    );
                }
                if size == (64, 36) && sharp == 0.0 {
                    assert_eq!(out, input, "the same size, unsharpened: the texels themselves");
                }
            }
        }
    }

    /// What the pass costs on the card for 1080p into 1440p, by timestamps
    /// (printed; not a pass or fail).
    #[test]
    fn gpu_scaling_cost_1080p_to_1440p() {
        let _turn = TURN.lock().unwrap_or_else(|e| e.into_inner());
        let Some(mut rig) = Rig::new() else { return };
        let (w, h) = (1920u32, 1080u32);
        let input: Vec<[u8; 4]> = (0..w * h).map(|i| [(i % 251) as u8, (i / w % 241) as u8, (i % 7 * 30) as u8, 255]).collect();
        let shown = rig.source((w, h), &input);
        let size = (2560, 1440);
        let back = texture(&rig.device, size, D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET, D3D12_RESOURCE_STATE_COPY_DEST);
        const RUNS: u32 = 20;
        // SAFETY: plain creation; an open list and live resources; a mapped
        // readback buffer written by the run waited for.
        unsafe {
            let mut stamps: Option<ID3D12QueryHeap> = None;
            rig.device
                .CreateQueryHeap(&D3D12_QUERY_HEAP_DESC { Type: D3D12_QUERY_HEAP_TYPE_TIMESTAMP, Count: RUNS + 1, NodeMask: 0 }, &mut stamps)
                .unwrap();
            let stamps = stamps.unwrap();
            let read = buffer(&rig.device, D3D12_HEAP_TYPE_READBACK, u64::from(RUNS + 1) * 8, D3D12_RESOURCE_STATE_COPY_DEST);
            rig.begin();
            rig.list.EndQuery(&stamps, D3D12_QUERY_TYPE_TIMESTAMP, 0);
            for i in 1..=RUNS {
                assert!(record(&rig.device, &rig.list, &shown, &back, size, DXGI_FORMAT_R8G8B8A8_UNORM, 0.3));
                rig.list.EndQuery(&stamps, D3D12_QUERY_TYPE_TIMESTAMP, i);
            }
            rig.list.ResolveQueryData(&stamps, D3D12_QUERY_TYPE_TIMESTAMP, 0, RUNS + 1, &read, 0);
            rig.run();
            let frequency = rig.queue.GetTimestampFrequency().unwrap() as f64;
            let mut at = std::ptr::null_mut();
            read.Map(0, None, Some(&mut at)).unwrap();
            let ticks = std::slice::from_raw_parts(at.cast::<u64>(), RUNS as usize + 1);
            let us = (ticks[RUNS as usize] - ticks[0]) as f64 / frequency * 1e6 / f64::from(RUNS);
            read.Unmap(0, None);
            eprintln!("present scale 1920x1080 -> 2560x1440: {us:.1} us on the GPU per frame (offline)");
        }
    }
}
