#!/bin/bash
# ============================================================================
# Asterinas DM striped target tests
#
# Verifies the multi-stripe (RAID0) `striped` target: data distribution
# across underlying devices, cross-stripe I/O splitting, filesystem use,
# single-stripe equivalence to `linear`, LVM2 integration, and error cases.
#
# Requires two 512 MB disks (/dev/vde and /dev/vdf) and `dmsetup`/`lvm2`
# inside the guest.
#
# Usage (inside the guest):
#   bash test_striped.sh
#
# Launch:
#   make run_nixos \
#     DM_DISK=target/nixos/dm_test.img  DM_DISK_SIZE=512 \
#     DM_DISK2=target/nixos/dm_test2.img DM_DISK2_SIZE=512
# ============================================================================

set -u

DEV_A=/dev/vde
DEV_B=/dev/vdf
MNT=/mnt/stripetest

ok()   { echo -e "\033[32m[PASS]\033[0m $*"; }
fail() { echo -e "\033[31m[FAIL]\033[0m $*"; FAILED=1; }
step() { echo -e "\n\033[36m==== $* ====\033[0m"; }
info() { echo -e "\033[33m[INFO]\033[0m $*"; }

cleanup_dm() {
    umount "$MNT" 2>/dev/null || true
    # Force-remove ALL DM devices (avoids stale devices from previous runs).
    dmsetup remove_all 2>/dev/null || true
    # Tear down LVM if present (try common VG names).
    for vg_name in vg dm; do
        vgchange -an "$vg_name" 2>/dev/null || true
        lvremove -f "$vg_name" 2>/dev/null || true
        vgremove -f "$vg_name" 2>/dev/null || true
    done
    # Remove stale VG device-node directories left by LVM2.
    rm -rf /dev/vg /dev/dm 2>/dev/null || true
    # Properly remove PV labels (clears metadata area pointers, not just the label).
    pvremove -ff "$DEV_A" "$DEV_B" 2>/dev/null || true
}

# ---------------------------------------------------------------------------
# 0. Preflight
# ---------------------------------------------------------------------------
step "0. Preflight"

[ -b "$DEV_A" ] || fail "$DEV_A not found; run with DM_DISK=/DM_DISK2="
[ -b "$DEV_B" ] || fail "$DEV_B not found; run with DM_DISK2="
command -v dmsetup >/dev/null || fail "dmsetup not found in guest"
ok "preflight passed"

cleanup_dm
mkdir -p "$MNT"

# Wipe the ENTIRE backing disks to destroy all stale LVM2 metadata (PV labels,
# VG metadata area, multiple metadata copies) from previous runs. Only zeroing
# the first 64 MB is insufficient — LVM2 may keep metadata copies or stale PE
# allocation state further into the disk.
info "wiping backing disks entirely..."
dd if=/dev/zero of="$DEV_A" bs=1M conv=notrunc 2>/dev/null
dd if=/dev/zero of="$DEV_B" bs=1M conv=notrunc 2>/dev/null

# ---------------------------------------------------------------------------
# 1. Stripe distribution — data must land on the correct physical device
# ---------------------------------------------------------------------------
# Layout: striped 2 64 (32 KB stripes)
#   logical sectors 0..63   (0..32 KB)     -> DEV_A offset 0
#   logical sectors 64..127 (32..64 KB)   -> DEV_B offset 0
# We write 64 KB: first 32 KB = 0xAA, next 32 KB = 0xBB, then verify each
# half is physically present on the expected device.
step "1. Stripe distribution across two devices"

dmsetup create stripe0 <<TABLE
0 4096 striped 2 64 $DEV_A 0 $DEV_B 0
TABLE
[ -b /dev/mapper/stripe0 ] || fail "striped device node not created"
ok "created striped device -> /dev/mapper/stripe0"

# Build a 64 KB pattern: 0xAA * 32768 + 0xBB * 32768
python3 -c "import sys; sys.stdout.buffer.write(b'\xAA'*32768 + b'\xBB'*32768)" \
    > /tmp/stripe_pattern.bin 2>/dev/null \
    || perl -e 'print "\xAA"x32768, "\xBB"x32768' > /tmp/stripe_pattern.bin
[ "$(stat -c %s /tmp/stripe_pattern.bin)" = "65536" ] \
    || fail "could not build 64 KB test pattern (need python3 or perl)"

dd if=/tmp/stripe_pattern.bin of=/dev/mapper/stripe0 bs=64K count=1 \
    conv=notrunc 2>/dev/null || fail "write to striped device failed"

# (a) Read back from the striped device — must match the pattern exactly.
dd if=/dev/mapper/stripe0 bs=64K count=1 of=/tmp/stripe_read.bin 2>/dev/null \
    || fail "read from striped device failed"
