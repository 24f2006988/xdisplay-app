use anyhow::{anyhow, Result};
use tracing::{info, warn};

#[cfg(windows)]
use windows::{
    core::Interface,
    Win32::Graphics::{
        Direct3D::{
            D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0,
            D3D_FEATURE_LEVEL_11_1,
        },
        Direct3D11::{
            D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
            D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ,
            D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
        },
        Dxgi::{
            Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC},
            CreateDXGIFactory1, IDXGIDevice, IDXGIFactory1,
            IDXGIOutput1, IDXGIOutputDuplication, DXGI_OUTDUPL_FRAME_INFO,
        },
    },
};

#[allow(dead_code)]
pub struct FrameData {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
    pub pts_us: u64,
}

pub struct DxgiCapture {
    #[cfg(windows)]
    device: ID3D11Device,
    #[cfg(windows)]
    context: ID3D11DeviceContext,
    #[cfg(windows)]
    duplication: IDXGIOutputDuplication,
    #[cfg(windows)]
    staging_texture: Option<ID3D11Texture2D>,
    width: u32,
    height: u32,
    #[allow(dead_code)]
    display_index: u32,
}

impl DxgiCapture {
    pub fn new(display_index: u32) -> Result<Self> {
        #[cfg(windows)]
        {
            let feature_levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];

            // Strategy 1: Default Hardware Device (Matches DWM primary render adapter)
            info!("Attempting DXGI capture on default DWM render device for display index {}...", display_index);
            let mut device: Option<ID3D11Device> = None;
            let mut context: Option<ID3D11DeviceContext> = None;

            let default_res = unsafe {
                D3D11CreateDevice(
                    None,
                    D3D_DRIVER_TYPE_HARDWARE,
                    None,
                    D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                    Some(&feature_levels),
                    D3D11_SDK_VERSION,
                    Some(&mut device),
                    None,
                    Some(&mut context),
                )
            };

            if default_res.is_ok() {
                if let (Some(dev), Some(ctx)) = (device.clone(), context.clone()) {
                    if let Ok(dxgi_device) = dev.cast::<IDXGIDevice>() {
                        if let Ok(adapter) = unsafe { dxgi_device.GetAdapter() } {
                            if let Ok(output) = unsafe { adapter.EnumOutputs(display_index) } {
                                if let Ok(output1) = output.cast::<IDXGIOutput1>() {
                                    match unsafe { output1.DuplicateOutput(&dev) } {
                                        Ok(duplication) => {
                                            let desc = unsafe { output.GetDesc()? };
                                            let width = (desc.DesktopCoordinates.right - desc.DesktopCoordinates.left) as u32;
                                            let height = (desc.DesktopCoordinates.bottom - desc.DesktopCoordinates.top) as u32;
                                            info!(
                                                "DXGI Desktop Duplication active on default DWM adapter: resolution {}x{}",
                                                width, height
                                            );
                                            return Ok(Self {
                                                device: dev,
                                                context: ctx,
                                                duplication,
                                                staging_texture: None,
                                                width,
                                                height,
                                                display_index,
                                            });
                                        }
                                        Err(e) => {
                                            warn!("Default adapter DuplicateOutput returned: {}, searching explicit GPU adapters...", e);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // Strategy 2: Iterate through all factory adapters (Hybrid Laptop Optimus fallback)
            let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1()? };
            let mut a_idx = 0;
            while let Ok(adapter) = unsafe { factory.EnumAdapters1(a_idx) } {
                let mut o_idx = 0;
                while let Ok(output) = unsafe { adapter.EnumOutputs(o_idx) } {
                    if let Ok(desc) = unsafe { adapter.GetDesc1() } {
                        let name = String::from_utf16_lossy(&desc.Description);
                        info!("Evaluating GPU Adapter {}: '{}' -> Output {}", a_idx, name.trim_matches('\0'), o_idx);
                    }

                    let mut dev = None;
                    let mut ctx = None;
                    let hr = unsafe {
                        D3D11CreateDevice(
                            &adapter,
                            D3D_DRIVER_TYPE_UNKNOWN,
                            None,
                            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                            Some(&feature_levels),
                            D3D11_SDK_VERSION,
                            Some(&mut dev),
                            None,
                            Some(&mut ctx),
                        )
                    };

                    if hr.is_ok() {
                        if let (Some(d), Some(c)) = (dev, ctx) {
                            if let Ok(output1) = output.cast::<IDXGIOutput1>() {
                                match unsafe { output1.DuplicateOutput(&d) } {
                                    Ok(duplication) => {
                                        let desc = unsafe { output.GetDesc()? };
                                        let width = (desc.DesktopCoordinates.right - desc.DesktopCoordinates.left) as u32;
                                        let height = (desc.DesktopCoordinates.bottom - desc.DesktopCoordinates.top) as u32;
                                        info!(
                                            "DXGI Desktop Duplication active on GPU Adapter {} Output {}: resolution {}x{}",
                                            a_idx, o_idx, width, height
                                        );
                                        return Ok(Self {
                                            device: d,
                                            context: c,
                                            duplication,
                                            staging_texture: None,
                                            width,
                                            height,
                                            display_index,
                                        });
                                    }
                                    Err(e) => {
                                        warn!("DuplicateOutput failed on Adapter {} Output {}: {}", a_idx, o_idx, e);
                                    }
                                }
                            }
                        }
                    }
                    o_idx += 1;
                }
                a_idx += 1;
            }

            Err(anyhow!(
                "Could not duplicate desktop on any GPU adapter. Ensure desktop is unlocked and not on UAC prompt."
            ))
        }

        #[cfg(not(windows))]
        {
            Err(anyhow!("DXGI Desktop Duplication is only supported on Windows"))
        }
    }

    pub fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    #[cfg(windows)]
    fn get_or_create_staging_texture(&mut self) -> Result<ID3D11Texture2D> {
        if let Some(ref tex) = self.staging_texture {
            return Ok(tex.clone());
        }

        let desc = D3D11_TEXTURE2D_DESC {
            Width: self.width,
            Height: self.height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };

        let mut staging: Option<ID3D11Texture2D> = None;
        unsafe {
            self.device
                .CreateTexture2D(&desc, None, Some(&mut staging))?;
        }

        let staging = staging.ok_or_else(|| anyhow!("Failed to create staging texture"))?;
        self.staging_texture = Some(staging.clone());
        Ok(staging)
    }

    /// Acquires the next available screen frame.
    /// In a pure zero-copy pipeline, the GPU texture handle is directly given to NVENC/AMF.
    /// This method also supports CPU staging extraction for benchmarking and fallback encoding.
    pub fn acquire_frame(&mut self, timeout_ms: u32) -> Result<Option<FrameData>> {
        #[cfg(windows)]
        {
            let mut frame_info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource = None;

            let hr = unsafe {
                self.duplication
                    .AcquireNextFrame(timeout_ms, &mut frame_info, &mut resource)
            };

            if let Err(e) = hr {
                // DXGI_ERROR_WAIT_TIMEOUT is expected when the screen has no updates
                if e.code().0 == 0x887A0027u32 as i32 {
                    return Ok(None);
                }
                return Err(anyhow!("Failed to acquire next DXGI frame: {}", e));
            }

            let resource = resource.ok_or_else(|| anyhow!("Resource is null"))?;
            let texture: ID3D11Texture2D = resource.cast()?;
            let staging = self.get_or_create_staging_texture()?;

            unsafe {
                self.context.CopyResource(&staging, &texture);
                self.duplication.ReleaseFrame()?;

                let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                self.context
                    .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;

                let row_pitch = mapped.RowPitch as usize;
                let height = self.height as usize;
                let width = self.width as usize;
                let bytes_per_pixel = 4;
                let line_len = width * bytes_per_pixel;

                let mut buffer = vec![0u8; line_len * height];
                let src_slice = std::slice::from_raw_parts(mapped.pData as *const u8, row_pitch * height);

                for y in 0..height {
                    let src_start = y * row_pitch;
                    let dst_start = y * line_len;
                    buffer[dst_start..dst_start + line_len]
                        .copy_from_slice(&src_slice[src_start..src_start + line_len]);
                }

                self.context.Unmap(&staging, 0);

                let pts_us = (frame_info.LastPresentTime / 10) as u64; // Convert 100ns units to us

                Ok(Some(FrameData {
                    width: self.width,
                    height: self.height,
                    data: buffer,
                    pts_us,
                }))
            }
        }

        #[cfg(not(windows))]
        {
            Ok(None)
        }
    }
}
