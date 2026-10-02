// SPDX-License-Identifier: GPL-2.0

//! Pixel Formats and Color Definitions.
//!
//! Provides color representations, pixel format conversions, and standard
//! ANSI 16-color palettes for framebuffer rendering.

/// A 24-bit RGB color.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Color {
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    // Standard ANSI colors
    pub const BLACK: Self = Self::new(0, 0, 0);
    pub const RED: Self = Self::new(170, 0, 0);
    pub const GREEN: Self = Self::new(0, 170, 0);
    pub const YELLOW: Self = Self::new(170, 85, 0);
    pub const BLUE: Self = Self::new(0, 0, 170);
    pub const MAGENTA: Self = Self::new(170, 0, 170);
    pub const CYAN: Self = Self::new(0, 170, 170);
    pub const WHITE: Self = Self::new(170, 170, 170);

    // Bright ANSI colors
    pub const BRIGHT_BLACK: Self = Self::new(85, 85, 85);
    pub const BRIGHT_RED: Self = Self::new(255, 85, 85);
    pub const BRIGHT_GREEN: Self = Self::new(85, 255, 85);
    pub const BRIGHT_YELLOW: Self = Self::new(255, 255, 85);
    pub const BRIGHT_BLUE: Self = Self::new(85, 85, 255);
    pub const BRIGHT_MAGENTA: Self = Self::new(255, 85, 255);
    pub const BRIGHT_CYAN: Self = Self::new(85, 255, 255);
    pub const BRIGHT_WHITE: Self = Self::new(255, 255, 255);

    /// Converts standard ANSI 3-bit / 4-bit color index (0..15) to a `Color`.
    pub const fn from_ansi(code: u8) -> Self {
        match code {
            0 => Self::BLACK,
            1 => Self::RED,
            2 => Self::GREEN,
            3 => Self::YELLOW,
            4 => Self::BLUE,
            5 => Self::MAGENTA,
            6 => Self::CYAN,
            7 => Self::WHITE,
            8 => Self::BRIGHT_BLACK,
            9 => Self::BRIGHT_RED,
            10 => Self::BRIGHT_GREEN,
            11 => Self::BRIGHT_YELLOW,
            12 => Self::BRIGHT_BLUE,
            13 => Self::BRIGHT_MAGENTA,
            14 => Self::BRIGHT_CYAN,
            15 => Self::BRIGHT_WHITE,
            _ => Self::WHITE,
        }
    }
}

/// Pixel format supported by the linear framebuffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PixelFormat {
    /// 32-bit BGRX (Blue at byte 0, Green at byte 1, Red at byte 2, byte 3 reserved).
    /// Standard format used by UEFI GOP on x86_64.
    BgrReserved,
    /// 24-bit RGB (Red at byte 0, Green at byte 1, Blue at byte 2).
    Rgb888,
    /// 16-bit RGB 5-6-5.
    Rgb565,
    /// 8-bit Grayscale.
    Grayscale8,
}

impl PixelFormat {
    /// Bytes required per pixel.
    pub const fn bytes_per_pixel(&self) -> usize {
        match self {
            Self::BgrReserved => 4,
            Self::Rgb888 => 3,
            Self::Rgb565 => 2,
            Self::Grayscale8 => 1,
        }
    }

    /// Writes a single color into a destination byte slice according to this format.
    #[inline]
    pub fn write_color(&self, dest: &mut [u8], color: Color) {
        match self {
            Self::BgrReserved => {
                if dest.len() >= 4 {
                    dest[0] = color.b;
                    dest[1] = color.g;
                    dest[2] = color.r;
                    dest[3] = 0;
                }
            }
            Self::Rgb888 => {
                if dest.len() >= 3 {
                    dest[0] = color.r;
                    dest[1] = color.g;
                    dest[2] = color.b;
                }
            }
            Self::Rgb565 => {
                if dest.len() >= 2 {
                    let r = (color.r >> 3) as u16;
                    let g = (color.g >> 2) as u16;
                    let b = (color.b >> 3) as u16;
                    let val = (r << 11) | (g << 5) | b;
                    let bytes = val.to_ne_bytes();
                    dest[0] = bytes[0];
                    dest[1] = bytes[1];
                }
            }
            Self::Grayscale8 => {
                if !dest.is_empty() {
                    // Standard luminance formula: 0.299R + 0.587G + 0.114B
                    let gray = ((color.r as u32 * 77 + color.g as u32 * 150 + color.b as u32 * 29) >> 8) as u8;
                    dest[0] = gray;
                }
            }
        }
    }
}
