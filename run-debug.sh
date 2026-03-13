#!/bin/bash
cargo build --release -p vibepanel && target/release/vibepanel 2>&1 | tee /tmp/vibepanel-debug.log
