// SPDX-License-Identifier: GPL-2.0

//! Initramfs CPIO filesystem driver and boot loader integration.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use spin::RwLock;

use crate::errno::{Errno, Result};
use crate::fs::vfs::{
    FileOps, FileSystem, INode, INodeAttr, Mode, NodeOps, OpenFlags, SeekAnchor,
};
use crate::utils::cpio::{CpioArchive, FileType};

/// File operations for reading from an in-memory initramfs file slice.
pub struct InitramfsFileOps {
    data: &'static [u8],
}

impl FileOps for InitramfsFileOps {
    fn read(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let offset = offset as usize;
        if offset >= self.data.len() {
            return Ok(0);
        }
        let available = &self.data[offset..];
        let copy_len = buf.len().min(available.len());
        buf[..copy_len].copy_from_slice(&available[..copy_len]);
        Ok(copy_len)
    }

    fn write(&self, _offset: u64, _buf: &[u8]) -> Result<usize> {
        crate::return_errno!(EROFS, "initramfs is read-only");
    }

    fn seek(&self, curr_offset: u64, anchor: SeekAnchor) -> Result<u64> {
        let new_offset = match anchor {
            SeekAnchor::Start(pos) => pos as i64,
            SeekAnchor::Current(delta) => curr_offset as i64 + delta,
            SeekAnchor::End(delta) => self.data.len() as i64 + delta,
        };
        if new_offset < 0 {
            crate::return_errno!(EINVAL, "invalid seek position");
        }
        Ok(new_offset as u64)
    }
}

pub struct InitramfsFileNodeOps {
    data: &'static [u8],
}

impl NodeOps for InitramfsFileNodeOps {
    fn open(&self, flags: OpenFlags) -> Result<Box<dyn FileOps>> {
        if flags.contains(OpenFlags::WRITE) {
            crate::return_errno!(EROFS, "initramfs is read-only");
        }
        Ok(Box::new(InitramfsFileOps { data: self.data }))
    }

    fn getattr(&self) -> Result<INodeAttr> {
        Ok(INodeAttr {
            size: self.data.len(),
            mode: Mode::FILE | Mode::RUSR | Mode::RGRP | Mode::ROTH | Mode::XUSR | Mode::XGRP | Mode::XOTH,
            nlink: 1,
            uid: 0,
            gid: 0,
            rdev: 0,
        })
    }
}

pub struct InitramfsDirFileOps;

impl FileOps for InitramfsDirFileOps {
    fn read(&self, _offset: u64, _buf: &mut [u8]) -> Result<usize> {
        crate::return_errno!(EISDIR, "cannot read from directory");
    }

    fn write(&self, _offset: u64, _buf: &[u8]) -> Result<usize> {
        crate::return_errno!(EISDIR, "cannot write to directory");
    }
}

pub struct InitramfsDirNodeOps {
    pub entries: Arc<RwLock<BTreeMap<String, Arc<INode>>>>,
}

impl InitramfsDirNodeOps {
    pub fn new() -> Self {
        Self {
            entries: Arc::new(RwLock::new(BTreeMap::new())),
        }
    }

    pub fn insert(&self, name: String, node: Arc<INode>) {
        self.entries.write().insert(name, node);
    }
}

impl NodeOps for InitramfsDirNodeOps {
    fn lookup(&self, name: &str) -> Result<Arc<INode>> {
        self.entries.read().get(name).cloned().ok_or(Errno::ENOENT)
    }

    fn open(&self, flags: OpenFlags) -> Result<Box<dyn FileOps>> {
        if flags.contains(OpenFlags::WRITE) {
            crate::return_errno!(EROFS, "initramfs is read-only");
        }
        Ok(Box::new(InitramfsDirFileOps))
    }
}

/// The read-only CPIO initramfs filesystem instance.
pub struct InitramfsFs {
    root: Arc<INode>,
}

