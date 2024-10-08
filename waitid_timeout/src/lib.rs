use libc::{c_int};

#[cfg(not(target_os = "linux"))]
compile_error!("wait4 is a linux specific feature");

// TODO only on x86-64 I think
const NR_WAITID: c_int = 247;

extern "C" {
    fn syscall(num: c_int, ...) -> c_int;
}


// int waitid(idtype_t idtype, id_t id, siginfo_t *infop, int options, struct rusage*);
//unsafe fn wait4(

pub fn add(left: u64, right: u64) -> u64 {
    left + right
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_works() {
        let result = add(2, 2);
        assert_eq!(result, 4);
    }
}
