pub mod initramfs;

pub fn init() {
    initramfs::initramfs_init();
}