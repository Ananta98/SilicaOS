// SPDX-License-Identifier: GPL-2.0

//! Linux-Compatible Framebuffer Character Device (`/dev/fb0`).
//!
//! Provides a standard Linux `/dev/fb0` character device interface,
//! including support for `FBIOGET_VSCREENINFO`, `FBIOPUT_VSCREENINFO`,
//! `FBIOGET_FSCREENINFO`, and direct framebuffer reading and writing.

use alloc::{string::String, sync::Arc};
use core::mem::size_of;
use spin::Mutex;

use ostd::mm::{io::FallibleVmRead, FallibleVmWrite};

use super::{fb::Framebuffer, pixel::PixelFormat};
use crate::{
    drivers::CharacterDevice,
    errno::{Errno, Result},
    proc::thread::Thread,
};

// ----------------------------------------------------------------------------
// Linux fbdev ioctl Command Constants
// ----------------------------------------------------------------------------

pub const FBIOGET_VSCREENINFO: u32 = 0x4600;
pub const FBIOPUT_VSCREENINFO: u32 = 0x4601;
pub const FBIOGET_FSCREENINFO: u32 = 0x4602;
pub const FBIOPAN_DISPLAY: u32 = 0x4606;
pub const FBIOBLANK: u32 = 0x4611;

pub const FB_TYPE_PACKED_PIXELS: u32 = 0;
pub const FB_VISUAL_TRUECOLOR: u32 = 2;
pub const FB_ACTIVATE_NOW: u32 = 0;

// ----------------------------------------------------------------------------
// Linux fbdev Data Structures
// ----------------------------------------------------------------------------

/// Bitfield specification for RGB components in `FbVarScreeninfo`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FbBitfield {
    pub offset: u32,
    pub length: u32,
    pub msb_right: u32,
}

/// Linux variable screen info structure (`struct fb_var_screeninfo`).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FbVarScreeninfo {
    pub xres: u32,
    pub yres: u32,
    pub xres_virtual: u32,
    pub yres_virtual: u32,
    pub xoffset: u32,
    pub yoffset: u32,
    pub bits_per_pixel: u32,
    pub grayscale: u32,
    pub red: FbBitfield,
    pub green: FbBitfield,
    pub blue: FbBitfield,
    pub transp: FbBitfield,
    pub nonstd: u32,
    pub activate: u32,
    pub height: u32,
    pub width: u32,
    pub accel_flags: u32,
    pub pixclock: u32,
    pub left_margin: u32,
    pub right_margin: u32,
    pub upper_margin: u32,
    pub lower_margin: u32,
    pub hsync_len: u32,
    pub vsync_len: u32,
    pub sync: u32,
    pub vmode: u32,
    pub rotate: u32,
    pub colorspace: u32,
    pub reserved: [u32; 4],
}

impl FbVarScreeninfo {
    pub const SIZE: usize = size_of::<Self>();

    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut buf = [0u8; Self::SIZE];
        let mut offset = 0;

        let mut put_u32 = |val: u32| {
            buf[offset..offset + 4].copy_from_slice(&val.to_ne_bytes());
            offset += 4;
        };

        put_u32(self.xres);
        put_u32(self.yres);
        put_u32(self.xres_virtual);
        put_u32(self.yres_virtual);
        put_u32(self.xoffset);
        put_u32(self.yoffset);
        put_u32(self.bits_per_pixel);
        put_u32(self.grayscale);

        // red
        put_u32(self.red.offset);
        put_u32(self.red.length);
        put_u32(self.red.msb_right);
        // green
        put_u32(self.green.offset);
        put_u32(self.green.length);
        put_u32(self.green.msb_right);
        // blue
        put_u32(self.blue.offset);
        put_u32(self.blue.length);
        put_u32(self.blue.msb_right);
        // transp
        put_u32(self.transp.offset);
        put_u32(self.transp.length);
        put_u32(self.transp.msb_right);

