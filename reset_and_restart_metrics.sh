#!/usr/bin/env bash
./metrics/scripts/stop.sh
rm -rf ./metrics/scripts/lib
./metrics/scripts/start.sh
