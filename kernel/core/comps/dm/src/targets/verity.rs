// SPDX-License-Identifier: MPL-2.0

//! The `verity` target.
//!
//! A read-only target that verifies the integrity of a data device against a
//! precomputed Merkle hash tree stored on a separate hash device. Each data
//! block read is hashed and checked, level by level, up to a trusted root
//! digest supplied out of band. Any mismatch fails the read with an I/O error,
//! so tampering with either the data or the hash device is detected before
//! corrupted data is ever returned.
//!
//! Reference: Linux `Documentation/admin-guide/device-mapper/verity.rst`
//! (<https://docs.kernel.org/admin-guide/device-mapper/verity.html>).

use alloc::{boxed::Box, format, string::String, sync::Arc, vec, vec::Vec};
use core::fmt;

use aster_block::{
    BLOCK_SIZE, BlockDevice, BlockDeviceLease, BlockDeviceMeta, SECTOR_SIZE,
    bio::{BioEnqueueError, BioStatus, BioType, SubmittedBio},
};
use device_id::DeviceId;
use ostd::{mm::VmIo, sync::Mutex};

use crate::{
    DmError,
    hash::{self, HashAlgorithm},
    lookup_block_device,
    target::Target,
};

/// The number of mandatory arguments in a `verity` table line.
const NR_TABLE_ARGS: usize = 10;

/// One level of the hash tree, in the order it is stored on the hash device.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HashLevel {
    /// The first hash block of the level, relative to the start of the hash device.
    pub first_block: u64,
    /// The number of hash blocks in the level.
    pub nr_blocks: u64,
}

/// Computes the hash-tree levels for a data device of `num_data_blocks` blocks.
///
/// Levels are returned top level first (the single root block), matching the
/// on-device layout: the data block digests fill the last (leaf) level, every
/// hash block holds up to `hashes_per_block` digests of the level below, and
/// the levels stack contiguously starting at `hash_start_block`.
pub fn build_hash_levels(
    num_data_blocks: u64,
    hashes_per_block: u64,
    hash_start_block: u64,
) -> Option<Vec<HashLevel>> {
    if num_data_blocks == 0 || hashes_per_block == 0 {
        return None;
    }

    // Block counts from the leaf level up to (but not including) the root.
    let mut counts_leaf_to_root = Vec::new();
    let mut count = num_data_blocks.div_ceil(hashes_per_block);
    while count > 1 {
        counts_leaf_to_root.push(count);
        count = count.div_ceil(hashes_per_block);
    }
    counts_leaf_to_root.push(1);

    let mut levels = Vec::with_capacity(counts_leaf_to_root.len());
    let mut first_block = hash_start_block;
    for &nr_blocks in counts_leaf_to_root.iter().rev() {
        levels.push(HashLevel {
            first_block,
            nr_blocks,
        });
        first_block = first_block.checked_add(nr_blocks)?;
    }
    Some(levels)
}

/// A read-only `verity` target.
pub struct VerityTarget {
    data_device: Arc<dyn BlockDevice>,
    hash_device: Arc<dyn BlockDevice>,
    /// Leases preventing the underlying data device from being unregistered.
    /// `None` only when the device is not in the block registry (e.g., tests).
    _data_lease: Option<BlockDeviceLease>,
    /// Leases preventing the underlying hash device from being unregistered.
    /// `None` only when the device is not in the block registry (e.g., tests).
    _hash_lease: Option<BlockDeviceLease>,
    num_data_blocks: u64,
    size_sectors: u64,
    version: u8,
    salt: Vec<u8>,
    algorithm: Arc<dyn HashAlgorithm>,
    root_digest: Vec<u8>,
    /// Hash-tree levels, top level first.
    levels: Vec<HashLevel>,
    /// Pre-formatted parameters string for `DM_TABLE_STATUS`.
    params: String,
    /// Pre-collected device IDs for `DM_TABLE_DEPS`.
    deps: [DeviceId; 2],
    /// Size of a data block in bytes.
    data_block_size: usize,
    /// Size of a hash block in bytes.
    hash_block_size: usize,
    /// Reusable buffer for reading data blocks. Stored in the target instead of
    /// the stack to avoid large stack frames on the I/O path. Sized at
    /// construction to match the parsed data block size.
    data_block: Mutex<Box<[u8]>>,
    /// Reusable scratch buffer for reading hash blocks. Sized at construction
    /// to match the parsed hash block size.
    hash_scratch: Mutex<Box<[u8]>>,
    /// Reusable digest output buffer. Sized at construction to match the
    /// algorithm digest size so that `hash_block` performs no heap allocation.
    digest_buf: Mutex<Box<[u8]>>,
}

