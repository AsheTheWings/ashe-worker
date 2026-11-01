use tiny_skia::{Color, FillRule, Paint, Path, PathBuilder, Pixmap, Transform};

/// Compact pill shown while a text action runs: one centered status word.
pub const MINI_WIDTH: f32 = 208.0;
pub const MINI_HEIGHT: f32 = 56.0;
const BORDER_WIDTH: f32 = 2.0;
/// Keep the antialiased edge inside the layered bitmap.
const PILL_EDGE_INSET: f32 = 1.0;
const BACKGROUND: [u8; 4] = [6, 19, 25, 255];
const BORDER: [u8; 4] = [0, 101, 115, 255];
const _: () = assert!(MINI_HEIGHT < MINI_WIDTH, "the pill stays wider than tall");

/// Render the pill as premultiplied RGBA pixels: one bordered capsule over
/// the background. The overlay text rasterizer composites the status later.
pub fn render_mini_rgba(pixel_width: u32, pixel_height: u32) -> Option<Vec<u8>> {
    let mut pixmap = Pixmap::new(pixel_width, pixel_height)?;
    let scale = pixel_width as f32 / MINI_WIDTH;
    let transform = Transform::from_scale(scale, scale);
    let x = PILL_EDGE_INSET;
    let y = PILL_EDGE_INSET;
    let width = MINI_WIDTH - 2.0 * PILL_EDGE_INSET;
    let height = MINI_HEIGHT - 2.0 * PILL_EDGE_INSET;
    fill_capsule(
        &mut pixmap,
        capsule_path(x, y, width, height)?,
        BORDER,
        transform,
    );
    fill_capsule(
        &mut pixmap,
        capsule_path(
            x + BORDER_WIDTH,
            y + BORDER_WIDTH,
            width - 2.0 * BORDER_WIDTH,
            height - 2.0 * BORDER_WIDTH,
        )?,
        BACKGROUND,
        transform,
    );
    symmetrize_horizontal(&mut pixmap);
    symmetrize_vertical(&mut pixmap);
    Some(pixmap.take())
}

fn fill_capsule(pixmap: &mut Pixmap, path: Path, rgba: [u8; 4], transform: Transform) {
    let mut paint = Paint::default();
    paint.set_color(Color::from_rgba8(rgba[0], rgba[1], rgba[2], rgba[3]));
    pixmap.fill_path(&path, &paint, FillRule::Winding, transform, None);
}

fn symmetrize_horizontal(pixmap: &mut Pixmap) {
    let width = pixmap.width() as usize;
    let height = pixmap.height() as usize;
    let pixels = pixmap.data_mut();
    for y in 0..height {
        for x in 0..width / 2 {
            average_pixel_pair(pixels, y * width + x, y * width + (width - 1 - x));
        }
    }
}

fn symmetrize_vertical(pixmap: &mut Pixmap) {
    let width = pixmap.width() as usize;
    let height = pixmap.height() as usize;
    let pixels = pixmap.data_mut();
    for offset in 0..height / 2 {
        let upper = offset;
        let lower = height - 1 - offset;
        for x in 0..width {
            average_pixel_pair(pixels, upper * width + x, lower * width + x);
        }
    }
}

fn average_pixel_pair(pixels: &mut [u8], first_pixel: usize, second_pixel: usize) {
    let first = first_pixel * 4;
    let second = second_pixel * 4;
    for channel in 0..4 {
        let average =
            (pixels[first + channel] as u16 + pixels[second + channel] as u16).div_ceil(2);
        pixels[first + channel] = average as u8;
        pixels[second + channel] = average as u8;
    }
}

/// Build a single closed capsule path so one rasterizer owns all four edges.
fn capsule_path(x: f32, y: f32, width: f32, height: f32) -> Option<Path> {
    if width <= 0.0 || height <= 0.0 {
        return None;
    }
    let radius = (height / 2.0).min(width / 2.0);
    let tangent = radius * 0.552_284_8;
    let left = x;
    let right = x + width;
    let top = y;
    let bottom = y + height;
    let mut path = PathBuilder::new();
    path.move_to(left + radius, top);
    path.line_to(right - radius, top);
    path.cubic_to(
        right - radius + tangent,
        top,
        right,
        top + radius - tangent,
        right,
        top + radius,
    );
    path.line_to(right, bottom - radius);
    path.cubic_to(
        right,
        bottom - radius + tangent,
        right - radius + tangent,
        bottom,
        right - radius,
        bottom,
    );
    path.line_to(left + radius, bottom);
    path.cubic_to(
        left + radius - tangent,
        bottom,
        left,
        bottom - radius + tangent,
        left,
        bottom - radius,
    );
    path.line_to(left, top + radius);
    path.cubic_to(
        left,
        top + radius - tangent,
        left + radius - tangent,
        top,
        left + radius,
        top,
    );
    path.close();
    path.finish()
}

#[cfg(test)]
mod unit {
    use super::{MINI_HEIGHT, MINI_WIDTH, render_mini_rgba};

    /// The pill is the only feedback while a request runs, so it must stay
    /// visible at every scale the layered window presents.
    #[test]
    fn the_pill_stays_visible_at_presented_scales() {
        for scale in [1.0f32, 1.25, 1.5, 2.0] {
            let width = (MINI_WIDTH * scale).round() as u32;
            let height = (MINI_HEIGHT * scale).round() as u32;
            let pixels = render_mini_rgba(width, height).unwrap();
            assert_eq!(pixels.len(), (width as usize) * (height as usize) * 4);
            let opaque = pixels
                .as_chunks::<4>()
                .0
                .iter()
                .filter(|pixel| pixel[3] > 10)
                .count();
            assert!(
                opaque * 2 > pixels.len() / 4,
                "pill went transparent at scale={scale}"
            );
        }
    }
}
