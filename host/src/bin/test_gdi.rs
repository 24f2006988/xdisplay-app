use anyhow::Result;
use windows::Win32::Graphics::Gdi::*;

fn main() -> Result<()> {
    println!("Testing GDI capture on virtual monitor DISPLAY7...");
    unsafe {
        use windows::core::w;
        let hdc_screen = CreateDCW(w!("DISPLAY"), windows::core::PCWSTR::null(), windows::core::PCWSTR::null(), None);
        println!("hdc_screen from CreateDCW: {:?}", hdc_screen);


        let hdc_mem = CreateCompatibleDC(hdc_screen);
        println!("hdc_mem: {:?}", hdc_mem);

        let hbm = CreateCompatibleBitmap(hdc_screen, 1800, 1200);
        println!("hbm: {:?}", hbm);

        let old_obj = SelectObject(hdc_mem, hbm);
        println!("old_obj: {:?}", old_obj);

        let t0 = std::time::Instant::now();
        let res = BitBlt(hdc_mem, 0, 0, 1800, 1200, hdc_screen, 1920, 0, SRCCOPY);
        let elapsed = t0.elapsed();
        println!("BitBlt result: {:?}, time: {:?}", res, elapsed);


        SelectObject(hdc_mem, old_obj);
        let _ = DeleteObject(hbm);
        let _ = DeleteDC(hdc_mem);
        let _ = DeleteDC(hdc_screen);
    }
    Ok(())
}
