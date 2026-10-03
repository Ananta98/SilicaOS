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

# Assemble a freestanding source file into a static, libc-free ELF and install
# it at every given path in the image.
build_freestanding() {
    local src="$1"
    shift
    echo "Assembling $src..."
    # Both intermediates go to a scratch directory: anything left inside
    # "$BUILD_DIR" ends up in the image, and an ELF at the image root would be
    # picked up by init's candidate path list.
    local work out
    work="$(mktemp -d -t silica-asm-XXXXXX)"
    as "$src" -o "$work/unlinked.o"
    out="$work/$(basename "$src" .s)"
    ld -nostdlib -no-pie "$work/unlinked.o" -o "$out"
    local dest
    for dest in "$@"; do
        cp "$out" "$BUILD_DIR/$dest"
        chmod 0755 "$BUILD_DIR/$dest"
    done
    rm -rf "$work"
    echo "Installed $*"
}

# The boot process. Must call exit(2) as well as write(2): process teardown is
# only observable if something actually exits.
if [ -f "tools/init.s" ]; then
    build_freestanding tools/init.s init sbin/init
else
    echo "WARNING: tools/init.s is missing; the image will have no init process."
fi

# The signal test. Not run by default: boot it deliberately with
# `init=/sbin/sigtest` so an ordinary boot stays quiet.
if [ -f "tools/sigtest.s" ]; then
    build_freestanding tools/sigtest.s sbin/sigtest
else
    echo "WARNING: tools/sigtest.s is missing; the signal test will be absent."
fi

# Build the CPIO archive (newc format)
mkdir -p $(dirname "$OUTPUT")
cd "$BUILD_DIR"
find . | cpio -o -H newc > "../../$OUTPUT"

echo "Initramfs built successfully at $OUTPUT"