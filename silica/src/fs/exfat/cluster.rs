// SPDX-License-Identifier: GPL-2.0

//! ExFAT cluster allocation, FAT lookup, and data reading.

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use crate::{
    drivers::block::BlockDevice,
    errno::{Errno, Result},
    fs::exfat::boot_sector::ExFatBootSector,
};

/// End of cluster chain marker in ExFAT.
pub const EXFAT_CLUSTER_EOF: u32 = 0xFFFFFFF8;

/// Reads cluster data and follows FAT cluster chains.
pub struct ClusterManager {
    pub dev: Arc<dyn BlockDevice>,
    pub bs: ExFatBootSector,
}

impl ClusterManager {
    pub fn new(dev: Arc<dyn BlockDevice>, bs: ExFatBootSector) -> Self {
        Self { dev, bs }
    }

    /// Converts a cluster number (starting at 2) to logical block/sector on the device.
    pub fn cluster_to_sector(&self, cluster: u32) -> Result<u64> {
        if cluster < 2 || cluster >= self.bs.cluster_count + 2 {
            return Err(Errno::EINVAL);
        }
        let heap_offset = self.bs.cluster_heap_offset as u64;
        let cluster_offset = (cluster - 2) as u64 * (self.bs.sectors_per_cluster() as u64);
        Ok(heap_offset + cluster_offset)
    }

    /// Looks up the next cluster in the FAT table.
    pub fn next_cluster(&self, current: u32, no_fat_chain: bool) -> Result<Option<u32>> {
        if no_fat_chain {
            let next = current + 1;
            if next >= self.bs.cluster_count + 2 {
                return Ok(None);
            }
            return Ok(Some(next));
        }

        let dev_bs = self.dev.block_size();
        let fat_start_sector = self.bs.fat_offset as u64;
        let byte_offset = (current as u64) * 4;
        let sector_offset = byte_offset / (dev_bs as u64);
        let offset_in_sector = (byte_offset % (dev_bs as u64)) as usize;

        let mut sector_buf = vec![0u8; dev_bs];
        self.dev.read_blocks(fat_start_sector + sector_offset, &mut sector_buf)?;

        let val = u32::from_le_bytes(
            sector_buf[offset_in_sector..offset_in_sector + 4]
                .try_into()
                .unwrap_or([0; 4]),
        );

        if val >= EXFAT_CLUSTER_EOF || val < 2 {
            Ok(None)
        } else {
            Ok(Some(val))
        }
    }

    /// Reads a whole cluster into `buf`.
    pub fn read_cluster(&self, cluster: u32, buf: &mut [u8]) -> Result<()> {
        let cluster_size = self.bs.cluster_size();
        if buf.len() < cluster_size {
            return Err(Errno::EINVAL);
        }

        let start_sector = self.cluster_to_sector(cluster)?;
        let dev_bs = self.dev.block_size();
        let sectors_to_read = cluster_size / dev_bs;

        self.dev.read_blocks(start_sector, &mut buf[..sectors_to_read * dev_bs])?;
        Ok(())
    }

    /// Reads arbitrary bytes from a cluster chain.
    pub fn read_chain(
        &self,
        first_cluster: u32,
        no_fat_chain: bool,
        offset: u64,
        total_len: u64,
        buf: &mut [u8],
    ) -> Result<usize> {
        if offset >= total_len || first_cluster < 2 {
            return Ok(0);
        }

        let cluster_size = self.bs.cluster_size() as u64;
        let to_read = (buf.len() as u64).min(total_len - offset) as usize;

        let mut current_cluster = first_cluster;
        let mut cluster_skip = offset / cluster_size;

        // Skip leading clusters
        while cluster_skip > 0 {
            if let Some(next) = self.next_cluster(current_cluster, no_fat_chain)? {
                current_cluster = next;
                cluster_skip -= 1;
            } else {
                return Ok(0);
            }
        }

        let mut read_bytes = 0;
        let mut cluster_buf = vec![0u8; self.bs.cluster_size()];
        let mut offset_in_cluster = (offset % cluster_size) as usize;

        while read_bytes < to_read {
            self.read_cluster(current_cluster, &mut cluster_buf)?;

            let available = cluster_size as usize - offset_in_cluster;
            let chunk = available.min(to_read - read_bytes);

            buf[read_bytes..read_bytes + chunk]
                .copy_from_slice(&cluster_buf[offset_in_cluster..offset_in_cluster + chunk]);

            read_bytes += chunk;
            offset_in_cluster = 0;

            if read_bytes < to_read {
                if let Some(next) = self.next_cluster(current_cluster, no_fat_chain)? {
                    current_cluster = next;
                } else {
                    break;
                }
            }
        }

        Ok(read_bytes)
    }

    /// Reads the entire contents of a cluster chain into memory.
    pub fn read_all(
        &self,
        first_cluster: u32,
        no_fat_chain: bool,
        data_len: u64,
    ) -> Result<Vec<u8>> {
        let mut data = vec![0u8; data_len as usize];
        self.read_chain(first_cluster, no_fat_chain, 0, data_len, &mut data)?;
        Ok(data)
    }
}
