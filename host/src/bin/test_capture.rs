use anyhow::Result;
use windows::{
    core::Interface,
    Win32::Graphics::{
        Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1},
        Direct3D11::{D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION},
        Dxgi::{CreateDXGIFactory1, IDXGIFactory1, IDXGIOutput1},
    },
};

use std::fs::File;
use std::io::Write;

fn main() -> Result<()> {
    let mut log = File::create("d:/xdisplay/test_capture_out.txt")?;
    writeln!(log, "=== Test Capture Diagnostics ===")?;


    let feature_levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];
    unsafe {
        // Test 1: Default Hardware Device
        let mut dev: Option<ID3D11Device> = None;
        let mut ctx: Option<ID3D11DeviceContext> = None;
        let hr = D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            None,
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&feature_levels),
            D3D11_SDK_VERSION,
            Some(&mut dev),
            None,
            Some(&mut ctx),
        );
        writeln!(log, "D3D11CreateDevice(None, HARDWARE): {:?}", hr)?;
        if let Some(dev) = dev {
            let dxgi_dev: windows::Win32::Graphics::Dxgi::IDXGIDevice = dev.cast()?;
            let adapter = dxgi_dev.GetAdapter()?;
            let desc = adapter.GetDesc()?;
            writeln!(log, "Default Adapter: {}", String::from_utf16_lossy(&desc.Description))?;
            let mut o_idx = 0;
            while let Ok(output) = adapter.EnumOutputs(o_idx) {
                let o_desc = output.GetDesc()?;
                let _ = writeln!(log, "  Output {}: {}", o_idx, String::from_utf16_lossy(&o_desc.DeviceName));
                let output1 = output.cast::<IDXGIOutput1>()?;
                match output1.DuplicateOutput(&dev) {
                    Ok(_) => writeln!(log, "    DuplicateOutput SUCCESS on default dev output {}", o_idx)?,
                    Err(e) => writeln!(log, "    DuplicateOutput FAILED on default dev output {}: {:?}", o_idx, e)?,
                }
                o_idx += 1;
            }
        }

        // Test 2: Explicit Adapter 0
        let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
        let adapter = factory.EnumAdapters1(0)?;
        let desc = adapter.GetDesc1()?;
        writeln!(log, "\nExplicit Adapter 0: {}", String::from_utf16_lossy(&desc.Description))?;

        let mut dev: Option<ID3D11Device> = None;
        let mut ctx: Option<ID3D11DeviceContext> = None;
        let hr = D3D11CreateDevice(
            &adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            None,
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&feature_levels),
            D3D11_SDK_VERSION,
            Some(&mut dev),
            None,
            Some(&mut ctx),
        );
        writeln!(log, "D3D11CreateDevice(&adapter, UNKNOWN): {:?}", hr)?;
        let dev = dev.unwrap();

        for o_idx in 0..=1 {
            if let Ok(output) = adapter.EnumOutputs(o_idx) {
                let o_desc = output.GetDesc()?;
                let name = String::from_utf16_lossy(&o_desc.DeviceName);
                writeln!(log, "Output {}: {}", o_idx, name.trim_matches('\0'))?;

                let output1 = output.cast::<IDXGIOutput1>()?;
                match output1.DuplicateOutput(&dev) {
                    Ok(_) => writeln!(log, "  DuplicateOutput SUCCEEDED on Output {}", o_idx)?,
                    Err(e) => writeln!(log, "  DuplicateOutput FAILED on Output {}: {:?}", o_idx, e)?,
                }
            }
        }
    }
    println!("Done. Output written to d:/xdisplay/test_capture_out.txt");
    Ok(())
}

