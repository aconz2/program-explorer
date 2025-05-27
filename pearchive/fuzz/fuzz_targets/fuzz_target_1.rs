#![no_main]

use libfuzzer_sys::fuzz_target;
use pearchive::unpack_to_hashmap;

fuzz_target!(|data: &[u8]| {
    let _ = unpack_to_hashmap(data);
});