/// Registers the `verity` target type and its version.
pub fn register() {
    crate::register_target_type("verity", [1, 0, 0]);
}

impl VerityTarget {
    /// Creates a new verity target from parsed table arguments.
    ///
    /// The accepted form mirrors the Linux dm-verity table:
    /// ```text
    /// <version> <data_dev> <hash_dev> <data_block_size> <hash_block_size>
    /// <num_data_blocks> <hash_start_block> <algorithm> <root_digest> <salt> [0]
    /// ```
    pub fn from_table_args(args: &[&str]) -> Result<Self, DmError> {
        if args.len() != NR_TABLE_ARGS && args.len() != NR_TABLE_ARGS + 1 {
            return Err(DmError::InvalidParameters(
                "verity target expects exactly 10 arguments",
            ));
        }
        if args.len() == NR_TABLE_ARGS + 1 && args[NR_TABLE_ARGS] != "0" {
            return Err(DmError::InvalidParameters(
                "verity target optional parameters are not supported",
            ));
        }

        let version: u8 = args[0]
            .parse()
            .map_err(|_| DmError::InvalidParameters("verity version must be 0 or 1"))?;
        if version > 1 {
            return Err(DmError::InvalidParameters("verity version must be 0 or 1"));
        }

        let data_device = lookup_block_device(args[1])?;
        let hash_device = lookup_block_device(args[2])?;

        let data_block_size = parse_block_size(args[3])
            .map_err(|_| DmError::InvalidParameters("verity data block size is invalid"))?;
        let hash_block_size = parse_block_size(args[4])
            .map_err(|_| DmError::InvalidParameters("verity hash block size is invalid"))?;

        let num_data_blocks: u64 = args[5]
            .parse()
            .map_err(|_| DmError::InvalidParameters("verity data block count is invalid"))?;
        if num_data_blocks == 0 {
            return Err(DmError::InvalidParameters(
                "verity data block count must be nonzero",
            ));
        }
        let hash_start_block: u64 = args[6]
            .parse()
            .map_err(|_| DmError::InvalidParameters("verity hash start block is invalid"))?;

        let algorithm = hash::lookup_algorithm(args[7]).ok_or(DmError::InvalidParameters(
            "verity algorithm is not supported",
        ))?;
        let digest_size = algorithm.digest_size();

        if hash_block_size < digest_size {
            return Err(DmError::InvalidParameters(
                "verity hash block size must be at least the digest size",
            ));
        }

        let root_digest = parse_hex_bytes(args[8]).ok_or(DmError::InvalidParameters(
            "verity root digest is not valid hex",
        ))?;
        if root_digest.len() != digest_size {
            return Err(DmError::InvalidParameters(
                "verity root digest length does not match the selected algorithm",
            ));
        }

        let salt = parse_hex_bytes(args[9])
            .ok_or(DmError::InvalidParameters("verity salt is not valid hex"))?;

        let hashes_per_block = (hash_block_size / digest_size) as u64;
        let levels = build_hash_levels(num_data_blocks, hashes_per_block, hash_start_block).ok_or(
            DmError::InvalidParameters("verity hash tree geometry is invalid"),
        )?;
        let data_sectors_per_block = (data_block_size / SECTOR_SIZE) as u64;
        let size_sectors = num_data_blocks.checked_mul(data_sectors_per_block).ok_or(
            DmError::InvalidParameters("verity data device is too large"),
        )?;
        if size_sectors > data_device.metadata().nr_sectors as u64 {
            return Err(DmError::InvalidParameters(
                "verity data device is smaller than the table geometry",
            ));
        }
        let hash_sectors_per_block = (hash_block_size / SECTOR_SIZE) as u64;
        let hash_end_block = levels
            .last()
            .and_then(|level| level.first_block.checked_add(level.nr_blocks))
            .ok_or(DmError::InvalidParameters("verity hash tree is too large"))?;
        let hash_end_sector = hash_end_block.checked_mul(hash_sectors_per_block).ok_or(
            DmError::InvalidParameters("verity hash device is too large"),
        )?;
        if hash_end_sector > hash_device.metadata().nr_sectors as u64 {
            return Err(DmError::InvalidParameters(
                "verity hash device is smaller than the table geometry",
            ));
        }

        let data_lease = aster_block::lookup_lease(data_device.id());
        let hash_lease = aster_block::lookup_lease(hash_device.id());
        let data_device = data_lease
            .as_ref()
            .map(|l| l.device().clone())
            .unwrap_or(data_device);
        let hash_device = hash_lease
            .as_ref()
            .map(|l| l.device().clone())
            .unwrap_or(hash_device);
        let data_id = data_device.id();
        let hash_id = hash_device.id();
        let hash_start_block = levels.first().map(|l| l.first_block).unwrap_or(0);
        let params = Self::format_params(
            data_id,
            hash_id,
            data_block_size,
            hash_block_size,
            num_data_blocks,
            hash_start_block,
            version,
            algorithm.name(),
            &root_digest,
            &salt,
        );

        Ok(Self {
            data_device,
            hash_device,
            _data_lease: data_lease,
            _hash_lease: hash_lease,
            num_data_blocks,
            size_sectors,
            version,
            salt,
            algorithm,
            root_digest,
            levels,
            params,
            deps: [data_id, hash_id],
            data_block_size,
            hash_block_size,
            data_block: Mutex::new(vec![0u8; data_block_size].into_boxed_slice()),
            hash_scratch: Mutex::new(vec![0u8; hash_block_size].into_boxed_slice()),
            digest_buf: Mutex::new(vec![0u8; digest_size].into_boxed_slice()),
        })
    }

