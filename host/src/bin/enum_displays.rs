use anyhow::Result;
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1};

fn main() -> Result<()> {
    println!("=== DXGI Display Enumerator ===");
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
        let mut a_idx = 0;
        while let Ok(adapter) = factory.EnumAdapters1(a_idx) {
            let desc = adapter.GetDesc1()?;
            let name = String::from_utf16_lossy(&desc.Description);
            println!("\nAdapter {}: {}", a_idx, name.trim_matches('\0'));

            let mut o_idx = 0;
            while let Ok(output) = adapter.EnumOutputs(o_idx) {
                let o_desc = output.GetDesc()?;
                let o_name = String::from_utf16_lossy(&o_desc.DeviceName);
                let coords = o_desc.DesktopCoordinates;
                let w = coords.right - coords.left;
                let h = coords.bottom - coords.top;
                println!(
                    "  Output {}: '{}' [{}x{}] at ({}, {}) Attached={}",
                    o_idx,
                    o_name.trim_matches('\0'),
                    w,
                    h,
                    coords.left,
                    coords.top,
                    o_desc.AttachedToDesktop.as_bool()
                );
                o_idx += 1;
            }
            if o_idx == 0 {
                println!("  (No outputs attached to this adapter)");
            }
            a_idx += 1;
        }
    }
    Ok(())
}