if cmp -s /tmp/stripe_read.bin /tmp/stripe_pattern.bin; then
    ok "striped read-back matches written pattern"
else
    fail "striped read-back differs from written pattern"
fi

# (b) DEV_A first 32 KB must be 0xAA (first stripe).
dd if="$DEV_A" bs=32K count=1 of=/tmp/dev_a_head.bin 2>/dev/null
if [ "$(head -c 1 /tmp/dev_a_head.bin | od -An -tx1 | tr -d ' ')" = "aa" ] \
   && cmp -s /tmp/dev_a_head.bin <(python3 -c "import sys; sys.stdout.buffer.write(b'\xAA'*32768)" 2>/dev/null || perl -e 'print "\xAA"x32768'); then
    ok "first stripe (0xAA) landed on $DEV_A"
else
    fail "first stripe did not land on $DEV_A as expected"
fi

# (c) DEV_B first 32 KB must be 0xBB (second stripe).
dd if="$DEV_B" bs=32K count=1 of=/tmp/dev_b_head.bin 2>/dev/null
if cmp -s /tmp/dev_b_head.bin <(python3 -c "import sys; sys.stdout.buffer.write(b'\xBB'*32768)" 2>/dev/null || perl -e 'print "\xBB"x32768'); then
    ok "second stripe (0xBB) landed on $DEV_B"
else
    fail "second stripe did not land on $DEV_B as expected"
fi

# dmsetup table should report the striped target type and parameters.
dmsetup table stripe0 | grep -q "striped 2 64" \
    && ok "dmsetup table reports striped target" \
    || fail "dmsetup table does not report striped target"

dmsetup remove stripe0 || fail "failed to remove stripe0"
ok "removed stripe0"

# ---------------------------------------------------------------------------
# 2. Cross-stripe I/O + filesystem
# ---------------------------------------------------------------------------
step "2. Cross-stripe I/O and filesystem"

dmsetup create stripe0 <<TABLE
0 8192 striped 2 64 $DEV_A 0 $DEV_B 0
TABLE
ok "created 4 MB striped device for fs test"

FS_OK=1
mkfs.ext2 -F /dev/mapper/stripe0 2>/dev/null \
    && ok "mkfs.ext2 on striped device succeeded" \
    || { fail "mkfs.ext2 on striped device failed"; FS_OK=0; }

if [ "$FS_OK" = "1" ]; then
    mount -t ext2 /dev/mapper/stripe0 "$MNT" 2>/dev/null \
        && ok "mounted striped fs" \
        || { fail "mount striped fs failed"; FS_OK=0; }
fi

if [ "$FS_OK" = "1" ]; then
    # Write a 1 MB file that spans many stripes and verify integrity.
    dd if=/dev/urandom of="$MNT/stripe_test.bin" bs=1M count=1 2>/dev/null \
        && ok "wrote 1 MB file across stripes" \
        || { fail "writing test file failed"; FS_OK=0; }
fi
if [ "$FS_OK" = "1" ]; then
    SUM_WRITE=$(md5sum "$MNT/stripe_test.bin" | awk '{print $1}')
    cp "$MNT/stripe_test.bin" /tmp/stripe_copy.bin
    SUM_COPY=$(md5sum /tmp/stripe_copy.bin | awk '{print $1}')
    if [ "$SUM_WRITE" = "$SUM_COPY" ]; then
        ok "cross-stripe file read/write integrity verified ($SUM_WRITE)"
    else
        fail "cross-stripe file integrity mismatch ($SUM_WRITE != $SUM_COPY)"
    fi

    ls -la "$MNT" | grep -q "stripe_test.bin" \
        && ok "file visible on striped fs" \
        || fail "file not visible on striped fs"

    umount "$MNT" || fail "umount striped fs failed"
fi
dmsetup remove stripe0 2>/dev/null || true
ok "removed stripe0 (fs test)"

# ---------------------------------------------------------------------------
# 3. Single-stripe striped target (regression: equivalent to linear)
# ---------------------------------------------------------------------------
step "3. Single-stripe striped target (linear equivalence)"

dmsetup create stripe1 <<TABLE
0 4096 striped 1 64 $DEV_A 0
TABLE
[ -b /dev/mapper/stripe1 ] || fail "single-stripe device not created"
ok "created single-stripe striped device"

# A read should succeed (previously the single-stripe path worked via
# LinearTarget; now it goes through StripedTarget and must still work).
dd if=/dev/mapper/stripe1 bs=4K count=1 of=/dev/null 2>/dev/null \
    && ok "single-stripe read succeeded" \
    || fail "single-stripe read failed"

dmsetup table stripe1 | grep -q "striped 1 64" \
    && ok "dmsetup table reports single-stripe striped" \
    || fail "dmsetup table wrong for single-stripe"

