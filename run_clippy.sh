#!/bin/bash
cd /Users/sergeybelov/Projects/sshore
cargo clippy -- -D warnings 2>&1 | tee /tmp/clippy_output.txt
echo "EXIT_CODE: $?"
