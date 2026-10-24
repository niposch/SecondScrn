//! GPU colour conversion: BGRA desktop texture → NV12 encoder input.
//!
//! Two pixel-shader passes write directly into the luma and chroma planes of
//! an NV12 texture (D3D11 allows R8/R8G8 render-target views on NV12 planes).
//! The RGB→Y'CbCr matrix is computed by `secondscrn_graphics::color` and
//! uploaded as constants, so the GPU path and the CPU reference used in tests
//! cannot drift apart. Chroma uses a 2×2 box filter (co-sited averaging),
//! which keeps coloured text edges stable.

use anyhow::{Context, Result, bail};
use secondscrn_graphics::color::RgbToYuv;
use windows::Win32::Graphics::Direct3D::Fxc::{D3DCOMPILE_OPTIMIZATION_LEVEL3, D3DCompile};
use windows::Win32::Graphics::Direct3D::{D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST, D3D_SRV_DIMENSION_TEXTURE2D, ID3DBlob};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::{Interface, PCSTR, s};

use crate::d3d::Gpu;

const SHADER: &str = r#"
cbuffer Matrix : register(b0) { float4 row_y; float4 row_u; float4 row_v; };
Texture2D<float4> src : register(t0);

float4 vs(uint id : SV_VertexID) : SV_Position {
    float2 uv = float2((id << 1) & 2, id & 2);
    return float4(uv * float2(2, -2) + float2(-1, 1), 0, 1);
}

float ps_y(float4 pos : SV_Position) : SV_Target {
    float3 rgb = src.Load(int3(pos.xy, 0)).rgb;
    return dot(row_y.xyz, rgb) + row_y.w;
}

float2 ps_uv(float4 pos : SV_Position) : SV_Target {
    int2 p = int2(pos.xy) * 2;
    float3 rgb = (src.Load(int3(p, 0)).rgb + src.Load(int3(p + int2(1, 0), 0)).rgb
                + src.Load(int3(p + int2(0, 1), 0)).rgb + src.Load(int3(p + int2(1, 1), 0)).rgb) * 0.25;
    return float2(dot(row_u.xyz, rgb) + row_u.w, dot(row_v.xyz, rgb) + row_v.w);
}
"#;

fn compile(entry: PCSTR, target: PCSTR) -> Result<Vec<u8>> {
    let mut code: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    let r = unsafe {
        D3DCompile(
            SHADER.as_ptr() as _,
            SHADER.len(),
            s!("convert.hlsl"),
            None,
            None,
            entry,
            target,
            D3DCOMPILE_OPTIMIZATION_LEVEL3,
            0,
            &mut code,
            Some(&mut errors),
        )
    };
    if let Err(e) = r {
        let msg = errors
            .map(|b| unsafe {
                String::from_utf8_lossy(std::slice::from_raw_parts(
                    b.GetBufferPointer() as *const u8,
                    b.GetBufferSize(),
                ))
                .into_owned()
            })
            .unwrap_or_default();
        bail!("shader compilation failed: {e} {msg}");
    }
    let code = code.context("no shader code")?;
    Ok(unsafe { std::slice::from_raw_parts(code.GetBufferPointer() as *const u8, code.GetBufferSize()).to_vec() })
}

pub struct Converter {
    gpu: Gpu,
    vs: ID3D11VertexShader,
    ps_y: ID3D11PixelShader,
    ps_uv: ID3D11PixelShader,
    constants: ID3D11Buffer,
}

/// An NV12 texture with render-target views on both planes.
pub struct Nv12Target {
    pub texture: ID3D11Texture2D,
    luma: ID3D11RenderTargetView,
    chroma: ID3D11RenderTargetView,
    pub width: u32,
    pub height: u32,
}

impl Converter {
    pub fn new(gpu: &Gpu, matrix: &RgbToYuv) -> Result<Self> {
        let dev = &gpu.device;
        let vs_code = compile(s!("vs"), s!("vs_5_0"))?;
        let y_code = compile(s!("ps_y"), s!("ps_5_0"))?;
        let uv_code = compile(s!("ps_uv"), s!("ps_5_0"))?;
        let (mut vs, mut ps_y, mut ps_uv) = (None, None, None);
        let mut constants = None;
        let data: Vec<f32> = matrix.rows.iter().flatten().copied().collect();
        unsafe {
            dev.CreateVertexShader(&vs_code, None, Some(&mut vs))?;
            dev.CreatePixelShader(&y_code, None, Some(&mut ps_y))?;
            dev.CreatePixelShader(&uv_code, None, Some(&mut ps_uv))?;
            dev.CreateBuffer(
                &D3D11_BUFFER_DESC {
                    ByteWidth: (data.len() * 4) as u32,
                    Usage: D3D11_USAGE_IMMUTABLE,
                    BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                    ..Default::default()
                },
                Some(&D3D11_SUBRESOURCE_DATA { pSysMem: data.as_ptr() as _, ..Default::default() }),
                Some(&mut constants),
            )?;
        }
        Ok(Converter {
            gpu: gpu.clone(),
            vs: vs.context("vs")?,
            ps_y: ps_y.context("ps_y")?,
            ps_uv: ps_uv.context("ps_uv")?,
            constants: constants.context("cb")?,
        })
    }

