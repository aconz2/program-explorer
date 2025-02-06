#!/usr/bin/env bash

set -e

TARGET=~/.config/containers/systemd/program-explorer-dev
mkdir -p ~/.config/containers/systemd
if [ ! -d $TARGET ]; then
    ln -s $(realpath quadlets) $TARGET
fi

systemctl --user daemon-reload

/usr/lib/systemd/system-generators/podman-system-generator --user --dryrun

#systemctl --user start pe-server-lb.service
#journalctl --user -feu pe-server-lb.service
