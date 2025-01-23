#!/usr/bin/env bash

foot -D pefrontend bash -c 'toolbox run npm run dev; exec /bin/bash' & disown
foot -D pefrontend bash -c './nginx.sh; exec /bin/bash' & disown
foot -D peserver bash -c 'cargo run --bin peserver; exec /bin/bash' & disown
foot -D peserver bash -c 'cargo run --bin lb; exec /bin/bash' & disown
