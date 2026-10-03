// SPDX-License-Identifier: GPL-2.0

//! Framebuffer Hardware Abstraction.
//!
//! Provides safe memory-mapped I/O access to the linear display surface,
//! with accelerated primitive rendering for pixels, rectangles, and glyphs.

use alloc::vec;
use ostd::{
    boot::boot_info,
    io::IoMem,
    mm::{CachePolicy, VmIo, VmIoOnce},
};

use super::{
    font::FONT_HEIGHT,
    pixel::{Color, PixelFormat},
};
use crate::api::errno::{Errno, Result};

/// A physical or virtual linear framebuffer display device.
pub struct Framebuffer {
    io_mem: IoMem,
    pub base_address: usize,
    pub total_size: usize,
    pub width: usize,
    pub height: usize,
    pub line_size: usize,
    pub pixel_format: PixelFormat,
}

impl Framebuffer {
    /// Probes and maps the linear framebuffer provided by the bootloader.
    pub fn probe() -> Result<Self> {
        let Some(arg) = boot_info().framebuffer_arg else {
            ostd::warn!("Framebuffer: No framebuffer argument found in boot info");
            return Err(Errno::ENODEV);
        };

        if arg.address == 0 || arg.width == 0 || arg.height == 0 {
            ostd::error!("Framebuffer: Invalid framebuffer parameters: {:?}", arg);
            return Err(Errno::EINVAL);
        }

        let pixel_format = match arg.bpp {
            32 => PixelFormat::BgrReserved,
            24 => PixelFormat::Rgb888,
            16 => PixelFormat::Rgb565,
            8 => PixelFormat::Grayscale8,
            _ => {
                ostd::warn!("Framebuffer: Unsupported BPP {}, defaulting to 32 BPP", arg.bpp);
                PixelFormat::BgrReserved
            }
        };

        let bpp_bytes = pixel_format.bytes_per_pixel();
        let line_size = arg.width * bpp_bytes;
        let total_size = line_size * arg.height;
        let range = arg.address..(arg.address + total_size);

        ostd::info!(
            "Framebuffer: Acquiring MMIO at {:#x}..{:#x} ({}x{} @ {} bpp)",
            range.start,
            range.end,
            arg.width,
            arg.height,
            arg.bpp
        );

        // Acquire with WriteCombining for high performance graphics write speeds
        let io_mem = IoMem::acquire_with_cache_policy(range.clone(), CachePolicy::WriteCombining)
            .or_else(|_| IoMem::acquire(range))
            .map_err(|_| {
                ostd::error!("Framebuffer: Failed to acquire MMIO memory region");
                Errno::EIO
            })?;

        Ok(Self {
            io_mem,
            base_address: arg.address,
            total_size,
            width: arg.width,
            height: arg.height,
            line_size,
            pixel_format,
        })
    }

    /// Read raw bytes from the linear framebuffer.
    pub fn read_bytes(&self, offset: usize, buf: &mut [u8]) -> Result<()> {
        if offset + buf.len() > self.total_size {
            return Err(Errno::EINVAL);
        }
        self.io_mem.read_bytes(offset, buf).map_err(|_| Errno::EIO)
    }

    /// Write raw bytes directly to the linear framebuffer.
    pub fn write_bytes(&self, offset: usize, buf: &[u8]) -> Result<()> {
        if offset + buf.len() > self.total_size {
            return Err(Errno::EINVAL);
        }
        self.io_mem.write_bytes(offset, buf).map_err(|_| Errno::EIO)
    }

    /// Draws a single pixel at (x, y) with color.
    #[inline]
    pub fn write_pixel(&self, x: usize, y: usize, color: Color) {
        if x >= self.width || y >= self.height {
            return;
        }

        let bpp = self.pixel_format.bytes_per_pixel();
        let offset = y * self.line_size + x * bpp;

        if bpp == 4 {
            let packed: u32 = ((color.r as u32) << 16) | ((color.g as u32) << 8) | (color.b as u32);
            let _ = self.io_mem.write_once(offset, &packed);
        } else {
            let mut buf = [0u8; 4];
            self.pixel_format.write_color(&mut buf[..bpp], color);
            let _ = self.io_mem.write_bytes(offset, &buf[..bpp]);
        }
    }

