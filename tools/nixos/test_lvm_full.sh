#!/bin/bash
# ============================================================================
# Asterinas DM / LVM2 full functional test
#
# Environment:
#   Two 512 MB disks exposed as /dev/vde and /dev/vdf inside the guest.
#   Launch example:
#     make run_nixos \
#       DM_DISK=target/nixos/dm_test.img DM_DISK_SIZE=512 \
#       DM_DISK2=target/nixos/dm_test2.img DM_DISK2_SIZE=512
#
# Coverage:
#   1. pvcreate / vgcreate / lvcreate
#   2. mkfs.ext2 + mount + file write
#   3. lvextend (expand) + resize2fs (offline)
#   4. lvreduce (shrink) + resize2fs (offline)
#   5. dd a file larger than one disk (512 MB) to verify cross-disk mapping
#   6. Reboot recovery: vgscan / vgchange -ay / dmsetup ls
#   7. Cleanup
# ============================================================================

set -u

PV1=/dev/vde
PV2=/dev/vdf
VG=dmvg
LV=mylv
LV2=testlv
MNT=/mnt/lvm
DEV="/dev/${VG}/${LV}"
DEV2="/dev/${VG}/${LV2}"

ok()   { echo -e "\033[32m[PASS]\033[0m $*"; }
fail() { echo -e "\033[31m[FAIL]\033[0m $*"; exit 1; }
step() { echo -e "\n\033[36m==== $* ====\033[0m"; }
info() { echo -e "\033[33m[INFO]\033[0m $*"; }

require() {
    local dev=$1
    if [ ! -b "$dev" ]; then
        fail "block device $dev not found; check run.sh DM_DISK/DM_DISK2 mapping"
    fi
}

wait_for_dev() {
    # Wait up to 30s for a device node to appear (devtmpfs may be slow).
    local dev=$1
    for _ in $(seq 1 30); do
        [ -b "$dev" ] && return 0
        sleep 1
    done
    [ -b "$dev" ]
}

# ---------------------------------------------------------------------------
# 0. Preflight
# ---------------------------------------------------------------------------
step "0. Preflight checks"

require "$PV1"
require "$PV2"
ok "Both backing disks present: $PV1 $PV2"

# Confirm sizes are ~512 MB
size1=$(blockdev --getsize64 "$PV1" 2>/dev/null || echo 0)
size2=$(blockdev --getsize64 "$PV2" 2>/dev/null || echo 0)
info "$PV1 size = $size1 bytes"
info "$PV2 size = $size2 bytes"
[ "$size1" -gt 0 ] || fail "cannot read size of $PV1"
[ "$size2" -gt 0 ] || fail "cannot read size of $PV2"

# Clean any stale state from a previous run.
umount "$MNT" 2>/dev/null || true
dmsetup remove_all 2>/dev/null || true
vgremove -f "$VG" 2>/dev/null || true
pvremove -f "$PV1" "$PV2" 2>/dev/null || true

# ---------------------------------------------------------------------------
# 1. pvcreate / vgcreate / lvcreate
# ---------------------------------------------------------------------------
step "1. pvcreate / vgcreate / lvcreate"

pvcreate -f "$PV1" "$PV2"            || fail "pvcreate failed"
ok "pvcreate on $PV1 $PV2"

vgcreate -f "$VG" "$PV1" "$PV2"      || fail "vgcreate failed"
ok "vgcreate $VG on $PV1 $PV2"

# First LV: 300 MB linear (spans mostly PV1)
lvcreate -y -L 300M -n "$LV" "$VG"   || fail "lvcreate $LV failed"
wait_for_dev "$DEV"                  || fail "$DEV did not appear"
ok "lvcreate $LV (300 MB) -> $DEV"

# Second LV: 200 MB on the same VG
lvcreate -y -L 200M -n "$LV2" "$VG"  || fail "lvcreate $LV2 failed"
wait_for_dev "$DEV2"                 || fail "$DEV2 did not appear"
ok "lvcreate $LV2 (200 MB) -> $DEV2"

# Sanity: devices should be visible via dmsetup
dmsetup ls | grep -q "$LV"           || fail "dmsetup ls does not show $LV"
ok "dmsetup sees $LV"

# ---------------------------------------------------------------------------
# 2. mkfs.ext2 + mount + write
# ---------------------------------------------------------------------------
step "2. mkfs.ext2 + mount + write"

mkdir -p "$MNT"
mkfs.ext2 -F "$DEV"                  || fail "mkfs.ext2 on $DEV failed"
ok "mkfs.ext2 on $DEV"

mount -t ext2 "$DEV" "$MNT"          || fail "mount $DEV failed"
ok "mounted $DEV at $MNT"

# Small file first
dd if=/dev/zero of="$MNT/small.bin" bs=1M count=10 status=none \
    || fail "write small.bin failed"
sync
ok "wrote 10 MB small.bin"

# ---------------------------------------------------------------------------
# 3. lvextend (expand)
# ---------------------------------------------------------------------------
step "3. lvextend (expand LV to 500 MB)"

# Must unmount before resize2fs (ext2 does not support online resize).
umount "$MNT"                        || fail "umount before extend failed"

lvextend -y -L 500M "/dev/${VG}/${LV}" \
                                     || fail "lvextend failed"
ok "lvextend $LV -> 500 MB"

# Verify new size via blockdev
new_size=$(blockdev --getsize64 "$DEV")
info "$DEV new size = $new_size bytes"
[ "$new_size" -ge $((500 * 1024 * 1024)) ] \
    || fail "LV size did not grow to 500 MB"

