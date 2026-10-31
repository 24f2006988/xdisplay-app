use anyhow::Result;
use windows_capture::{
    capture::{Context, GraphicsCaptureApiHandler},
    frame::Frame,
    graphics_capture_api::InternalCaptureControl,
    monitor::Monitor,
    settings::{
        ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
        MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
    },

};

struct CaptureHandler {
    frame_count: u32,
}

impl GraphicsCaptureApiHandler for CaptureHandler {
    type Flags = ();
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn new(_ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        Ok(Self { frame_count: 0 })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        capture_control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        self.frame_count += 1;
        let width = frame.width();
        let height = frame.height();
        println!(
            "Captured Frame #{}: {}x{}, buffer len={}",
            self.frame_count,
            width,
            height,
            frame.buffer().unwrap().as_raw_buffer().len()
        );

        if self.frame_count >= 5 {
            println!("Got 5 frames! Stopping capture.");
            capture_control.stop();
        }
        Ok(())
    }
}

fn main() -> Result<()> {
    println!("Enumerating monitors...");
    let monitors = Monitor::enumerate()?;
    for (i, m) in monitors.iter().enumerate() {
        println!("Monitor {}: name='{}'", i, m.name().unwrap_or_default());
    }

    // Pick monitor 1 (VDD by MTT) or monitor 0
    let monitor = monitors.into_iter().find(|m| m.name().map(|n| n.contains("VDD")).unwrap_or(false))
        .expect("Could not find VDD monitor");

    println!("Starting capture on VDD monitor: {}", monitor.name().unwrap_or_default());

    let settings = Settings::new(
        monitor,
        CursorCaptureSettings::Default,
        DrawBorderSettings::Default,
        SecondaryWindowSettings::Default,
        MinimumUpdateIntervalSettings::Default,
        DirtyRegionSettings::Default,
        ColorFormat::Bgra8,
        (),
    );


    CaptureHandler::start(settings)?;
    println!("Capture finished successfully!");
    Ok(())
}
