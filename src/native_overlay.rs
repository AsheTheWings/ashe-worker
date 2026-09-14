#![allow(unsafe_op_in_unsafe_fn)]

use crate::logger;
use crate::overlay_text::TextRasterizer;
use crate::pill_renderer::{self, PillState, TopBarContent};
use crate::util::{pcwstr, wide};
use anyhow::{Context, Result, anyhow};
use std::ffi::c_void;
use std::mem::size_of;
use std::ptr::null_mut;
use std::slice;
use windows::Win32::Foundation::{COLORREF, HMODULE, HWND, LPARAM, LRESULT, POINT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{
    AC_SRC_ALPHA, AC_SRC_OVER, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION,
    CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS, DeleteDC, DeleteObject, HBITMAP, HDC,
    HGDIOBJ, MONITOR_DEFAULTTONEAREST, MonitorFromPoint, SelectObject,
};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, HTTRANSPARENT, IsWindowVisible,
    RegisterClassExW, SW_HIDE, SW_SHOWNOACTIVATE, ShowWindow, ULW_ALPHA, UpdateLayeredWindow,
    WINDOW_EX_STYLE, WM_ERASEBKGND, WM_NCHITTEST, WNDCLASSEXW, WS_EX_LAYERED, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};

const CLASS_NAME: &str = "AsheWorkerLayeredOverlay";
const WINDOW_NAME: &str = "Ashe Worker Dictation Overlay";

/// A Win32 layered window whose shape comes exclusively from per-pixel alpha.
/// No chroma key, clipping region, or opaque backing surface participates.
pub struct NativeOverlay {
    hwnd: HWND,
    surface: Option<LayeredSurface>,
    text: TextRasterizer,
    visible: bool,
}

pub struct OverlayFrame<'a> {
    pub x: f32,
    pub y: f32,
    pub visible: bool,
    pub bars: &'a [f32],
    pub state: PillState,
    pub main_text: Option<&'a str>,
    pub top_bar: Option<&'a TopBarContent>,
    /// Compact text-action pill (grammar, question): a small window with
    /// one centered status word instead of the full dictate pill.
    pub mini: bool,
}

impl NativeOverlay {
    pub fn new(instance: HMODULE) -> Result<Self> {
        unsafe {
            let class_name = wide(CLASS_NAME);
            let class = WNDCLASSEXW {
                cbSize: size_of::<WNDCLASSEXW>() as u32,
                hInstance: instance.into(),
                lpfnWndProc: Some(window_proc),
                lpszClassName: pcwstr(&class_name),
                ..Default::default()
            };
            let _ = RegisterClassExW(&class);
            let ex_style = WINDOW_EX_STYLE(
                WS_EX_LAYERED.0
                    | WS_EX_TRANSPARENT.0
                    | WS_EX_TOOLWINDOW.0
                    | WS_EX_NOACTIVATE.0
                    | WS_EX_TOPMOST.0,
            );
            let hwnd = CreateWindowExW(
                ex_style,
                pcwstr(&class_name),
                pcwstr(&wide(WINDOW_NAME)),
                WS_POPUP,
                0,
                0,
                1,
                1,
                None,
                None,
                Some(instance.into()),
                Some(null_mut()),
            )
            .context("CreateWindowExW failed for layered overlay")?;
            logger::info(format!("Native layered overlay created hwnd={:p}", hwnd.0));
            Ok(Self {
                hwnd,
                surface: None,
                text: TextRasterizer::new(),
                visible: false,
            })
        }
    }

    pub fn update(&mut self, frame: OverlayFrame<'_>) -> Result<()> {
        unsafe { self.update_inner(frame) }
    }

