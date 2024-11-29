#!/usr/bin/env bash

podman run --net=host --rm -v $(realpath nginx.conf):/etc/nginx/nginx.conf:z,ro -p 8000:8000 -p 6188:6188 -p 5173:5173 docker.io/library/nginx