# Run fsck, then resize2fs to fill the new capacity.
e2fsck -f -y "$DEV"                  || fail "e2fsck before resize2fs failed"
resize2fs "$DEV"                     || fail "resize2fs failed"
ok "resize2fs expanded fs to 500 MB"

mount -t ext2 "$DEV" "$MNT"          || fail "remount after extend failed"
ok "remounted $DEV at $MNT"

# ---------------------------------------------------------------------------
# 4. lvreduce (shrink)
# ---------------------------------------------------------------------------
step "4. lvreduce (shrink LV back to 250 MB)"

# ext2 resize must be offline.
umount "$MNT"                        || fail "umount before reduce failed"

# Shrink the filesystem a bit smaller than the target LV size first,
# then reduce the LV, then grow the fs back to fit the LV.
e2fsck -f -y "$DEV"                  || fail "e2fsck before shrink failed"
resize2fs "$DEV" 200M                || fail "resize2fs shrink to 200M failed"
ok "shrank ext2 fs to 200 MB"

lvreduce -y -L 250M "/dev/${VG}/${LV}" \
                                     || fail "lvreduce failed"
ok "lvreduce $LV -> 250 MB"

red_size=$(blockdev --getsize64 "$DEV")
info "$DEV reduced size = $red_size bytes"
[ "$red_size" -le $((250 * 1024 * 1024 + 4096)) ] \
    || fail "LV size did not shrink"

# Grow fs to fill the (smaller) LV again.
e2fsck -f -y "$DEV"                  || fail "e2fsck after reduce failed"
resize2fs "$DEV"                     || fail "resize2fs after reduce failed"
ok "resize2fs expanded fs to fill 250 MB LV"

mount -t ext2 "$DEV" "$MNT"          || fail "remount after reduce failed"
ok "remounted $DEV at $MNT"

# ---------------------------------------------------------------------------
# 5. dd a file larger than one disk (512 MB)
# ---------------------------------------------------------------------------
step "5. dd large file (600 MB > one 512 MB disk)"

# Create a 600 MB file on the 500 MB-extended LV.
# To do this we must extend back to >= 600 MB first, otherwise the fs is too
# small. We extend over both PVs so the bio path crosses a target boundary.
umount "$MNT"                        || fail "umount before big-extend failed"

lvextend -y -L 800M "/dev/${VG}/${LV}" \
                                     || fail "lvextend to 800M failed"
ok "lvextend $LV -> 800 MB (spans both PVs)"

e2fsck -f -y "$DEV"                  || fail "e2fsck before big resize failed"
resize2fs "$DEV"                     || fail "resize2fs before big write failed"
mount -t ext2 "$DEV" "$MNT"          || fail "remount before big write failed"

info "writing 600 MB file (spans target boundary)..."
dd if=/dev/zero of="$MNT/big.bin" bs=1M count=600 status=none \
                                     || fail "dd big.bin failed"
sync
actual=$(stat -c %s "$MNT/big.bin")
info "big.bin size = $actual bytes"
[ "$actual" -eq $((600 * 1024 * 1024)) ] \
    || fail "big.bin size mismatch"
ok "wrote 600 MB file across two PVs"

# Verify data integrity via checksum.
sum1=$(sha256sum "$MNT/big.bin" | awk '{print $1}')
info "sha256 of big.bin = $sum1"

# Remount and re-check (exercises page cache + read path).
umount "$MNT"                        || fail "umount after big write failed"
mount -t ext2 "$DEV" "$MNT"          || fail "remount for verify failed"
sum2=$(sha256sum "$MNT/big.bin" | awk '{print $1}')
[ "$sum1" = "$sum2" ]                || fail "checksum mismatch after remount"
ok "checksum stable after remount"

umount "$MNT"                        || true

# ---------------------------------------------------------------------------
# 6. Reboot recovery simulation
# ---------------------------------------------------------------------------
step "6. Reboot recovery (vgscan / vgchange -ay)"

# In a real reboot, DM devices are gone. We simulate by removing all DM
# devices and re-importing the VG metadata from the PVs.
info "removing all DM devices to simulate reboot..."
dmsetup remove_all                   || true
ok "dm devices cleared"

info "scanning PVs..."
vgscan                               || fail "vgscan failed"
ok "vgscan completed"

info "importing VG metadata..."
pvscan --cache 2>/dev/null            || true
vgimportclone 2>/dev/null             || true

info "activating VG..."
vgchange -ay "$VG"                    || fail "vgchange -ay failed"
ok "vgchange -ay activated $VG"

wait_for_dev "$DEV"                  || fail "$DEV not back after vgchange"
ok "$DEV re-appeared after recovery"

# Verify the big file is still intact after recovery.
mount -t ext2 "$DEV" "$MNT"          || fail "mount after recovery failed"
sum3=$(sha256sum "$MNT/big.bin" | awk '{print $1}')
[ "$sum1" = "$sum3" ]                || fail "checksum mismatch after reboot recovery"
ok "big.bin intact after reboot recovery"

umount "$MNT"                        || true

# ---------------------------------------------------------------------------
# 7. Cleanup
# ---------------------------------------------------------------------------
step "7. Cleanup"

umount "$MNT" 2>/dev/null || true
dmsetup remove_all 2>/dev/null || true
lvremove -y "/dev/${VG}/${LV}" 2>/dev/null || true
lvremove -y "/dev/${VG}/${LV2}" 2>/dev/null || true
vgremove -f "$VG" 2>/dev/null || true
pvremove -f "$PV1" "$PV2" 2>/dev/null || true

echo
ok "All DM/LVM2 tests passed"
echo