    unsafe fn update_inner(&mut self, frame: OverlayFrame<'_>) -> Result<()> {
        if !frame.visible {
            self.hide();
            return Ok(());
        }

        let anchor = POINT {
            x: frame.x.round() as i32,
            y: frame.y.round() as i32,
        };
        let monitor_scale = monitor_scale_factor(anchor);
        // The mini pill is its own window size: just the compact capsule
        // with its bottom edge at the anchor, like the dictate pill.
        let logical_width = if frame.mini {
            pill_renderer::MINI_WIDTH
        } else {
            pill_renderer::WIDTH
        };
        let width = (logical_width * monitor_scale).round().max(1.0) as u32;
        let scale = width as f32 / logical_width;
        let (height, destination, rgba) = if frame.mini {
            let height = (pill_renderer::MINI_HEIGHT * scale).round().max(1.0) as u32;
            let destination = POINT {
                x: anchor.x,
                y: anchor.y - height as i32,
            };
            let mut rgba = pill_renderer::render_mini_rgba(width, height, frame.state)
                .ok_or_else(|| anyhow!("failed to allocate mini overlay frame"))?;
            self.text
                .draw_mini_text(&mut rgba, width, height, scale, frame.main_text);
            (height, destination, rgba)
        } else {
            let main_top = (pill_renderer::MAIN_TOP * scale).round().max(0.0) as u32;
            let main_height = (pill_renderer::PILL_HEIGHT * scale).round().max(1.0) as u32;
            let height = main_top.saturating_add(main_height);
            let destination = POINT {
                x: anchor.x,
                y: anchor.y - main_top as i32,
            };
            let mut rgba = pill_renderer::render_rgba(
                width,
                height,
                frame.bars,
                frame.state,
                frame.main_text.is_none(),
                frame.top_bar.is_some(),
            )
            .ok_or_else(|| anyhow!("failed to allocate layered overlay frame"))?;
            self.text.draw(
                &mut rgba,
                width,
                height,
                scale,
                frame.main_text,
                frame.top_bar,
            );
            (height, destination, rgba)
        };

        if self
            .surface
            .as_ref()
            .is_none_or(|surface| surface.width != width || surface.height != height)
        {
            self.surface = Some(LayeredSurface::new(width, height)?);
        }
        let surface = self.surface.as_mut().expect("surface initialized");
        surface.copy_premultiplied_rgba(&rgba)?;

        let source = POINT::default();
        let size = SIZE {
            cx: width as i32,
            cy: height as i32,
        };
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: u8::MAX,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        UpdateLayeredWindow(
            self.hwnd,
            None,
            Some(&destination),
            Some(&size),
            Some(surface.dc),
            Some(&source),
            COLORREF(0),
            Some(&blend),
            ULW_ALPHA,
        )
        .context("UpdateLayeredWindow failed")?;

        // Recover when Windows hides the overlay out from under us (display
        // change, workspace switch, DWM reset). The internal flag alone
        // cannot detect an external hide, which would otherwise leave
        // dictation working with no visible pill and no further ShowWindow
        // call to recover it.
        let actually_visible = IsWindowVisible(self.hwnd).as_bool();
        if !self.visible || !actually_visible {
            if ShowWindow(self.hwnd, SW_SHOWNOACTIVATE).as_bool() || !actually_visible {
                logger::info(format!(
                    "Native layered overlay shown size={}x{} scale={scale:.2} \
                     tracked_visible={} actual_visible={actually_visible} \
                     mini={} at=({},{})",
                    width,
                    height,
                    self.visible,
                    frame.mini,
                    destination.x,
                    destination.y,
                ));
            }
            self.visible = true;
        }
        Ok(())
    }

    fn hide(&mut self) {
        if self.visible {
            unsafe {
                let _ = ShowWindow(self.hwnd, SW_HIDE);
            }
            self.visible = false;
            logger::info("Native layered overlay hidden");
        }
    }
}

impl Drop for NativeOverlay {
    fn drop(&mut self) {
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_HIDE);
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

struct LayeredSurface {
    width: u32,
    height: u32,
    dc: HDC,
    bitmap: HBITMAP,
    previous_bitmap: HGDIOBJ,
    bits: *mut c_void,
}

impl LayeredSurface {
    unsafe fn new(width: u32, height: u32) -> Result<Self> {
        let byte_count = width
            .checked_mul(height)
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or_else(|| anyhow!("layered overlay dimensions overflow"))?;
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width as i32,
                biHeight: -(height as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                biSizeImage: byte_count,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = null_mut();
        let bitmap = CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0)
            .context("CreateDIBSection failed for layered overlay")?;
        if bits.is_null() {
            let _ = DeleteObject(HGDIOBJ(bitmap.0));
            return Err(anyhow!("CreateDIBSection returned no pixel buffer"));
        }
        let dc = CreateCompatibleDC(None);
        if dc.is_invalid() {
            let _ = DeleteObject(HGDIOBJ(bitmap.0));
            return Err(anyhow!("CreateCompatibleDC failed for layered overlay"));
        }
        let previous_bitmap = SelectObject(dc, HGDIOBJ(bitmap.0));
        if previous_bitmap.is_invalid() {
            let _ = DeleteObject(HGDIOBJ(bitmap.0));
            let _ = DeleteDC(dc);
            return Err(anyhow!("SelectObject failed for layered overlay bitmap"));
        }
        Ok(Self {
            width,
            height,
            dc,
            bitmap,
            previous_bitmap,
            bits,
        })
    }

    unsafe fn copy_premultiplied_rgba(&mut self, rgba: &[u8]) -> Result<()> {
        let expected = self.width as usize * self.height as usize * 4;
        if rgba.len() != expected {
            return Err(anyhow!(
                "layered overlay frame has {} bytes, expected {expected}",
                rgba.len()
            ));
        }
        let bgra = slice::from_raw_parts_mut(self.bits.cast::<u8>(), expected);
        let (source_pixels, _) = rgba.as_chunks::<4>();
        let (destination_pixels, _) = bgra.as_chunks_mut::<4>();
        for (source, destination) in source_pixels.iter().zip(destination_pixels) {
            destination[0] = source[2];
            destination[1] = source[1];
            destination[2] = source[0];
            destination[3] = source[3];
        }
        Ok(())
    }
}

impl Drop for LayeredSurface {
    fn drop(&mut self) {
        unsafe {
            let _ = SelectObject(self.dc, self.previous_bitmap);
            let _ = DeleteObject(HGDIOBJ(self.bitmap.0));
            let _ = DeleteDC(self.dc);
        }
    }
}

fn monitor_scale_factor(point: POINT) -> f32 {
    unsafe {
        let monitor = MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST);
        let mut dpi_x = 96;
        let mut dpi_y = 96;
        if GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y).is_err()
            || dpi_x == 0
        {
            1.0
        } else {
            dpi_x as f32 / 96.0
        }
    }
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),
        WM_ERASEBKGND => LRESULT(1),
        _ => DefWindowProcW(hwnd, message, wparam, lparam),
    }
}
