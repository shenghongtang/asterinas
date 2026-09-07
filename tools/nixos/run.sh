#!/bin/sh

# SPDX-License-Identifier: MPL-2.0

# Run a NixOS installation or NixOS ISO installer image built by the root Makefile inside a VM
#
# Usage: ./run.sh [nixos | iso]

set -e

usage() {
    echo "Usage: $0 [nixos | iso]"
    exit 1
}

if [ "$#" -ne 1 ]; then
    usage
fi

MODE=$1
TARGET_ARCH=${TARGET_ARCH:-x86_64}
SCRIPT_DIR=$(dirname "$0")
ASTERINAS_DIR=$(realpath "${SCRIPT_DIR}/../..")

# tools/qemu_args.sh currently emits x86_64-specific arguments.
# Reject other architectures to avoid invoking non-x86 QEMU with incompatible args.
if [ "${TARGET_ARCH}" != "x86_64" ]; then
    echo "Error: tools/nixos/run.sh currently supports only TARGET_ARCH=x86_64; got ${TARGET_ARCH}" >&2
    exit 1
fi

# Change to Asterinas root directory to ensure all scripts run from the correct location.
cd "${ASTERINAS_DIR}"

# Get base QEMU arguments from qemu_args.sh script
QEMU_ARGS=$(${ASTERINAS_DIR}/tools/qemu_args.sh common 2>/dev/null)

# Add mode-specific disk and device arguments
case "$MODE" in
    nixos)
        NIXOS_DIR="${ASTERINAS_DIR}/target/nixos"
        QEMU_ARGS="${QEMU_ARGS} \
            -drive if=none,format=raw,id=u0,file=${NIXOS_DIR}/asterinas.img \
            -device virtio-blk-pci,drive=u0,disable-legacy=on,disable-modern=off \
        "
        ;;
    iso)
        ASTER_IMAGE_PATH=${ASTERINAS_DIR}/target/nixos/asterinas.img
        NIXOS_DISK_SIZE_IN_MB=${NIXOS_DISK_SIZE_IN_MB:-16384}
        ISO_IMAGE_PATH=$(find "${ASTERINAS_DIR}/target/nixos/iso_image/iso" -name "*.iso" | head -n 1)

        if [ ! -f "$ISO_IMAGE_PATH" ]; then
            echo "Error: ISO_IMAGE not found!"
            exit 1
        fi

        rm -f "${ASTER_IMAGE_PATH}"
        echo "Creating image at ${ASTER_IMAGE_PATH} of size ${NIXOS_DISK_SIZE_IN_MB}MB......"
        dd if=/dev/zero of="${ASTER_IMAGE_PATH}" bs=1M count=${NIXOS_DISK_SIZE_IN_MB} status=none
        echo "Image created successfully!"

        QEMU_ARGS="${QEMU_ARGS} \
            -cdrom ${ISO_IMAGE_PATH} -boot d \
            -drive if=none,format=raw,id=u0,file=${ASTER_IMAGE_PATH} \
            -device virtio-blk-pci,drive=u0,disable-legacy=on,disable-modern=off \
        "
        ;;
    *)
        usage
        ;;
esac

# Support an extra disk for device mapper testing.
# Usage: DM_DISK=/path/to/disk.img make run_nixos
#        DM_DISK_SIZE=512 make run_nixos  (auto-create a 512MB disk)
if [ -n "${DM_DISK}" ]; then
    DM_DISK_PATH="${DM_DISK}"
    # Auto-create the disk if it doesn't exist
    if [ ! -f "${DM_DISK_PATH}" ]; then
        DM_DISK_SIZE=${DM_DISK_SIZE:-512}
        echo "Creating DM test disk at ${DM_DISK_PATH} (${DM_DISK_SIZE}MB)..."
        qemu-img create -f raw "${DM_DISK_PATH}" "${DM_DISK_SIZE}M" >/dev/null
    fi
    QEMU_ARGS="${QEMU_ARGS} \
        -drive if=none,format=raw,id=dm0,file=${DM_DISK_PATH},cache=writethrough \
        -device virtio-blk-pci,drive=dm0,disable-legacy=on,disable-modern=off \
    "
    echo "DM test disk: ${DM_DISK_PATH} -> /dev/vde"
fi

# Support a second extra disk for device mapper testing.
# Usage: DM_DISK2=/path/to/disk2.img make run_nixos
#        DM_DISK2_SIZE=512 make run_nixos  (auto-create a 512MB disk)
if [ -n "${DM_DISK2}" ]; then
    DM_DISK2_PATH="${DM_DISK2}"
    # Auto-create the disk if it doesn't exist
    if [ ! -f "${DM_DISK2_PATH}" ]; then
        DM_DISK2_SIZE=${DM_DISK2_SIZE:-512}
        echo "Creating DM test disk 2 at ${DM_DISK2_PATH} (${DM_DISK2_SIZE}MB)..."
        qemu-img create -f raw "${DM_DISK2_PATH}" "${DM_DISK2_SIZE}M" >/dev/null
    fi
    QEMU_ARGS="${QEMU_ARGS} \
        -drive if=none,format=raw,id=dm1,file=${DM_DISK2_PATH},cache=writethrough \
        -device virtio-blk-pci,drive=dm1,disable-legacy=on,disable-modern=off \
    "
    echo "DM test disk 2: ${DM_DISK2_PATH} -> /dev/vdf"
fi

if [ "${ENABLE_KVM}" = "1" ]; then
    QEMU_ARGS="${QEMU_ARGS} -accel kvm"
fi

QEMU_BIN=${QEMU_BIN:-qemu-system-${TARGET_ARCH}}

# The kernel uses a specific value to signal a successful shutdown via the
# isa-debug-exit device.
KERNEL_SUCCESS_EXIT_CODE=16 # 0x10 in hexadecimal
# QEMU translates the value written to the isa-debug-exit device into a final
# process exit code using following formula.
QEMU_SUCCESS_EXIT_CODE=$(((KERNEL_SUCCESS_EXIT_CODE << 1) | 1))

# Execute QEMU
# shellcheck disable=SC2086
${QEMU_BIN} ${QEMU_ARGS} || exit_code=$?
exit_code=${exit_code:-0}

# Check if the execution was successful:
# - Exit code 0: Normal successful exit (e.g., ACPI shutdown or clean termination)
# - Exit code $QEMU_SUCCESS_EXIT_CODE: Kernel signaled success via isa-debug-exit device
if [ ${exit_code} -eq 0 ] || [ ${exit_code} -eq ${QEMU_SUCCESS_EXIT_CODE} ]; then
    exit 0
fi

exit ${exit_code}
