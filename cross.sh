#!/bin/bash

set -ex

RUSTFLAGS= cross build --target armv7-unknown-linux-gnueabihf
scp -C target/armv7-unknown-linux-gnueabihf/debug/rf config.toml pi@192.168.86.35:
ssh pi@192.168.86.35 ./rf
