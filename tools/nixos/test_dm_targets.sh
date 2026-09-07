#!/bin/bash
# ============================================================================
# Asterinas DM target tests: error / zero / verity
#
# These tests exercise the three non-linear DM targets from userspace via
# `dmsetup`. They require only /dev/vde (a 512 MB disk) and the `dmsetup`
# utility inside the guest.
#
# Usage (inside the guest):
#   bash test_dm_targets.sh
#
# Launch:
#   make run_nixos DM_DISK=target/nixos/dm_test.img DM_DISK_SIZE=512
# ============================================================================

set -u

DATA_DEV=/dev/vde
MNT=/mnt/dmtest

ok()   { echo -e "\033[32m[PASS]\033[0m $*"; }
fail() { echo -e "\033[31m[FAIL]\033[0m $*"; exit 1; }
step() { echo -e "\n\033[36m==== $* ====\033[0m"; }
info() { echo -e "\033[33m[INFO]\033[0m $*"; }

cleanup_dm() {
    # Unmount anything on our mount point
    umount "$MNT" 2>/dev/null || true
    # Remove DM devices we created (by name prefix "err-", "zero-", "verity-")
    for name in err-target zero-target verity-good verity-bad; do
        dmsetup remove "$name" 2>/dev/null || true
    done
}

# ---------------------------------------------------------------------------
# Preflight
# ---------------------------------------------------------------------------
step "0. Preflight"

[ -b "$DATA_DEV" ] || fail "$DATA_DEV not found; run with DM_DISK=..."
command -v dmsetup >/dev/null || fail "dmsetup not found in guest"
ok "preflight passed"

# Clean any stale state
cleanup_dm
mkdir -p "$MNT"

# ---------------------------------------------------------------------------
# 1. error target
# ---------------------------------------------------------------------------
# The error target fails all reads and writes with an I/O error.
# We create a small error device, then verify that reading it produces
# an I/O error (dd reports a read error).
step "1. error target — all I/O must fail"

# Create a 64-sector (32 KB) error device.
dmsetup create err-target <<TABLE
0 64 error
TABLE
[ -b /dev/mapper/err-target ] || fail "error target device node not created"
ok "created error target -> /dev/mapper/err-target"

# Reading should fail
if dd if=/dev/mapper/err-target of=/dev/null bs=512 count=1 2>/dev/null; then
    fail "read from error target unexpectedly succeeded"
else
    ok "read from error target correctly returned I/O error"
fi

# Writing should fail
if dd if=/dev/zero of=/dev/mapper/err-target bs=512 count=1 2>/dev/null conv=notrunc; then
    fail "write to error target unexpectedly succeeded"
else
    ok "write to error target correctly returned I/O error"
fi

# dmsetup status should show the error target
dmsetup status err-target | grep -q error \
    || fail "dmsetup status does not show error target"
ok "dmsetup status shows error target"

# dmsetup table should show the error target
dmsetup table err-target | grep -q error \
    || fail "dmsetup table does not show error target"
ok "dmsetup table shows error target"

dmsetup remove err-target || fail "failed to remove error target"
ok "removed error target"

# ---------------------------------------------------------------------------
# 2. zero target
# ---------------------------------------------------------------------------
# The zero target returns zero-filled blocks on read and discards writes.
# We verify:
#   - Reads return all zeros
#   - Writes succeed silently (no error)
#   - Data read back after write is still zero (writes are discarded)
step "2. zero target — reads return zeros, writes are discarded"

# Create a 128-sector (64 KB) zero device.
dmsetup create zero-target <<TABLE
0 128 zero
TABLE
[ -b /dev/mapper/zero-target ] || fail "zero target device node not created"
ok "created zero target -> /dev/mapper/zero-target"

# Read 4 KB and verify all zeros
dd if=/dev/mapper/zero-target of=/tmp/zero_read.bin bs=4096 count=1 2>/dev/null \
    || fail "read from zero target failed"
if cmp -s /tmp/zero_read.bin /dev/zero; then
    ok "read from zero target returned all zeros"
else
    fail "zero target read returned non-zero data"
fi

# Write some non-zero data — should succeed (silently discarded)
dd if=/dev/urandom of=/dev/mapper/zero-target bs=4096 count=1 2>/dev/null \
    conv=notrunc || fail "write to zero target failed"
