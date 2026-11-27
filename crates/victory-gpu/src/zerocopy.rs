//! How title memory is best put where the card reads it: measured, not
//! assumed (docs/shared-memory.md).
//!
//! The console has one memory. Here every byte a shader reads of title
//! memory is either read over the bus from the PC's memory, or copied to the
//! card's memory first. The ways that exist in this crate:
//!
//! - **shared**: a host copy in the PC's memory, write-back, mapped for
//!   good (`Host::shared_buffer`). The CPU copies title bytes into it, the
//!   card reads it over the bus. Today's path for windows and small buffers.
//! - **card**: the same copy as the CPU's side, and a buffer in the card's
//!   memory that the card copies the changed ranges into before the
//!   submission (`shadow::cards`). Today's path for big title buffers.
//! - **upload** / **combined**: an upload heap, or a custom heap in the PC's
//!   memory with write-combined pages. The CPU only writes it; texture
//!   staging is this (`pool::staging`).
//! - **bar**: card memory the CPU maps (`D3D12_HEAP_TYPE_GPU_UPLOAD`,
//!   `VICTORY_WINDOWS_BAR_MB`).
//! - **in place**: a heap opened on the title memory itself
//!   (`OpenExistingHeapFromAddress`, `crate::inplace`, `VICTORY_INPLACE`): no
//!   copy at all, the card reads the title's pages over the bus.
//!
//! The benchmarks below build each of them with real objects on the PC's
//! card and time what the CPU spends to fill it and what the card spends to
//! read it, once or many times. `cargo test --release -p victory-gpu --lib
//! -- --ignored --nocapture bench_shared_memory`.

#[cfg(test)]
mod tests {
    use std::ffi::c_void;

    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
    use windows::Win32::Graphics::Direct3D::{D3D_FEATURE_LEVEL_12_0, ID3DBlob};
    use windows::Win32::Graphics::Direct3D12::*;
    use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory2, DXGI_ADAPTER_FLAG_SOFTWARE, DXGI_CREATE_FACTORY_FLAGS, IDXGIAdapter1, IDXGIFactory4};
    use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
    use windows::core::{Interface, s};
    use windows_sys::Win32::System::Memory as mem;

    const K64: usize = 64 << 10;
    const MEM_WRITE_WATCH: u32 = 0x20_0000;
    /// Threads of a group, and the most groups a read dispatches.
    const GROUP: u32 = 256;
    const GROUPS: u32 = 2048;

    /// Reads every 16 bytes of `src` once (mode 0) or copies them to `dst`
    /// (mode 1). The read keeps its loads: a store depends on all of them.
    const SOURCE: &str = r#"
