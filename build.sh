#!/bin/sh
#

make nixos && make run_nixos \
    DM_DISK=target/nixos/dm_test.img DM_DISK_SIZE=512 \
    DM_DISK2=target/nixos/dm_test2.img DM_DISK2_SIZE=512