ok "write to zero target succeeded (discarded)"

# Read back — should still be zero (write was discarded)
dd if=/dev/mapper/zero-target of=/tmp/zero_read2.bin bs=4096 count=1 2>/dev/null \
    || fail "second read from zero target failed"
if cmp -s /tmp/zero_read2.bin /dev/zero; then
    ok "read after write still returned zeros (write discarded)"
else
    fail "zero target returned non-zero data after write (write not discarded?)"
fi

# mkfs on zero target should work (it only writes)
mkfs.ext2 -F /dev/mapper/zero-target 2>/dev/null \
    || fail "mkfs.ext2 on zero target failed"
ok "mkfs.ext2 on zero target succeeded"

# Mount and verify it's empty
mount -t ext2 /dev/mapper/zero-target "$MNT" 2>/dev/null \
    || fail "mount zero target fs failed"
ls -la "$MNT" | grep -q "lost+found" && ok "zero target fs has lost+found"
umount "$MNT" || fail "umount zero target fs failed"

dmsetup remove zero-target || fail "failed to remove zero target"
ok "removed zero target"

rm -f /tmp/zero_read.bin /tmp/zero_read2.bin

# ---------------------------------------------------------------------------
# 3. verity target
# ---------------------------------------------------------------------------
# The verity target is read-only and verifies data integrity via a Merkle
# hash tree. We need:
#   - A data device with known content
#   - A hash device storing the Merkle tree
#   - The root digest (computed from the data + salt)
#
# Since the guest may not have `veritysetup`, we build the hash tree manually
# using the same algorithm as the kernel (SHA-256, salt prepended).
#
# The verity target parameters (from parser.rs):
#   <version> <data_dev> <hash_dev> <data_block_size> <hash_block_size>
#   <num_data_blocks> <hash_start_block> <algorithm> <root_digest> <salt>
step "3. verity target — integrity verification"

# Use /dev/vde for both data and hash (data in first blocks, hash after).
# We'll create a small verity device over 1 data block (4096 bytes = 8 sectors).
DATA_BLOCK_SIZE=4096
HASH_BLOCK_SIZE=4096
NUM_DATA_BLOCKS=1
SECTORS_PER_BLOCK=8  # 4096/512

# Write known data to block 0 of /dev/vde (first 4096 bytes)
# Use a simple pattern: all 0x41 ('A')
dd if=/dev/zero of=/tmp/verity_data.bin bs=4096 count=1 2>/dev/null
printf 'A%.0s' {1..4096} | tr -d '\n' > /tmp/verity_data.bin 2>/dev/null || \
    dd if=/dev/zero bs=4096 count=1 2>/dev/null | tr '\0' 'A' > /tmp/verity_data.bin

# Actually fill with 'A' using a simpler method
python3 -c "import sys; sys.stdout.buffer.write(b'A' * 4096)" > /tmp/verity_data.bin 2>/dev/null || \
    perl -e 'print "A" x 4096' > /tmp/verity_data.bin 2>/dev/null || \
    { echo "need python3 or perl for verity test"; exit 1; }

dd if=/tmp/verity_data.bin of="$DATA_DEV" bs=4096 count=1 conv=notrunc 2>/dev/null \
    || fail "failed to write verity data to $DATA_DEV"

# Compute the hash tree.
# verity format (version 1): each hash block = SHA-256(salt || data_block)
# For 1 data block, the hash tree is just 1 hash block containing 1 digest.
SALT="0123456789abcdef0123456789abcdef"  # 16 bytes hex = 32 bytes binary

# Compute leaf hash: SHA-256(salt_bytes || data_block)
python3 << 'PYEOF' > /tmp/verity_hash.hex 2>/dev/null
import hashlib, struct

salt_hex = "0123456789abcdef0123456789abcdef"
salt = bytes.fromhex(salt_hex)
data_block = b'A' * 4096

# Leaf hash: SHA-256(salt || data_block)
leaf = hashlib.sha256(salt + data_block).digest()

# Root hash (for 1 data block, the leaf IS the root)
root = leaf

# Hash block: 4096 bytes, first 32 bytes = root digest, rest zero-padded
hash_block = root + b'\x00' * (4096 - 32)

