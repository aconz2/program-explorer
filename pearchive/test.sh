#!/bin/bash

set -e

cargo build

cargo run pack . /tmp/pearchive.pear
rm -rf /tmp/dest
mkdir /tmp/dest
cargo run unpack /tmp/pearchive.pear /tmp/dest

./scripts/dirdigest.sh $(pwd) /tmp/dest
