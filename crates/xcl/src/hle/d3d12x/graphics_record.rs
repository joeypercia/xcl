//! `XCL_GRAPHICS_RECORD=1`: each graphics pipeline a title's console shaders were translated for,
//! kept in `graphics-pipelines.txt` beside the DXIL cache, a pipeline a line, so that it can be
//! built again without the title (#2088).
//!
//! Compute pipelines can be built ahead of a run from the shader pack alone
//! (`xcl shaders <pack> --precompile --only cs --pipelines`). A graphics pipeline cannot: its
//! state (blend, rasterizer, depth, formats, input layout and which VS goes with which PS) only
//! exists once the title creates it. After an XCL change that alters a translation, or a driver
//! update, every one of them is built cold again in the title's next run. A line holds the VS and
//! PS by the hash of their console containers (`[SHADER]` lines; the shaders themselves are not
//! written) and the description's state, enough to make the same description again
//! (`Recorded::desc`) – the one whose driver blob the title's run loads.
//!
//! Recorded: pipelines of a console VS (and a console PS, or none) without a GS, HS, DS or stream
//! output (`translated_graphics_pso`'s VS / PS path). Mesh, GS and two-pass pipelines are not.

use std::collections::HashSet;
use std::ffi::CString;
use std::io::Write;
use std::sync::{Mutex, OnceLock};
use windows::core::PCSTR;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;

/// The first field of a line: lines of another layout are passed over.
const VERSION: &str = "gfx1";

/// `XCL_GRAPHICS_RECORD=1`.
pub fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("XCL_GRAPHICS_RECORD").is_ok_and(|v| v.trim() == "1"))
}

/// The file, beside `used-pipelines.txt`.
pub fn path() -> std::path::PathBuf {
    crate::gpu::recomp::dxc::cache_dir().join("graphics-pipelines.txt")
}

/// The bytes of a plain-data value, as hexadecimal.
fn pod_hex<T: Copy>(v: &T) -> String {
    let bytes = unsafe { std::slice::from_raw_parts(v as *const T as *const u8, std::mem::size_of::<T>()) };
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A plain-data value from `pod_hex`'s text (None: not hexadecimal, or not its size).
fn pod_from_hex<T: Copy>(s: &str) -> Option<T> {
    if s.len() != 2 * std::mem::size_of::<T>() || !s.is_ascii() {
        return None;
    }
    let bytes: Option<Vec<u8>> = (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok()).collect();
    let bytes = bytes?;
    // SAFETY: T is plain data (the D3D12 state structs) and the bytes are its size
    Some(unsafe { std::ptr::read_unaligned(bytes.as_ptr() as *const T) })
}

/// One line for the pipeline of `d` with the console VS of hash `vs` and the console PS of hash
/// `ps` (0: none). None: a pipeline this does not record (stream output, a GS / HS / DS, an input
/// element whose semantic name cannot be written in a line).
///
/// # Safety
/// `d`'s input layout pointers are valid (as for the title's `CreateGraphicsPipelineState`).
pub unsafe fn line(d: &D3D12_GRAPHICS_PIPELINE_STATE_DESC, vs: u64, ps: u64) -> Option<String> {
    let has = |b: &D3D12_SHADER_BYTECODE| !b.pShaderBytecode.is_null() && b.BytecodeLength != 0;
    if d.StreamOutput.NumEntries > 0 || has(&d.GS) || has(&d.HS) || has(&d.DS) {
        return None;
    }
    let mut il = Vec::new();
    let n = d.InputLayout.NumElements as usize;
    if n > 0 {
        if d.InputLayout.pInputElementDescs.is_null() {
            return None;
        }
        for e in std::slice::from_raw_parts(d.InputLayout.pInputElementDescs, n) {
            let name = std::ffi::CStr::from_ptr(e.SemanticName.0 as *const std::ffi::c_char).to_str().ok()?;
            if name.is_empty() || !name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_') {
                return None;
            }
            il.push(format!("{name},{},{},{},{},{},{}", e.SemanticIndex, e.Format.0, e.InputSlot, e.AlignedByteOffset, e.InputSlotClass.0, e.InstanceDataStepRate));
        }
    }
    let rtv: Vec<String> = d.RTVFormats.iter().map(|f| f.0.to_string()).collect();
    Some(format!(
        "{VERSION}\t{vs:016x}\t{ps:016x}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{},{}\t{}\t{}\t{}",
        pod_hex(&d.BlendState),
        d.SampleMask,
        pod_hex(&d.RasterizerState),
        pod_hex(&d.DepthStencilState),
        d.IBStripCutValue.0,
        d.PrimitiveTopologyType.0,
        d.NumRenderTargets,
        rtv.join(","),
        d.DSVFormat.0,
        d.SampleDesc.Count,
        d.SampleDesc.Quality,
        d.NodeMask,
        d.Flags.0,
        il.join(";"),
    ))
}

