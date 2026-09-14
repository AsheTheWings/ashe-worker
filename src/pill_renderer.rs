use tiny_skia::{Color, FillRule, Paint, Path, PathBuilder, Pixmap, Transform};

pub const WIDTH: f32 = 428.0;
pub const PILL_HEIGHT: f32 = 64.0;
pub const MAIN_TOP: f32 = 31.2;
pub const HEIGHT: f32 = MAIN_TOP + PILL_HEIGHT;
pub const TOP_BAR_X: f32 = 52.0;
pub const TOP_BAR_Y: f32 = 0.0;
pub const TOP_BAR_WIDTH: f32 = WIDTH - 2.0 * TOP_BAR_X;
pub const TOP_BAR_HEIGHT: f32 = 31.2;
pub const TOP_BAR_TEXT_INSET: f32 = 12.0;
pub const TOP_BAR_COUNT_WIDTH: f32 = 96.0;
pub const TOP_BAR_CONTENT_GAP: f32 = 2.0;
pub const TOP_BAR_ACCESSORY_WIDTH: f32 = 20.0;
const BORDER_WIDTH: f32 = 2.0;
const TOP_BAR_CORNER_RADIUS: f32 = 10.0;
const TOP_BAR_SHOULDER_RADIUS: f32 = 10.0;
/// Keep the antialiased edge inside the layered bitmap.
const PILL_EDGE_INSET: f32 = 1.0;
/// Horizontal margin between the pill edge and the bar area. Identical on
/// both ends by construction.
const BAR_MARGIN: f32 = 12.0;
const BAR_VERTICAL_INSET: f32 = 8.0;
const BAR_MIN_HEIGHT: f32 = 3.0;
const IDLE_BAR_VALUE: f32 = 0.06;
const BACKGROUND: [u8; 4] = [6, 19, 25, 255];
const LISTENING_BORDER: [u8; 4] = [0, 197, 224, 255];
const WORKING_BORDER: [u8; 4] = [0, 101, 115, 255];
const ERROR_BORDER: [u8; 4] = [217, 69, 61, 255];
const _: () = assert!(HEIGHT <= 104.0, "dictate overlay stays compact");
const _: () = assert!(WIDTH <= 480.0, "dictate overlay stays compact");
const _: () = assert!(PILL_HEIGHT < WIDTH, "pill stays wider than tall");

