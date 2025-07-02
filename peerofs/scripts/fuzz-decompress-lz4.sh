#!/bin/bash

# https://llvm.org/docs/LibFuzzer.html
# len_control=0 makes it try long input lengths immediately
cargo +nightly fuzz run fuzz_decompress_lz4 -- -max_len=10000 -len_control=1