/// A recorded pipeline: its shaders by hash and its description's state, with the input layout
/// it points at.
pub struct Recorded {
    pub vs: u64,
    /// 0: no PS
    pub ps: u64,
    state: D3D12_GRAPHICS_PIPELINE_STATE_DESC,
    elems: Vec<D3D12_INPUT_ELEMENT_DESC>,
    _names: Vec<CString>,
}

// the input layout points into `_names`, which moves with it (CString keeps its buffer); nothing
// changes it after `parse`, so threads may read it together
unsafe impl Send for Recorded {}
unsafe impl Sync for Recorded {}

impl Recorded {
    /// The description, with no root signature and no shaders yet; its input layout points into
    /// `self`.
    pub fn desc(&self) -> D3D12_GRAPHICS_PIPELINE_STATE_DESC {
        // SAFETY: the state holds no root signature (None) and no pointers but the input layout,
        // set here: a bitwise copy owns nothing twice
        let mut d = unsafe { std::ptr::read(&self.state) };
        d.InputLayout = D3D12_INPUT_LAYOUT_DESC {
            pInputElementDescs: if self.elems.is_empty() { std::ptr::null() } else { self.elems.as_ptr() },
            NumElements: self.elems.len() as u32,
        };
        d
    }
}

/// A line written by `line` (None: another layout, or not a line of this file).
pub fn parse(l: &str) -> Option<Recorded> {
    let f: Vec<&str> = l.trim_end_matches(['\r', '\n']).split('\t').collect();
    if f.len() != 16 || f[0] != VERSION {
        return None;
    }
    let num = |s: &str| s.parse::<i64>().ok();
    let vs = u64::from_str_radix(f[1], 16).ok().filter(|&v| v != 0)?;
    let ps = u64::from_str_radix(f[2], 16).ok()?;
    let mut d = D3D12_GRAPHICS_PIPELINE_STATE_DESC {
        BlendState: pod_from_hex(f[3])?,
        SampleMask: num(f[4])? as u32,
        RasterizerState: pod_from_hex(f[5])?,
        DepthStencilState: pod_from_hex(f[6])?,
        IBStripCutValue: D3D12_INDEX_BUFFER_STRIP_CUT_VALUE(num(f[7])? as i32),
        PrimitiveTopologyType: D3D12_PRIMITIVE_TOPOLOGY_TYPE(num(f[8])? as i32),
        NumRenderTargets: num(f[9])? as u32,
        DSVFormat: DXGI_FORMAT(num(f[11])? as i32),
        NodeMask: num(f[13])? as u32,
        Flags: D3D12_PIPELINE_STATE_FLAGS(num(f[14])? as i32),
        ..Default::default()
    };
    let rtv: Vec<&str> = f[10].split(',').collect();
    if rtv.len() != d.RTVFormats.len() || d.NumRenderTargets as usize > d.RTVFormats.len() {
        return None;
    }
    for (slot, v) in d.RTVFormats.iter_mut().zip(rtv) {
        *slot = DXGI_FORMAT(num(v)? as i32);
    }
    let (count, quality) = f[12].split_once(',')?;
    d.SampleDesc = DXGI_SAMPLE_DESC { Count: num(count)? as u32, Quality: num(quality)? as u32 };
    let mut names = Vec::new();
    let mut elems = Vec::new();
    for e in f[15].split(';').filter(|e| !e.is_empty()) {
        let p: Vec<&str> = e.split(',').collect();
        if p.len() != 7 {
            return None;
        }
        names.push(CString::new(p[0]).ok()?);
        elems.push(D3D12_INPUT_ELEMENT_DESC {
            SemanticName: PCSTR(names.last().unwrap().as_ptr() as *const u8),
            SemanticIndex: num(p[1])? as u32,
            Format: DXGI_FORMAT(num(p[2])? as i32),
            InputSlot: num(p[3])? as u32,
            AlignedByteOffset: num(p[4])? as u32,
            InputSlotClass: D3D12_INPUT_CLASSIFICATION(num(p[5])? as i32),
            InstanceDataStepRate: num(p[6])? as u32,
        });
    }
    Some(Recorded { vs, ps, state: d, elems, _names: names })
}

