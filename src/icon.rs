use eframe::egui::IconData;

use crate::icon_raster;

const SIZE: u32 = 256;

/// Window, taskbar and dock icon.
pub fn app_icon() -> IconData {
    IconData {
        rgba: icon_raster::raster(SIZE),
        width: SIZE,
        height: SIZE,
    }
}
