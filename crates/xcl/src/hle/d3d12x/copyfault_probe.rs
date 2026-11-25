//! Local probe (not for upstream): does a copy of a reserved texture fault when its tiles are in a
//! CUSTOM L0 heap, split across L1 / L0 heaps, or NULL-mapped?
//! `cargo test -p xcl --lib copyfault_probe -- --ignored --nocapture --test-threads=1`
#![cfg(test)]
use std::ffi::c_void;
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_0;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;

const TILES: u32 = 32;
const PAGE: u64 = 65536;

#[derive(Clone, Copy, Debug, PartialEq)]
enum T {
    Vram,
    Sys,
    Null,
}

unsafe fn case(name: &str, map: impl Fn(u32) -> T, kind: D3D12_COMMAND_LIST_TYPE, write: bool) {
    let factory: IDXGIFactory4 = CreateDXGIFactory1().unwrap();
    let adapter = factory.EnumAdapters1(0).unwrap();
    let d = adapter.GetDesc1().unwrap();
    let mut dev: Option<ID3D12Device> = None;
    D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_11_0, &mut dev).unwrap();
    let dev = dev.unwrap();
    let heap = |props: D3D12_HEAP_PROPERTIES| {
        let desc = D3D12_HEAP_DESC { SizeInBytes: TILES as u64 * PAGE, Properties: props, Alignment: 0, Flags: D3D12_HEAP_FLAG_NONE };
        let mut h: Option<ID3D12Heap> = None;
        dev.CreateHeap(&desc, &mut h).unwrap();
        h.unwrap()
    };
    let vram = heap(D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() });
    let sys = heap(D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_CUSTOM, CPUPageProperty: D3D12_CPU_PAGE_PROPERTY_NOT_AVAILABLE, MemoryPoolPreference: D3D12_MEMORY_POOL_L0, ..Default::default() });
    let tdesc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
        Width: 1024,
        Height: 512,
        DepthOrArraySize: 1,
        MipLevels: 1,
        Format: DXGI_FORMAT_R8G8B8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Layout: D3D12_TEXTURE_LAYOUT_64KB_UNDEFINED_SWIZZLE,
        Flags: D3D12_RESOURCE_FLAG_NONE,
        ..Default::default()
    };
    let mut tex: Option<ID3D12Resource> = None;
    dev.CreateReservedResource(&tdesc, D3D12_RESOURCE_STATE_COMMON, None, &mut tex).unwrap();
    let tex = tex.unwrap();
    let qdesc = D3D12_COMMAND_QUEUE_DESC { Type: kind, ..Default::default() };
    let queue: ID3D12CommandQueue = dev.CreateCommandQueue(&qdesc).unwrap();
    for i in 0..TILES {
        let t = map(i);
        let coord = D3D12_TILED_RESOURCE_COORDINATE { X: 0, Y: 0, Z: 0, Subresource: 0 };
        // tile i of a 1024x512 RGBA8 texture: 128x128 texels per 64 KiB tile, 8 x 4 tiles
        let coord = D3D12_TILED_RESOURCE_COORDINATE { X: i % 8, Y: i / 8, ..coord };
        let region = D3D12_TILE_REGION_SIZE { NumTiles: 1, UseBox: false.into(), ..Default::default() };
        let (flag, h) = match t {
            T::Vram => (D3D12_TILE_RANGE_FLAG_NONE, Some(&vram)),
            T::Sys => (D3D12_TILE_RANGE_FLAG_NONE, Some(&sys)),
            T::Null => (D3D12_TILE_RANGE_FLAG_NULL, None),
        };
        let off = i;
        let one = 1u32;
        queue.UpdateTileMappings(&tex, 1, Some(&coord), Some(&region), h, 1, Some(&flag), Some(&off), Some(&one), D3D12_TILE_MAPPING_FLAG_NONE);
    }
    let mut fp = D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default();
    let mut total = 0u64;
    dev.GetCopyableFootprints(&tdesc, 0, 1, 0, Some(&mut fp), None, None, Some(&mut total));
    let buf = |ty: D3D12_HEAP_TYPE, state: D3D12_RESOURCE_STATES| {
        let bd = D3D12_RESOURCE_DESC { Dimension: D3D12_RESOURCE_DIMENSION_BUFFER, Width: total, Height: 1, DepthOrArraySize: 1, MipLevels: 1, SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 }, Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR, ..Default::default() };
        let mut r: Option<ID3D12Resource> = None;
        dev.CreateCommittedResource(&D3D12_HEAP_PROPERTIES { Type: ty, ..Default::default() }, D3D12_HEAP_FLAG_NONE, &bd, state, None, &mut r).unwrap();
        r.unwrap()
    };
    let up = buf(D3D12_HEAP_TYPE_UPLOAD, D3D12_RESOURCE_STATE_GENERIC_READ);
    let rb = buf(D3D12_HEAP_TYPE_READBACK, D3D12_RESOURCE_STATE_COPY_DEST);
    let mut p: *mut c_void = std::ptr::null_mut();
    up.Map(0, None, Some(&mut p)).unwrap();
    let words = std::slice::from_raw_parts_mut(p as *mut u32, (total / 4) as usize);
    for (i, w) in words.iter_mut().enumerate() {
        *w = i as u32 ^ 0x5a5a_0000;
    }
    up.Unmap(0, None);
    let alloc: ID3D12CommandAllocator = dev.CreateCommandAllocator(kind).unwrap();
    let list: ID3D12GraphicsCommandList = dev.CreateCommandList(0, kind, &alloc, None).unwrap();
    let tloc = D3D12_TEXTURE_COPY_LOCATION { pResource: std::mem::transmute_copy(&tex), Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX, Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: 0 } };
    let uloc = D3D12_TEXTURE_COPY_LOCATION { pResource: std::mem::transmute_copy(&up), Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT, Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint: fp } };
    let rloc = D3D12_TEXTURE_COPY_LOCATION { pResource: std::mem::transmute_copy(&rb), Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT, Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint: fp } };
    if write {
        list.CopyTextureRegion(&tloc, 0, 0, 0, &uloc, None);
        if kind == D3D12_COMMAND_LIST_TYPE_DIRECT {
            let b = D3D12_RESOURCE_BARRIER { Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION, Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE, Anonymous: D3D12_RESOURCE_BARRIER_0 { Transition: std::mem::ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER { pResource: std::mem::transmute_copy(&tex), Subresource: 0, StateBefore: D3D12_RESOURCE_STATE_COPY_DEST, StateAfter: D3D12_RESOURCE_STATE_COPY_SOURCE }) } };
            list.ResourceBarrier(&[b]);
        }
    }
    list.CopyTextureRegion(&rloc, 0, 0, 0, &tloc, None);
    list.Close().unwrap();
    queue.ExecuteCommandLists(&[Some(list.cast().unwrap())]);
    let fence: ID3D12Fence = dev.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap();
    queue.Signal(&fence, 1).unwrap();
    let t0 = std::time::Instant::now();
    while fence.GetCompletedValue() < 1 && t0.elapsed().as_secs() < 10 {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let removed = dev.GetDeviceRemovedReason();
    let mut ok = 0;
    let mut zero = 0;
    let mut bad = 0;
    if removed.is_ok() && fence.GetCompletedValue() >= 1 {
        let mut q: *mut c_void = std::ptr::null_mut();
        rb.Map(0, None, Some(&mut q)).unwrap();
        let got = std::slice::from_raw_parts(q as *const u32, (total / 4) as usize);
        for (i, &g) in got.iter().enumerate() {
            if !write {
                if g == 0 { zero += 1 } else { bad += 1 }
            } else if g == i as u32 ^ 0x5a5a_0000 {
                ok += 1
            } else if g == 0 {
                zero += 1
            } else {
                bad += 1
            }
        }
        rb.Unmap(0, None);
    }
    let counts = (0..TILES).fold([0; 3], |mut c, i| {
        c[map(i) as usize] += 1;
        c
    });
    println!(
        "PROBE {:<34} {} queue={:?} write={} tiles vram/sys/null={:?}: removed={:?} fence={} words ok={} zero={} other={}",
        name,
        String::from_utf16_lossy(&d.Description).trim_end_matches('\0'),
        kind.0,
        write,
        counts,
        removed.err().map(|e| e.code()),
        fence.GetCompletedValue(),
        ok,
        zero,
        bad
    );
}

