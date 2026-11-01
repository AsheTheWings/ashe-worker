//! Text rasterization for the overlay pill. No Win32 dependencies;
//! `native_overlay` owns presentation.

use crate::pill_renderer;
use cosmic_text::{Attrs, Buffer, Color as TextColor, Family, FontSystem, Metrics, Shaping};
use cosmic_text::{SwashCache, Wrap};

const FONT_PIXELS: f32 = 14.0;

pub(crate) struct TextRasterizer {
    fonts: FontSystem,
    cache: SwashCache,
}

impl TextRasterizer {
    pub(crate) fn new() -> Self {
        Self {
            fonts: FontSystem::new(),
            cache: SwashCache::new(),
        }
    }

    /// Draw one status word centered in the pill.
    pub(crate) fn draw_mini_text(
        &mut self,
        rgba: &mut [u8],
        width: u32,
        height: u32,
        scale: f32,
        text: Option<&str>,
    ) {
        let Some(text) = text.filter(|text| !text.is_empty()) else {
            return;
        };
        let clip =
            PixelRect::from_logical(pill_renderer::MINI_WIDTH, pill_renderer::MINI_HEIGHT, scale);
        let font_size = (FONT_PIXELS * scale).max(1.0);
        let mut buffer = Buffer::new(&mut self.fonts, Metrics::new(font_size, font_size * 1.35));
        buffer.set_wrap(&mut self.fonts, Wrap::None);
        buffer.set_size(&mut self.fonts, None, Some(clip.height as f32));
        let attrs = Attrs::new().family(Family::Name("Segoe UI"));
        buffer.set_text(&mut self.fonts, text, &attrs, Shaping::Advanced, None);
        buffer.shape_until_scroll(&mut self.fonts, false);
        let mut raster = Vec::new();
        buffer.draw(
            &mut self.fonts,
            &mut self.cache,
            TextColor::rgb(255, 255, 255),
            |x, y, pixel_width, pixel_height, color| {
                raster.push(RasterPixel {
                    x,
                    y,
                    width: pixel_width,
                    height: pixel_height,
                    color,
                });
            },
        );
        blit_centered(rgba, width, height, &raster, clip);
    }
}

fn blit_centered(
    rgba: &mut [u8],
    width: u32,
    height: u32,
    raster: &[RasterPixel],
    clip: PixelRect,
) {
    let Some(bounds) = RasterBounds::for_pixels(raster) else {
        return;
    };
    let origin_x = (clip.width - bounds.width()) / 2 - bounds.left;
    let origin_y = (clip.height - bounds.height()) / 2 - bounds.top;
    for pixel in raster {
        for offset_y in 0..pixel.height as i32 {
            for offset_x in 0..pixel.width as i32 {
                let x = origin_x + pixel.x + offset_x;
                let y = origin_y + pixel.y + offset_y;
                if clip.contains(x, y) {
                    composite_pixel(rgba, width, height, x, y, pixel.color, u8::MAX);
                }
            }
        }
    }
}

#[derive(Clone, Copy)]
struct PixelRect {
    width: i32,
    height: i32,
}

impl PixelRect {
    fn from_logical(width: f32, height: f32, scale: f32) -> Self {
        Self {
            width: (width * scale).round() as i32,
            height: (height * scale).round() as i32,
        }
    }

    fn contains(self, x: i32, y: i32) -> bool {
        x >= 0 && x < self.width && y >= 0 && y < self.height
    }
}

struct RasterPixel {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    color: TextColor,
}

struct RasterBounds {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

impl RasterBounds {
    fn for_pixels(pixels: &[RasterPixel]) -> Option<Self> {
        let first = pixels.first()?;
        let mut bounds = Self {
            left: first.x,
            top: first.y,
            right: first.x + first.width as i32,
            bottom: first.y + first.height as i32,
        };
        for pixel in &pixels[1..] {
            bounds.left = bounds.left.min(pixel.x);
            bounds.top = bounds.top.min(pixel.y);
            bounds.right = bounds.right.max(pixel.x + pixel.width as i32);
            bounds.bottom = bounds.bottom.max(pixel.y + pixel.height as i32);
        }
        Some(bounds)
    }

    fn width(&self) -> i32 {
        self.right - self.left
    }

    fn height(&self) -> i32 {
        self.bottom - self.top
    }
}

fn composite_pixel(
    rgba: &mut [u8],
    width: u32,
    height: u32,
    x: i32,
    y: i32,
    color: TextColor,
    opacity: u8,
) {
    if x < 0 || y < 0 || x >= width as i32 || y >= height as i32 {
        return;
    }
    let index = (y as usize * width as usize + x as usize) * 4;
    let source_alpha = (color.a() as u16 * opacity as u16 / 255) as u8;
    let inverse = 255_u16 - source_alpha as u16;
    let source = [color.r(), color.g(), color.b()];
    for channel in 0..3 {
        let source_premultiplied = source[channel] as u16 * source_alpha as u16 / 255;
        rgba[index + channel] =
            (source_premultiplied + rgba[index + channel] as u16 * inverse / 255).min(255) as u8;
    }
    rgba[index + 3] = (source_alpha as u16 + rgba[index + 3] as u16 * inverse / 255).min(255) as u8;
}

#[cfg(test)]
mod unit {
    use super::composite_pixel;
    use cosmic_text::Color;

    #[test]
    fn text_compositing_creates_premultiplied_alpha_on_transparency() {
        let mut rgba = [0_u8; 4];
        composite_pixel(&mut rgba, 1, 1, 0, 0, Color::rgba(255, 255, 255, 128), 128);
        assert_eq!(rgba, [64, 64, 64, 64]);
    }

    #[test]
    fn text_compositing_obeys_the_surface_bounds() {
        let mut rgba = [0_u8; 4];
        composite_pixel(&mut rgba, 1, 1, 1, 0, Color::rgb(255, 255, 255), 255);
        assert_eq!(rgba, [0, 0, 0, 0]);
    }
}
