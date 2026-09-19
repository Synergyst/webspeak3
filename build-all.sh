#!/bin/bash

cd "$(dirname -- "${BASH_SOURCE[0]}")" || exit

./daemon/build.sh && docker compose down && docker compose up -d --build