    /// Formats the dm-verity parameter string for `DM_TABLE_STATUS`.
    #[expect(clippy::too_many_arguments)]
    fn format_params(
        data_id: DeviceId,
        hash_id: DeviceId,
        data_block_size: usize,
        hash_block_size: usize,
        num_data_blocks: u64,
        hash_start_block: u64,
        version: u8,
        algorithm_name: &str,
        root_digest: &[u8],
        salt: &[u8],
    ) -> String {
        let salt_hex: String = salt.iter().map(|b| format!("{:02x}", b)).collect();
        let root_hex: String = root_digest.iter().map(|b| format!("{:02x}", b)).collect();
        format!(
            "{} {}:{} {}:{} {} {} {} {} {} {} {}",
            version,
            data_id.major().get(),
            data_id.minor().get(),
            hash_id.major().get(),
            hash_id.minor().get(),
            data_block_size,
            hash_block_size,
            num_data_blocks,
            hash_start_block,
            algorithm_name,
            root_hex,
            if salt_hex.is_empty() { "-" } else { &salt_hex },
        )
    }

    /// Builds a `VerityTarget` directly from already-resolved devices, without
    /// going through the textual table parser.
    ///
    /// This is intended for unit tests that construct the target in-memory
    /// against mock block devices. The hash-tree levels are computed from
    /// `num_data_blocks` using the standard 4096-byte block / 32-byte digest
    /// geometry, with the hash region starting at block 0 of `hash_device`.
    #[doc(hidden)]
    pub fn new_direct(
        data_device: Arc<dyn BlockDevice>,
        hash_device: Arc<dyn BlockDevice>,
        num_data_blocks: u64,
        version: u8,
        salt: Vec<u8>,
        root_digest: [u8; 32],
    ) -> Self {
        Self::new_direct_with_algorithm(
            data_device,
            hash_device,
            num_data_blocks,
            version,
            salt,
            Arc::new(hash::Sha256),
            root_digest.to_vec(),
        )
    }