#define RS "RootConstants(num32BitConstants=4, b0), SRV(t0), UAV(u0)"
cbuffer C : register(b0) { uint n16; uint stride; uint mode; uint pad; };
ByteAddressBuffer src : register(t0);
RWByteAddressBuffer dst : register(u0);
[RootSignature(RS)]
[numthreads(256, 1, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    if (mode == 1) {
        for (uint i = id.x; i < n16; i += stride) dst.Store4(i * 16, src.Load4(i * 16));
        return;
    }
    uint4 acc = 0;
    for (uint j = id.x; j < n16; j += stride) acc ^= src.Load4(j * 16);
    uint v = acc.x ^ acc.y ^ acc.z ^ acc.w;
    if (v == 0x9e3779b9u) dst.Store(0, v);
}
"#;

    /// The card with the most memory of its own (the RTX here, not the
    /// processor's graphics), or none.
    pub(crate) fn device() -> Option<(ID3D12Device, String)> {
        // SAFETY: plain DXGI queries and device creation.
        unsafe {
            let factory: IDXGIFactory4 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)).ok()?;
            let mut best: Option<(u64, IDXGIAdapter1, String)> = None;
            for i in 0.. {
                let Ok(adapter) = factory.EnumAdapters1(i) else { break };
                let Ok(desc) = adapter.GetDesc1() else { continue };
                if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0 {
                    continue;
                }
                let name = String::from_utf16_lossy(&desc.Description).trim_end_matches('\0').to_owned();
                let vram = desc.DedicatedVideoMemory as u64;
                if best.as_ref().is_none_or(|b| vram > b.0) {
                    best = Some((vram, adapter, name));
                }
            }
            let (_, adapter, name) = best?;
            let mut device: Option<ID3D12Device> = None;
            windows::Win32::Graphics::Direct3D12::D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_12_0, &mut device).ok()?;
            Some((device?, name))
        }
    }

    fn desc(size: u64, flags: D3D12_RESOURCE_FLAGS) -> D3D12_RESOURCE_DESC {
        D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: size.max(1),
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            Flags: flags,
            ..Default::default()
        }
    }

    pub(crate) fn turn(buffer: &ID3D12Resource, before: D3D12_RESOURCE_STATES, after: D3D12_RESOURCE_STATES) -> D3D12_RESOURCE_BARRIER {
        D3D12_RESOURCE_BARRIER {
            Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
            Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
            Anonymous: D3D12_RESOURCE_BARRIER_0 {
                Transition: std::mem::ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                    // SAFETY: the same pointer, not counted; the barrier is
                    // used while the buffer lives.
                    pResource: unsafe { std::mem::transmute_copy(buffer) },
                    Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                    StateBefore: before,
                    StateAfter: after,
                }),
            },
        }
    }

    /// A queue, list, fence, timestamps and the read shader of our own.
    pub(crate) struct Rig {
        pub(crate) device: ID3D12Device,
        queue: ID3D12CommandQueue,
        allocator: ID3D12CommandAllocator,
        list: ID3D12GraphicsCommandList,
        fence: ID3D12Fence,
        done: u64,
        event: HANDLE,
        stamps: ID3D12QueryHeap,
        stamped: ID3D12Resource,
        frequency: f64,
        root: ID3D12RootSignature,
        pso: ID3D12PipelineState,
        /// Where reads store their (never taken) result; copies' target.
        pub(crate) sink: ID3D12Resource,
        sink_size: u64,
    }

    impl Drop for Rig {
        fn drop(&mut self) {
            // SAFETY: our event.
            unsafe {
                let _ = CloseHandle(self.event);
            }
        }
    }

    impl Rig {
        pub(crate) fn new(device: &ID3D12Device, sink_size: u64) -> windows::core::Result<Rig> {
            // SAFETY: plain creation of objects of our own.
            unsafe {
                let queue: ID3D12CommandQueue =
                    device.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC { Type: D3D12_COMMAND_LIST_TYPE_DIRECT, ..Default::default() })?;
                let allocator: ID3D12CommandAllocator = device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)?;
                let list: ID3D12GraphicsCommandList = device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)?;
                list.Close()?;
                let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE)?;
                let event = CreateEventW(None, false, false, None)?;
                let mut stamps: Option<ID3D12QueryHeap> = None;
                device.CreateQueryHeap(&D3D12_QUERY_HEAP_DESC { Type: D3D12_QUERY_HEAP_TYPE_TIMESTAMP, Count: 8, NodeMask: 0 }, &mut stamps)?;
                let stamped = committed(device, D3D12_HEAP_TYPE_READBACK, 64, D3D12_RESOURCE_FLAG_NONE, D3D12_RESOURCE_STATE_COPY_DEST)?;
                let frequency = queue.GetTimestampFrequency()? as f64;
                let mut code: Option<ID3DBlob> = None;
                D3DCompile(SOURCE.as_ptr().cast(), SOURCE.len(), s!("zerocopy"), None, None, s!("main"), s!("cs_5_1"), 0, 0, &mut code, None)?;
                let code = code.ok_or_else(windows::core::Error::empty)?;
                let bytes = std::slice::from_raw_parts(code.GetBufferPointer().cast::<u8>(), code.GetBufferSize());
                let root: ID3D12RootSignature = device.CreateRootSignature(0, bytes)?;
                let pso = device.CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                    pRootSignature: std::mem::transmute_copy(&root),
                    CS: D3D12_SHADER_BYTECODE { pShaderBytecode: bytes.as_ptr().cast(), BytecodeLength: bytes.len() },
                    ..Default::default()
                })?;
                let sink = committed(
                    device,
                    D3D12_HEAP_TYPE_DEFAULT,
                    sink_size,
                    D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
                    D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                )?;
                Ok(Rig {
                    device: device.clone(),
                    queue,
                    allocator,
                    list,
                    fence,
                    done: 0,
                    event,
                    stamps: stamps.ok_or_else(windows::core::Error::empty)?,
                    stamped,
                    frequency,
                    root,
                    pso,
                    sink,
                    sink_size,
                })
            }
        }

        /// Records with `record`, runs it and waits.
        pub(crate) fn run(&mut self, record: impl FnOnce(&ID3D12GraphicsCommandList, &Rig)) -> windows::core::Result<()> {
            // SAFETY: our own list and queue, one run at a time.
            unsafe {
                self.allocator.Reset()?;
                self.list.Reset(&self.allocator, None)?;
                record(&self.list, self);
                self.list.Close()?;
                self.queue.ExecuteCommandLists(&[Some(self.list.cast()?)]);
                self.done += 1;
                self.queue.Signal(&self.fence, self.done)?;
                self.fence.SetEventOnCompletion(self.done, self.event)?;
                if WaitForSingleObject(self.event, 20_000).0 != 0 {
                    return Err(windows::core::Error::from_hresult(windows::Win32::Foundation::E_FAIL));
                }
                self.device.GetDeviceRemovedReason()
            }
        }

        pub(crate) fn stamp(&self, list: &ID3D12GraphicsCommandList, i: u32) {
            // SAFETY: an open list and our query heap of 8.
            unsafe { list.EndQuery(&self.stamps, D3D12_QUERY_TYPE_TIMESTAMP, i) };
        }

        pub(crate) fn resolve(&self, list: &ID3D12GraphicsCommandList, n: u32) {
            // SAFETY: an open list; the readback holds 8 stamps.
            unsafe { list.ResolveQueryData(&self.stamps, D3D12_QUERY_TYPE_TIMESTAMP, 0, n, &self.stamped, 0) };
        }

        /// The microseconds between stamps, after a run that resolved `n`.
        pub(crate) fn between(&self, n: usize) -> Vec<f64> {
            let mut data = std::ptr::null_mut();
            // SAFETY: the readback buffer holds 8 stamps.
            unsafe {
                if self.stamped.Map(0, None, Some(&mut data)).is_err() {
                    return vec![0.0; n.saturating_sub(1)];
                }
                let t = std::slice::from_raw_parts(data.cast::<u64>(), n);
                let out = t.windows(2).map(|w| w[1].saturating_sub(w[0]) as f64 / self.frequency * 1e6).collect();
                self.stamped.Unmap(0, None);
                out
            }
        }

        /// A dispatch reading (or, `copy`, copying into the sink) `bytes`
        /// at GPU address `gpu`.
        pub(crate) fn shade(&self, list: &ID3D12GraphicsCommandList, gpu: u64, bytes: u64, copy: bool) {
            let n16 = (bytes / 16) as u32;
            let groups = n16.div_ceil(GROUP).clamp(1, GROUPS);
            let stride = groups * GROUP;
            // SAFETY: an open list; the views name live buffers of `bytes`
            // (and the sink of at least as many when copying).
            unsafe {
                list.SetComputeRootSignature(&self.root);
                list.SetPipelineState(&self.pso);
                let constants = [n16, stride, u32::from(copy), 0];
                list.SetComputeRoot32BitConstants(0, 4, constants.as_ptr().cast(), 0);
                list.SetComputeRootShaderResourceView(1, gpu);
                // SAFETY: a live buffer.
                list.SetComputeRootUnorderedAccessView(2, self.sink.GetGPUVirtualAddress());
                list.Dispatch(groups, 1, 1);
            }
        }

        /// The first `n` bytes of the sink, through a readback.
        pub(crate) fn sink_bytes(&mut self, n: usize) -> windows::core::Result<Vec<u8>> {
            assert!(n as u64 <= self.sink_size);
            let back = committed(&self.device, D3D12_HEAP_TYPE_READBACK, n as u64, D3D12_RESOURCE_FLAG_NONE, D3D12_RESOURCE_STATE_COPY_DEST)?;
            let sink = self.sink.clone();
            self.run(|list, _| {
                // SAFETY: an open list; both buffers live and large enough.
                unsafe {
                    list.ResourceBarrier(&[turn(&sink, D3D12_RESOURCE_STATE_UNORDERED_ACCESS, D3D12_RESOURCE_STATE_COPY_SOURCE)]);
                    list.CopyBufferRegion(&back, 0, &sink, 0, n as u64);
                    list.ResourceBarrier(&[turn(&sink, D3D12_RESOURCE_STATE_COPY_SOURCE, D3D12_RESOURCE_STATE_UNORDERED_ACCESS)]);
                }
            })?;
            let mut data = std::ptr::null_mut();
            // SAFETY: the mapping holds `n` bytes until unmapped.
            unsafe {
                back.Map(0, None, Some(&mut data))?;
                let bytes = std::slice::from_raw_parts(data.cast::<u8>(), n).to_vec();
                back.Unmap(0, None);
                Ok(bytes)
            }
        }
    }

    pub(crate) fn committed(
        device: &ID3D12Device,
        kind: D3D12_HEAP_TYPE,
        size: u64,
        flags: D3D12_RESOURCE_FLAGS,
        state: D3D12_RESOURCE_STATES,
    ) -> windows::core::Result<ID3D12Resource> {
        heap_buffer(device, &D3D12_HEAP_PROPERTIES { Type: kind, ..Default::default() }, size, flags, state)
    }

    fn heap_buffer(
        device: &ID3D12Device,
        heap: &D3D12_HEAP_PROPERTIES,
        size: u64,
        flags: D3D12_RESOURCE_FLAGS,
        state: D3D12_RESOURCE_STATES,
    ) -> windows::core::Result<ID3D12Resource> {
        let mut buffer: Option<ID3D12Resource> = None;
        // SAFETY: plain creation.
        unsafe { device.CreateCommittedResource(heap, D3D12_HEAP_FLAG_NONE, &desc(size, flags), state, None, &mut buffer)? };
        buffer.ok_or_else(windows::core::Error::empty)
    }

    fn mapped(buffer: &ID3D12Resource) -> windows::core::Result<*mut u8> {
        let mut data = std::ptr::null_mut();
        // SAFETY: a CPU-visible buffer, mapped for good.
        unsafe { buffer.Map(0, None, Some(&mut data))? };
        Ok(data.cast())
    }

    /// Title-like memory: private, committed, read-write, write-watched,
    /// as `xmem` reserves the title's heaps.
    pub(crate) struct Title(pub(crate) *mut u8, pub(crate) usize);

    impl Title {
        pub(crate) fn new(len: usize, seed: u64) -> Title {
            // SAFETY: a fresh allocation of our own.
            let at = unsafe { mem::VirtualAlloc(std::ptr::null(), len, mem::MEM_RESERVE | mem::MEM_COMMIT | MEM_WRITE_WATCH, mem::PAGE_READWRITE) };
            assert!(!at.is_null(), "no title-like memory");
            let t = Title(at.cast(), len);
            t.fill(seed);
            t
        }

        pub(crate) fn fill(&self, seed: u64) {
            let mut x = seed | 1;
            // SAFETY: our committed memory of `self.1` bytes.
            let words = unsafe { std::slice::from_raw_parts_mut(self.0.cast::<u64>(), self.1 / 8) };
            for w in words {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                *w = x;
            }
        }

        pub(crate) fn bytes(&self) -> &[u8] {
            // SAFETY: our committed memory.
            unsafe { std::slice::from_raw_parts(self.0, self.1) }
        }
    }

    impl Drop for Title {
        fn drop(&mut self) {
            // SAFETY: our allocation; no heap is open on it any more.
            unsafe { mem::VirtualFree(self.0.cast(), 0, mem::MEM_RELEASE) };
        }
    }

    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum Way {
        Shared,
        Card,
        Upload,
        Combined,
        Bar,

        InPlace,
        /// In place, opened and released every frame.
        InPlaceEachFrame,
    }

    impl Way {
        fn name(self) -> &'static str {
            match self {
                Way::Shared => "shared WB L0 (today, small)",
                Way::Card => "WB staging + copy to VRAM (today, big)",
                Way::Upload => "UPLOAD heap (WC)",
                Way::Combined => "custom WC L0",
                Way::Bar => "GPU_UPLOAD (ReBAR)",

                Way::InPlace => "in place, opened once",
                Way::InPlaceEachFrame => "in place, opened each frame",
            }
        }
    }

    fn bar_supported(device: &ID3D12Device) -> bool {
        let mut o = D3D12_FEATURE_DATA_D3D12_OPTIONS16::default();
        // SAFETY: writes `o` of the given size.
        let ok = unsafe { device.CheckFeatureSupport(D3D12_FEATURE_D3D12_OPTIONS16, (&raw mut o).cast(), size_of_val(&o) as u32).is_ok() };
        ok && o.GPUUploadHeapSupported.as_bool()
    }

    /// What one frame of a way costs: CPU microseconds to fill (or open),
    /// card microseconds to copy and to read, CPU microseconds to let go.
    #[derive(Default, Clone, Copy)]
    struct Cost {
        fill: f64,
        copy: f64,
        read: f64,
        close: f64,
    }

    fn median(mut v: Vec<f64>) -> f64 {
        v.sort_by(f64::total_cmp);
        v[v.len() / 2]
    }

    /// The buffer a way's shader reads, and the CPU's side if any.
    struct Made {
        read: ID3D12Resource,
        cpu: Option<*mut u8>,
        staging: Option<ID3D12Resource>,
        /// For in place: the heap, which must live while the buffer is read.
        _heap: Option<ID3D12Heap>,
    }

    fn open_in_place(device: &ID3D12Device, title: &Title) -> windows::core::Result<(ID3D12Heap, ID3D12Resource)> {
        // The whole allocation, by its base: `ID3D12Device3`.
        let heap: ID3D12Heap = unsafe { device.cast::<ID3D12Device3>()?.OpenExistingHeapFromAddress(title.0 as *const c_void)? };
        let flags = D3D12_RESOURCE_FLAG_ALLOW_CROSS_ADAPTER;
        let mut placed: Option<ID3D12Resource> = None;
        // SAFETY: plain creation in a live heap at its start.
        unsafe { device.CreatePlacedResource(&heap, 0, &desc(title.1 as u64, flags), D3D12_RESOURCE_STATE_COMMON, None, &mut placed)? };
        Ok((heap, placed.ok_or_else(windows::core::Error::empty)?))
    }

    fn make(rig: &Rig, way: Way, title: &Title) -> windows::core::Result<Made> {
        let d = &rig.device;
        let size = title.1 as u64;
        let none = D3D12_RESOURCE_FLAG_NONE;
        let common = D3D12_RESOURCE_STATE_COMMON;
        let custom = |page| D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_CUSTOM,
            CPUPageProperty: page,
            MemoryPoolPreference: D3D12_MEMORY_POOL_L0,
            ..Default::default()
        };
        Ok(match way {
            Way::Shared => {
                let b = heap_buffer(d, &custom(D3D12_CPU_PAGE_PROPERTY_WRITE_BACK), size, D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS, common)?;
                Made { cpu: Some(mapped(&b)?), read: b, staging: None, _heap: None }
            }
            Way::Card => {
                let s = heap_buffer(d, &custom(D3D12_CPU_PAGE_PROPERTY_WRITE_BACK), size, D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS, common)?;
                let b = committed(d, D3D12_HEAP_TYPE_DEFAULT, size, D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS, D3D12_RESOURCE_STATE_COPY_DEST)?;
                Made { cpu: Some(mapped(&s)?), read: b, staging: Some(s), _heap: None }
            }
            Way::Upload => {
                let b = committed(d, D3D12_HEAP_TYPE_UPLOAD, size, none, D3D12_RESOURCE_STATE_GENERIC_READ)?;
                Made { cpu: Some(mapped(&b)?), read: b, staging: None, _heap: None }
            }
            Way::Combined => {
                let b = heap_buffer(d, &custom(D3D12_CPU_PAGE_PROPERTY_WRITE_COMBINE), size, none, common)?;
                Made { cpu: Some(mapped(&b)?), read: b, staging: None, _heap: None }
            }
            Way::Bar => {
                let b = committed(d, D3D12_HEAP_TYPE_GPU_UPLOAD, size, none, common)?;
                Made { cpu: Some(mapped(&b)?), read: b, staging: None, _heap: None }
            }
            Way::InPlace | Way::InPlaceEachFrame => {
                let (heap, b) = open_in_place(d, title)?;
                Made { cpu: None, read: b, staging: None, _heap: Some(heap) }
            }
        })
    }

    /// One frame: the CPU fills (title memory changed), the card copies if
    /// the way has two sides, then reads it `passes` times.
    fn frame(rig: &mut Rig, way: Way, made: &mut Made, title: &Title, passes: u32) -> windows::core::Result<Cost> {
        let size = title.1 as u64;
        let started = std::time::Instant::now();
        if way == Way::InPlaceEachFrame {
            let (heap, b) = open_in_place(&rig.device, title)?;
            made.read = b;
            made._heap = Some(heap);
        } else if let Some(cpu) = made.cpu {
            // As a refresh copies: the safe copy, title memory to our side.
            // SAFETY: both ranges are ours and `size` bytes long.
            assert!(unsafe { crate::safecopy::copy(cpu, title.0, title.1) });
        }
        let fill = started.elapsed().as_secs_f64() * 1e6;
        let read = made.read.clone();
        let staging = made.staging.clone();
        // SAFETY: a live buffer.
        let gpu = unsafe { read.GetGPUVirtualAddress() };
        rig.run(|list, rig| {
            rig.stamp(list, 0);
            if let Some(s) = &staging {
                // SAFETY: an open list; both buffers live, `size` bytes.
                unsafe {
                    list.CopyBufferRegion(&read, 0, s, 0, size);
                    list.ResourceBarrier(&[turn(&read, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE)]);
                }
            }
            rig.stamp(list, 1);
            for _ in 0..passes {
                rig.shade(list, gpu, size, false);
            }
            rig.stamp(list, 2);
            if staging.is_some() {
                // SAFETY: as above.
                unsafe { list.ResourceBarrier(&[turn(&read, D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE, D3D12_RESOURCE_STATE_COPY_DEST)]) };
            }
            rig.resolve(list, 3);
        })?;
        let t = rig.between(3);
        let started = std::time::Instant::now();
        if way == Way::InPlaceEachFrame {
            // The list has run (the fence): the heap may go.
            made._heap = None;
            made.read = rig.sink.clone();
        }
        let close = started.elapsed().as_secs_f64() * 1e6;
        Ok(Cost { fill, copy: t[0], read: t[1], close })
    }

    fn measure(rig: &mut Rig, way: Way, title: &Title, passes: u32, frames: usize) -> windows::core::Result<Cost> {
        let mut made = make(rig, way, title)?;
        for _ in 0..2 {
            frame(rig, way, &mut made, title, passes)?;
        }
        let mut all = Vec::new();
        for _ in 0..frames {
            all.push(frame(rig, way, &mut made, title, passes)?);
        }
        Ok(Cost {
            fill: median(all.iter().map(|c| c.fill).collect()),
            copy: median(all.iter().map(|c| c.copy).collect()),
            read: median(all.iter().map(|c| c.read).collect()),
            close: median(all.iter().map(|c| c.close).collect()),
        })
    }

    /// The CPU fills the way's side as a refresh does, the card copies if it
    /// has two sides, and a shader copies what it reads into the sink: the
    /// bytes the shader saw, read back.
    fn seen(rig: &mut Rig, way: Way, made: &mut Made, title: &Title) -> windows::core::Result<Vec<u8>> {
        let size = title.1 as u64;
        if way == Way::InPlaceEachFrame {
            let (heap, b) = open_in_place(&rig.device, title)?;
            made.read = b;
            made._heap = Some(heap);
        } else if let Some(cpu) = made.cpu {
            // SAFETY: both ranges are ours and `size` bytes long.
            assert!(unsafe { crate::safecopy::copy(cpu, title.0, title.1) });
        }
        let read = made.read.clone();
        let staging = made.staging.clone();
        // SAFETY: a live buffer.
        let gpu = unsafe { read.GetGPUVirtualAddress() };
        rig.run(|list, rig| {
            if let Some(s) = &staging {
                // SAFETY: an open list; both buffers live, `size` bytes.
                unsafe {
                    list.CopyBufferRegion(&read, 0, s, 0, size);
                    list.ResourceBarrier(&[turn(&read, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE)]);
                }
            }
            rig.shade(list, gpu, size, true);
            if staging.is_some() {
                // SAFETY: as above.
                unsafe { list.ResourceBarrier(&[turn(&read, D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE, D3D12_RESOURCE_STATE_COPY_DEST)]) };
            }
        })?;
        if way == Way::InPlaceEachFrame {
            made._heap = None;
            made.read = rig.sink.clone();
        }
        rig.sink_bytes(title.1)
    }

    /// Every way shows the shader the title's bytes, and the title's next
    /// bytes after it wrote them (in place without any copy).
    #[test]
    fn every_way_shows_the_shader_the_titles_bytes() {
        let Some((device, name)) = device() else {
            eprintln!("no card: skipped");
            return;
        };
        assert!(crate::safecopy::on());
        const SIZE: usize = 1 << 20;
        let mut rig = Rig::new(&device, SIZE as u64).expect("the rig");
        let bar = bar_supported(&device);
        for way in [Way::Shared, Way::Card, Way::Upload, Way::Combined, Way::Bar, Way::InPlace, Way::InPlaceEachFrame] {
            if way == Way::Bar && !bar {
                continue;
            }
            let title = Title::new(SIZE, 0x5eed + way as u64);
            let mut made = make(&rig, way, &title).unwrap_or_else(|e| panic!("{name}: {:?} not made: {e}", way));
            for seed in [7u64, 8, 9] {
                title.fill(seed * 0x9e37_79b9 + way as u64);
                let got = seen(&mut rig, way, &mut made, &title).expect("the run");
                assert!(got == title.bytes(), "{:?}, seed {seed}: the shader saw other bytes", way);
            }
        }
    }
    /// The table of docs/shared-memory.md.
    #[test]
    #[ignore = "benchmark on the PC's card; see the module notes"]
    fn bench_shared_memory() {
        let Some((device, name)) = device() else {
            eprintln!("no card");
            return;
        };
        assert!(crate::safecopy::on());
        let mut rig = Rig::new(&device, 1 << 20).expect("the rig");
        let bar = bar_supported(&device);
        eprintln!("card: {name}; GPU_UPLOAD (ReBAR) supported: {bar}");
        eprintln!("all figures are medians of 9 frames, microseconds; GB/s = bytes x passes / read time");
        eprintln!("| way | size | passes | CPU fill/open | card copy | card read | GB/s | CPU close | total card+CPU |");
        eprintln!("|---|---|---|---|---|---|---|---|---|");
        let ways = [Way::Shared, Way::Card, Way::Upload, Way::Combined, Way::Bar, Way::InPlace, Way::InPlaceEachFrame];
        for &size in &[K64, 1 << 20, 4 << 20, 16 << 20, 64 << 20] {
            let title = Title::new(size, size as u64);
            for passes in [1u32, 16] {
                for &way in &ways {
                    if way == Way::Bar && !bar {
                        continue;
                    }
                    match measure(&mut rig, way, &title, passes, 9) {
                        Ok(c) => {
                            let gbs = size as f64 * f64::from(passes) / (c.read * 1e-6) / 1e9;
                            let label = if size >= 1 << 20 { format!("{} MiB", size >> 20) } else { format!("{} KiB", size >> 10) };
                            eprintln!(
                                "| {} | {label} | {passes} | {:.1} | {:.1} | {:.1} | {gbs:.1} | {:.1} | {:.1} |",
                                way.name(),
                                c.fill,
                                c.copy,
                                c.read,
                                c.close,
                                c.fill + c.copy + c.read + c.close
                            );
                        }
                        Err(e) => eprintln!("| {} | {} KiB | {passes} | failed: {e} |", way.name(), size >> 10),
                    }
                }
            }
        }
    }

    fn private_now() -> u64 {
        use windows_sys::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        let mut c = PROCESS_MEMORY_COUNTERS { cb: size_of::<PROCESS_MEMORY_COUNTERS>() as u32, ..Default::default() };
        // SAFETY: our own process and a sized structure.
        unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) };
        c.PagefileUsage as u64
    }

    /// What of the PC's memory host buffers and list allocators hold
    /// (cargo test -p victory-gpu --lib -- --ignored --nocapture
    /// what_buffers_and_allocators_hold). Measured on an RTX 4070 SUPER: a
    /// shared buffer (a window's kind) is in the working set and the card's
    /// shared account whole from its making, written or not, so a window's
    /// pages nothing copied are the PC's memory all the same; an allocator
    /// keeps the command memory of its largest recording over a reset, and
    /// the driver keeps it after the allocator is let go, so making allocators
    /// anew gives nothing back.
    #[test]
    #[ignore = "measures; prints"]
    fn what_buffers_and_allocators_hold() {
        let Some(host) = crate::host::host() else { return };
        let ws = || {
            use windows_sys::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
            use windows_sys::Win32::System::Threading::GetCurrentProcess;
            let mut c = PROCESS_MEMORY_COUNTERS { cb: size_of::<PROCESS_MEMORY_COUNTERS>() as u32, ..Default::default() };
            // SAFETY: our own process and a sized structure.
            unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) };
            c.WorkingSetSize as u64
        };
        let say = |what: &str| {
            let v = host.video_memory().unwrap_or_default();
            println!(
                "{what}: private {} MB, working set {} MB, avail phys {} MB, gpu shared used {} MB",
                private_now() >> 20,
                ws() >> 20,
                crate::host::available_physical_ram().unwrap_or(0) >> 20,
                v[3] >> 20
            );
        };
        say("start");
        let bufs: Vec<_> = (0..64).map(|_| host.shared_buffer(16 << 20).unwrap()).collect();
        std::thread::sleep(std::time::Duration::from_millis(500));
        say("64 x 16 MiB shared made");
        for (_, d) in &bufs {
            // SAFETY: the buffer's mapping, 16 MiB.
            unsafe { d.write_volatile(1) };
        }
        say("first byte of each written");
        for (_, d) in &bufs {
            // SAFETY: as above.
            unsafe { std::ptr::write_bytes(*d, 1, 8 << 20) };
        }
        say("half of each written");
        let ups: Vec<_> = (0..16).map(|_| host.upload_buffer(64 << 20).unwrap()).collect();
        std::thread::sleep(std::time::Duration::from_millis(500));
        say("16 x 64 MiB upload made");
        drop(ups);
        drop(bufs);
        say("let go");
        let device = &host.device;
        let a = host.upload_buffer(1 << 20).unwrap().0;
        let b = host.shared_buffer(1 << 20).unwrap().0;
        // SAFETY (the blocks below): plain creation and recording on a live
        // device; nothing is executed.
        let lists: Vec<(ID3D12CommandAllocator, ID3D12GraphicsCommandList)> = (0..8)
            .map(|_| unsafe {
                let al: ID3D12CommandAllocator = device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).unwrap();
                let l: ID3D12GraphicsCommandList = device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &al, None).unwrap();
                (al, l)
            })
            .collect();
        say("8 lists made");
        for (_, l) in &lists {
            for i in 0..200_000u64 {
                unsafe { l.CopyBufferRegion(&b, (i * 16) % (1 << 20), &a, 0, 16) };
            }
            unsafe { l.Close().unwrap() };
        }
        say("200,000 copies recorded in each");
        for (al, l) in &lists {
            unsafe {
                al.Reset().unwrap();
                l.Reset(al, None).unwrap();
                l.CopyBufferRegion(&b, 0, &a, 0, 16);
                l.Close().unwrap();
            }
        }
        say("reset, one copy each");
        let fresh: Vec<ID3D12CommandAllocator> = lists
            .iter()
            .map(|(_, l)| unsafe {
                let al: ID3D12CommandAllocator = device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).unwrap();
                l.Reset(&al, None).unwrap();
                l.CopyBufferRegion(&b, 0, &a, 0, 16);
                l.Close().unwrap();
                al
            })
            .collect();
        let lists: Vec<_> = lists.into_iter().map(|(_, l)| l).collect();
        say("old allocators let go, new ones with one copy");
        drop((fresh, lists));
        std::thread::sleep(std::time::Duration::from_millis(1500));
        say("all lists and allocators let go");
    }

    /// Which D3D12 objects count in the private bytes (`PagefileUsage`) the
    /// memory line's `private=` reads. `cargo test -p victory-gpu --lib --
    /// --ignored --nocapture what_counts_as_private`.
    #[test]
    #[ignore = "measures; prints"]
    fn what_counts_as_private() {
        let Some((device, name)) = device() else { return };
        const G: u64 = 1 << 30;
        let report = |what: &str, before: u64| {
            println!("{what}: private {:+} MB", (private_now() as i64 - before as i64) / (1 << 20));
        };
        println!("card: {name}");
        let mut kept: Vec<windows::core::IUnknown> = Vec::new();
        // SAFETY: plain object creation on a live device.
        unsafe {
            for (what, kind, flags) in [
                ("default heap of textures 1 GiB", D3D12_HEAP_TYPE_DEFAULT, D3D12_HEAP_FLAG_ALLOW_ONLY_NON_RT_DS_TEXTURES),
                ("default heap of buffers 1 GiB", D3D12_HEAP_TYPE_DEFAULT, D3D12_HEAP_FLAG_ALLOW_ONLY_BUFFERS),
                ("upload heap 1 GiB", D3D12_HEAP_TYPE_UPLOAD, D3D12_HEAP_FLAG_ALLOW_ONLY_BUFFERS),
            ] {
                let before = private_now();
                let mut heap: Option<ID3D12Heap> = None;
                device
                    .CreateHeap(
                        &D3D12_HEAP_DESC {
                            SizeInBytes: G,
                            Properties: D3D12_HEAP_PROPERTIES { Type: kind, ..Default::default() },
                            Alignment: 0,
                            Flags: flags,
                        },
                        &mut heap,
                    )
                    .expect("a heap");
                report(what, before);
                kept.push(heap.expect("a heap").cast().expect("unknown"));
            }
            let before = private_now();
            let tex = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
                Width: 8192,
                Height: 8192,
                DepthOrArraySize: 4,
                MipLevels: 1,
                Format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R8G8B8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                ..Default::default()
            };
            let mut t: Option<ID3D12Resource> = None;
            device
                .CreateCommittedResource(
                    &D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() },
                    D3D12_HEAP_FLAG_NONE,
                    &tex,
                    D3D12_RESOURCE_STATE_COMMON,
                    None,
                    &mut t,
                )
                .expect("a texture");
            report("committed texture 1 GiB", before);
            kept.push(t.expect("a texture").cast().expect("unknown"));
            let before = private_now();
            drop(kept);
            report("all let go", before);
        }
    }}
