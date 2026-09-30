#!/bin/bash
set -e

# Define directories
BUILD_DIR="build/initramfs_base"
OUTPUT="build/initramfs.cpio"

# Clean up previous build
rm -rf "$BUILD_DIR"
mkdir -p "$BUILD_DIR"

echo "Creating base files for initramfs..."
# Base directories (UNIX compliant)
mkdir -p "$BUILD_DIR"/{bin,boot,dev,etc/defaults,lib,libexec,media,mnt,proc,rescue,root,sbin,sys,tmp,usr/bin,usr/lib,usr/libexec,usr/sbin,usr/share,var/log,var/run,var/tmp}

# Set permissions
chmod 1777 "$BUILD_DIR"/tmp "$BUILD_DIR"/var/tmp
chmod 0700 "$BUILD_DIR"/root

# Build the init binary if tools/init.s exists
if [ -f "tools/init.s" ]; then
    echo "Assembling tools/init.s..."
    as tools/init.s -o /tmp/init.o
    ld -nostdlib -no-pie /tmp/init.o -o "$BUILD_DIR/init"
    rm -f /tmp/init.o
    chmod 0755 "$BUILD_DIR/init"
    cp "$BUILD_DIR/init" "$BUILD_DIR/sbin/init"
    echo "Installed /init and /sbin/init in initramfs"
fi

# Build the CPIO archive (newc format)
mkdir -p $(dirname "$OUTPUT")
cd "$BUILD_DIR"
find . | cpio -o -H newc > "../../$OUTPUT"

echo "Initramfs built successfully at $OUTPUT"