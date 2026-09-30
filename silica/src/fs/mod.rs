pub mod initramfs;

pub use initramfs::read_file_from_initramfs;

pub fn init() {
    initramfs::initramfs_init();
}