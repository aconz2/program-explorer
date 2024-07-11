#!/usr/bin/env bash

qemu-system-x86_64 -smp 2 -enable-kvm -m 2048 -drive file=~/Downloads/Fedora-Cloud-Base-Generic.x86_64-40-1.14.qcow2

