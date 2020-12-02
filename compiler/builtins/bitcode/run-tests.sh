#!/bin/bash

set -euxo pipefail

# Test every zig
find src/*.zig -type f -exec zig test --library c {} \;
