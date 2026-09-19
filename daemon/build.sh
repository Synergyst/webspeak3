#!/bin/bash

cd "$(dirname -- "${BASH_SOURCE[0]}")" || exit

sudo systemctl stop webspeak3-daemon.service && ~/.cargo/bin/cargo build --release -p network_manager_daemon && sudo install -m 755 target/release/network_manager_daemon /usr/local/bin/network_manager_daemon && sudo systemctl start webspeak3-daemon.service