        put_u32(self.nonstd);
        put_u32(self.activate);
        put_u32(self.height);
        put_u32(self.width);
        put_u32(self.accel_flags);
        put_u32(self.pixclock);
        put_u32(self.left_margin);
        put_u32(self.right_margin);
        put_u32(self.upper_margin);
        put_u32(self.lower_margin);
        put_u32(self.hsync_len);
        put_u32(self.vsync_len);
        put_u32(self.sync);
        put_u32(self.vmode);
        put_u32(self.rotate);
        put_u32(self.colorspace);

        for r in self.reserved {
            put_u32(r);
        }

        buf
    }

    pub fn from_bytes(bytes: &[u8; Self::SIZE]) -> Self {
        let mut offset = 0;
        let mut get_u32 = || {
            let val = u32::from_ne_bytes(bytes[offset..offset + 4].try_into().unwrap());
            offset += 4;
            val
        };

        let xres = get_u32();
        let yres = get_u32();
        let xres_virtual = get_u32();
        let yres_virtual = get_u32();
        let xoffset = get_u32();
        let yoffset = get_u32();
        let bits_per_pixel = get_u32();
        let grayscale = get_u32();

        let red = FbBitfield {
            offset: get_u32(),
            length: get_u32(),
            msb_right: get_u32(),
        };
        let green = FbBitfield {
            offset: get_u32(),
            length: get_u32(),
            msb_right: get_u32(),
        };
        let blue = FbBitfield {
            offset: get_u32(),
            length: get_u32(),
            msb_right: get_u32(),
        };
        let transp = FbBitfield {
            offset: get_u32(),
            length: get_u32(),
            msb_right: get_u32(),
        };

        let nonstd = get_u32();
        let activate = get_u32();
        let height = get_u32();
        let width = get_u32();
        let accel_flags = get_u32();
        let pixclock = get_u32();
        let left_margin = get_u32();
        let right_margin = get_u32();
        let upper_margin = get_u32();
        let lower_margin = get_u32();
        let hsync_len = get_u32();
        let vsync_len = get_u32();
        let sync = get_u32();
        let vmode = get_u32();
        let rotate = get_u32();
        let colorspace = get_u32();

        let mut reserved = [0u32; 4];
        for r in &mut reserved {
            *r = get_u32();
        }

        Self {
            xres,
            yres,
            xres_virtual,
            yres_virtual,
            xoffset,
            yoffset,
            bits_per_pixel,
            grayscale,
            red,
            green,
            blue,
            transp,
            nonstd,
            activate,
            height,
            width,
            accel_flags,
            pixclock,
            left_margin,
            right_margin,
            upper_margin,
            lower_margin,
            hsync_len,
            vsync_len,
            sync,
            vmode,
            rotate,
            colorspace,
            reserved,
        }
    }
}

/// Linux fixed screen info structure (`struct fb_fix_screeninfo`).
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct FbFixScreeninfo {
    pub id: [u8; 16],
    pub smem_start: usize,
    pub smem_len: u32,
    pub type_: u32,
    pub type_aux: u32,
    pub visual: u32,
    pub xpanstep: u16,
    pub ypanstep: u16,
    pub ywrapstep: u16,
    pub line_length: u32,
    pub mmio_start: usize,
    pub mmio_len: u32,
    pub accel: u32,
    pub capabilities: u16,
    pub reserved: [u16; 2],
}

impl FbFixScreeninfo {
    pub const SIZE: usize = size_of::<Self>();

    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut buf = [0u8; Self::SIZE];
        let mut offset = 0;

        buf[offset..offset + 16].copy_from_slice(&self.id);
        offset += 16;

        let ptr_bytes = self.smem_start.to_ne_bytes();
        buf[offset..offset + size_of::<usize>()].copy_from_slice(&ptr_bytes);
        offset += size_of::<usize>();

        buf[offset..offset + 4].copy_from_slice(&self.smem_len.to_ne_bytes());
        offset += 4;
        buf[offset..offset + 4].copy_from_slice(&self.type_.to_ne_bytes());
        offset += 4;
        buf[offset..offset + 4].copy_from_slice(&self.type_aux.to_ne_bytes());
        offset += 4;
        buf[offset..offset + 4].copy_from_slice(&self.visual.to_ne_bytes());
        offset += 4;