# Write hash block as hex for inspection
print("root_digest={}".format(root.hex()))
print("hash_block_hex={}".format(hash_block.hex()))

# Also write the raw hash block to a file
with open('/tmp/verity_hash_block.bin', 'wb') as f:
    f.write(hash_block)
PYEOF

[ -s /tmp/verity_hash.hex ] || fail "failed to compute verity hash tree (need python3)"

ROOT_DIGEST=$(grep root_digest /tmp/verity_hash.hex | cut -d= -f2)
info "root digest = $ROOT_DIGEST"

# Write hash block to /dev/vde at block 1 (offset 4096)
dd if=/tmp/verity_hash_block.bin of="$DATA_DEV" bs=4096 count=1 seek=1 conv=notrunc 2>/dev/null \
    || fail "failed to write hash block to $DATA_DEV"

# Get the major:minor of /dev/vde for the table
DATA_MAJMIN=$(stat -c '%t:%T' "$DATA_DEV" 2>/dev/null || echo "")
if [ -z "$DATA_MAJMIN" ]; then
    # Fallback: use device name
    DATA_SPEC="$DATA_DEV"
else
    DATA_SPEC="$DATA_MAJMIN"
fi
info "data device spec = $DATA_SPEC"

# Create the verity device.
# Table format:
#   0 <size_sectors> verity <version> <data_dev> <hash_dev>
#       <data_bs> <hash_bs> <num_data_blocks> <hash_start>
#       <algorithm> <root_digest> <salt>
SIZE_SECTORS=$((NUM_DATA_BLOCKS * SECTORS_PER_BLOCK))

dmsetup create verity-good <<TABLE
0 $SIZE_SECTORS verity 1 $DATA_SPEC $DATA_SPEC 4096 4096 $NUM_DATA_BLOCKS 1 sha256 $ROOT_DIGEST $SALT
TABLE
[ -b /dev/mapper/verity-good ] || fail "verity-good device not created"
ok "created verity device (correct hash) -> /dev/mapper/verity-good"

# Read from the verity device — should succeed and return 'A' * 4096
dd if=/dev/mapper/verity-good of=/tmp/verity_read.bin bs=4096 count=1 2>/dev/null \
    || fail "read from verity-good failed"
if cmp -s /tmp/verity_read.bin /tmp/verity_data.bin; then
    ok "verity-good returned correct data (all 'A')"
else
    fail "verity-good returned wrong data"
fi

# Now test the error case: corrupt the data block and verify read fails.
step "3b. verity target — tampered data must fail"

dmsetup remove verity-good || true

# Corrupt data block 0 on /dev/vde: change first byte from 'A' to 'B'
printf 'B' | dd of="$DATA_DEV" bs=1 count=1 conv=notrunc 2>/dev/null \
    || fail "failed to corrupt data"

dmsetup create verity-bad <<TABLE
0 $SIZE_SECTORS verity 1 $DATA_SPEC $DATA_SPEC 4096 4096 $NUM_DATA_BLOCKS 1 sha256 $ROOT_DIGEST $SALT
TABLE
[ -b /dev/mapper/verity-bad ] || fail "verity-bad device not created"
ok "created verity device (tampered data) -> /dev/mapper/verity-bad"

# Read from verity-bad — should fail with I/O error (hash mismatch)
if dd if=/dev/mapper/verity-bad of=/dev/null bs=4096 count=1 2>/dev/null; then
    fail "read from verity-bad unexpectedly succeeded (hash check missed tamper)"
else
    ok "read from verity-bad correctly failed (hash mismatch detected)"
fi

# Restore data block for cleanup
dd if=/tmp/verity_data.bin of="$DATA_DEV" bs=4096 count=1 conv=notrunc 2>/dev/null || true

dmsetup remove verity-bad || fail "failed to remove verity-bad"
ok "removed verity-bad"

# ---------------------------------------------------------------------------
# 4. Cleanup
# ---------------------------------------------------------------------------
step "4. Cleanup"

cleanup_dm
rm -f /tmp/verity_data.bin /tmp/verity_hash.hex /tmp/verity_hash_block.bin \
      /tmp/verity_read.bin /tmp/zero_read.bin /tmp/zero_read2.bin

echo
ok "All DM target tests (error / zero / verity) passed"
echo