    /// Fills a rectangular region with a solid color.
    pub fn fill_rect(&self, x: usize, y: usize, w: usize, h: usize, color: Color) {
        if x >= self.width || y >= self.height {
            return;
        }

        let w = w.min(self.width - x);
        let h = h.min(self.height - y);
        let bpp = self.pixel_format.bytes_per_pixel();

        if bpp == 4 {
            let packed: u32 = ((color.r as u32) << 16) | ((color.g as u32) << 8) | (color.b as u32);
            for row in 0..h {
                let start_offset = (y + row) * self.line_size + x * 4;
                for col in 0..w {
                    let _ = self.io_mem.write_once(start_offset + col * 4, &packed);
                }
            }
        } else {
            let mut pixel_bytes = [0u8; 4];
            self.pixel_format.write_color(&mut pixel_bytes[..bpp], color);

            let mut row_buf = vec![0u8; w * bpp];
            for i in 0..w {
                row_buf[i * bpp..(i + 1) * bpp].copy_from_slice(&pixel_bytes[..bpp]);
            }

            for row in 0..h {
                let start_offset = (y + row) * self.line_size + x * bpp;
                let _ = self.io_mem.write_bytes(start_offset, &row_buf);
            }
        }
    }

    /// Clears the entire framebuffer surface to the specified background color.
    pub fn clear(&self, color: Color) {
        self.fill_rect(0, 0, self.width, self.height, color);
    }

    /// Blits an 8x16 glyph at pixel position (x, y).
    pub fn draw_glyph(&self, x: usize, y: usize, glyph: &[u8; FONT_HEIGHT], fg: Color, bg: Color) {
        if x + 8 > self.width || y + FONT_HEIGHT > self.height {
            return;
        }

        let bpp = self.pixel_format.bytes_per_pixel();
        let packed_fg: u32 = ((fg.r as u32) << 16) | ((fg.g as u32) << 8) | (fg.b as u32);
        let packed_bg: u32 = ((bg.r as u32) << 16) | ((bg.g as u32) << 8) | (bg.b as u32);

        if bpp == 4 {
            for row in 0..FONT_HEIGHT {
                let row_bits = glyph[row];
                let offset = (y + row) * self.line_size + x * 4;
                for bit in 0..8 {
                    let pixel_val = if (row_bits & (0x80 >> bit)) != 0 {
                        packed_fg
                    } else {
                        packed_bg
                    };
                    let _ = self.io_mem.write_once(offset + bit * 4, &pixel_val);
                }
            }
        } else {
            for row in 0..FONT_HEIGHT {
                let row_bits = glyph[row];
                for bit in 0..8 {
                    let color = if (row_bits & (0x80 >> bit)) != 0 { fg } else { bg };
                    self.write_pixel(x + bit, y + row, color);
                }
            }
        }
    }

    /// Scrolls the framebuffer upward by `lines_px` pixels and clears the bottom strip.
    pub fn scroll_up(&self, lines_px: usize, bg: Color) {
        if lines_px == 0 || lines_px >= self.height {
            self.clear(bg);
            return;
        }

        let copy_height = self.height - lines_px;
        let mut line_buf = vec![0u8; self.line_size];

        // Copy scanlines upward from top to bottom
        for row in 0..copy_height {
            let src_offset = (row + lines_px) * self.line_size;
            let dst_offset = row * self.line_size;

            if self.io_mem.read_bytes(src_offset, &mut line_buf).is_ok() {
                let _ = self.io_mem.write_bytes(dst_offset, &line_buf);
            }
        }

        // Fill remaining bottom region with background color
        self.fill_rect(0, copy_height, self.width, lines_px, bg);
    }
}