        buf[offset..offset + 2].copy_from_slice(&self.xpanstep.to_ne_bytes());
        offset += 2;
        buf[offset..offset + 2].copy_from_slice(&self.ypanstep.to_ne_bytes());
        offset += 2;
        buf[offset..offset + 2].copy_from_slice(&self.ywrapstep.to_ne_bytes());
        offset += 2;

        buf[offset..offset + 4].copy_from_slice(&self.line_length.to_ne_bytes());
        offset += 4;

        let mmio_ptr = self.mmio_start.to_ne_bytes();
        buf[offset..offset + size_of::<usize>()].copy_from_slice(&mmio_ptr);
        offset += size_of::<usize>();

        buf[offset..offset + 4].copy_from_slice(&self.mmio_len.to_ne_bytes());
        offset += 4;
        buf[offset..offset + 4].copy_from_slice(&self.accel.to_ne_bytes());
        offset += 4;

        buf[offset..offset + 2].copy_from_slice(&self.capabilities.to_ne_bytes());
        offset += 2;
        buf[offset..offset + 2].copy_from_slice(&self.reserved[0].to_ne_bytes());
        offset += 2;
        buf[offset..offset + 2].copy_from_slice(&self.reserved[1].to_ne_bytes());

        buf
    }
}

// ----------------------------------------------------------------------------
// Character Device Implementation
// ----------------------------------------------------------------------------

/// The `/dev/fb0` character device representing the linear display surface.
pub struct FbDev {
    fb: Arc<Framebuffer>,
    pos: Mutex<usize>,
}

impl FbDev {
    pub fn new(fb: Arc<Framebuffer>) -> Self {
        Self {
            fb,
            pos: Mutex::new(0),
        }
    }

    fn read_user(addr: usize, buf: &mut [u8]) -> Result<()> {
        let proc = Thread::current_proc().ok_or(Errno::ESRCH)?;
        let vmar = proc.vmspace();
        let mut reader = vmar
            .vm_space()
            .reader(addr, buf.len())
            .map_err(|_| Errno::EFAULT)?;
        let mut writer = ostd::mm::VmWriter::from(&mut buf[..]);
        reader.read_fallible(&mut writer).map_err(|_| Errno::EFAULT)?;
        Ok(())
    }

    fn write_user(addr: usize, buf: &[u8]) -> Result<()> {
        let proc = Thread::current_proc().ok_or(Errno::ESRCH)?;
        let vmar = proc.vmspace();
        let mut writer = vmar
            .vm_space()
            .writer(addr, buf.len())
            .map_err(|_| Errno::EFAULT)?;
        let mut reader = ostd::mm::VmReader::from(&buf[..]);
        writer.write_fallible(&mut reader).map_err(|_| Errno::EFAULT)?;
        Ok(())
    }

