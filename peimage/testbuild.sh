#!/usr/bin/env bash

rm ../ocismall.sqfs
go run peimage.go export /tmp/peimage/ocismall busybox:1.37 busybox:1.36.1 busybox:1.36.0 | sqfstar ../ocismall.sqfs