    /// Builds a `VerityTarget` directly with an explicit hash algorithm.
    #[doc(hidden)]
    pub fn new_direct_with_algorithm(
        data_device: Arc<dyn BlockDevice>,
        hash_device: Arc<dyn BlockDevice>,
        num_data_blocks: u64,
        version: u8,
        salt: Vec<u8>,
        algorithm: Arc<dyn HashAlgorithm>,
        root_digest: Vec<u8>,
    ) -> Self {
        // Existing test fixtures are built around 4096-byte blocks; keep that
        // default until callers migrate to explicit sizes.
        Self::new_direct_with_algorithm_and_sizes(
            data_device,
            hash_device,
            num_data_blocks,
            version,
            salt,
            algorithm,
            root_digest,
            BLOCK_SIZE,
            BLOCK_SIZE,
        )
    }

    /// Builds a `VerityTarget` directly with an explicit hash algorithm and
    /// block sizes. This is intended for tests that need non-default geometry.
    #[doc(hidden)]
    #[expect(clippy::too_many_arguments)]
    pub fn new_direct_with_algorithm_and_sizes(
        data_device: Arc<dyn BlockDevice>,
        hash_device: Arc<dyn BlockDevice>,
        num_data_blocks: u64,
        version: u8,
        salt: Vec<u8>,
        algorithm: Arc<dyn HashAlgorithm>,
        root_digest: Vec<u8>,
        data_block_size: usize,
        hash_block_size: usize,
    ) -> Self {
        assert!(
            data_block_size.is_power_of_two() && data_block_size >= SECTOR_SIZE,
            "verity test fixture: data block size must be a power of two and at least one sector"
        );
        assert!(
            hash_block_size.is_power_of_two() && hash_block_size >= algorithm.digest_size(),
            "verity test fixture: hash block size must be a power of two and at least the digest size"
        );
        let digest_size = algorithm.digest_size();
        let hashes_per_block = (hash_block_size / digest_size) as u64;
        // Hash tree starts at block 0 of the hash device in the test fixture.
        let levels = build_hash_levels(num_data_blocks, hashes_per_block, 0)
            .expect("verity test fixture: invalid hash tree geometry");
        let sectors_per_data_block = (data_block_size / SECTOR_SIZE) as u64;
        let size_sectors = num_data_blocks * sectors_per_data_block;
        let data_lease = aster_block::lookup_lease(data_device.id());
        let hash_lease = aster_block::lookup_lease(hash_device.id());
        let data_device = data_lease
            .as_ref()
            .map(|l| l.device().clone())
            .unwrap_or(data_device);
        let hash_device = hash_lease
            .as_ref()
            .map(|l| l.device().clone())
            .unwrap_or(hash_device);
        let data_id = data_device.id();
        let hash_id = hash_device.id();
        let hash_start_block = levels.first().map(|l| l.first_block).unwrap_or(0);
        let params = Self::format_params(
            data_id,
            hash_id,
            data_block_size,
            hash_block_size,
            num_data_blocks,
            hash_start_block,
            version,
            algorithm.name(),
            &root_digest,
            &salt,
        );
        Self {
            data_device,
            hash_device,
            _data_lease: data_lease,
            _hash_lease: hash_lease,
            num_data_blocks,
            size_sectors,
            version,
            salt,
            algorithm,
            root_digest,
            levels,
            params,
            deps: [data_id, hash_id],
            data_block_size,
            hash_block_size,
            data_block: Mutex::new(vec![0u8; data_block_size].into_boxed_slice()),
            hash_scratch: Mutex::new(vec![0u8; hash_block_size].into_boxed_slice()),
            digest_buf: Mutex::new(vec![0u8; digest_size].into_boxed_slice()),
        }
    }