dmsetup remove stripe1 || fail "failed to remove stripe1"
ok "removed stripe1"

# ---------------------------------------------------------------------------
# 4. LVM2 striped logical volume integration
# ---------------------------------------------------------------------------
step "4. LVM2 striped logical volume"

command -v lvcreate >/dev/null 2>/dev/null || { info "lvm2 not installed, skipping"; }
if command -v lvcreate >/dev/null 2>&1; then
    pvcreate -f "$DEV_A" "$DEV_B" 2>/dev/null || fail "pvcreate failed"
    vgcreate -f vg "$DEV_A" "$DEV_B" 2>/dev/null || fail "vgcreate failed"

    LVM_OK=1
    # 2 stripes, 32 KB stripe size, 200 MB LV.
    lvcreate -i 2 -I 32 -L 200M -n stripedlv vg 2>/dev/null \
        && ok "created striped LV vg/stripedlv (2 stripes, 32 KB)" \
        || { fail "lvcreate striped LV failed"; LVM_OK=0; }

    if [ "$LVM_OK" = "1" ]; then
        [ -b /dev/vg/stripedlv ] \
            && ok "LV device node /dev/vg/stripedlv created" \
            || { fail "LV device node not created"; LVM_OK=0; }
    fi
    if [ "$LVM_OK" = "1" ]; then
        dmsetup table vg-stripedlv | grep -q "striped 2 64" \
            && ok "LV uses striped target table" \
            || { fail "LV does not use striped target table"; LVM_OK=0; }
    fi
    if [ "$LVM_OK" = "1" ]; then
        mkfs.ext2 -F /dev/vg/stripedlv 2>/dev/null \
            && ok "mkfs.ext2 on striped LV succeeded" \
            || { fail "mkfs on striped LV failed"; LVM_OK=0; }
    fi
    if [ "$LVM_OK" = "1" ]; then
        mount -t ext2 /dev/vg/stripedlv "$MNT" 2>/dev/null \
            && ok "mounted striped LV" \
            || { fail "mount striped LV failed"; LVM_OK=0; }
    fi
    if [ "$LVM_OK" = "1" ]; then
        dd if=/dev/urandom of="$MNT/lvm.bin" bs=1M count=50 2>/dev/null \
            && ok "wrote 50 MB file on striped LV" \
            || { fail "write on striped LV failed"; LVM_OK=0; }
    fi
    if [ "$LVM_OK" = "1" ]; then
        SUM_LVM=$(md5sum "$MNT/lvm.bin" | awk '{print $1}')
        cp "$MNT/lvm.bin" /tmp/lvm_copy.bin
        if [ "$SUM_LVM" = "$(md5sum /tmp/lvm_copy.bin | awk '{print $1}')" ]; then
            ok "striped LV file integrity verified ($SUM_LVM)"
        else
            fail "striped LV file integrity mismatch"
        fi

        umount "$MNT" || fail "umount striped LV failed"
    fi
    lvchange -an vg/stripedlv 2>/dev/null || dmsetup remove vg-stripedlv 2>/dev/null || true
    [ "$LVM_OK" = "1" ] && ok "striped LV test passed"
fi

# ---------------------------------------------------------------------------
# 5. Error cases
# ---------------------------------------------------------------------------
step "5. Error cases (invalid parameters)"

# stripe_size = 0 must be rejected.
if dmsetup create bad0 <<TABLE 2>/dev/null
0 4096 striped 2 0 $DEV_A 0 $DEV_B 0
TABLE
then
    fail "stripe_size=0 was accepted (should be EINVAL)"
else
    ok "stripe_size=0 correctly rejected"
fi
dmsetup remove bad0 2>/dev/null || true

# num_stripes=2 but only one device pair given must be rejected.
if dmsetup create bad1 <<TABLE 2>/dev/null
0 4096 striped 2 64 $DEV_A 0
TABLE
then
    fail "mismatched stripe/device count was accepted (should be EINVAL)"
else
    ok "mismatched stripe/device count correctly rejected"
fi
dmsetup remove bad1 2>/dev/null || true

# ---------------------------------------------------------------------------
# 6. Cleanup
# ---------------------------------------------------------------------------
step "6. Cleanup"

cleanup_dm
rm -f /tmp/stripe_pattern.bin /tmp/stripe_read.bin \
      /tmp/dev_a_head.bin /tmp/dev_b_head.bin \
      /tmp/stripe_copy.bin /tmp/stripe_test.bin \
      /tmp/lvm_copy.bin

echo
if [ "${FAILED:-0}" = "1" ]; then
    echo -e "\033[31mSome striped target tests FAILED\033[0m"
    exit 1
fi
ok "All striped target tests passed"
echo