/// The recorded pipelines in `text` (lines it cannot read are passed over).
pub fn parse_all(text: &str) -> Vec<Recorded> {
    text.lines().filter_map(parse).collect()
}

/// The lines written so far (this run's and earlier runs'), and the file they go to.
fn file() -> &'static Mutex<(HashSet<u64>, Option<std::fs::File>)> {
    static F: OnceLock<Mutex<(HashSet<u64>, Option<std::fs::File>)>> = OnceLock::new();
    F.get_or_init(|| {
        // (a test neither reads nor writes the user's file)
        if cfg!(test) {
            return Mutex::new((HashSet::new(), None));
        }
        let path = path();
        let seen: HashSet<u64> = std::fs::read_to_string(&path).unwrap_or_default().lines().map(|l| crate::gpu::shader::fnv64(l.as_bytes())).collect();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let file = std::fs::OpenOptions::new().create(true).append(true).open(&path).ok();
        crate::xlog_info!("D3D12.X: XCL_GRAPHICS_RECORD: graphics pipelines are kept in {} ({} there already)", path.display(), seen.len());
        Mutex::new((seen, file))
    })
}

/// The title's pipeline `d` with the console VS `vs` and the console PS `ps` (None: no PS) was
/// built from translated shaders: kept, once, when recording is on.
///
/// # Safety
/// As `line`.
pub unsafe fn note(d: &D3D12_GRAPHICS_PIPELINE_STATE_DESC, vs: &[u8], ps: Option<&[u8]>) {
    if !on() {
        return;
    }
    let Some(l) = line(d, crate::gpu::shader::fnv64(vs), ps.map_or(0, crate::gpu::shader::fnv64)) else { return };
    let mut g = file().lock().unwrap();
    if g.0.insert(crate::gpu::shader::fnv64(l.as_bytes())) {
        if let Some(f) = g.1.as_mut() {
            let _ = writeln!(f, "{l}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A description with every recorded field set to something other than its default.
    fn sample(elems: &[D3D12_INPUT_ELEMENT_DESC]) -> D3D12_GRAPHICS_PIPELINE_STATE_DESC {
        let mut d = D3D12_GRAPHICS_PIPELINE_STATE_DESC::default();
        d.BlendState.IndependentBlendEnable = true.into();
        d.BlendState.RenderTarget[0].BlendEnable = true.into();
        d.BlendState.RenderTarget[0].SrcBlend = D3D12_BLEND_SRC_ALPHA;
        d.BlendState.RenderTarget[0].DestBlend = D3D12_BLEND_INV_SRC_ALPHA;
        d.BlendState.RenderTarget[1].RenderTargetWriteMask = 0x5;
        d.SampleMask = 0xffff_fff0;
        d.RasterizerState.FillMode = D3D12_FILL_MODE_SOLID;
        d.RasterizerState.CullMode = D3D12_CULL_MODE_BACK;
        d.RasterizerState.DepthBias = -7;
        d.RasterizerState.SlopeScaledDepthBias = 1.5;
        d.DepthStencilState.DepthEnable = true.into();
        d.DepthStencilState.DepthFunc = D3D12_COMPARISON_FUNC_GREATER_EQUAL;
        d.DepthStencilState.StencilEnable = true.into();
        d.DepthStencilState.StencilWriteMask = 0x3c;
        d.IBStripCutValue = D3D12_INDEX_BUFFER_STRIP_CUT_VALUE_0xFFFFFFFF;
        d.PrimitiveTopologyType = D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE;
        d.NumRenderTargets = 2;
        d.RTVFormats[0] = DXGI_FORMAT_R16G16B16A16_FLOAT;
        d.RTVFormats[1] = DXGI_FORMAT_R10G10B10A2_UNORM;
        d.DSVFormat = DXGI_FORMAT_D32_FLOAT_S8X24_UINT;
        d.SampleDesc = DXGI_SAMPLE_DESC { Count: 4, Quality: 1 };
        d.NodeMask = 1;
        d.InputLayout = D3D12_INPUT_LAYOUT_DESC { pInputElementDescs: elems.as_ptr(), NumElements: elems.len() as u32 };
        d
    }

    fn elems() -> Vec<D3D12_INPUT_ELEMENT_DESC> {
        vec![
            D3D12_INPUT_ELEMENT_DESC { SemanticName: windows::core::s!("POSITION"), SemanticIndex: 0, Format: DXGI_FORMAT_R32G32B32_FLOAT, InputSlot: 0, AlignedByteOffset: 0, InputSlotClass: D3D12_INPUT_CLASSIFICATION_PER_VERTEX_DATA, InstanceDataStepRate: 0 },
            D3D12_INPUT_ELEMENT_DESC { SemanticName: windows::core::s!("TEXCOORD"), SemanticIndex: 3, Format: DXGI_FORMAT_R16G16_FLOAT, InputSlot: 1, AlignedByteOffset: 12, InputSlotClass: D3D12_INPUT_CLASSIFICATION_PER_INSTANCE_DATA, InstanceDataStepRate: 2 },
        ]
    }

    #[test]
    fn a_line_gives_the_same_description_back() {
        let e = elems();
        let d = sample(&e);
        let l = unsafe { line(&d, 0x1234, 0xabcd) }.expect("recorded");
        let r = parse(&l).expect("read back");
        assert_eq!((r.vs, r.ps), (0x1234, 0xabcd));
        let back = r.desc();
        // the same line again: every recorded field came back
        assert_eq!(unsafe { line(&back, r.vs, r.ps) }.as_deref(), Some(l.as_str()));
        assert_eq!(back.InputLayout.NumElements, 2);
        let b = unsafe { std::slice::from_raw_parts(back.InputLayout.pInputElementDescs, 2) };
        assert_eq!(unsafe { b[1].SemanticName.to_string() }.unwrap(), "TEXCOORD");
        assert_eq!((b[1].SemanticIndex, b[1].Format, b[1].InputSlot, b[1].AlignedByteOffset, b[1].InstanceDataStepRate), (3, DXGI_FORMAT_R16G16_FLOAT, 1, 12, 2));
        assert_eq!(back.RTVFormats[1], DXGI_FORMAT_R10G10B10A2_UNORM);
        assert_eq!(back.DepthStencilState.StencilWriteMask, 0x3c);
        assert_eq!(back.RasterizerState.DepthBias, -7);
    }

    /// The driver blob of a pipeline built from a read-back line is the one the title's run keeps:
    /// `graphics_blob_key` (what the blob's file is named after) sees the same state.
    #[test]
    fn a_read_back_description_has_the_same_driver_blob_key() {
        let e = elems();
        let d = sample(&e);
        let r = parse(&unsafe { line(&d, 1, 2) }.unwrap()).unwrap();
        let back = r.desc();
        let (vs, ps) = (b"vs dxil".as_slice(), b"ps dxil".as_slice());
        let key = |x: &D3D12_GRAPHICS_PIPELINE_STATE_DESC| unsafe { super::super::real::graphics_blob_key(x, vs, Some(ps), None, &[]) };
        assert_eq!(key(&d), key(&back));
        // and a changed field changes it (the test would pass vacuously otherwise)
        let mut other = back;
        other.RasterizerState.DepthBias = 8;
        assert_ne!(key(&d), key(&other));
    }

    #[test]
    fn pipelines_it_cannot_build_again_are_not_recorded() {
        let e = elems();
        let mut d = sample(&e);
        let gs = [0u8; 4];
        d.GS = D3D12_SHADER_BYTECODE { pShaderBytecode: gs.as_ptr().cast(), BytecodeLength: gs.len() };
        assert!(unsafe { line(&d, 1, 2) }.is_none(), "a GS pipeline");
        let mut d = sample(&e);
        d.StreamOutput.NumEntries = 1;
        assert!(unsafe { line(&d, 1, 2) }.is_none(), "stream output");
        let odd = [D3D12_INPUT_ELEMENT_DESC { SemanticName: windows::core::s!("A,B"), ..e[0] }];
        let d = sample(&odd);
        assert!(unsafe { line(&d, 1, 2) }.is_none(), "a semantic name with a separator in it");
    }

    #[test]
    fn lines_of_another_layout_are_passed_over() {
        let e = elems();
        let l = unsafe { line(&sample(&e), 1, 0) }.unwrap();
        assert!(parse(&l.replacen(VERSION, "gfx0", 1)).is_none());
        assert!(parse("not a line").is_none());
        // a line missing its input layout field
        assert!(parse(&l[..l.rfind('\t').unwrap()]).is_none());
        // a VS hash of 0 is not a pipeline
        assert!(parse(&l.replacen("\t0000000000000001\t", "\t0000000000000000\t", 1)).is_none());
        let both = format!("{l}\n\ngarbage\n{l}\n");
        assert_eq!(parse_all(&both).len(), 2);
    }
}