impl InitramfsFs {
    /// Creates and populates an Initramfs filesystem from boot memory.
    pub fn new() -> Self {
        let root_entries = Arc::new(RwLock::new(BTreeMap::new()));
        let root = Arc::new(INode::new(
            Box::new(InitramfsDirNodeOps {
                entries: Arc::clone(&root_entries),
            }),
            Mode::DIR | Mode::RUSR | Mode::WUSR | Mode::XUSR | Mode::RGRP | Mode::XGRP | Mode::ROTH | Mode::XOTH,
            1,
        ));

        let boot_info = ostd::boot::boot_info();
        if let Some(initramfs_buf) = boot_info.initramfs {
            ostd::info!("Building VFS tree from initramfs ({} bytes)...", initramfs_buf.len());
            let archive = CpioArchive::new(initramfs_buf);
            let mut ino = 2;
            let mut count = 0;

            // Maps directory path -> directory entries map
            let mut dir_map: BTreeMap<String, Arc<RwLock<BTreeMap<String, Arc<INode>>>>> = BTreeMap::new();
            dir_map.insert(String::new(), Arc::clone(&root_entries));

            for entry in archive.flatten() {
                let clean = entry.name.trim_start_matches('.').trim_start_matches('/');
                if clean.is_empty() {
                    continue;
                }

                let parts: Vec<&str> = clean.split('/').filter(|s| !s.is_empty()).collect();
                if parts.is_empty() {
                    continue;
                }

                // Ensure intermediate directories exist
                let mut current_path = String::new();
                for i in 0..parts.len() - 1 {
                    let parent_path = current_path.clone();
                    if !current_path.is_empty() {
                        current_path.push('/');
                    }
                    current_path.push_str(parts[i]);

                    if !dir_map.contains_key(&current_path) {
                        let new_dir_entries = Arc::new(RwLock::new(BTreeMap::new()));
                        let new_dir_node = Arc::new(INode::new(
                            Box::new(InitramfsDirNodeOps {
                                entries: Arc::clone(&new_dir_entries),
                            }),
                            Mode::DIR | Mode::RUSR | Mode::WUSR | Mode::XUSR | Mode::RGRP | Mode::XGRP | Mode::ROTH | Mode::XOTH,
                            ino,
                        ));
                        ino += 1;

                        if let Some(parent_entries) = dir_map.get(&parent_path) {
                            parent_entries.write().insert(parts[i].into(), new_dir_node);
                        }
                        dir_map.insert(current_path.clone(), new_dir_entries);
                    }
                }

                let parent_path = if parts.len() > 1 {
                    parts[..parts.len() - 1].join("/")
                } else {
                    String::new()
                };

                let leaf_name = parts.last().unwrap();
                match entry.file_type {
                    FileType::Directory => {
                        let mut full_dir_path = parent_path.clone();
                        if !full_dir_path.is_empty() {
                            full_dir_path.push('/');
                        }
                        full_dir_path.push_str(leaf_name);

                        if !dir_map.contains_key(&full_dir_path) {
                            let new_dir_entries = Arc::new(RwLock::new(BTreeMap::new()));
                            let new_dir_node = Arc::new(INode::new(
                                Box::new(InitramfsDirNodeOps {
                                    entries: Arc::clone(&new_dir_entries),
                                }),
                                Mode::DIR | Mode::RUSR | Mode::WUSR | Mode::XUSR | Mode::RGRP | Mode::XGRP | Mode::ROTH | Mode::XOTH,
                                ino,
                            ));
                            ino += 1;

                            if let Some(parent_entries) = dir_map.get(&parent_path) {
                                parent_entries.write().insert((*leaf_name).into(), new_dir_node);
                            }
                            dir_map.insert(full_dir_path, new_dir_entries);
                        }
                    }
                    _ => {
                        let file_node = Arc::new(INode::new_with_attr(
                            Box::new(InitramfsFileNodeOps { data: entry.data }),
                            ino,
                            INodeAttr {
                                size: entry.data.len(),
                                mode: Mode::FILE | Mode::RUSR | Mode::WUSR | Mode::XUSR | Mode::RGRP | Mode::XGRP | Mode::ROTH | Mode::XOTH,
                                nlink: 1,
                                uid: entry.uid,
                                gid: entry.gid,
                                rdev: 0,
                            },
                        ));
                        ino += 1;
                        count += 1;

                        if let Some(parent_entries) = dir_map.get(&parent_path) {
                            parent_entries.write().insert((*leaf_name).into(), file_node);
                        }
                    }
                }
            }
            ostd::info!("Initramfs VFS tree mounted: {} files loaded", count);
        } else {
            ostd::warn!("No initramfs provided by bootloader");
        }

        Self { root }
    }
}

impl FileSystem for InitramfsFs {
    fn name(&self) -> &'static str {
        "initramfs"
    }

    fn root(&self) -> Result<Arc<INode>> {
        Ok(Arc::clone(&self.root))
    }
}

/// Reads file data from initramfs by path (legacy compatibility helper).
pub fn read_file_from_initramfs(path: &str) -> Option<&'static [u8]> {
    let clean = path.trim_start_matches('/');
    let initramfs_buf = ostd::boot::boot_info().initramfs?;
    let archive = CpioArchive::new(initramfs_buf);
    for entry in archive.flatten() {
        let name = entry.name.trim_start_matches('.').trim_start_matches('/');
        if name == clean {
            return Some(entry.data);
        }
    }
    None
}