/// Visual lifecycle of the dictate pill.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PillState {
    /// Microphone hot. Bars follow the live input level.
    Listening,
    /// Transcribing or inserting. Static status text, distinct from listening.
    Working,
    /// Last operation failed.
    Error,
    /// Text actions and other quiet states.
    Idle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TopBarAlignment {
    Center,
    Trailing,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopBarContent {
    pub count: String,
    pub content: String,
    pub alignment: TopBarAlignment,
    pub accessory: Option<String>,
}

impl PillState {
    fn border(self) -> [u8; 4] {
        match self {
            Self::Listening => LISTENING_BORDER,
            Self::Working => WORKING_BORDER,
            Self::Error => ERROR_BORDER,
            Self::Idle => BACKGROUND,
        }
    }
}

/// Render one antialiased pill frame as premultiplied RGBA pixels.
///
/// Windows presentation swaps these bytes to premultiplied BGRA before
/// passing the frame to `UpdateLayeredWindow`.
///
/// The pill background must render even if the connected top-bar geometry
/// cannot be built, so a path construction failure falls back to a plain
/// pill instead of returning `None` and leaving the overlay hidden while
/// dictation itself keeps working.
pub fn render_rgba(
    pixel_width: u32,
    pixel_height: u32,
    bars: &[f32],
    state: PillState,
    show_visualizer: bool,
    show_top_bar: bool,
) -> Option<Vec<u8>> {
    let mut pixmap = Pixmap::new(pixel_width, pixel_height)?;
    let scale = pixel_width as f32 / WIDTH;
    let transform = Transform::from_scale(scale, scale);

    if show_top_bar {
        match (
            connected_body_path(PILL_EDGE_INSET),
            connected_body_path(PILL_EDGE_INSET + BORDER_WIDTH),
        ) {
            (Some(outer), Some(inner)) => {
                fill_capsule(&mut pixmap, outer, state.border(), transform);
                fill_capsule(&mut pixmap, inner, BACKGROUND, transform);
            }
            // Geometry must never hide the dictation UI. Fall back to the
            // standalone pill so a future constant change cannot turn every
            // frame into a silent `None`.
            _ => {
                paint_standalone_pill(&mut pixmap, state, transform)?;
            }
        }
        symmetrize_horizontal(&mut pixmap);
    } else {
        paint_standalone_pill(&mut pixmap, state, transform)?;
        symmetrize_horizontal(&mut pixmap);
    }

    if state != PillState::Working && show_visualizer {
        let (bars_x, bars_width) = bars_area(WIDTH);
        let count = bars.len().max(1);
        let specs = bar_specs(bars, bars_width, count);
        let bars_height = PILL_HEIGHT - 2.0 * BAR_VERTICAL_INSET;
        for spec in &specs {
            let value = match state {
                PillState::Listening | PillState::Error => spec.value,
                PillState::Working | PillState::Idle => IDLE_BAR_VALUE,
            };
            let height = BAR_MIN_HEIGHT + value * (bars_height - BAR_MIN_HEIGHT);
            let color = match state {
                PillState::Listening => [0, 224, 255, alpha(0.30 + 0.65 * value)],
                PillState::Error => [255, 82, 71, alpha(0.35 + 0.55 * value)],
                PillState::Working | PillState::Idle => [219, 242, 255, alpha(0.22)],
            };
            if let Some(path) = capsule_path(
                bars_x + spec.x,
                MAIN_TOP + (PILL_HEIGHT - height) / 2.0,
                spec.width,
                height,
            ) {
                fill_capsule(&mut pixmap, path, color, transform);
            }
        }
    }
    if !show_top_bar {
        symmetrize_vertical_region(&mut pixmap, MAIN_TOP, PILL_HEIGHT, scale);
    }
    Some(pixmap.take())
}

/// Paint the standalone main pill (no top bar) with the given lifecycle
/// border. Returns `None` only when the capsule geometry itself is
/// degenerate, which indicates a programming error rather than a transient
/// frame condition.
fn paint_standalone_pill(
    pixmap: &mut Pixmap,
    state: PillState,
    transform: Transform,
) -> Option<()> {
    let outer = PILL_EDGE_INSET;
    fill_capsule(
        pixmap,
        capsule_path(
            outer,
            MAIN_TOP + outer,
            WIDTH - 2.0 * outer,
            PILL_HEIGHT - 2.0 * outer,
        )?,
        state.border(),
        transform,
    );
    let inner = outer + BORDER_WIDTH;
    fill_capsule(
        pixmap,
        capsule_path(
            inner,
            MAIN_TOP + inner,
            WIDTH - 2.0 * inner,
            PILL_HEIGHT - 2.0 * inner,
        )?,
        BACKGROUND,
        transform,
    );
    Some(())
}

fn alpha(value: f32) -> u8 {
    (value.clamp(0.0, 1.0) * 255.0).round() as u8
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

fn symmetrize_vertical_region(pixmap: &mut Pixmap, top: f32, height: f32, scale: f32) {
    let width = pixmap.width() as usize;
    let start = (top * scale).round().max(0.0) as usize;
    let region_height = (height * scale).round().max(0.0) as usize;
    let end = start
        .saturating_add(region_height)
        .min(pixmap.height() as usize);
    let pixels = pixmap.data_mut();
    for offset in 0..(end - start) / 2 {
        let upper = start + offset;
        let lower = end - 1 - offset;
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

/// Build the outline shared by the raised bar and main pill. The bar has no
/// bottom edge: concave shoulders turn its sides directly into the main top
/// edge so the two regions read as one continuous body.
fn connected_body_path(inset: f32) -> Option<Path> {
    let border_offset = (inset - PILL_EDGE_INSET).max(0.0);
    let main_left = inset;
    let main_right = WIDTH - inset;
    let main_top = MAIN_TOP + inset;
    let main_bottom = HEIGHT - inset;
    let main_radius = (main_bottom - main_top) / 2.0;

    let tab_left = TOP_BAR_X + inset;
    let tab_right = TOP_BAR_X + TOP_BAR_WIDTH - inset;
    let tab_top = TOP_BAR_Y + inset;
    let top_radius = (TOP_BAR_CORNER_RADIUS - border_offset).max(1.0);
    // An inward offset grows a concave radius. This keeps the inner and outer
    // shoulder centers identical and therefore the border width uniform.
    let shoulder_radius = TOP_BAR_SHOULDER_RADIUS + border_offset;
    if main_radius <= 0.0
        || tab_right <= tab_left
        || main_top - shoulder_radius <= tab_top + top_radius
        || tab_left - shoulder_radius <= main_left + main_radius
        || tab_right + shoulder_radius >= main_right - main_radius
    {
        return None;
    }

    let top_tangent = top_radius * 0.552_284_8;
    let shoulder_tangent = shoulder_radius * 0.552_284_8;
    let main_tangent = main_radius * 0.552_284_8;
    let mut path = PathBuilder::new();

    path.move_to(main_left + main_radius, main_top);
    path.line_to(tab_left - shoulder_radius, main_top);
    path.cubic_to(
        tab_left - shoulder_radius + shoulder_tangent,
        main_top,
        tab_left,
        main_top - shoulder_radius + shoulder_tangent,
        tab_left,
        main_top - shoulder_radius,
    );
    path.line_to(tab_left, tab_top + top_radius);
    path.cubic_to(
        tab_left,
        tab_top + top_radius - top_tangent,
        tab_left + top_radius - top_tangent,
        tab_top,
        tab_left + top_radius,
        tab_top,
    );
    path.line_to(tab_right - top_radius, tab_top);
    path.cubic_to(
        tab_right - top_radius + top_tangent,
        tab_top,
        tab_right,
        tab_top + top_radius - top_tangent,
        tab_right,
        tab_top + top_radius,
    );
    path.line_to(tab_right, main_top - shoulder_radius);
    path.cubic_to(
        tab_right,
        main_top - shoulder_radius + shoulder_tangent,
        tab_right + shoulder_radius - shoulder_tangent,
        main_top,
        tab_right + shoulder_radius,
        main_top,
    );
    path.line_to(main_right - main_radius, main_top);
    path.cubic_to(
        main_right - main_radius + main_tangent,
        main_top,
        main_right,
        main_top + main_radius - main_tangent,
        main_right,
        main_top + main_radius,
    );
    path.cubic_to(
        main_right,
        main_bottom - main_radius + main_tangent,
        main_right - main_radius + main_tangent,
        main_bottom,
        main_right - main_radius,
        main_bottom,
    );
    path.line_to(main_left + main_radius, main_bottom);
    path.cubic_to(
        main_left + main_radius - main_tangent,
        main_bottom,
        main_left,
        main_bottom - main_radius + main_tangent,
        main_left,
        main_bottom - main_radius,
    );
    path.cubic_to(
        main_left,
        main_top + main_radius - main_tangent,
        main_left + main_radius - main_tangent,
        main_top,
        main_left + main_radius,
        main_top,
    );
    path.close();
    path.finish()
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

/// Horizontal bar area for a window of the given width: `(x_offset,
/// width)`. One margin on each side keeps both pill ends identical.
pub fn bars_area(total_width: f32) -> (f32, f32) {
    (BAR_MARGIN, (total_width - 2.0 * BAR_MARGIN).max(0.0))
}

/// Horizontal layout of one bar per value: even pitch across the area with
/// neighbor-averaged values so motion stays fluid instead of jagged.
#[derive(Debug, Clone, PartialEq)]
pub struct BarSpec {
    pub x: f32,
    pub width: f32,
    pub value: f32,
}

pub fn bar_specs(values: &[f32], area_width: f32, bar_count: usize) -> Vec<BarSpec> {
    if bar_count == 0 || area_width <= 0.0 {
        return Vec::new();
    }
    let pitch = area_width / bar_count as f32;
    // The pill is wider, but the approved fine waveform stroke stays the
    // same; extra width becomes breathing room instead of thicker bars.
    let width = (pitch * 0.5).clamp(1.0, 2.0);
    let start = values.len().saturating_sub(bar_count);
    let window = &values[start..];
    let pad = bar_count.saturating_sub(window.len());
    (0..bar_count)
        .map(|index| {
            let value = if index < pad || window.is_empty() {
                0.0
            } else {
                let position = index - pad;
                let at = |offset: usize| window[offset.min(window.len() - 1)];
                (at(position.saturating_sub(1)) + at(position) + at(position + 1)) / 3.0
            };
            let mirrored_index = bar_count - 1 - index;
            let left_index = index.min(mirrored_index);
            let left_x = left_index as f32 * pitch + (pitch - width) / 2.0;
            let x = if index <= mirrored_index {
                left_x
            } else {
                area_width - left_x - width
            };
            BarSpec {
                x,
                width,
                value: value.clamp(0.0, 1.0),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{BORDER_WIDTH, HEIGHT, MAIN_TOP, PILL_HEIGHT, PillState, WIDTH};

    #[test]
    fn main_pill_keeps_the_approved_geometry() {
        assert_eq!(WIDTH, 428.0);
        assert_eq!(PILL_HEIGHT, 64.0);
        assert_eq!(BORDER_WIDTH, 2.0);
        assert_eq!(HEIGHT, MAIN_TOP + PILL_HEIGHT);
    }

    #[test]
    fn bar_area_keeps_equal_margins_on_both_ends() {
        assert_eq!(super::bars_area(428.0), (12.0, 404.0));
        let (offset, width) = super::bars_area(428.0);
        assert_eq!(428.0 - (offset + width), offset);
        assert_eq!(super::bars_area(10.0), (12.0, 0.0));
    }

    #[test]
    fn bar_specs_cover_the_area_with_even_pitch() {
        let values = vec![0.5; 8];
        let specs = super::bar_specs(&values, 160.0, 8);
        assert_eq!(specs.len(), 8);
        for pair in specs.windows(2) {
            let pitch = pair[1].x - pair[0].x;
            assert!((pitch - 20.0).abs() < 0.001);
            assert_eq!(pair[0].width, pair[1].width);
        }
        for spec in &specs {
            assert!(spec.x >= 0.0);
            assert!(spec.x + spec.width <= 160.0 + 0.001);
            assert_eq!(spec.width, 2.0);
        }
    }

    #[test]
    fn bar_specs_pad_short_histories_and_clamp() {
        let specs = super::bar_specs(&[2.0, -1.0], 100.0, 4);
        assert_eq!(specs.len(), 4);
        assert_eq!(specs[0].value, 0.0);
        assert_eq!(specs[1].value, 0.0);
        for spec in &specs {
            assert!((0.0..=1.0).contains(&spec.value));
        }
        assert!(super::bar_specs(&[0.5], 100.0, 0).is_empty());
        assert!(super::bar_specs(&[0.5], 0.0, 4).is_empty());
    }

    #[test]
    fn bar_specs_smooth_neighbors() {
        let specs = super::bar_specs(&[0.0, 0.3, 0.6, 0.9], 80.0, 4);
        assert!((specs[0].value - 0.1).abs() < 0.0001);
        assert!((specs[1].value - 0.3).abs() < 0.0001);
        assert!((specs[2].value - 0.6).abs() < 0.0001);
        assert!((specs[3].value - 0.8).abs() < 0.0001);
    }

    #[test]
    fn layered_bitmap_has_transparent_corners_and_symmetric_edges() {
        let width = (WIDTH * 1.25).round() as u32;
        let height = (HEIGHT * 1.25).round() as u32;
        let pixels = super::render_rgba(width, height, &[], PillState::Working, false, false)
            .expect("pill should render");
        let pixel_at = |x: u32, y: u32| {
            let offset = ((y * width + x) * 4) as usize;
            &pixels[offset..offset + 4]
        };

        let main_top = (MAIN_TOP * 1.25).round() as u32;
        let main_height = (PILL_HEIGHT * 1.25).round() as u32;
        assert_eq!(pixel_at(0, main_top)[3], 0);
        assert_eq!(pixel_at(width - 1, main_top)[3], 0);
        assert_eq!(pixel_at(0, main_top + main_height - 1)[3], 0);
        assert_eq!(pixel_at(width - 1, main_top + main_height - 1)[3], 0);

        for y in 0..main_height {
            for x in 0..width {
                assert_eq!(
                    pixel_at(x, main_top + y),
                    pixel_at(x, main_top + main_height - 1 - y)
                );
            }
        }
        for y in 0..height {
            for x in 0..width {
                assert_eq!(pixel_at(x, y), pixel_at(width - 1 - x, y), "x={x} y={y}");
            }
        }
    }

    #[test]
    fn waveform_is_exactly_centered_vertically() {
        let width = (WIDTH * 1.25).round() as u32;
        let height = (HEIGHT * 1.25).round() as u32;
        let bars: Vec<f32> = (0..64).map(|index| index as f32 / 63.0).collect();
        let pixels = super::render_rgba(width, height, &bars, PillState::Listening, true, false)
            .expect("waveform should render");
        let pixel_at = |x: u32, y: u32| {
            let offset = ((y * width + x) * 4) as usize;
            &pixels[offset..offset + 4]
        };
        let main_top = (MAIN_TOP * 1.25).round() as u32;
        let main_height = (PILL_HEIGHT * 1.25).round() as u32;
        for y in 0..main_height {
            for x in 0..width {
                assert_eq!(
                    pixel_at(x, main_top + y),
                    pixel_at(x, main_top + main_height - 1 - y)
                );
            }
        }
    }

    #[test]
    fn equal_waveform_values_keep_equal_end_padding() {
        let width = (WIDTH * 1.25).round() as u32;
        let height = (HEIGHT * 1.25).round() as u32;
        let pixels =
            super::render_rgba(width, height, &[0.5; 64], PillState::Listening, true, false)
                .expect("waveform should render");
        let base = super::render_rgba(width, height, &[], PillState::Listening, true, false)
            .expect("base pill should render");
        let changed_columns: Vec<u32> = (0..width)
            .filter(|&x| {
                (0..height).any(|y| {
                    let offset = ((y * width + x) * 4) as usize;
                    pixels[offset..offset + 4] != base[offset..offset + 4]
                })
            })
            .collect();
        let first = *changed_columns.first().expect("waveform has pixels");
        let last = *changed_columns.last().expect("waveform has pixels");
        assert_eq!(first, width - 1 - last);
    }

    #[test]
    fn typing_bar_uses_native_alpha_above_the_main_pill() {
        let width = WIDTH as u32;
        let height = HEIGHT as u32;
        let hidden = super::render_rgba(width, height, &[], PillState::Listening, true, false)
            .expect("hidden tab frame");
        let shown = super::render_rgba(width, height, &[], PillState::Listening, true, true)
            .expect("shown tab frame");
        let sample_x = (super::TOP_BAR_X + super::TOP_BAR_WIDTH / 2.0) as u32;
        let sample_y = 2_u32;
        let offset = ((sample_y * width + sample_x) * 4 + 3) as usize;
        assert_eq!(hidden[offset], 0);
        assert!(shown[offset] > 0);
    }

    /// Dictation must never run with an invisible pill: every lifecycle
    /// state used by the overlay has to produce an opaque frame at the
    /// DPIs the layered window actually presents, with or without the
    /// always-on status bar.
    #[test]
    fn dictation_frames_stay_visible_in_every_lifecycle_state() {
        let states = [
            PillState::Listening,
            PillState::Working,
            PillState::Error,
            PillState::Idle,
        ];
        for scale in [1.0f32, 1.25, 1.5, 2.0] {
            let width = (WIDTH * scale).round().max(1.0) as u32;
            let main_top = (MAIN_TOP * scale).round().max(0.0) as u32;
            let main_height = (PILL_HEIGHT * scale).round().max(1.0) as u32;
            let height = main_top.saturating_add(main_height).max(1);
            for state in states {
                for show_top_bar in [false, true] {
                    let pixels = super::render_rgba(
                        width,
                        height,
                        &[0.2; 64],
                        state,
                        true,
                        show_top_bar,
                    )
                    .unwrap_or_else(|| {
                        panic!(
                            "pill must render for state={state:?} \
                             top_bar={show_top_bar} scale={scale}"
                        )
                    });
                    let opaque = pixels
                        .chunks_exact(4)
                        .filter(|pixel| pixel[3] > 10)
                        .count();
                    let total = pixels.len() / 4;
                    // The pill background alone covers well over half the
                    // window even before bars or text. Anything far below
                    // that means the frame went transparent while dictation
                    // itself would keep working.
                    assert!(
                        opaque * 2 > total,
                        "pill went transparent for state={state:?} \
                         top_bar={show_top_bar} scale={scale}: \
                         opaque={opaque}/{total}"
                    );
                }
            }
        }
    }

    /// The connected top-bar body must build for both the outer border and
    /// the inset background. A `None` here used to propagate out of
    /// `render_rgba` as a silent `None` frame, which left dictation working
    /// with no visible UI and only one log line.
    #[test]
    fn connected_body_builds_for_both_border_insets() {
        assert!(super::connected_body_path(super::PILL_EDGE_INSET).is_some());
        assert!(super::connected_body_path(super::PILL_EDGE_INSET + super::BORDER_WIDTH).is_some());
    }
}
