#!/bin/sh
#
# Build mt-capture (macOS host). Links Apple's private MultitouchSupport.framework.
#
set -e
cd "$(dirname "$0")"
clang -O2 -Wall mt-capture.c -o mt-capture \
    -F/System/Library/PrivateFrameworks \
    -framework MultitouchSupport -framework CoreFoundation
echo "built ./mt-capture"