    /// Construct `FbVarScreeninfo` from current framebuffer parameters.
    pub fn var_screeninfo(&self) -> FbVarScreeninfo {
        let (red, green, blue) = match self.fb.pixel_format {
            PixelFormat::BgrReserved => (
                FbBitfield { offset: 16, length: 8, msb_right: 0 },
                FbBitfield { offset: 8, length: 8, msb_right: 0 },
                FbBitfield { offset: 0, length: 8, msb_right: 0 },
            ),
            PixelFormat::Rgb888 => (
                FbBitfield { offset: 0, length: 8, msb_right: 0 },
                FbBitfield { offset: 8, length: 8, msb_right: 0 },
                FbBitfield { offset: 16, length: 8, msb_right: 0 },
            ),
            PixelFormat::Rgb565 => (
                FbBitfield { offset: 11, length: 5, msb_right: 0 },
                FbBitfield { offset: 5, length: 6, msb_right: 0 },
                FbBitfield { offset: 0, length: 5, msb_right: 0 },
            ),
            PixelFormat::Grayscale8 => (
                FbBitfield { offset: 0, length: 8, msb_right: 0 },
                FbBitfield { offset: 0, length: 8, msb_right: 0 },
                FbBitfield { offset: 0, length: 8, msb_right: 0 },
            ),
        };

        FbVarScreeninfo {
            xres: self.fb.width as u32,
            yres: self.fb.height as u32,
            xres_virtual: self.fb.width as u32,
            yres_virtual: self.fb.height as u32,
            xoffset: 0,
            yoffset: 0,
            bits_per_pixel: (self.fb.pixel_format.bytes_per_pixel() * 8) as u32,
            grayscale: if self.fb.pixel_format == PixelFormat::Grayscale8 { 1 } else { 0 },
            red,
            green,
            blue,
            transp: FbBitfield::default(),
            nonstd: 0,
            activate: FB_ACTIVATE_NOW,
            height: 0,
            width: 0,
            accel_flags: 0,
            pixclock: 0,
            left_margin: 0,
            right_margin: 0,
            upper_margin: 0,
            lower_margin: 0,
            hsync_len: 0,
            vsync_len: 0,
            sync: 0,
            vmode: 0,
            rotate: 0,
            colorspace: 0,
            reserved: [0; 4],
        }
    }

    /// Construct `FbFixScreeninfo` from current framebuffer parameters.
    pub fn fix_screeninfo(&self) -> FbFixScreeninfo {
        let mut id = [0u8; 16];
        let tag = b"silica-fb";
        id[..tag.len()].copy_from_slice(tag);

        FbFixScreeninfo {
            id,
            smem_start: self.fb.base_address,
            smem_len: self.fb.total_size as u32,
            type_: FB_TYPE_PACKED_PIXELS,
            type_aux: 0,
            visual: FB_VISUAL_TRUECOLOR,
            xpanstep: 0,
            ypanstep: 0,
            ywrapstep: 0,
            line_length: self.fb.line_size as u32,
            mmio_start: self.fb.base_address,
            mmio_len: self.fb.total_size as u32,
            accel: 0,
            capabilities: 0,
            reserved: [0; 2],
        }
    }
}

impl CharacterDevice for FbDev {
    fn name(&self) -> String {
        String::from("fb0")
    }

    fn read(&self, buf: &mut [u8]) -> Result<usize> {
        let mut pos = self.pos.lock();
        if *pos >= self.fb.total_size {
            return Ok(0);
        }

        let to_read = buf.len().min(self.fb.total_size - *pos);
        self.fb.read_bytes(*pos, &mut buf[..to_read])?;
        *pos += to_read;
        Ok(to_read)
    }

    fn write(&self, buf: &[u8]) -> Result<usize> {
        let mut pos = self.pos.lock();
        if *pos >= self.fb.total_size {
            return Ok(0);
        }

        let to_write = buf.len().min(self.fb.total_size - *pos);
        self.fb.write_bytes(*pos, &buf[..to_write])?;
        *pos += to_write;
        Ok(to_write)
    }

    fn ioctl(&self, cmd: u32, arg: usize) -> Result<usize> {
        match cmd {
            FBIOGET_VSCREENINFO => {
                let info = self.var_screeninfo();
                let bytes = info.to_bytes();
                Self::write_user(arg, &bytes)?;
                Ok(0)
            }
            FBIOPUT_VSCREENINFO => {
                let mut bytes = [0u8; FbVarScreeninfo::SIZE];
                Self::read_user(arg, &mut bytes)?;
                // Currently modesetting is fixed by UEFI GOP; acknowledge request
                Ok(0)
            }
            FBIOGET_FSCREENINFO => {
                let fix = self.fix_screeninfo();
                let bytes = fix.to_bytes();
                Self::write_user(arg, &bytes)?;
                Ok(0)
            }
            FBIOBLANK => {
                // If arg != 0, blank screen (fill black); otherwise unblank
                if arg != 0 {
                    self.fb.clear(super::pixel::Color::BLACK);
                }
                Ok(0)
            }
            _ => crate::return_errno!(ENOTTY, "inappropriate ioctl for fbdev"),
        }
    }
}
