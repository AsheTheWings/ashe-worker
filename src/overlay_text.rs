//! Text rasterization for the dictation overlay.
//!
//! Bar labels, the typing preview, and pixel compositing. No Win32
//! dependencies; `native_overlay` owns presentation.

use crate::pill_renderer::{self, TopBarAlignment, TopBarContent};
use cosmic_text::{Attrs, Buffer, Color as TextColor, Family, FontSystem, Metrics, Shaping};
use cosmic_text::{SwashCache, Wrap};

const MAIN_FONT_PIXELS: f32 = 14.0;
const TOP_BAR_FONT_PIXELS: f32 = 13.2;
/// Marker scale and color: 1.5x palette cyan, inline and accessory.
const TOP_BAR_LINE_BREAK_SCALE: f32 = 1.5;
const LINE_BREAK_CYAN: TextColor = TextColor::rgb(0, 224, 255);
const LINE_BREAK_MARKER: char = '↵';

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

    pub(crate) fn draw(
        &mut self,
        rgba: &mut [u8],
        width: u32,
        height: u32,
        scale: f32,
        main_text: Option<&str>,
        top_bar: Option<&TopBarContent>,
    ) {
        if let Some(text) = main_text.filter(|text| !text.is_empty()) {
            let rect = PixelRect::from_logical(
                0.0,
                pill_renderer::MAIN_TOP,
                pill_renderer::WIDTH,
                pill_renderer::PILL_HEIGHT,
                scale,
            );
            self.draw_text(
                rgba,
                width,
                height,
                text,
                MAIN_FONT_PIXELS * scale,
                rect,
                TextAlign::Center,
                TextColor::rgb(255, 255, 255),
                u8::MAX,
            );
        }
        if let Some(content) = top_bar {
            let count_left = pill_renderer::TOP_BAR_X + pill_renderer::TOP_BAR_TEXT_INSET;
            let count_rect = PixelRect::from_logical(
                count_left,
                pill_renderer::TOP_BAR_Y,
                pill_renderer::TOP_BAR_COUNT_WIDTH,
                pill_renderer::TOP_BAR_HEIGHT,
                scale,
            );
            self.draw_text(
                rgba,
                width,
                height,
                &content.count,
                TOP_BAR_FONT_PIXELS * scale,
                count_rect,
                TextAlign::Left,
                TextColor::rgb(255, 255, 255),
                204,
            );
            // Centered status uses the full bar; the trailing preview stays
            // right of the count, so the two runs never overlap.
            let (content_rect, alignment) = match content.alignment {
                TopBarAlignment::Center => (
                    PixelRect::from_logical(
                        pill_renderer::TOP_BAR_X + pill_renderer::TOP_BAR_TEXT_INSET,
                        pill_renderer::TOP_BAR_Y,
                        pill_renderer::TOP_BAR_WIDTH - 2.0 * pill_renderer::TOP_BAR_TEXT_INSET,
                        pill_renderer::TOP_BAR_HEIGHT,
                        scale,
                    ),
                    TextAlign::Center,
                ),
                TopBarAlignment::Trailing => {
                    let content_left = count_left
                        + pill_renderer::TOP_BAR_COUNT_WIDTH
                        + pill_renderer::TOP_BAR_CONTENT_GAP;
                    let content_right = pill_renderer::TOP_BAR_X + pill_renderer::TOP_BAR_WIDTH
                        - pill_renderer::TOP_BAR_TEXT_INSET;
                    (
                        PixelRect::from_logical(
                            content_left,
                            pill_renderer::TOP_BAR_Y,
                            (content_right - content_left).max(0.0),
                            pill_renderer::TOP_BAR_HEIGHT,
                            scale,
                        ),
                        TextAlign::Right,
                    )
                }
            };
            // Every preview shares one rich path and fixed line box for a
            // stable baseline. Marker padding is display-only; commits
            // still hold `\n`.
            if content.alignment == TopBarAlignment::Trailing {
                self.draw_line_break_preview(
                    rgba,
                    width,
                    height,
                    &content.content,
                    TOP_BAR_FONT_PIXELS * scale,
                    content_rect,
                    alignment,
                    204,
                );
            } else {
                self.draw_text(
                    rgba,
                    width,
                    height,
                    &content.content,
                    TOP_BAR_FONT_PIXELS * scale,
                    content_rect,
                    alignment,
                    TextColor::rgb(255, 255, 255),
                    204,
                );
            }
            if let Some(accessory) = content.accessory.as_deref() {
                let accessory_right = pill_renderer::TOP_BAR_X + pill_renderer::TOP_BAR_WIDTH
                    - pill_renderer::TOP_BAR_TEXT_INSET;
                let accessory_rect = PixelRect::from_logical(
                    accessory_right - pill_renderer::TOP_BAR_ACCESSORY_WIDTH,
                    pill_renderer::TOP_BAR_Y,
                    pill_renderer::TOP_BAR_ACCESSORY_WIDTH,
                    pill_renderer::TOP_BAR_HEIGHT,
                    scale,
                );
                self.draw_text(
                    rgba,
                    width,
                    height,
                    accessory,
                    TOP_BAR_FONT_PIXELS * TOP_BAR_LINE_BREAK_SCALE * scale,
                    accessory_rect,
                    TextAlign::Center,
                    LINE_BREAK_CYAN,
                    204,
                );
            }
        }
    }

    /// Draw the single centered status word for the mini text-action pill
    /// (grammar, question). The rect is the whole mini pill; nothing else
    /// is composited onto a mini frame.
    pub(crate) fn draw_mini_text(
        &mut self,
        rgba: &mut [u8],
        width: u32,
        height: u32,
        scale: f32,
        text: Option<&str>,
    ) {
        if let Some(text) = text.filter(|text| !text.is_empty()) {
            let rect = PixelRect::from_logical(
                0.0,
                0.0,
                pill_renderer::MINI_WIDTH,
                pill_renderer::MINI_HEIGHT,
                scale,
            );
            self.draw_text(
                rgba,
                width,
                height,
                text,
                MAIN_FONT_PIXELS * scale,
                rect,
                TextAlign::Center,
                TextColor::rgb(255, 255, 255),
                u8::MAX,
            );
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_text(
        &mut self,
        rgba: &mut [u8],
        width: u32,
        height: u32,
        text: &str,
        font_size: f32,
        clip: PixelRect,
        alignment: TextAlign,
        color: TextColor,
        opacity: u8,
    ) {
        if text.is_empty() || clip.width <= 0 || clip.height <= 0 {
            return;
        }
        let line_height = (font_size * 1.35).max(1.0);
        let mut buffer = Buffer::new(
            &mut self.fonts,
            Metrics::new(font_size.max(1.0), line_height),
        );
        buffer.set_wrap(&mut self.fonts, Wrap::None);
        buffer.set_size(&mut self.fonts, None, Some(clip.height as f32));
        let attrs = Attrs::new().family(Family::Name("Segoe UI"));
        buffer.set_text(&mut self.fonts, text, &attrs, Shaping::Advanced, None);
        buffer.shape_until_scroll(&mut self.fonts, false);

        let mut raster = Vec::new();
        buffer.draw(
            &mut self.fonts,
            &mut self.cache,
            color,
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
        Self::blit(rgba, width, height, &raster, clip, alignment, opacity);
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_line_break_preview(
        &mut self,
        rgba: &mut [u8],
        width: u32,
        height: u32,
        preview: &str,
        font_size: f32,
        clip: PixelRect,
        alignment: TextAlign,
        opacity: u8,
    ) {
        let spans = line_break_spans(preview, font_size);
        let refs: Vec<(&str, Attrs)> = spans
            .iter()
            .map(|(text, attrs)| (text.as_str(), attrs.clone()))
            .collect();
        // Mixed-size spans move the laid-out baseline; rebase onto the
        // uniform-size baseline.
        let baseline_dy = if preview.contains(LINE_BREAK_MARKER) {
            let line_height = (font_size * TOP_BAR_LINE_BREAK_SCALE * 1.35).max(1.0);
            match (
                self.uniform_line_y(preview, font_size, line_height, clip.height),
                self.rich_line_y(&refs, font_size, line_height, clip.height),
            ) {
                // Match `Buffer::draw`, which truncates the baseline.
                (Some(canonical), Some(real)) => canonical as i32 - real as i32,
                _ => 0,
            }
        } else {
            0
        };
        self.draw_rich_text(
            rgba,
            width,
            height,
            &refs,
            font_size,
            clip,
            alignment,
            TextColor::rgb(255, 255, 255),
            opacity,
            baseline_dy,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_rich_text(
        &mut self,
        rgba: &mut [u8],
        width: u32,
        height: u32,
        spans: &[(&str, Attrs)],
        base_font_size: f32,
        clip: PixelRect,
        alignment: TextAlign,
        default_color: TextColor,
        opacity: u8,
        baseline_dy: i32,
    ) {
        if spans.is_empty() || clip.width <= 0 || clip.height <= 0 {
            return;
        }
        // Size the line box for the enlarged marker so it never clips.
        let line_height = (base_font_size * TOP_BAR_LINE_BREAK_SCALE * 1.35).max(1.0);
        let mut buffer = Buffer::new(
            &mut self.fonts,
            Metrics::new(base_font_size.max(1.0), line_height),
        );
        buffer.set_wrap(&mut self.fonts, Wrap::None);
        buffer.set_size(&mut self.fonts, None, Some(clip.height as f32));
        let default_attrs = Attrs::new().family(Family::Name("Segoe UI"));
        buffer.set_rich_text(
            &mut self.fonts,
            spans.iter().map(|(text, attrs)| (*text, attrs.clone())),
            &default_attrs,
            Shaping::Advanced,
            None,
        );
        buffer.shape_until_scroll(&mut self.fonts, false);

        let mut raster = Vec::new();
        buffer.draw(
            &mut self.fonts,
            &mut self.cache,
            default_color,
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
        Self::blit_pinned(
            rgba,
            width,
            height,
            &raster,
            clip,
            alignment,
            opacity,
            line_height,
            baseline_dy,
        );
    }

    /// Laid-out baseline of a uniform-size buffer, in buffer pixels.
    fn uniform_line_y(
        &mut self,
        text: &str,
        font_size: f32,
        line_height: f32,
        height: i32,
    ) -> Option<f32> {
        let mut buffer = Buffer::new(
            &mut self.fonts,
            Metrics::new(font_size.max(1.0), line_height),
        );
        buffer.set_wrap(&mut self.fonts, Wrap::None);
        buffer.set_size(&mut self.fonts, None, Some(height as f32));
        let attrs = Attrs::new().family(Family::Name("Segoe UI"));
        buffer.set_text(&mut self.fonts, text, &attrs, Shaping::Advanced, None);
        buffer.shape_until_scroll(&mut self.fonts, false);
        buffer.layout_runs().next().map(|run| run.line_y)
    }

    /// Laid-out baseline of the mixed-size rich buffer, in buffer pixels.
    fn rich_line_y(
        &mut self,
        spans: &[(&str, Attrs)],
        base_font_size: f32,
        line_height: f32,
        height: i32,
    ) -> Option<f32> {
        let mut buffer = Buffer::new(
            &mut self.fonts,
            Metrics::new(base_font_size.max(1.0), line_height),
        );
        buffer.set_wrap(&mut self.fonts, Wrap::None);
        buffer.set_size(&mut self.fonts, None, Some(height as f32));
        let default_attrs = Attrs::new().family(Family::Name("Segoe UI"));
        buffer.set_rich_text(
            &mut self.fonts,
            spans.iter().map(|(text, attrs)| (*text, attrs.clone())),
            &default_attrs,
            Shaping::Advanced,
            None,
        );
        buffer.shape_until_scroll(&mut self.fonts, false);
        buffer.layout_runs().next().map(|run| run.line_y)
    }

    fn blit(
        rgba: &mut [u8],
        width: u32,
        height: u32,
        raster: &[RasterPixel],
        clip: PixelRect,
        alignment: TextAlign,
        opacity: u8,
    ) {
        let Some(bounds) = RasterBounds::for_pixels(raster) else {
            return;
        };
        let origin_x = match alignment {
            TextAlign::Left => clip.left - bounds.left,
            TextAlign::Center => clip.left + (clip.width - bounds.width()) / 2 - bounds.left,
            TextAlign::Right => clip.right() - bounds.width() - bounds.left,
        };
        let origin_y = clip.top + (clip.height - bounds.height()) / 2 - bounds.top;

        Self::blit_at(
            rgba, width, height, raster, clip, origin_x, origin_y, opacity,
        );
    }

    /// Blit with the vertical origin pinned to the fixed line box, which
    /// unlike the ink box does not vary with content.
    #[allow(clippy::too_many_arguments)]
    fn blit_pinned(
        rgba: &mut [u8],
        width: u32,
        height: u32,
        raster: &[RasterPixel],
        clip: PixelRect,
        alignment: TextAlign,
        opacity: u8,
        line_height: f32,
        baseline_dy: i32,
    ) {
        let Some(bounds) = RasterBounds::for_pixels(raster) else {
            return;
        };
        let origin_x = match alignment {
            TextAlign::Left => clip.left - bounds.left,
            TextAlign::Center => clip.left + (clip.width - bounds.width()) / 2 - bounds.left,
            TextAlign::Right => clip.right() - bounds.width() - bounds.left,
        };
        let origin_y =
            clip.top + ((clip.height as f32 - line_height) / 2.0).round() as i32 + baseline_dy;

        Self::blit_at(
            rgba, width, height, raster, clip, origin_x, origin_y, opacity,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn blit_at(
        rgba: &mut [u8],
        width: u32,
        height: u32,
        raster: &[RasterPixel],
        clip: PixelRect,
        origin_x: i32,
        origin_y: i32,
        opacity: u8,
    ) {
        for pixel in raster {
            for offset_y in 0..pixel.height as i32 {
                for offset_x in 0..pixel.width as i32 {
                    let x = origin_x + pixel.x + offset_x;
                    let y = origin_y + pixel.y + offset_y;
                    if clip.contains(x, y) {
                        composite_pixel(rgba, width, height, x, y, pixel.color, opacity);
                    }
                }
            }
        }
    }
}

/// Split a preview into render spans; each `↵` becomes a padded, larger
/// cyan span.
fn line_break_spans(preview: &str, base_font_size: f32) -> Vec<(String, Attrs<'_>)> {
    let base = Attrs::new().family(Family::Name("Segoe UI"));
    let marker_font = (base_font_size * TOP_BAR_LINE_BREAK_SCALE).max(1.0);
    let marker = Attrs::new()
        .family(Family::Name("Segoe UI"))
        .color(LINE_BREAK_CYAN)
        .metrics(Metrics::new(marker_font, (marker_font * 1.35).max(1.0)));
    let mut spans = Vec::new();
    for (index, segment) in preview.split(LINE_BREAK_MARKER).enumerate() {
        if index > 0 {
            spans.push((" ".to_string(), base.clone()));
            spans.push((LINE_BREAK_MARKER.to_string(), marker.clone()));
            spans.push((" ".to_string(), base.clone()));
        }
        if !segment.is_empty() {
            spans.push((segment.to_string(), base.clone()));
        }
    }
    spans
}

#[derive(Clone, Copy)]
enum TextAlign {
    Left,
    Center,
    Right,
}

#[derive(Clone, Copy)]
struct PixelRect {
    left: i32,
    top: i32,
    width: i32,
    height: i32,
}

impl PixelRect {
    fn from_logical(x: f32, y: f32, width: f32, height: f32, scale: f32) -> Self {
        Self {
            left: (x * scale).round() as i32,
            top: (y * scale).round() as i32,
            width: (width * scale).round() as i32,
            height: (height * scale).round() as i32,
        }
    }

    fn right(self) -> i32 {
        self.left + self.width
    }

    fn contains(self, x: i32, y: i32) -> bool {
        x >= self.left && x < self.right() && y >= self.top && y < self.top + self.height
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
mod tests {
    use super::{
        LINE_BREAK_CYAN, PixelRect, TextAlign, TextRasterizer, composite_pixel, line_break_spans,
    };
    use cosmic_text::Color;

    fn render_preview(preview: &str) -> Vec<u8> {
        let clip = PixelRect::from_logical(0.0, 0.0, 300.0, 31.2, 1.0);
        let mut canvas = vec![0_u8; 300 * 31 * 4];
        TextRasterizer::new().draw_line_break_preview(
            &mut canvas,
            300,
            31,
            preview,
            13.2,
            clip,
            TextAlign::Left,
            204,
        );
        canvas
    }

    fn ink_pixel_count(canvas: &[u8]) -> usize {
        canvas.chunks_exact(4).filter(|pixel| pixel[3] != 0).count()
    }

    fn assert_shared_prefix_stable(base: &[u8], extended: &[u8], typed: char) {
        assert!(ink_pixel_count(extended) > ink_pixel_count(base));
        for (index, (a, b)) in base
            .chunks_exact(4)
            .zip(extended.chunks_exact(4))
            .enumerate()
        {
            if a != [0, 0, 0, 0] {
                assert_eq!(
                    a, b,
                    "shared prefix pixel {index} moved after typing '{typed}'"
                );
            }
        }
    }

    /// Shared prefix pixels are stable when a taller glyph is typed.
    #[test]
    fn typing_a_taller_glyph_keeps_shared_prefix_pixels_stable() {
        let base = render_preview("pro");
        let taller = render_preview("prol");
        assert_shared_prefix_stable(&base, &taller, 'l');
    }

    /// Shared prefix pixels are stable across marker insertion.
    #[test]
    fn inserting_a_line_break_keeps_shared_prefix_pixels_stable() {
        let base = render_preview("ab");
        let marked = render_preview("ab↵cd");
        assert_shared_prefix_stable(&base, &marked, '↵');
    }

    #[test]
    fn text_compositing_creates_premultiplied_alpha_on_transparency() {
        let mut rgba = [0_u8; 4];
        composite_pixel(&mut rgba, 1, 1, 0, 0, Color::rgba(255, 255, 255, 128), 128);
        assert_eq!(rgba, [64, 64, 64, 64]);
    }

    #[test]
    fn text_compositing_obeys_the_clip_surface_bounds() {
        let mut rgba = [0_u8; 4];
        composite_pixel(&mut rgba, 1, 1, 1, 0, Color::rgb(255, 255, 255), 255);
        assert_eq!(rgba, [0, 0, 0, 0]);
    }

    #[test]
    fn line_break_preview_pads_the_marker_and_marks_it_cyan() {
        let spans = line_break_spans("ab↵cd", 13.2);
        let texts: Vec<&str> = spans.iter().map(|(text, _)| text.as_str()).collect();
        assert_eq!(texts, ["ab", " ", "↵", " ", "cd"]);
        assert!(spans[0].1.color_opt.is_none());
        assert!(spans[0].1.metrics_opt.is_none());
        assert_eq!(spans[2].1.color_opt, Some(LINE_BREAK_CYAN));
        assert!(spans[2].1.metrics_opt.is_some());
    }
}
