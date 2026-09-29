use alloc::string::String;
use alloc::vec::Vec;
use spin::Mutex;
use spin::once::Once;

pub struct Cmdline {
    pub init_path: String,
    pub extra_args: Vec<String>,
}

static CMDLINE: Once<Mutex<Cmdline>> = Once::new();

/// Initialize the command line arguments from the bootloader.
/// This processes `init=` or `rdinit=` configuration dynamically.
pub fn init(cmdline_str: &str) {
    let mut init_path = String::from("/sbin/init"); // Default FreeBSD-style UNIX init path
    let mut extra_args = Vec::new();

    for arg in cmdline_str.split_whitespace() {
        if let Some(path) = arg.strip_prefix("init=") {
            init_path = String::from(path);
        } else if let Some(path) = arg.strip_prefix("rdinit=") {
            init_path = String::from(path);
        } else {
            extra_args.push(String::from(arg));
        }
    }

    let cmdline = Cmdline {
        init_path,
        extra_args,
    };

    CMDLINE.call_once(|| Mutex::new(cmdline));
}

/// Retrieve the configured path to the initial user space program.
pub fn get_init_path() -> String {
    if let Some(cmdline) = CMDLINE.get() {
        cmdline.lock().init_path.clone()
    } else {
        String::from("/sbin/init")
    }
}

/// Retrieve extra kernel command line arguments.
pub fn get_extra_args() -> Vec<String> {
    if let Some(cmdline) = CMDLINE.get() {
        cmdline.lock().extra_args.clone()
    } else {
        Vec::new()
    }
}