    /// Hashes `block` into `out`, ordered per the dm-verity version.
    ///
    /// `out` must be exactly `algorithm.digest_size()` bytes long.
    fn hash_block_into(&self, block: &[u8], out: &mut [u8]) {
        match self.version {
            0 => self.algorithm.digest(&[block, &self.salt], out),
            _ => self.algorithm.digest(&[&self.salt, block], out),
        }
    }

    /// Verifies `block` (at index `data_block_index`) against the hash tree,
    /// returning whether every level up to the root digest matches.
    fn verify_block(
        &self,
        data_block_index: u64,
        block: &[u8],
        hash_scratch: &mut [u8],
        digest_buf: &mut [u8],
    ) -> bool {
        let digest_size = self.algorithm.digest_size();
        let hashes_per_block = (self.hash_block_size / digest_size) as u64;
        self.hash_block_into(block, digest_buf);
        let mut child_index = data_block_index;

        // Walk from the leaf level up to the root.
        for level in self.levels.iter().rev() {
            let block_in_level = child_index / hashes_per_block;
            let slot = (child_index % hashes_per_block) as usize;
            if block_in_level >= level.nr_blocks {
                return false;
            }

            let Some(hash_block_index) = level.first_block.checked_add(block_in_level) else {
                return false;
            };
            let Some(hash_block_offset) = hash_block_offset(hash_block_index, self.hash_block_size)
            else {
                return false;
            };
            if self
                .hash_device
                .read_bytes(hash_block_offset, hash_scratch)
                .is_err()
            {
                return false;
            }

            let start = slot * digest_size;
            let stored = &hash_scratch[start..start + digest_size];
            if stored != &digest_buf[..digest_size] {
                return false;
            }

            // Hash the whole block so the digest can be checked against the
            // parent level. For the top level this produces the final root
            // digest that is compared with the trusted value below.
            self.hash_block_into(hash_scratch, digest_buf);
            child_index = block_in_level;
        }

        // The hash of the top-level block must equal the trusted root digest.
        digest_buf[..digest_size] == self.root_digest[..digest_size]
    }

    fn handle_read(&self, bio: SubmittedBio, target_start_sector: u64) {
        // The verity unit of trust is a full data block, so verification always
        // operates on whole blocks even when the request is sector-granular.
        let Some(mut device_offset) = sector_offset(target_start_sector) else {
            bio.complete(BioStatus::IoError);
            return;
        };

        // Reusable target-level buffers avoid large stack allocations and
        // per-read heap allocations. The per-block cache avoids re-reading
        // the same block for small bios.
        let mut data_block = self.data_block.lock();
        let mut cached_block: Option<u64> = None;

        for segment in bio.segments() {
            let nbytes = segment.nbytes();
            let mut segment_offset = 0;
            while segment_offset < nbytes {
                let block_index = (device_offset / self.data_block_size) as u64;
                if block_index >= self.num_data_blocks {
                    bio.complete(BioStatus::IoError);
                    return;
                }

                if cached_block != Some(block_index) {
                    let Some(block_offset) = data_block_offset(block_index, self.data_block_size)
                    else {
                        bio.complete(BioStatus::IoError);
                        return;
                    };
                    if self
                        .data_device
                        .read_bytes(block_offset, &mut data_block)
                        .is_err()
                        || !{
                            let mut hash_scratch = self.hash_scratch.lock();
                            let mut digest_buf = self.digest_buf.lock();
                            self.verify_block(
                                block_index,
                                &data_block,
                                &mut hash_scratch,
                                &mut digest_buf,
                            )
                        }
                    {
                        bio.complete(BioStatus::IoError);
                        return;
                    }
                    cached_block = Some(block_index);
                }

                let within_block = device_offset % self.data_block_size;
                let chunk = (self.data_block_size - within_block).min(nbytes - segment_offset);
                if segment
                    .inner_dma_slice()
                    .write_bytes(
                        segment_offset,
                        &(*data_block)[within_block..within_block + chunk],
                    )
                    .is_err()
                {
                    bio.complete(BioStatus::IoError);
                    return;
                }

                segment_offset += chunk;
                let Some(next_device_offset) = device_offset.checked_add(chunk) else {
                    bio.complete(BioStatus::IoError);
                    return;
                };
                device_offset = next_device_offset;
            }
        }

        bio.complete(BioStatus::Complete);
    }
}

