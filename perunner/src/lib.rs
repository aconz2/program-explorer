mod cloudhypervisor; 

use std::ffi::OsString;

struct CloudHypervisor {
    bin: OsString,
    kernel: OsString,
    initramfs: OsString,
    log_file: bool,
    console: bool,
}
