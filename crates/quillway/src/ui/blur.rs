//! The panel's blur mask in surface-local logical pixels, excluding its shadow.

use iced_exwlshell::reexport::{BlurOption, BlurRegion};

use super::style::RADIUS;

/// Rasterize rounded corners into horizontal bands. `visible_height` clips the
/// natural panel at the scroll viewport, without rounding the clipped edge.
#[expect(clippy::cast_possible_truncation, reason = "corner insets are at most RADIUS (16 pixels)")]
pub(super) fn panel(width: u32, height: u32, visible_height: u32, margin: u32, scroll_y: f32) -> BlurOption {
    let radius = f64::from(RADIUS).min(f64::from(width) / 2.0).min(f64::from(height) / 2.0);
    let width = i32::try_from(width).expect("validated panel width");
    let margin = i32::try_from(margin).expect("shadow margin");
    let offset = f64::from(scroll_y).clamp(0.0, f64::from(height.saturating_sub(visible_height)));
    let mut bands: Vec<BlurRegion> = Vec::new();
    for y in 0..visible_height.min(height) {
        // Test pixel centers against the corner circle; the middle has no inset.
        let row = offset + f64::from(y) + 0.5;
        let edge = row.min(f64::from(height) - row);
        let dy = (radius - edge).max(0.0);
        let inset = (radius - (radius * radius - dy * dy).sqrt() - 0.5).ceil().max(0.0) as i32;
        let x = margin + inset;
        let band_width = width - 2 * inset;
        if let Some(last) = bands.last_mut()
            && last.x == x
            && last.width == band_width
        {
            last.height += 1;
        } else {
            bands.push(BlurRegion {
                x,
                y: margin + i32::try_from(y).expect("capped viewport height"),
                width: band_width,
                height: 1,
            });
        }
    }
    BlurOption::Region(bands)
}
