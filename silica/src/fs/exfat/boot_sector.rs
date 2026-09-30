// SPDX-License-Identifier: GPL-2.0

//! ExFAT Volume Boot Record (Boot Sector) parsing and validation.

use crate::errno::{Errno, Result};

/// ExFAT filesystem identifier ("EXFAT   ").
pub const EXFAT_FS_NAME: &[u8; 8] = b"EXFAT   ";

/// Standard boot signature (0xAA55).
pub const EXFAT_BOOT_SIGNATURE: u16 = 0xAA55;

/// In-memory representation of the ExFAT Volume Boot Record.
#[derive(Clone, Copy, Debug)]
pub struct ExFatBootSector {
    pub partition_offset: u64,
    pub volume_length: u64,
    pub fat_offset: u32,
    pub fat_length: u32,
    pub cluster_heap_offset: u32,
    pub cluster_count: u32,
    pub root_dir_first_cluster: u32,
    pub volume_serial_number: u32,
    pub fs_revision: u16,
    pub volume_flags: u16,
    pub bytes_per_sector_shift: u8,
    pub sectors_per_cluster_shift: u8,
    pub number_of_fats: u8,
}

impl ExFatBootSector {
    /// Parses an ExFAT boot sector from raw bytes (at least 512 bytes).
    pub fn parse(buf: &[u8]) -> Result<Self> {
        if buf.len() < 512 {
            return Err(Errno::EINVAL);
        }

        // Verify FS Name: "EXFAT   "
        if &buf[3..11] != EXFAT_FS_NAME {
            return Err(Errno::EINVAL);
        }

        // Verify Boot Signature: 0xAA55 at offset 510
        let sig = u16::from_le_bytes(buf[510..512].try_into().unwrap_or([0, 0]));
        if sig != EXFAT_BOOT_SIGNATURE {
            return Err(Errno::EINVAL);
        }

        let partition_offset = u64::from_le_bytes(buf[64..72].try_into().unwrap_or([0; 8]));
        let volume_length = u64::from_le_bytes(buf[72..80].try_into().unwrap_or([0; 8]));
        let fat_offset = u32::from_le_bytes(buf[80..84].try_into().unwrap_or([0; 4]));
        let fat_length = u32::from_le_bytes(buf[84..88].try_into().unwrap_or([0; 4]));
        let cluster_heap_offset = u32::from_le_bytes(buf[88..92].try_into().unwrap_or([0; 4]));
        let cluster_count = u32::from_le_bytes(buf[92..96].try_into().unwrap_or([0; 4]));
        let root_dir_first_cluster = u32::from_le_bytes(buf[96..100].try_into().unwrap_or([0; 4]));
        let volume_serial_number = u32::from_le_bytes(buf[100..104].try_into().unwrap_or([0; 4]));
        let fs_revision = u16::from_le_bytes(buf[104..106].try_into().unwrap_or([0; 2]));
        let volume_flags = u16::from_le_bytes(buf[106..108].try_into().unwrap_or([0; 2]));
        let bytes_per_sector_shift = buf[108];
        let sectors_per_cluster_shift = buf[109];
        let number_of_fats = buf[110];

        // Sanity checks on shifts (sectors 512B to 4096B, cluster shift 0 to 25)
        if bytes_per_sector_shift < 9 || bytes_per_sector_shift > 12 {
            return Err(Errno::EINVAL);
        }

        Ok(Self {
            partition_offset,
            volume_length,
            fat_offset,
            fat_length,
            cluster_heap_offset,
            cluster_count,
            root_dir_first_cluster,
            volume_serial_number,
            fs_revision,
            volume_flags,
            bytes_per_sector_shift,
            sectors_per_cluster_shift,
            number_of_fats,
        })
    }

    /// Sector size in bytes.
    pub fn sector_size(&self) -> usize {
        1usize << self.bytes_per_sector_shift
    }

    /// Sectors per cluster.
    pub fn sectors_per_cluster(&self) -> usize {
        1usize << self.sectors_per_cluster_shift
    }

    /// Cluster size in bytes.
    pub fn cluster_size(&self) -> usize {
        self.sector_size() << self.sectors_per_cluster_shift
    }
}