macro_rules! probe {
    ($n:ident, $map:expr, $k:expr, $w:expr) => {
        #[test]
        #[ignore]
        fn $n() {
            unsafe { case(stringify!($n), $map, $k, $w) }
        }
    };
}
use T::*;
const C: D3D12_COMMAND_LIST_TYPE = D3D12_COMMAND_LIST_TYPE_COPY;
const D: D3D12_COMMAND_LIST_TYPE = D3D12_COMMAND_LIST_TYPE_DIRECT;
probe!(a_vram_copyq, |_| Vram, C, true);
probe!(b_sys_copyq, |_| Sys, C, true);
probe!(c_split_copyq, |i| if i < 16 { Vram } else { Sys }, C, true);
probe!(d_sys_direct, |_| Sys, D, true);
probe!(e_split_direct, |i| if i < 16 { Vram } else { Sys }, D, true);
probe!(f_half_null_copyq_write, |i| if i < 16 { Vram } else { Null }, C, true);
probe!(g_half_null_direct_write, |i| if i < 16 { Vram } else { Null }, D, true);
probe!(h_all_null_copyq_read, |_| Null, C, false);
probe!(i_all_null_direct_read, |_| Null, D, false);
probe!(j_sys_null_copyq_write, |i| if i < 16 { Sys } else { Null }, C, true);

