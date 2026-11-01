use anyhow::{Context, Result, anyhow};
use image::ExtendedColorType;
use image::codecs::webp::WebPEncoder;

pub struct CapturedFrame {
    pub width: u32,
    pub height: u32,
    pub webp: Vec<u8>,
    pub fingerprint: Vec<u8>,
}

#[cfg(target_os = "windows")]
pub fn capture_screen(all_monitors: bool) -> Result<CapturedFrame> {
    use std::ffi::c_void;
    use std::mem::size_of;
    use windows::Win32::Graphics::Gdi::{
        BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CAPTUREBLT, CreateCompatibleBitmap,
        CreateCompatibleDC, DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, GetDIBits, HGDIOBJ,
        ROP_CODE, ReleaseDC, SRCCOPY, SelectObject,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, SM_CXSCREEN, SM_CXVIRTUALSCREEN, SM_CYSCREEN, SM_CYVIRTUALSCREEN,
        SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
    };

    let (x, y, width, height) = unsafe {
        if all_monitors {
            (
                GetSystemMetrics(SM_XVIRTUALSCREEN),
                GetSystemMetrics(SM_YVIRTUALSCREEN),
                GetSystemMetrics(SM_CXVIRTUALSCREEN),
                GetSystemMetrics(SM_CYVIRTUALSCREEN),
            )
        } else {
            (
                0,
                0,
                GetSystemMetrics(SM_CXSCREEN),
                GetSystemMetrics(SM_CYSCREEN),
            )
        }
    };
    if width <= 0 || height <= 0 {
        return Err(anyhow!(
            "Windows returned an invalid screen size {width}x{height}"
        ));
    }

    let screen = unsafe { GetDC(None) };
    if screen.is_invalid() {
        return Err(anyhow!("GetDC failed"));
    }
    let memory = unsafe { CreateCompatibleDC(Some(screen)) };
    if memory.is_invalid() {
        unsafe { ReleaseDC(None, screen) };
        return Err(anyhow!("CreateCompatibleDC failed"));
    }
    let bitmap = unsafe { CreateCompatibleBitmap(screen, width, height) };
    if bitmap.is_invalid() {
        unsafe {
            let _ = DeleteDC(memory);
            ReleaseDC(None, screen);
        }
        return Err(anyhow!("CreateCompatibleBitmap failed"));
    }

    let previous = unsafe { SelectObject(memory, HGDIOBJ(bitmap.0)) };
    let copied = unsafe {
        BitBlt(
            memory,
            0,
            0,
            width,
            height,
            Some(screen),
            x,
            y,
            ROP_CODE(SRCCOPY.0 | CAPTUREBLT.0),
        )
    };
    if copied.is_err() {
        unsafe {
            let _ = SelectObject(memory, previous);
            let _ = DeleteObject(HGDIOBJ(bitmap.0));
            let _ = DeleteDC(memory);
            ReleaseDC(None, screen);
        }
        return Err(anyhow!("BitBlt failed"));
    }

    let mut info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width,
            biHeight: -height,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            biSizeImage: (width * height * 4) as u32,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bgra = vec![0_u8; width as usize * height as usize * 4];
    let rows = unsafe {
        GetDIBits(
            memory,
            bitmap,
            0,
            height as u32,
            Some(bgra.as_mut_ptr().cast::<c_void>()),
            &mut info,
            DIB_RGB_COLORS,
        )
    };
    unsafe {
        let _ = SelectObject(memory, previous);
        let _ = DeleteObject(HGDIOBJ(bitmap.0));
        let _ = DeleteDC(memory);
        ReleaseDC(None, screen);
    }
    if rows == 0 {
        return Err(anyhow!("GetDIBits failed"));
    }

    for pixel in bgra.as_chunks_mut::<4>().0 {
        pixel.swap(0, 2);
        pixel[3] = 255;
    }
    let fingerprint = fingerprint(&bgra, width as usize, height as usize);
    let mut webp = Vec::new();
    WebPEncoder::new_lossless(&mut webp)
        .encode(&bgra, width as u32, height as u32, ExtendedColorType::Rgba8)
        .context("lossless WebP encoding failed")?;
    Ok(CapturedFrame {
        width: width as u32,
        height: height as u32,
        webp,
        fingerprint,
    })
}

#[cfg(not(target_os = "windows"))]
pub fn capture_screen(_all_monitors: bool) -> Result<CapturedFrame> {
    Err(anyhow!("screen capture is only available on Windows"))
}

fn fingerprint(rgba: &[u8], width: usize, height: usize) -> Vec<u8> {
    const SIDE: usize = 128;
    let mut output = vec![0_u8; SIDE * SIDE];
    for fy in 0..SIDE {
        let y = fy * height / SIDE;
        for fx in 0..SIDE {
            let x = fx * width / SIDE;
            let offset = (y * width + x) * 4;
            if offset + 2 < rgba.len() {
                output[fy * SIDE + fx] = ((rgba[offset] as u16 * 54
                    + rgba[offset + 1] as u16 * 183
                    + rgba[offset + 2] as u16 * 19)
                    / 256) as u8;
            }
        }
    }
    output
}

pub fn changed_percent(previous: &[u8], current: &[u8]) -> f32 {
    if previous.len() != current.len() || current.is_empty() {
        return 100.0;
    }
    let changed = previous
        .iter()
        .zip(current)
        .filter(|(left, right)| left.abs_diff(**right) > 8)
        .count();
    changed as f32 * 100.0 / current.len() as f32
}

#[cfg(test)]
mod unit {
    use super::changed_percent;

    #[test]
    fn change_is_reported_in_percentage_points() {
        let previous = vec![0; 100];
        let mut current = previous.clone();
        current[0] = 9;
        current[1] = 255;
        current[2] = 8;

        assert_eq!(changed_percent(&previous, &current), 2.0);
    }

    #[test]
    fn missing_baseline_is_a_fully_changed_frame() {
        assert_eq!(changed_percent(&[], &[0; 100]), 100.0);
    }
}