    pub fn create_target(&self, width: u32, height: u32) -> Result<Nv12Target> {
        if !width.is_multiple_of(2) || !height.is_multiple_of(2) {
            bail!("NV12 needs even dimensions, got {width}x{height}");
        }
        let dev = &self.gpu.device;
        let mut texture = None;
        unsafe {
            dev.CreateTexture2D(
                &D3D11_TEXTURE2D_DESC {
                    Width: width,
                    Height: height,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: DXGI_FORMAT_NV12,
                    SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    Usage: D3D11_USAGE_DEFAULT,
                    BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
                    ..Default::default()
                },
                None,
                Some(&mut texture),
            )?;
        }
        let texture: ID3D11Texture2D = texture.context("nv12 texture")?;
        let rtv = |format| -> Result<ID3D11RenderTargetView> {
            let mut v = None;
            unsafe {
                dev.CreateRenderTargetView(
                    &texture,
                    Some(&D3D11_RENDER_TARGET_VIEW_DESC {
                        Format: format,
                        ViewDimension: D3D11_RTV_DIMENSION_TEXTURE2D,
                        Anonymous: D3D11_RENDER_TARGET_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_RTV { MipSlice: 0 } },
                    }),
                    Some(&mut v),
                )?;
            }
            v.context("rtv")
        };
        Ok(Nv12Target {
            luma: rtv(DXGI_FORMAT_R8_UNORM)?,
            chroma: rtv(DXGI_FORMAT_R8G8_UNORM)?,
            texture,
            width,
            height,
        })
    }

    /// Creates a shader resource view for a BGRA source texture.
    pub fn source_view(&self, texture: &ID3D11Texture2D) -> Result<ID3D11ShaderResourceView> {
        let mut srv = None;
        unsafe {
            self.gpu.device.CreateShaderResourceView(
                texture,
                Some(&D3D11_SHADER_RESOURCE_VIEW_DESC {
                    Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    ViewDimension: D3D_SRV_DIMENSION_TEXTURE2D,
                    Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
                        Texture2D: D3D11_TEX2D_SRV { MostDetailedMip: 0, MipLevels: 1 },
                    },
                }),
                Some(&mut srv),
            )?;
        }
        srv.context("srv")
    }

    /// Converts `source` (BGRA, at least target-sized) into `target`.
    pub fn convert(&self, source: &ID3D11ShaderResourceView, target: &Nv12Target) {
        let ctx = &self.gpu.context;
        unsafe {
            ctx.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            ctx.IASetInputLayout(None);
            ctx.VSSetShader(&self.vs, None);
            ctx.PSSetConstantBuffers(0, Some(&[Some(self.constants.clone())]));
            ctx.PSSetShaderResources(0, Some(&[Some(source.clone())]));
            ctx.RSSetState(None);
            ctx.OMSetBlendState(None, None, u32::MAX);

            let vp = |w: u32, h: u32| D3D11_VIEWPORT {
                Width: w as f32,
                Height: h as f32,
                MaxDepth: 1.0,
                ..Default::default()
            };
            ctx.OMSetRenderTargets(Some(&[Some(target.luma.clone())]), None);
            ctx.RSSetViewports(Some(&[vp(target.width, target.height)]));
            ctx.PSSetShader(&self.ps_y, None);
            ctx.Draw(3, 0);

            ctx.OMSetRenderTargets(Some(&[Some(target.chroma.clone())]), None);
            ctx.RSSetViewports(Some(&[vp(target.width / 2, target.height / 2)]));
            ctx.PSSetShader(&self.ps_uv, None);
            ctx.Draw(3, 0);

            // Unbind so the source can be released/reused by the capture pool.
            ctx.PSSetShaderResources(0, Some(&[None]));
            ctx.OMSetRenderTargets(None, None);
        }
    }
}

/// Uploads a BGRA image into a new GPU texture (tests and test patterns).
pub fn upload_bgra(gpu: &Gpu, width: u32, height: u32, bgra: &[u8]) -> Result<ID3D11Texture2D> {
    let mut tex = None;
    unsafe {
        gpu.device.CreateTexture2D(
            &D3D11_TEXTURE2D_DESC {
                Width: width,
                Height: height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
                ..Default::default()
            },
            Some(&D3D11_SUBRESOURCE_DATA { pSysMem: bgra.as_ptr() as _, SysMemPitch: width * 4, SysMemSlicePitch: 0 }),
            Some(&mut tex),
        )?;
    }
    tex.context("texture")
}

/// Reads an NV12 texture back to the CPU as (Y plane, CbCr plane). Slow;
/// tests and diagnostics only.
pub fn read_nv12(gpu: &Gpu, texture: &ID3D11Texture2D) -> Result<(Vec<u8>, Vec<u8>)> {
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    unsafe { texture.GetDesc(&mut desc) };
    let staging_desc = D3D11_TEXTURE2D_DESC {
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
        ..desc
    };
    let mut staging = None;
    unsafe { gpu.device.CreateTexture2D(&staging_desc, None, Some(&mut staging))? };
    let staging: ID3D11Texture2D = staging.context("staging")?;
    let (w, h) = (desc.Width as usize, desc.Height as usize);
    unsafe {
        gpu.context.CopyResource(&staging, texture);
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        gpu.context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
        let pitch = mapped.RowPitch as usize;
        let base = mapped.pData as *const u8;
        let mut y = vec![0u8; w * h];
        let mut uv = vec![0u8; w * h / 2];
        for row in 0..h {
            std::ptr::copy_nonoverlapping(base.add(row * pitch), y.as_mut_ptr().add(row * w), w);
        }
        // The chroma plane follows the luma plane at `pitch * height`.
        let chroma = base.add(pitch * h);
        for row in 0..h / 2 {
            std::ptr::copy_nonoverlapping(chroma.add(row * pitch), uv.as_mut_ptr().add(row * w), w);
        }
        gpu.context.Unmap(&staging, 0);
        let _ = staging.cast::<ID3D11Resource>();
        Ok((y, uv))
    }
}
