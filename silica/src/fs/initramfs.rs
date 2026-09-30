use crate::utils::cpio::CpioArchive;
use crate::cmdline;

pub fn initramfs_init() {
    let boot_info = ostd::boot::boot_info();
    cmdline::init(&boot_info.kernel_cmdline);
    ostd::info!("Kernel cmdline: {:?}", boot_info.kernel_cmdline);
    ostd::info!("Selected init path: {}", cmdline::get_init_path());

    if let Some(initramfs_buf) = boot_info.initramfs {
        ostd::info!("Found initramfs image ({} bytes)", initramfs_buf.len());
        let archive = CpioArchive::new(initramfs_buf);
        let mut count = 0;
        for entry in archive {
            match entry {
                Ok(e) => {
                    count += 1;
                    ostd::info!("  initramfs [{:?}]: /{}", e.file_type, e.name);
                }
                Err(err) => {
                    ostd::warn!("  initramfs cpio decode error: {:?}", err);
                    break;
                }
            }
        }
        ostd::info!("Initramfs successfully parsed: {} entries", count);
    } else {
        ostd::warn!("No initramfs provided by bootloader");
    }
}

/// Reads file data from initramfs by path.
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