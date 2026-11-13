//! Light / dark colours in the Windows 11 (Fluent) style, following the
//! user's Windows settings and accent colour.
//!
//! - popup (like the system brightness flyout): "SystemUsesLightTheme"
//! - app windows (curve editor, shortcut window): "AppsUseLightTheme"
//! - accent: DWM "AccentColor"; lighter in dark mode, darker in light mode,
//!   as Windows does.

use tao::window::{Theme, Window};

pub type Rgb = [u8; 3];

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Palette {
    pub dark: bool,
    /// App window background.
    pub window: Rgb,
    /// Flyout / popup background.
    pub flyout: Rgb,
    pub border: Rgb,
    pub text: Rgb,
    pub subtext: Rgb,
    pub faint: Rgb,
    pub grid: Rgb,
    pub track: Rgb,
    pub accent: Rgb,
    pub on_accent: Rgb,
    pub button: Rgb,
    pub button_border: Rgb,
    pub error: Rgb,
}

const WHITE: Rgb = [0xff, 0xff, 0xff];
const BLACK: Rgb = [0, 0, 0];
/// Windows 11 default accent.
const DEFAULT_ACCENT: Rgb = [0x00, 0x78, 0xd4];

fn mix(a: Rgb, b: Rgb, t: f32) -> Rgb {
    let m = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    [m(a[0], b[0]), m(a[1], b[1]), m(a[2], b[2])]
}

pub fn palette(dark: bool, accent: Rgb) -> Palette {
    if dark {
        Palette {
            dark,
            window: [0x20, 0x20, 0x20],
            flyout: [0x2c, 0x2c, 0x2c],
            border: [0x3d, 0x3d, 0x3d],
            text: WHITE,
            subtext: [0xc5, 0xc5, 0xc5],
            faint: [0x7a, 0x7a, 0x7a],
            grid: [0x38, 0x38, 0x38],
            track: [0x8a, 0x8a, 0x8a],
            accent: mix(accent, WHITE, 0.45),
            on_accent: BLACK,
            button: [0x2d, 0x2d, 0x2d],
            button_border: [0x45, 0x45, 0x45],
            error: [0xff, 0x99, 0xa4],
        }
    } else {
        Palette {
            dark,
            window: [0xf3, 0xf3, 0xf3],
            flyout: [0xf9, 0xf9, 0xf9],
            border: [0xe0, 0xe0, 0xe0],
            text: [0x1b, 0x1b, 0x1b],
            subtext: [0x5f, 0x5f, 0x5f],
            faint: [0x9e, 0x9e, 0x9e],
            grid: [0xe3, 0xe3, 0xe3],
            track: [0x86, 0x86, 0x86],
            accent: mix(accent, BLACK, 0.12),
            on_accent: WHITE,
            button: [0xfd, 0xfd, 0xfd],
            button_border: [0xd0, 0xd0, 0xd0],
            error: [0xc4, 0x2b, 0x1c],
        }
    }
}

/// Palette for app windows.
pub fn for_app(w: &Window) -> Palette {
    palette(app_dark(w), accent())
}

/// Palette for the popup (follows the taskbar / system flyouts).
pub fn for_shell(w: &Window) -> Palette {
    palette(shell_dark(w), accent())
}

const PERSONALIZE: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize";

fn app_dark(w: &Window) -> bool {
    match reg_dword(PERSONALIZE, "AppsUseLightTheme") {
        Some(v) => v == 0,
        None => w.theme() == Theme::Dark,
    }
}

fn shell_dark(w: &Window) -> bool {
    match reg_dword(PERSONALIZE, "SystemUsesLightTheme") {
        Some(v) => v == 0,
        None => w.theme() == Theme::Dark,
    }
}

fn accent() -> Rgb {
    // AccentColor is 0xAABBGGRR.
    reg_dword("Software\\Microsoft\\Windows\\DWM", "AccentColor")
        .map(|v| {
            [
                (v & 0xff) as u8,
                ((v >> 8) & 0xff) as u8,
                ((v >> 16) & 0xff) as u8,
            ]
        })
        .unwrap_or(DEFAULT_ACCENT)
}

#[cfg(windows)]
fn reg_dword(subkey: &str, value: &str) -> Option<u32> {
    use windows::core::PCWSTR;
    use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
    let wide = |s: &str| {
        s.encode_utf16()
            .chain(std::iter::once(0))
            .collect::<Vec<u16>>()
    };
    let (k, v) = (wide(subkey), wide(value));
    let mut data = 0u32;
    let mut size = std::mem::size_of::<u32>() as u32;
    let r = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            PCWSTR(k.as_ptr()),
            PCWSTR(v.as_ptr()),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut data as *mut u32 as *mut _),
            Some(&mut size),
        )
    };
    r.is_ok().then_some(data)
}

#[cfg(not(windows))]
fn reg_dword(_: &str, _: &str) -> Option<u32> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn luminance(c: Rgb) -> f32 {
        0.2126 * c[0] as f32 + 0.7152 * c[1] as f32 + 0.0722 * c[2] as f32
    }

    #[test]
    fn text_contrasts_with_background_in_both_modes() {
        for dark in [false, true] {
            let p = palette(dark, DEFAULT_ACCENT);
            for bg in [p.window, p.flyout] {
                assert!(
                    (luminance(p.text) - luminance(bg)).abs() > 150.0,
                    "dark={dark}"
                );
                assert!(
                    (luminance(p.subtext) - luminance(bg)).abs() > 70.0,
                    "dark={dark}"
                );
            }
            assert!(
                (luminance(p.on_accent) - luminance(p.accent)).abs() > 60.0,
                "dark={dark}"
            );
        }
    }

    #[test]
    fn accent_is_lightened_in_dark_and_darkened_in_light() {
        let base = [0x80, 0x40, 0x20];
        assert!(luminance(palette(true, base).accent) > luminance(base));
        assert!(luminance(palette(false, base).accent) < luminance(base));
    }
}