/// Unmap (NULL) a reserved texture's tiles on a second queue while copies into it run on the
/// copy queue, with no ordering between the two (what `vram::unmap_superseded` can do).
unsafe fn race(name: &str, kind: D3D12_COMMAND_LIST_TYPE, remap_to_other: bool) {
    let factory: IDXGIFactory4 = CreateDXGIFactory1().unwrap();
    let adapter = factory.EnumAdapters1(0).unwrap();
    let mut dev: Option<ID3D12Device> = None;
    D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_11_0, &mut dev).unwrap();
    let dev = dev.unwrap();
    const W: u64 = 4096;
    const H: u32 = 4096; // 64 MiB RGBA8: 1024 tiles
    const T: u32 = 1024;
    let hdesc = D3D12_HEAP_DESC { SizeInBytes: 2 * T as u64 * PAGE, Properties: D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() }, Alignment: 0, Flags: D3D12_HEAP_FLAG_NONE };
    let mut heap: Option<ID3D12Heap> = None;
    dev.CreateHeap(&hdesc, &mut heap).unwrap();
    let heap = heap.unwrap();
    let tdesc = D3D12_RESOURCE_DESC { Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D, Width: W, Height: H, DepthOrArraySize: 1, MipLevels: 1, Format: DXGI_FORMAT_R8G8B8A8_UNORM, SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 }, Layout: D3D12_TEXTURE_LAYOUT_64KB_UNDEFINED_SWIZZLE, ..Default::default() };
    let mut tex: Option<ID3D12Resource> = None;
    dev.CreateReservedResource(&tdesc, D3D12_RESOURCE_STATE_COMMON, None, &mut tex).unwrap();
    let tex = tex.unwrap();
    let mapq: ID3D12CommandQueue = dev.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC { Type: D3D12_COMMAND_LIST_TYPE_DIRECT, ..Default::default() }).unwrap();
    let q: ID3D12CommandQueue = dev.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC { Type: kind, ..Default::default() }).unwrap();
    let map_all = |offset: u32, null: bool| {
        let coord = D3D12_TILED_RESOURCE_COORDINATE::default();
        let region = D3D12_TILE_REGION_SIZE { NumTiles: T, UseBox: false.into(), ..Default::default() };
        let flag = if null { D3D12_TILE_RANGE_FLAG_NULL } else { D3D12_TILE_RANGE_FLAG_NONE };
        mapq.UpdateTileMappings(&tex, 1, Some(&coord), Some(&region), if null { None } else { Some(&heap) }, 1, Some(&flag), Some(&offset), Some(&T), D3D12_TILE_MAPPING_FLAG_NONE);
    };
    map_all(0, false);
    let mfence: ID3D12Fence = dev.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap();
    mapq.Signal(&mfence, 1).unwrap();
    q.Wait(&mfence, 1).unwrap();
    let mut fp = D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default();
    let mut total = 0u64;
    dev.GetCopyableFootprints(&tdesc, 0, 1, 0, Some(&mut fp), None, None, Some(&mut total));
    let bd = D3D12_RESOURCE_DESC { Dimension: D3D12_RESOURCE_DIMENSION_BUFFER, Width: total, Height: 1, DepthOrArraySize: 1, MipLevels: 1, SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 }, Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR, ..Default::default() };
    let mut up: Option<ID3D12Resource> = None;
    dev.CreateCommittedResource(&D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_UPLOAD, ..Default::default() }, D3D12_HEAP_FLAG_NONE, &bd, D3D12_RESOURCE_STATE_GENERIC_READ, None, &mut up).unwrap();
    let up = up.unwrap();
    let alloc: ID3D12CommandAllocator = dev.CreateCommandAllocator(kind).unwrap();
    let list: ID3D12GraphicsCommandList = dev.CreateCommandList(0, kind, &alloc, None).unwrap();
    let tloc = D3D12_TEXTURE_COPY_LOCATION { pResource: std::mem::transmute_copy(&tex), Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX, Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: 0 } };
    let uloc = D3D12_TEXTURE_COPY_LOCATION { pResource: std::mem::transmute_copy(&up), Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT, Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint: fp } };
    // ~40 full 64 MiB copies: long enough on the GPU for the remaps to land in the middle
    for _ in 0..40 {
        list.CopyTextureRegion(&tloc, 0, 0, 0, &uloc, None);
    }
    list.Close().unwrap();
    let fence: ID3D12Fence = dev.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap();
    let mut remaps = 0;
    let t0 = std::time::Instant::now();
    for round in 0..20u64 {
        q.ExecuteCommandLists(&[Some(list.cast().unwrap())]);
        q.Signal(&fence, round + 1).unwrap();
        // while it runs: flip the mapping to NULL (or to other tiles) and back, unordered
        while fence.GetCompletedValue() < round + 1 && t0.elapsed().as_secs() < 60 {
            map_all(if remap_to_other { T } else { 0 }, !remap_to_other);
            map_all(0, false);
            remaps += 2;
            std::thread::sleep(std::time::Duration::from_micros(300));
        }
        if dev.GetDeviceRemovedReason().is_err() {
            break;
        }
    }
    let removed = dev.GetDeviceRemovedReason();
    println!("PROBE {name:<28} queue={:?} remaps={remaps} rounds_done={} removed={:?} {:.1}s", kind.0, fence.GetCompletedValue(), removed.err().map(|e| e.code()), t0.elapsed().as_secs_f32());
}

#[test]
#[ignore]
fn r_null_during_copyq() {
    unsafe { race("r_null_during_copyq", C, false) }
}
#[test]
#[ignore]
fn r_null_during_direct() {
    unsafe { race("r_null_during_direct", D, false) }
}
#[test]
#[ignore]
fn r_other_tiles_during_copyq() {
    unsafe { race("r_other_tiles_during_copyq", C, true) }
}