impl Target for VerityTarget {
    fn name(&self) -> &str {
        "verity"
    }

    fn map_bio(&self, bio: SubmittedBio, logical_start: u64) -> Result<(), BioEnqueueError> {
        // Convert from the mapped device's absolute sector to the
        // target-local sector offset. Use the offset-adjusted range so that
        // stacked DM devices (which adjust `sid_offset` but leave
        // `sid_range` unchanged) compute the correct local offset, matching
        // the adjusted start the table lookup used.
        let bio_start = bio.current_sid_range().start.to_raw();
        let Some(target_start_sector) = bio_start.checked_sub(logical_start) else {
            return Err(BioEnqueueError::Refused);
        };

        match bio.type_() {
            BioType::Read => self.handle_read(bio, target_start_sector),
            // dm-verity is read-only; writes are rejected as I/O errors.
            BioType::Write => bio.complete(BioStatus::IoError),
            BioType::Flush => bio.complete(BioStatus::Complete),
        }
        Ok(())
    }

    fn metadata(&self) -> BlockDeviceMeta {
        BlockDeviceMeta {
            max_nr_segments_per_bio: self.data_device.metadata().max_nr_segments_per_bio,
            nr_sectors: self.size_sectors as usize,
        }
    }

    fn params(&self) -> &str {
        &self.params
    }

    fn deps(&self) -> &[DeviceId] {
        &self.deps
    }

    fn underlying_devices(&self) -> Vec<Arc<dyn BlockDevice>> {
        // dm-verity is read-only; there is no writable cache to flush.
        Vec::new()
    }
}

impl fmt::Debug for VerityTarget {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("VerityTarget")
            .field("version", &self.version)
            .field("num_data_blocks", &self.num_data_blocks)
            .field("size_sectors", &self.size_sectors)
            .finish_non_exhaustive()
    }
}

/// Parses a block size argument, requiring a positive power of two aligned to
/// at least one sector.
fn parse_block_size(input: &str) -> Result<usize, &'static str> {
    let size: usize = input.parse().map_err(|_| "block size is not a number")?;
    if size == 0 {
        return Err("block size must be non-zero");
    }
    if !size.is_multiple_of(SECTOR_SIZE) {
        return Err("block size must be a multiple of the sector size");
    }
    if !size.is_power_of_two() {
        return Err("block size must be a power of two");
    }
    Ok(size)
}

/// Parses a hex string into bytes. `-` means empty.
fn parse_hex_bytes(input: &str) -> Option<Vec<u8>> {
    if input == "-" {
        return Some(Vec::new());
    }
    if !input.len().is_multiple_of(2) {
        return None;
    }

    let mut out = Vec::with_capacity(input.len() / 2);
    let bytes = input.as_bytes();
    let mut offset = 0;
    while offset < bytes.len() {
        let hi = hex_value(bytes[offset])?;
        let lo = hex_value(bytes[offset + 1])?;
        out.push((hi << 4) | lo);
        offset += 2;
    }
    Some(out)
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn sector_offset(sector: u64) -> Option<usize> {
    usize::try_from(sector).ok()?.checked_mul(SECTOR_SIZE)
}

fn data_block_offset(block: u64, block_size: usize) -> Option<usize> {
    usize::try_from(block).ok()?.checked_mul(block_size)
}

fn hash_block_offset(block: u64, block_size: usize) -> Option<usize> {
    usize::try_from(block).ok()?.checked_mul(block_size)
}
