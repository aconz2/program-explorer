#!/bin/bash

function inspectdir() {
    cat <(cd $1 && find -type f -exec sha256sum '{}' '+' | sort) <(cd $1 && find -type d | sort)
}

function hashdir() {
    inspectdir $1 | sha256sum
}

for dir in "$@"; do
    h=$(hashdir "$dir")
    echo "$h $dir"
done
