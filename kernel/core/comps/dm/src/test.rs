// SPDX-License-Identifier: MPL-2.0

#![cfg(ktest)]

use alloc::{
    format,
    string::{String, ToString},
    sync::Arc,
    vec,
    vec::Vec,
};
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

use aster_block::{
    BLOCK_SIZE, BlockDevice, BlockDeviceMeta, SECTOR_SIZE,
    bio::{Bio, BioDirection, BioEnqueueError, BioSegment, BioStatus, BioType, SubmittedBio},
    id::{Bid, Sid},
};
use device_id::{DeviceId, MajorId, MinorId};
use io_util::batch::IoBatch;
use ostd::{
    mm::{VmIo, VmReader, VmWriter},
    prelude::ktest,
    sync::SpinLock,
    task::{Task, TaskOptions},
};

use crate::{
    DmTable, DmTarget, MappedDevice, TableError,
    targets::{linear::LinearTarget, striped::StripedTarget, verity::VerityTarget},
};

/// A simple in-memory block device for testing.
///
/// Stores data in a `Vec<u8>` and completes bios synchronously in `enqueue`.
struct MockBlockDevice {
    id: DeviceId,
    name: String,
    data: SpinLock<Vec<u8>>,
    nr_sectors: usize,
    /// Number of flush bios received by this device.
    flush_count: AtomicUsize,
}

/// Global counter for generating unique minor IDs for mock block devices.
///
/// Mock devices created during tests must have distinct [`DeviceId`]s so that
/// the device mapper table can de-duplicate underlying devices correctly when
/// building its flush set. Using the same ID for every mock would cause
/// multiple distinct mocks to be collapsed into one.
static MOCK_MINOR_COUNTER: AtomicU32 = AtomicU32::new(1);

impl MockBlockDevice {
    /// Creates a new mock device with the given number of sectors.
    fn new(name: &str, nr_sectors: usize) -> Arc<Self> {
        let minor = MOCK_MINOR_COUNTER.fetch_add(1, Ordering::Relaxed);
        Arc::new(Self {
            id: DeviceId::new(MajorId::new(240), MinorId::new(minor)),
            name: String::from(name),
            data: SpinLock::new(vec![0u8; nr_sectors * SECTOR_SIZE]),
            nr_sectors,
            flush_count: AtomicUsize::new(0),
        })
    }

    /// Returns the number of flush bios received by this device.
    fn flush_count(&self) -> usize {
        self.flush_count.load(Ordering::Acquire)
    }

    /// Reads `len` bytes from the device starting at `offset`.
    fn read_bytes(&self, offset: usize, len: usize) -> Vec<u8> {
        let data = self.data.lock();
        data[offset..offset + len].to_vec()
    }

    /// Returns the physical start sector of a submitted bio.
    ///
    /// Combines the bio's logical sector range with its signed offset to
    /// compute the actual sector on this device.
    fn physical_start_sector(bio: &SubmittedBio) -> u64 {
        let logical_start = bio.sid_range().start.to_raw() as i64;
        let offset = bio.sid_offset();
        (logical_start + offset) as u64
    }
}

impl BlockDevice for MockBlockDevice {
    fn enqueue(&self, bio: SubmittedBio) -> Result<(), BioEnqueueError> {
        match bio.type_() {
            BioType::Read => {
                let start_byte = Self::physical_start_sector(&bio) as usize * SECTOR_SIZE;
                let data = self.data.lock();
                let mut byte_offset = start_byte;
                for segment in bio.segments() {
                    let nbytes = segment.nbytes();
                    let slice = segment.inner_dma_slice();
                    let mut reader =
                        VmReader::from(&data[byte_offset..byte_offset + nbytes]).to_fallible();
                    // Write device data into the bio segment's DMA buffer.
                    slice.write(0, &mut reader).unwrap();
                    byte_offset += nbytes;
                }
            }
            BioType::Write => {
                let start_byte = Self::physical_start_sector(&bio) as usize * SECTOR_SIZE;
                let mut data = self.data.lock();
                let mut byte_offset = start_byte;
                for segment in bio.segments() {
                    let nbytes = segment.nbytes();
                    let slice = segment.inner_dma_slice();
                    let mut writer =
                        VmWriter::from(&mut data[byte_offset..byte_offset + nbytes]).to_fallible();
                    // Read data from the bio segment's DMA buffer into device storage.
                    slice.read(0, &mut writer).unwrap();
                    byte_offset += nbytes;
                }
            }
            BioType::Flush => {
                self.flush_count.fetch_add(1, Ordering::AcqRel);
            }
        }

        bio.complete(BioStatus::Complete);
        Ok(())
    }

    fn metadata(&self) -> BlockDeviceMeta {
        BlockDeviceMeta {
            max_nr_segments_per_bio: usize::MAX,
            nr_sectors: self.nr_sectors,
        }
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn id(&self) -> DeviceId {
        self.id
    }
}

impl core::fmt::Debug for MockBlockDevice {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        f.debug_struct("MockBlockDevice")
            .field("name", &self.name)
            .field("nr_sectors", &self.nr_sectors)
            .finish()
    }
}

/// A block device whose `enqueue` always refuses bios.
///
/// Used to exercise the error path of `MappedDevice::enqueue`, where the
/// in-flight counter incremented before mapping must be decremented again
/// because the dropped bio never fires its completion callback.
struct RefusingBlockDevice {
    id: DeviceId,
    name: String,
    nr_sectors: usize,
}

impl RefusingBlockDevice {
    fn new(name: &str, nr_sectors: usize) -> Arc<Self> {
        let minor = MOCK_MINOR_COUNTER.fetch_add(1, Ordering::Relaxed);
        Arc::new(Self {
            id: DeviceId::new(MajorId::new(240), MinorId::new(minor)),
            name: String::from(name),
            nr_sectors,
        })
    }
}

impl BlockDevice for RefusingBlockDevice {
    fn enqueue(&self, _bio: SubmittedBio) -> Result<(), BioEnqueueError> {
        Err(BioEnqueueError::Refused)
    }

    fn metadata(&self) -> BlockDeviceMeta {
        BlockDeviceMeta {
            max_nr_segments_per_bio: usize::MAX,
            nr_sectors: self.nr_sectors,
        }
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn id(&self) -> DeviceId {
        self.id
    }
}

impl core::fmt::Debug for RefusingBlockDevice {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        f.debug_struct("RefusingBlockDevice")
            .field("name", &self.name)
            .field("nr_sectors", &self.nr_sectors)
            .finish()
    }
}

/// Writes `buf` to `device` at the given block offset.
fn write_blocks(device: &dyn BlockDevice, bid: Bid, buf: &[u8]) {
    let nblocks = buf.len().div_ceil(BLOCK_SIZE);
    let segment = BioSegment::alloc(nblocks, BioDirection::ToDevice);
    segment
        .write(0, &mut VmReader::from(buf).to_fallible())
        .unwrap();
    let bio = Bio::new(BioType::Write, Sid::from(bid), vec![segment], None);
    let status = bio.submit_and_wait(device).unwrap();
    assert_eq!(status, BioStatus::Complete);
}

/// Reads `len` bytes from `device` at the given block offset.
fn read_blocks(device: &dyn BlockDevice, bid: Bid, len: usize) -> Vec<u8> {
    let nblocks = len.div_ceil(BLOCK_SIZE);
    let segment = BioSegment::alloc(nblocks, BioDirection::FromDevice);
    let bio = Bio::new(BioType::Read, Sid::from(bid), vec![segment.clone()], None);
    let status = bio.submit_and_wait(device).unwrap();
    let mut buf = vec![0u8; len];
    match status {
        BioStatus::Complete => {
            segment
                .read(0, &mut VmWriter::from(&mut buf[..]).to_fallible())
                .unwrap();
        }
        // Zero targets report that the region is all zeros; the allocated
        // buffer is already zero-filled, so just return it.
        BioStatus::Zeros => {}
        _ => panic!("read_blocks: unexpected bio status {:?}", status),
    }
    buf
}

/// Ensures the device mapper subsystem is initialized (major allocated).
fn ensure_initialized() {
    use component::{InitStage, init_all, parse_metadata};
    // This is idempotent - the `Once` for DM_MAJOR prevents double allocation.
    let _ = init_all(InitStage::Bootstrap, parse_metadata!());
}

/// Test: single-target linear mapping writes and reads through the mapped device.
#[ktest]
fn linear_single_target() {
    ensure_initialized();

    let mock = MockBlockDevice::new("mock-a", 256);
    let mut table = DmTable::new();
    table
        .add_target(0, 256, DmTarget::Linear(LinearTarget::new(mock.clone(), 0)))
        .unwrap();
    let mapped = MappedDevice::create("dm-test-0", table).unwrap();

    // Write a pattern to the mapped device.
    let pattern: Vec<u8> = (0..BLOCK_SIZE).map(|i| (i % 251) as u8).collect();
    write_blocks(mapped.as_ref(), Bid::new(0), &pattern);

    // Read back through the mapped device.
    let read_buf = read_blocks(mapped.as_ref(), Bid::new(0), BLOCK_SIZE);
    assert_eq!(read_buf, pattern);

    // Verify the data landed on the underlying mock at sector 0.
    let direct = mock.read_bytes(0, BLOCK_SIZE);
    assert_eq!(direct, pattern);
}

/// Test: linear mapping with a non-zero physical start on the underlying device.
#[ktest]
fn linear_with_offset() {
    ensure_initialized();

    let mock = MockBlockDevice::new("mock-b", 512);
    let mut table = DmTable::new();
    // Map logical sectors [0, 256) to physical sectors [256, 512) on the mock.
    table
        .add_target(
            0,
            256,
            DmTarget::Linear(LinearTarget::new(mock.clone(), 256)),
        )
        .unwrap();
    let mapped = MappedDevice::create("dm-test-1", table).unwrap();

    let pattern: Vec<u8> = (0..BLOCK_SIZE).map(|i| (i % 253) as u8).collect();
    write_blocks(mapped.as_ref(), Bid::new(0), &pattern);

    // The data should be at physical sector 256 on the mock, not sector 0.
    let at_0 = mock.read_bytes(0, BLOCK_SIZE);
    assert!(at_0.iter().all(|&b| b == 0), "sector 0 should be empty");

    let at_256 = mock.read_bytes(256 * SECTOR_SIZE, BLOCK_SIZE);
    assert_eq!(at_256, pattern);

    // Read back through the mapped device.
    let read_buf = read_blocks(mapped.as_ref(), Bid::new(0), BLOCK_SIZE);
    assert_eq!(read_buf, pattern);
}

/// Test: multi-target linear mapping routes I/O to the correct underlying device.
#[ktest]
fn linear_multi_target() {
    ensure_initialized();

    let mock_a = MockBlockDevice::new("mock-c", 256);
    let mock_b = MockBlockDevice::new("mock-d", 256);
    let mut table = DmTable::new();
    // First half → mock_a at sector 0
    table
        .add_target(
            0,
            256,
            DmTarget::Linear(LinearTarget::new(mock_a.clone(), 0)),
        )
        .unwrap();
    // Second half → mock_b at sector 0 (logical_start = 256)
    table
        .add_target(
            256,
            256,
            DmTarget::Linear(LinearTarget::new(mock_b.clone(), 0)),
        )
        .unwrap();
    let mapped = MappedDevice::create("dm-test-2", table).unwrap();

    // Write to the first target (block 0 = sectors 0..8).
    let pattern_a: Vec<u8> = (0..BLOCK_SIZE).map(|i| (i % 251) as u8).collect();
    write_blocks(mapped.as_ref(), Bid::new(0), &pattern_a);

    // Write to the second target (block 32 = sector 256, since 32*8=256).
    let pattern_b: Vec<u8> = (0..BLOCK_SIZE).map(|i| (i % 249) as u8).collect();
    write_blocks(mapped.as_ref(), Bid::new(32), &pattern_b);

    // Verify data on mock_a.
    let direct_a = mock_a.read_bytes(0, BLOCK_SIZE);
    assert_eq!(direct_a, pattern_a);

    // Verify data on mock_b.
    let direct_b = mock_b.read_bytes(0, BLOCK_SIZE);
    assert_eq!(direct_b, pattern_b);

    // Read back through the mapped device.
    let read_a = read_blocks(mapped.as_ref(), Bid::new(0), BLOCK_SIZE);
    assert_eq!(read_a, pattern_a);
    let read_b = read_blocks(mapped.as_ref(), Bid::new(32), BLOCK_SIZE);
    assert_eq!(read_b, pattern_b);
}

/// Test: a bio that spans two targets is split and forwarded to both.
#[ktest]
fn bio_spanning_targets_is_split() {
    ensure_initialized();

    let mock_a = MockBlockDevice::new("mock-e", 16);
    let mock_b = MockBlockDevice::new("mock-f", 16);
    let mut table = DmTable::new();
    // Target 1: logical [0, 8) sectors → mock_a
    table
        .add_target(0, 8, DmTarget::Linear(LinearTarget::new(mock_a.clone(), 0)))
        .unwrap();
    // Target 2: logical [8, 16) sectors → mock_b
    table
        .add_target(8, 8, DmTarget::Linear(LinearTarget::new(mock_b.clone(), 0)))
        .unwrap();
    let mapped = MappedDevice::create("dm-test-3", table).unwrap();

    // 1 block = 8 sectors (BLOCK_SIZE / SECTOR_SIZE = 4096 / 512 = 8).
    // A 2-block bio starting at block 0 spans sectors 0..16, crossing the
    // boundary at sector 8. The DM table should split it into two children:
    // sectors 0..8 → mock_a, sectors 8..16 → mock_b.
    let pattern_a: Vec<u8> = (0..BLOCK_SIZE).map(|i| (i % 251) as u8).collect();
    let pattern_b: Vec<u8> = (0..BLOCK_SIZE).map(|i| (i % 249) as u8).collect();
    let mut full_pattern = Vec::with_capacity(2 * BLOCK_SIZE);
    full_pattern.extend_from_slice(&pattern_a);
    full_pattern.extend_from_slice(&pattern_b);

    let segment = BioSegment::alloc(2, BioDirection::ToDevice);
    segment
        .write(0, &mut VmReader::from(&full_pattern[..]).to_fallible())
        .unwrap();
    let bio = Bio::new(BioType::Write, Sid::new(0), vec![segment], None);

    let status = bio.submit_and_wait(mapped.as_ref()).unwrap();
    assert_eq!(
        status,
        BioStatus::Complete,
        "split bio should complete successfully"
    );

    // Verify each half landed on the correct underlying device.
    let direct_a = mock_a.read_bytes(0, BLOCK_SIZE);
    assert_eq!(direct_a, pattern_a, "first half should be on mock_a");
    let direct_b = mock_b.read_bytes(0, BLOCK_SIZE);
    assert_eq!(direct_b, pattern_b, "second half should be on mock_b");
}

/// Test: table construction with contiguous entries produces correct metadata.
#[ktest]
fn table_metadata() {
    let mock = MockBlockDevice::new("mock-g", 256);

    let mut table = DmTable::new();
    table
        .add_target(0, 128, DmTarget::Linear(LinearTarget::new(mock.clone(), 0)))
        .unwrap();
    table
        .add_target(128, 128, DmTarget::Linear(LinearTarget::new(mock, 128)))
        .unwrap();
    assert_eq!(table.total_sectors(), 256);
    assert_eq!(table.num_targets(), 2);

    let metadata = table.metadata();
    assert_eq!(metadata.nr_sectors, 256);
}

// ===========================================================================
// SHA-256 Known-Answer Tests (KAT)
//
// Vectors are the FIPS 180-4 / NIST CAVP standard short messages.
// ===========================================================================

/// Helper: hex string -> [u8; 32].
fn hex_to_array_32(s: &str) -> [u8; 32] {
    let mut out = [0u8; 32];
    for i in 0..32 {
        out[i] = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).unwrap();
    }
    out
}

/// Helper: hex string -> [u8; 64].
fn hex_to_array_64(s: &str) -> [u8; 64] {
    let mut out = [0u8; 64];
    for i in 0..64 {
        out[i] = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).unwrap();
    }
    out
}

/// KAT 1: empty string.
///
/// SHA-256("") = e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855
#[ktest]
fn sha256_empty() {
    let digest = crate::sha256::digest(&[b"".as_ref()]);
    let expected =
        hex_to_array_32("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
    assert_eq!(digest, expected, "SHA-256(\"\") mismatch");
}

/// KAT 2: one-block message.
///
/// SHA-256("abc") = ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad
#[ktest]
fn sha256_abc() {
    let digest = crate::sha256::digest(&[b"abc".as_ref()]);
    let expected =
        hex_to_array_32("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    assert_eq!(digest, expected, "SHA-256(\"abc\") mismatch");
}

/// KAT 3: two-block message (448 bits + padding crosses 64-byte boundary).
///
/// Input: 56 bytes of 0x00..0x37 pattern (a-z + digits + some symbols).
/// SHA-256("abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq")
///       = 248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1
#[ktest]
fn sha256_two_block_pattern() {
    let msg = b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq";
    let digest = crate::sha256::digest(&[msg.as_ref()]);
    let expected =
        hex_to_array_32("248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1");
    assert_eq!(digest, expected, "SHA-256 56-byte pattern mismatch");
}

/// KAT 4: chunked input via multiple slices.
///
/// SHA-256 of "a" * 100000 chunked into 1024-byte pieces must equal the
/// single-shot result. This validates that the multi-slice API feeds all
/// data through the same compression pipeline.
#[ktest]
fn sha256_chunked_matches_single() {
    // 100_000 bytes of 'a' would be the standard NIST long-message KAT,
    // but to keep ktest cheap we use 4096 bytes (one block-sized buffer).
    let mut buf = vec![b'a'; 4096];
    let single = crate::sha256::digest(&[buf.as_ref()]);

    // Split into 4 slices of 1024 bytes each.
    let chunks: [&[u8]; 4] = [
        &buf[0..1024],
        &buf[1024..2048],
        &buf[2048..3072],
        &buf[3072..4096],
    ];
    let chunked = crate::sha256::digest(&chunks);
    assert_eq!(single, chunked, "chunked SHA-256 differs from single-shot");

    // Sanity: ensure the same input twice gives the same output.
    let again = crate::sha256::digest(&[buf.as_ref()]);
    assert_eq!(single, again, "SHA-256 is not deterministic");

    // Touch the last byte to make sure the digest actually changes.
    let last = buf[4095];
    buf[4095] = b'b';
    let mutated = crate::sha256::digest(&[buf.as_ref()]);
    assert_ne!(single, mutated, "SHA-256 did not change after mutation");
    buf[4095] = last;
}

/// KAT 5: exactly 55-byte message (padding pushes into a second block).
///
/// This is the boundary case where the 0x80 padding byte plus the 8-byte
/// length field overflow into a second 64-byte block, exercising the
/// "two-block tail" code path in the implementation.
///
/// SHA-256("A" * 55) = 8963cc0afd622cc7574ac2011f93a3059b3d65548a77542a1559e3d202e6ab00
#[ktest]
fn sha256_55_byte_boundary() {
    let msg = [b'A'; 55];
    let digest = crate::sha256::digest(&[msg.as_ref()]);
    let expected =
        hex_to_array_32("8963cc0afd622cc7574ac2011f93a3059b3d65548a77542a1559e3d202e6ab00");
    assert_eq!(digest, expected, "SHA-256(\"A\" * 55) mismatch");

    // Also ensure the same input twice gives the same result.
    let again = crate::sha256::digest(&[msg.as_ref()]);
    assert_eq!(digest, again, "55-byte boundary result is not stable");
}

// ===========================================================================
// SHA-512 Known-Answer Tests (KAT)
//
// Vectors are the FIPS 180-4 / NIST CAVP standard short messages.
// ===========================================================================

/// KAT 1: empty string.
///
/// SHA-512("") = cf83e1357eefb8bdf1542850d66d8007d620e4050b5715dc83f4a921d36ce9ce
///               47d0d13c5d85f2b0ff8318d2877eec2e63b011b8a14634f04e6e558f322e7b3b
#[ktest]
fn sha512_empty() {
    let digest = crate::sha512::digest(&[b"".as_ref()]);
    let expected = hex_to_array_64(
        "cf83e1357eefb8bdf1542850d66d8007d620e4050b5715dc83f4a921d36ce9ce\
         47d0d13c5d85f2b0ff8318d2877eec2f63b931bd47417a81a538327af927da3e",
    );
    assert_eq!(digest, expected, "SHA-512(\"\") mismatch");
}

/// KAT 2: one-block message.
///
/// SHA-512("abc") = ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a
///                  2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f
#[ktest]
fn sha512_abc() {
    let digest = crate::sha512::digest(&[b"abc".as_ref()]);
    let expected = hex_to_array_64(
        "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a\
         2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f",
    );
    assert_eq!(digest, expected, "SHA-512(\"abc\") mismatch");
}

/// KAT 3: two-block message (padding crosses 128-byte boundary).
///
/// SHA-512("abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq")
///       = 204a8fc6dda82f0c0da7f314d60e5d4e1b6b2050a6c88d1227c6274c9727839e
///         2d174c1a6a57f2c1b129e8b535d63c1747162978b53d6c266833a3b5bf0b1c88
#[ktest]
fn sha512_two_block_pattern() {
    let msg = b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq";
    let digest = crate::sha512::digest(&[msg.as_ref()]);
    let expected = hex_to_array_64(
        "204a8fc6dda82f0a0ced7beb8e08a41657c16ef468b228a8279be331a703c335\
         96fd15c13b1b07f9aa1d3bea57789ca031ad85c7a71dd70354ec631238ca3445",
    );
    assert_eq!(digest, expected, "SHA-512 56-byte pattern mismatch");
}

/// KAT 4: chunked input via multiple slices.
#[ktest]
fn sha512_chunked_matches_single() {
    let mut buf = vec![b'a'; 4096];
    let single = crate::sha512::digest(&[buf.as_ref()]);

    let chunks: [&[u8]; 4] = [
        &buf[0..1024],
        &buf[1024..2048],
        &buf[2048..3072],
        &buf[3072..4096],
    ];
    let chunked = crate::sha512::digest(&chunks);
    assert_eq!(single, chunked, "chunked SHA-512 differs from single-shot");

    let again = crate::sha512::digest(&[buf.as_ref()]);
    assert_eq!(single, again, "SHA-512 is not deterministic");

    let last = buf[4095];
    buf[4095] = b'b';
    let mutated = crate::sha512::digest(&[buf.as_ref()]);
    assert_ne!(single, mutated, "SHA-512 did not change after mutation");
    buf[4095] = last;
}

/// KAT 5: boundary message that pushes padding into a second 128-byte block.
#[ktest]
fn sha512_111_byte_boundary() {
    let msg = [b'B'; 111];
    let digest = crate::sha512::digest(&[msg.as_ref()]);
    let again = crate::sha512::digest(&[msg.as_ref()]);
    assert_eq!(digest, again, "111-byte boundary result is not stable");
}

/// KAT 6: hash algorithm registry exposes SHA-256 and SHA-512.
#[ktest]
fn hash_algorithm_registry() {
    let supported = crate::hash::supported_algorithms();
    assert!(supported.contains(&"sha256"), "sha256 must be supported");
    assert!(supported.contains(&"sha512"), "sha512 must be supported");

    let sha256 = crate::hash::lookup_algorithm("sha256").unwrap();
    assert_eq!(sha256.name(), "sha256");
    assert_eq!(sha256.digest_size(), 32);

    let sha512 = crate::hash::lookup_algorithm("sha512").unwrap();
    assert_eq!(sha512.name(), "sha512");
    assert_eq!(sha512.digest_size(), 64);

    assert!(crate::hash::lookup_algorithm("md5").is_none());

    // Verify digest outputs are deterministic and size-correct.
    let mut out256_a = [0u8; 32];
    sha256.digest(&[b"test".as_ref()], &mut out256_a);
    let mut out256_b = [0u8; 32];
    sha256.digest(&[b"test".as_ref()], &mut out256_b);
    assert_eq!(out256_a.len(), 32);
    assert_eq!(out256_a, out256_b);

    let mut out512_a = [0u8; 64];
    sha512.digest(&[b"test".as_ref()], &mut out512_a);
    let mut out512_b = [0u8; 64];
    sha512.digest(&[b"test".as_ref()], &mut out512_b);
    assert_eq!(out512_a.len(), 64);
    assert_eq!(out512_a, out512_b);
    assert_ne!(out256_a.as_slice(), out512_a.as_slice());
}

// ===========================================================================
// Verity target corruption-detection tests
//
// These build a small in-memory data + hash device pair, construct a
// `VerityTarget`, and exercise the read path to confirm that:
//   - uncorrupted data reads succeed
//   - corrupted data blocks are rejected
//   - corrupted hash blocks are rejected
//   - writes are rejected (read-only semantics)
// ===========================================================================

/// Builds a verity test fixture: 1 data block + 1 root hash block.
///
/// Returns (data_device, hash_device, root_digest, salt) where both devices
/// hold exactly one 4096-byte block. The hash device stores the leaf digest
/// (hash of the single data block) in its first slots and pads the rest with
/// zeros. The returned `root_digest` is the hash of the root hash block
/// (`SHA-256(salt || hash_block)`), matching dm-verity version 1 semantics.
fn build_verity_fixture(
    payload: &[u8; BLOCK_SIZE],
) -> (
    Arc<MockBlockDevice>,
    Arc<MockBlockDevice>,
    [u8; 32],
    Vec<u8>,
) {
    let salt = vec![0x5a; 16];

    // Data device: one data block at block 0.
    let data_device = MockBlockDevice::new("verity-data", BLOCK_SIZE / SECTOR_SIZE);
    {
        let mut data = data_device.data.lock();
        data[..BLOCK_SIZE].copy_from_slice(payload);
    }

    // Leaf digest = hash of the single data block.
    let leaf_digest = crate::sha256::digest(&[&salt, payload]);

    // Hash device: one block holding the leaf digest with zero padding.
    let hash_device = MockBlockDevice::new("verity-hash", BLOCK_SIZE / SECTOR_SIZE);
    let mut hash_block = vec![0u8; BLOCK_SIZE];
    hash_block[..32].copy_from_slice(&leaf_digest);
    {
        let mut hash = hash_device.data.lock();
        hash.copy_from_slice(&hash_block);
    }

    // Root digest is the hash of the (salt-prefixed) root hash block.
    let root_digest = crate::sha256::digest(&[&salt, &hash_block[..]]);

    (data_device, hash_device, root_digest, salt)
}

/// Builds a SHA-512 verity test fixture: 1 data block + 1 root hash block.
///
/// The returned `root_digest` is the hash of the (salt-prefixed) root hash
/// block, consistent with dm-verity version 1.
fn build_verity_fixture_sha512(
    payload: &[u8; BLOCK_SIZE],
) -> (
    Arc<MockBlockDevice>,
    Arc<MockBlockDevice>,
    [u8; 64],
    Vec<u8>,
) {
    let salt = vec![0x73; 16];

    let data_device = MockBlockDevice::new("verity-data-sha512", BLOCK_SIZE / SECTOR_SIZE);
    {
        let mut data = data_device.data.lock();
        data[..BLOCK_SIZE].copy_from_slice(payload);
    }

    let leaf_digest = crate::sha512::digest(&[&salt, &payload[..]]);

    let hash_device = MockBlockDevice::new("verity-hash-sha512", BLOCK_SIZE / SECTOR_SIZE);
    let mut hash_block = vec![0u8; BLOCK_SIZE];
    hash_block[..64].copy_from_slice(&leaf_digest);
    {
        let mut hash = hash_device.data.lock();
        hash.copy_from_slice(&hash_block);
    }

    let root_digest = crate::sha512::digest(&[&salt, &hash_block[..]]);

    (data_device, hash_device, root_digest, salt)
}

/// Builds a verity fixture with custom data/hash block sizes.
///
/// The fixture contains a single data block. The hash device stores the leaf
/// digest in its first block and zeros the remainder (dm-verity padding).
/// The returned `root_digest` is the hash of the (salt-prefixed) root hash
/// block, consistent with dm-verity version 1.
fn build_verity_fixture_with_sizes(
    payload: &[u8],
    salt: Vec<u8>,
    data_block_size: usize,
    hash_block_size: usize,
) -> (Arc<MockBlockDevice>, Arc<MockBlockDevice>, Vec<u8>, Vec<u8>) {
    let data_sectors = data_block_size / SECTOR_SIZE;
    let hash_sectors = hash_block_size / SECTOR_SIZE;

    let data_device = MockBlockDevice::new("verity-data-custom", data_sectors);
    {
        let mut data = data_device.data.lock();
        data[..payload.len()].copy_from_slice(payload);
    }

    let leaf_digest = crate::sha256::digest(&[&salt, payload]);

    let hash_device = MockBlockDevice::new("verity-hash-custom", hash_sectors);
    let mut hash_block = vec![0u8; hash_block_size];
    hash_block[..leaf_digest.len()].copy_from_slice(&leaf_digest);
    {
        let mut hash = hash_device.data.lock();
        hash[..hash_block_size].copy_from_slice(&hash_block);
    }

    let root_digest = crate::sha256::digest(&[&salt, &hash_block[..]]);

    (data_device, hash_device, root_digest.to_vec(), salt)
}

/// Constructs a `MappedDevice` wrapping a `VerityTarget` with custom block sizes.
fn build_verity_mapped_with_sizes(
    name: &str,
    data_device: Arc<MockBlockDevice>,
    hash_device: Arc<MockBlockDevice>,
    root_digest: Vec<u8>,
    salt: Vec<u8>,
    data_block_size: usize,
    hash_block_size: usize,
) -> Arc<MappedDevice> {
    ensure_initialized();
    let target = VerityTarget::new_direct_with_algorithm_and_sizes(
        data_device,
        hash_device,
        /* num_data_blocks */ 1,
        /* version */ 1,
        salt,
        Arc::new(crate::hash::Sha256),
        root_digest,
        data_block_size,
        hash_block_size,
    );
    let size_sectors = (data_block_size / SECTOR_SIZE) as u64;
    let mut table = DmTable::new();
    table
        .add_target(0, size_sectors, DmTarget::Verity(Arc::new(target)))
        .unwrap();
    MappedDevice::create(name, table).unwrap()
}

/// Constructs a `MappedDevice` wrapping a `VerityTarget` built from the fixture.
///
/// `VerityTarget` implements `Target` (not `BlockDevice`), so it cannot receive
/// bios directly. We wrap it in a `DmTable` + `MappedDevice` (which implements
/// `BlockDevice`) and return the mapped device ready to accept bios.
///
/// The caller keeps references to the underlying mock devices (via the fixture)
/// so it can tamper with data or hash blocks after construction.
fn build_verity_mapped(
    name: &str,
    data_device: Arc<MockBlockDevice>,
    hash_device: Arc<MockBlockDevice>,
    root_digest: [u8; 32],
    salt: Vec<u8>,
) -> Arc<MappedDevice> {
    ensure_initialized();
    let target = VerityTarget::new_direct(
        data_device,
        hash_device,
        /* num_data_blocks */ 1,
        /* version */ 1,
        salt,
        root_digest,
    );
    // 1 data block = BLOCK_SIZE / SECTOR_SIZE = 8 sectors.
    let size_sectors = (BLOCK_SIZE / SECTOR_SIZE) as u64;
    let mut table = DmTable::new();
    table
        .add_target(0, size_sectors, DmTarget::Verity(Arc::new(target)))
        .unwrap();
    MappedDevice::create(name, table).unwrap()
}

/// Constructs a `MappedDevice` wrapping a SHA-512 `VerityTarget`.
fn build_verity_mapped_sha512(
    name: &str,
    data_device: Arc<MockBlockDevice>,
    hash_device: Arc<MockBlockDevice>,
    root_digest: [u8; 64],
    salt: Vec<u8>,
) -> Arc<MappedDevice> {
    ensure_initialized();
    let target = VerityTarget::new_direct_with_algorithm(
        data_device,
        hash_device,
        /* num_data_blocks */ 1,
        /* version */ 1,
        salt,
        Arc::new(crate::hash::Sha512),
        root_digest.to_vec(),
    );
    let size_sectors = (BLOCK_SIZE / SECTOR_SIZE) as u64;
    let mut table = DmTable::new();
    table
        .add_target(0, size_sectors, DmTarget::Verity(Arc::new(target)))
        .unwrap();
    MappedDevice::create(name, table).unwrap()
}

/// Packs a list of SHA-256 digests into zero-padded hash blocks.
fn pack_hashes_into_blocks(
    hashes: &[[u8; 32]],
    hashes_per_block: usize,
    hash_block_size: usize,
) -> Vec<Vec<u8>> {
    let digest_size = 32;
    let mut blocks = Vec::new();
    for chunk in hashes.chunks(hashes_per_block) {
        let mut block = vec![0u8; hash_block_size];
        for (i, h) in chunk.iter().enumerate() {
            let start = i * digest_size;
            block[start..start + digest_size].copy_from_slice(h);
        }
        blocks.push(block);
    }
    blocks
}

/// Builds a verity test fixture with multiple 4096-byte data blocks.
///
/// `num_data_blocks` must be at least 1. Data blocks are filled with
/// deterministic, unique patterns. The hash device contains the full Merkle
/// hash tree starting at block 0, top level first. The returned `root_digest`
/// is the hash of the (salt-prefixed) root hash block, consistent with
/// dm-verity version 1.
fn build_verity_fixture_multi(
    num_data_blocks: usize,
) -> (Arc<MockBlockDevice>, Arc<MockBlockDevice>, Vec<u8>, Vec<u8>) {
    assert!(num_data_blocks >= 1, "num_data_blocks must be at least 1");
    let salt = vec![0x5a; 16];
    let data_block_size = BLOCK_SIZE;
    let hash_block_size = BLOCK_SIZE;
    let digest_size = 32;
    let hashes_per_block = hash_block_size / digest_size;

    // Generate unique data blocks.
    let mut payloads: Vec<Vec<u8>> = Vec::with_capacity(num_data_blocks);
    for i in 0..num_data_blocks {
        let mut block = vec![0u8; data_block_size];
        for (j, b) in block.iter_mut().enumerate() {
            *b = ((i + j) % 256) as u8;
        }
        payloads.push(block);
    }

    // Data device: `num_data_blocks` blocks back-to-back.
    let data_sectors = (num_data_blocks * data_block_size) / SECTOR_SIZE;
    let data_device = MockBlockDevice::new("verity-data-multi", data_sectors);
    {
        let mut data = data_device.data.lock();
        for (i, payload) in payloads.iter().enumerate() {
            let offset = i * data_block_size;
            data[offset..offset + data_block_size].copy_from_slice(payload);
        }
    }

    // Compute leaf hashes.
    let mut current_hashes: Vec<[u8; 32]> = payloads
        .iter()
        .map(|p| crate::sha256::digest(&[&salt, &p[..]]))
        .collect();

    // Build hash-tree levels bottom-up. Each entry is a vector of full hash
    // blocks for that level; the last entry is the root level.
    let mut level_blocks: Vec<Vec<Vec<u8>>> = Vec::new();
    loop {
        let blocks = pack_hashes_into_blocks(&current_hashes, hashes_per_block, hash_block_size);
        level_blocks.push(blocks.clone());
        if blocks.len() == 1 {
            break;
        }
        current_hashes = blocks
            .iter()
            .map(|b| crate::sha256::digest(&[&salt, &b[..]]))
            .collect();
    }

    let root_block = level_blocks.last().unwrap().first().unwrap().clone();
    let root_digest = crate::sha256::digest(&[&salt, &root_block[..]]);

    // Write the hash device with the top level first.
    let total_hash_blocks: usize = level_blocks.iter().map(|l| l.len()).sum();
    let hash_sectors = (total_hash_blocks * hash_block_size) / SECTOR_SIZE;
    let hash_device = MockBlockDevice::new("verity-hash-multi", hash_sectors);
    {
        let mut hash = hash_device.data.lock();
        let mut offset = 0;
        for level in level_blocks.iter().rev() {
            for block in level {
                hash[offset..offset + hash_block_size].copy_from_slice(block);
                offset += hash_block_size;
            }
        }
    }

    (data_device, hash_device, root_digest.to_vec(), salt)
}

/// Constructs a `MappedDevice` wrapping a multi-block SHA-256 `VerityTarget`.
fn build_verity_mapped_multi(
    name: &str,
    data_device: Arc<MockBlockDevice>,
    hash_device: Arc<MockBlockDevice>,
    num_data_blocks: usize,
    root_digest: Vec<u8>,
    salt: Vec<u8>,
) -> Arc<MappedDevice> {
    ensure_initialized();
    let target = VerityTarget::new_direct_with_algorithm_and_sizes(
        data_device,
        hash_device,
        num_data_blocks as u64,
        /* version */ 1,
        salt,
        Arc::new(crate::hash::Sha256),
        root_digest,
        BLOCK_SIZE,
        BLOCK_SIZE,
    );
    let size_sectors = ((num_data_blocks * BLOCK_SIZE) / SECTOR_SIZE) as u64;
    let mut table = DmTable::new();
    table
        .add_target(0, size_sectors, DmTarget::Verity(Arc::new(target)))
        .unwrap();
    MappedDevice::create(name, table).unwrap()
}

/// Test: reading an unmodified data block through verity succeeds.
#[ktest]
fn verity_read_ok() {
    let mut payload = [0u8; BLOCK_SIZE];
    for (i, b) in payload.iter_mut().enumerate() {
        *b = (i & 0xff) as u8;
    }
    let (data_dev, hash_dev, root, salt) = build_verity_fixture(&payload);
    let mapped = build_verity_mapped("dm-verity-0", data_dev, hash_dev, root, salt);

    let segment = BioSegment::alloc(1, BioDirection::FromDevice);
    let bio = Bio::new(BioType::Read, Sid::new(0), vec![segment], None);
    let status = bio.submit_and_wait(mapped.as_ref()).unwrap();
    assert_eq!(
        status,
        BioStatus::Complete,
        "verity read of clean block should succeed"
    );
}

/// Test: a corrupted data block is detected and the read fails.
#[ktest]
fn verity_data_corrupted() {
    let mut payload = [0u8; BLOCK_SIZE];
    for (i, b) in payload.iter_mut().enumerate() {
        *b = (i & 0xff) as u8;
    }
    let (data_dev, hash_dev, root, salt) = build_verity_fixture(&payload);
    let mapped = build_verity_mapped("dm-verity-1", data_dev.clone(), hash_dev, root, salt);

    // Tamper with one byte of the data block after building the hash tree.
    {
        let mut data = data_dev.data.lock();
        data[0] ^= 0xff;
    }

    let segment = BioSegment::alloc(1, BioDirection::FromDevice);
    let bio = Bio::new(BioType::Read, Sid::new(0), vec![segment], None);
    let status = bio.submit_and_wait(mapped.as_ref()).unwrap();
    assert_ne!(
        status,
        BioStatus::Complete,
        "verity read of corrupted data must not succeed"
    );
}

/// Test: a corrupted hash block is detected and the read fails.
#[ktest]
fn verity_hash_corrupted() {
    let mut payload = [0u8; BLOCK_SIZE];
    for (i, b) in payload.iter_mut().enumerate() {
        *b = (i & 0xff) as u8;
    }
    let (data_dev, hash_dev, root, salt) = build_verity_fixture(&payload);
    let mapped = build_verity_mapped("dm-verity-2", data_dev, hash_dev.clone(), root, salt);

    // Tamper with the root hash stored on the hash device.
    {
        let mut hash = hash_dev.data.lock();
        hash[0] ^= 0xff;
    }

    let segment = BioSegment::alloc(1, BioDirection::FromDevice);
    let bio = Bio::new(BioType::Read, Sid::new(0), vec![segment], None);
    let status = bio.submit_and_wait(mapped.as_ref()).unwrap();
    assert_ne!(
        status,
        BioStatus::Complete,
        "verity read with corrupted hash must not succeed"
    );
}

/// Test: writes through a verity target are rejected.
#[ktest]
fn verity_write_rejected() {
    let payload = [0u8; BLOCK_SIZE];
    let (data_dev, hash_dev, root, salt) = build_verity_fixture(&payload);
    let mapped = build_verity_mapped("dm-verity-3", data_dev.clone(), hash_dev, root, salt);

    let segment = BioSegment::alloc(1, BioDirection::ToDevice);
    let bio = Bio::new(BioType::Write, Sid::new(0), vec![segment], None);
    let _ = bio.submit_and_wait(mapped.as_ref());
    // Write should not modify the data device. Confirm the data block is
    // still all zeros (we never wrote anything into the segment buffer).
    let data = data_dev.data.lock();
    assert_eq!(&data[..BLOCK_SIZE], &payload[..], "verity accepted a write");
}

/// Test: a read at sector 0 through the full mapped-device path succeeds,
/// exercising the table's bio dispatch and the verity target's read handler.
#[ktest]
fn verity_read_via_mapped_device() {
    let mut payload = [0u8; BLOCK_SIZE];
    for (i, b) in payload.iter_mut().enumerate() {
        *b = (i & 0x7f) as u8;
    }
    let (data_dev, hash_dev, root, salt) = build_verity_fixture(&payload);
    let mapped = build_verity_mapped("dm-verity-4", data_dev, hash_dev, root, salt);

    // Read the single data block (8 sectors starting at sector 0).
    let segment = BioSegment::alloc(1, BioDirection::FromDevice);
    let bio = Bio::new(BioType::Read, Sid::new(0), vec![segment], None);
    let status = bio.submit_and_wait(mapped.as_ref()).unwrap();
    assert_eq!(
        status,
        BioStatus::Complete,
        "verity read through mapped device should succeed"
    );
}

/// Test: SHA-512 verity read of an uncorrupted block succeeds.
#[ktest]
fn verity_sha512_read_ok() {
    let mut payload = [0u8; BLOCK_SIZE];
    for (i, b) in payload.iter_mut().enumerate() {
        *b = (i & 0xff) as u8;
    }
    let (data_dev, hash_dev, root, salt) = build_verity_fixture_sha512(&payload);
    let mapped = build_verity_mapped_sha512("dm-verity-sha512-0", data_dev, hash_dev, root, salt);

    let segment = BioSegment::alloc(1, BioDirection::FromDevice);
    let bio = Bio::new(BioType::Read, Sid::new(0), vec![segment], None);
    let status = bio.submit_and_wait(mapped.as_ref()).unwrap();
    assert_eq!(
        status,
        BioStatus::Complete,
        "SHA-512 verity read of clean block should succeed"
    );
}

/// Test: SHA-512 verity detects corrupted data.
#[ktest]
fn verity_sha512_data_corrupted() {
    let mut payload = [0u8; BLOCK_SIZE];
    for (i, b) in payload.iter_mut().enumerate() {
        *b = (i & 0xff) as u8;
    }
    let (data_dev, hash_dev, root, salt) = build_verity_fixture_sha512(&payload);
    let mapped =
        build_verity_mapped_sha512("dm-verity-sha512-1", data_dev.clone(), hash_dev, root, salt);

    {
        let mut data = data_dev.data.lock();
        data[0] ^= 0xff;
    }

    let segment = BioSegment::alloc(1, BioDirection::FromDevice);
    let bio = Bio::new(BioType::Read, Sid::new(0), vec![segment], None);
    let status = bio.submit_and_wait(mapped.as_ref()).unwrap();
    assert_ne!(
        status,
        BioStatus::Complete,
        "SHA-512 verity read of corrupted data must not succeed"
    );
}

/// Test: a 512-byte data/hash block configuration reads successfully.
#[ktest]
fn verity_read_ok_512_byte_blocks() {
    const DATA_BLOCK_SIZE: usize = 512;
    const HASH_BLOCK_SIZE: usize = 512;

    let mut payload = vec![0u8; DATA_BLOCK_SIZE];
    for (i, b) in payload.iter_mut().enumerate() {
        *b = (i & 0xff) as u8;
    }
    let salt = vec![0x5a; 16];
    let (data_dev, hash_dev, root, salt) =
        build_verity_fixture_with_sizes(&payload, salt, DATA_BLOCK_SIZE, HASH_BLOCK_SIZE);
    let mapped = build_verity_mapped_with_sizes(
        "dm-verity-512",
        data_dev,
        hash_dev,
        root,
        salt,
        DATA_BLOCK_SIZE,
        HASH_BLOCK_SIZE,
    );

    let segment = BioSegment::alloc_with_len(DATA_BLOCK_SIZE, BioDirection::FromDevice);
    let bio = Bio::new(BioType::Read, Sid::new(0), vec![segment], None);
    let status = bio.submit_and_wait(mapped.as_ref()).unwrap();
    assert_eq!(
        status,
        BioStatus::Complete,
        "verity read with 512-byte blocks should succeed"
    );
}

/// Test: a 2048-byte data block with 1024-byte hash block reads successfully.
#[ktest]
fn verity_read_ok_mixed_block_sizes() {
    const DATA_BLOCK_SIZE: usize = 2048;
    const HASH_BLOCK_SIZE: usize = 1024;

    let mut payload = vec![0u8; DATA_BLOCK_SIZE];
    for (i, b) in payload.iter_mut().enumerate() {
        *b = (i & 0x7f) as u8;
    }
    let salt = vec![0x6b; 16];
    let (data_dev, hash_dev, root, salt) =
        build_verity_fixture_with_sizes(&payload, salt, DATA_BLOCK_SIZE, HASH_BLOCK_SIZE);
    let mapped = build_verity_mapped_with_sizes(
        "dm-verity-mixed",
        data_dev,
        hash_dev,
        root,
        salt,
        DATA_BLOCK_SIZE,
        HASH_BLOCK_SIZE,
    );

    let segment = BioSegment::alloc_with_len(DATA_BLOCK_SIZE, BioDirection::FromDevice);
    let bio = Bio::new(BioType::Read, Sid::new(0), vec![segment], None);
    let status = bio.submit_and_wait(mapped.as_ref()).unwrap();
    assert_eq!(
        status,
        BioStatus::Complete,
        "verity read with mixed block sizes should succeed"
    );
}

/// Test: corruption is detected with a 512-byte data block.
#[ktest]
fn verity_data_corrupted_512_byte_blocks() {
    const DATA_BLOCK_SIZE: usize = 512;
    const HASH_BLOCK_SIZE: usize = 512;

    let mut payload = vec![0u8; DATA_BLOCK_SIZE];
    for (i, b) in payload.iter_mut().enumerate() {
        *b = (i & 0xff) as u8;
    }
    let salt = vec![0x5a; 16];
    let (data_dev, hash_dev, root, salt) =
        build_verity_fixture_with_sizes(&payload, salt, DATA_BLOCK_SIZE, HASH_BLOCK_SIZE);
    let mapped = build_verity_mapped_with_sizes(
        "dm-verity-512-corrupt",
        data_dev.clone(),
        hash_dev,
        root,
        salt,
        DATA_BLOCK_SIZE,
        HASH_BLOCK_SIZE,
    );

    {
        let mut data = data_dev.data.lock();
        data[0] ^= 0xff;
    }

    let segment = BioSegment::alloc_with_len(DATA_BLOCK_SIZE, BioDirection::FromDevice);
    let bio = Bio::new(BioType::Read, Sid::new(0), vec![segment], None);
    let status = bio.submit_and_wait(mapped.as_ref()).unwrap();
    assert_ne!(
        status,
        BioStatus::Complete,
        "verity must detect corrupted data with 512-byte blocks"
    );
}

// ===========================================================================
// Multi-block / multi-layer Merkle verity tests
//
// The single-block fixtures above do not exercise hash trees with more than
// one data block. These tests build fixtures with 3 and 129 data blocks,
// forcing the leaf level and (for 129 blocks) an intermediate level, to
// validate that the verification path works for real multi-level Merkle trees.
// ===========================================================================

/// Test: reading multiple clean data blocks through verity succeeds.
#[ktest]
fn verity_multi_block_read_ok() {
    let num_blocks = 3;
    let (data_dev, hash_dev, root, salt) = build_verity_fixture_multi(num_blocks);
    let mapped = build_verity_mapped_multi(
        "dm-verity-multi-ok",
        data_dev,
        hash_dev,
        num_blocks,
        root,
        salt,
    );

    // Read block 0 and block 2; each bio is one 4096-byte block.
    let sectors_per_block = (BLOCK_SIZE / SECTOR_SIZE) as u64;
    for block_index in [0, 2] {
        let segment = BioSegment::alloc(1, BioDirection::FromDevice);
        let bio = Bio::new(
            BioType::Read,
            Sid::new(block_index as u64 * sectors_per_block),
            vec![segment],
            None,
        );
        let status = bio.submit_and_wait(mapped.as_ref()).unwrap();
        assert_eq!(
            status,
            BioStatus::Complete,
            "verity read of clean multi-block data {} should succeed",
            block_index
        );
    }
}

/// Test: tampering with one data block in a multi-block fixture is detected.
#[ktest]
fn verity_multi_block_data_corrupted() {
    let num_blocks = 3;
    let (data_dev, hash_dev, root, salt) = build_verity_fixture_multi(num_blocks);
    let mapped = build_verity_mapped_multi(
        "dm-verity-multi-data-corrupt",
        data_dev.clone(),
        hash_dev,
        num_blocks,
        root,
        salt,
    );

    // Corrupt data block 1.
    {
        let mut data = data_dev.data.lock();
        data[BLOCK_SIZE] ^= 0xff;
    }

    let sectors_per_block = (BLOCK_SIZE / SECTOR_SIZE) as u64;
    let segment = BioSegment::alloc(1, BioDirection::FromDevice);
    let bio = Bio::new(
        BioType::Read,
        Sid::new(sectors_per_block),
        vec![segment],
        None,
    );
    let status = bio.submit_and_wait(mapped.as_ref()).unwrap();
    assert_ne!(
        status,
        BioStatus::Complete,
        "verity must detect corrupted data block in multi-block fixture"
    );
}

/// Test: tampering with the hash block in a multi-block fixture is detected.
///
/// With only 3 data blocks and 128 digests per 4096-byte hash block, the leaf
/// block is also the root block, so a single hash block covers all data.
#[ktest]
fn verity_multi_block_hash_corrupted() {
    let num_blocks = 3;
    let (data_dev, hash_dev, root, salt) = build_verity_fixture_multi(num_blocks);
    let mapped = build_verity_mapped_multi(
        "dm-verity-multi-hash-corrupt",
        data_dev,
        hash_dev.clone(),
        num_blocks,
        root,
        salt,
    );

    // Corrupt the digest slot that covers data block 1.
    {
        let mut hash = hash_dev.data.lock();
        hash[32] ^= 0xff;
    }

    let sectors_per_block = (BLOCK_SIZE / SECTOR_SIZE) as u64;
    let segment = BioSegment::alloc(1, BioDirection::FromDevice);
    let bio = Bio::new(
        BioType::Read,
        Sid::new(sectors_per_block),
        vec![segment],
        None,
    );
    let status = bio.submit_and_wait(mapped.as_ref()).unwrap();
    assert_ne!(
        status,
        BioStatus::Complete,
        "verity must detect corrupted hash block in multi-block fixture"
    );
}

/// Test: reading through a two-level Merkle tree succeeds.
///
/// With 129 4096-byte data blocks and a 4096-byte hash block holding 128
/// SHA-256 digests, the leaf level requires 2 blocks and the root level 1
/// block, producing a true multi-level hash tree.
#[ktest]
fn verity_multi_layer_read_ok() {
    let num_blocks = 129;
    let (data_dev, hash_dev, root, salt) = build_verity_fixture_multi(num_blocks);
    let mapped = build_verity_mapped_multi(
        "dm-verity-layer-ok",
        data_dev,
        hash_dev,
        num_blocks,
        root,
        salt,
    );

    // Read block 0 (first leaf hash block) and block 128 (second leaf hash
    // block) to cover both leaf-level hash blocks and both root-block slots.
    let sectors_per_block = (BLOCK_SIZE / SECTOR_SIZE) as u64;
    for block_index in [0, 128] {
        let segment = BioSegment::alloc(1, BioDirection::FromDevice);
        let bio = Bio::new(
            BioType::Read,
            Sid::new(block_index as u64 * sectors_per_block),
            vec![segment],
            None,
        );
        let status = bio.submit_and_wait(mapped.as_ref()).unwrap();
        assert_eq!(
            status,
            BioStatus::Complete,
            "verity read of clean multi-layer data block {} should succeed",
            block_index
        );
    }
}

/// Test: data corruption in the second leaf block of a two-level tree is
/// detected.
#[ktest]
fn verity_multi_layer_data_corrupted() {
    let num_blocks = 129;
    let (data_dev, hash_dev, root, salt) = build_verity_fixture_multi(num_blocks);
    let mapped = build_verity_mapped_multi(
        "dm-verity-layer-data-corrupt",
        data_dev.clone(),
        hash_dev,
        num_blocks,
        root,
        salt,
    );

    // Corrupt data block 128, which belongs to the second leaf hash block.
    {
        let mut data = data_dev.data.lock();
        data[128 * BLOCK_SIZE] ^= 0xff;
    }

    let sectors_per_block = (BLOCK_SIZE / SECTOR_SIZE) as u64;
    let segment = BioSegment::alloc(1, BioDirection::FromDevice);
    let bio = Bio::new(
        BioType::Read,
        Sid::new(128 * sectors_per_block),
        vec![segment],
        None,
    );
    let status = bio.submit_and_wait(mapped.as_ref()).unwrap();
    assert_ne!(
        status,
        BioStatus::Complete,
        "verity must detect corrupted data block in multi-layer fixture"
    );
}

/// Test: hash corruption in the second leaf block of a two-level tree is
/// detected.
#[ktest]
fn verity_multi_layer_hash_corrupted() {
    let num_blocks = 129;
    let (data_dev, hash_dev, root, salt) = build_verity_fixture_multi(num_blocks);
    let mapped = build_verity_mapped_multi(
        "dm-verity-layer-hash-corrupt",
        data_dev,
        hash_dev.clone(),
        num_blocks,
        root,
        salt,
    );

    // The hash device layout is: root block at 0, leaf block 0 at BLOCK_SIZE,
    // leaf block 1 at 2*BLOCK_SIZE. Corrupt the first digest slot of leaf
    // block 1, which covers data block 128.
    {
        let mut hash = hash_dev.data.lock();
        hash[2 * BLOCK_SIZE] ^= 0xff;
    }

    let sectors_per_block = (BLOCK_SIZE / SECTOR_SIZE) as u64;
    let segment = BioSegment::alloc(1, BioDirection::FromDevice);
    let bio = Bio::new(
        BioType::Read,
        Sid::new(128 * sectors_per_block),
        vec![segment],
        None,
    );
    let status = bio.submit_and_wait(mapped.as_ref()).unwrap();
    assert_ne!(
        status,
        BioStatus::Complete,
        "verity must detect corrupted hash block in multi-layer fixture"
    );
}

// ===========================================================================
// Zero target tests
//
// The zero target returns zero-filled blocks on read and silently discards
// writes. It has no underlying device dependency.
// ===========================================================================

/// Test: reading from a zero target returns all zeros.
#[ktest]
fn zero_read_returns_zeros() {
    ensure_initialized();

    let mut table = DmTable::new();
    table
        .add_target(
            0,
            8,
            DmTarget::Zero(crate::targets::zero::ZeroTarget::new(8)),
        )
        .unwrap();
    let mapped = MappedDevice::create("dm-zero-0", table).unwrap();

    // Read one block (8 sectors = 4096 bytes).
    let buf = read_blocks(mapped.as_ref(), Bid::new(0), BLOCK_SIZE);
    assert!(
        buf.iter().all(|&b| b == 0),
        "zero target should return all-zero data"
    );
}

/// Test: writing to a zero target succeeds (data is silently discarded).
#[ktest]
fn zero_write_succeeds() {
    ensure_initialized();

    let mut table = DmTable::new();
    table
        .add_target(
            0,
            8,
            DmTarget::Zero(crate::targets::zero::ZeroTarget::new(8)),
        )
        .unwrap();
    let mapped = MappedDevice::create("dm-zero-1", table).unwrap();

    let pattern: Vec<u8> = (0..BLOCK_SIZE).map(|i| (i % 251) as u8).collect();
    // Write should complete successfully (data is discarded internally).
    write_blocks(mapped.as_ref(), Bid::new(0), &pattern);

    // Read back - should still be all zeros.
    let buf = read_blocks(mapped.as_ref(), Bid::new(0), BLOCK_SIZE);
    assert!(
        buf.iter().all(|&b| b == 0),
        "zero target should discard writes and return zeros"
    );
}

// ===========================================================================
// Error target tests
//
// The error target fails every read and write with an I/O error.
// ===========================================================================

/// Test: reading from an error target returns an I/O error.
#[ktest]
fn error_read_fails() {
    ensure_initialized();

    let mut table = DmTable::new();
    table
        .add_target(
            0,
            8,
            DmTarget::Error(crate::targets::error::ErrorTarget::new(8)),
        )
        .unwrap();
    let mapped = MappedDevice::create("dm-error-0", table).unwrap();

    let segment = BioSegment::alloc(1, BioDirection::FromDevice);
    let bio = Bio::new(BioType::Read, Sid::new(0), vec![segment], None);
    let status = bio.submit_and_wait(mapped.as_ref()).unwrap();
    assert_eq!(
        status,
        BioStatus::IoError,
        "error target should fail reads with IoError"
    );
}

/// Test: writing to an error target returns an I/O error.
#[ktest]
fn error_write_fails() {
    ensure_initialized();

    let mut table = DmTable::new();
    table
        .add_target(
            0,
            8,
            DmTarget::Error(crate::targets::error::ErrorTarget::new(8)),
        )
        .unwrap();
    let mapped = MappedDevice::create("dm-error-1", table).unwrap();

    let segment = BioSegment::alloc(1, BioDirection::ToDevice);
    let bio = Bio::new(BioType::Write, Sid::new(0), vec![segment], None);
    let status = bio.submit_and_wait(mapped.as_ref()).unwrap();
    assert_eq!(
        status,
        BioStatus::IoError,
        "error target should fail writes with IoError"
    );
}

// ===========================================================================
// Suspend/Resume and Rename tests
//
// These exercise the control-plane state machine: suspend blocks I/O,
// resume allows it, and rename updates the registry.
// ===========================================================================

/// Test: while suspended, new I/O is deferred (not failed) and replayed on
/// resume — matching Linux's `md->deferred` semantics.
#[ktest]
fn suspend_blocks_io() {
    ensure_initialized();

    let mock = MockBlockDevice::new("mock-suspend", 256);
    let mut table = DmTable::new();
    table
        .add_target(0, 256, DmTarget::Linear(LinearTarget::new(mock.clone(), 0)))
        .unwrap();
    let mapped = MappedDevice::create("dm-suspend-0", table).unwrap();

    // Sanity: device is resumed (create() resumes).
    assert!(!mapped.is_suspended());

    // Suspend. New bios are now deferred instead of being refused.
    mapped.set_suspended(true);
    assert!(mapped.is_suspended());

    // Submit a read asynchronously. It must not be refused; instead it is
    // queued in the deferred list and not yet completed.
    let segment = BioSegment::alloc(1, BioDirection::FromDevice);
    let bio = Bio::new(BioType::Read, Sid::new(0), vec![segment], None);
    let mut io_batch = IoBatch::new();
    assert!(
        bio.submit(mapped.as_ref(), &mut io_batch).is_ok(),
        "suspended device should defer I/O, not refuse it"
    );
    assert_eq!(
        mapped.deferred_len(),
        1,
        "bio issued while suspended must be deferred"
    );

    // Resume replays the deferred bio against the active table.
    mapped.set_suspended(false);
    assert!(!mapped.is_suspended());
    assert_eq!(mapped.deferred_len(), 0, "deferred bios must be replayed");
    io_batch.wait_all().unwrap();

    // Reads work normally after resume.
    let read_buf = read_blocks(mapped.as_ref(), Bid::new(0), BLOCK_SIZE);
    assert!(
        read_buf.iter().all(|&b| b == 0),
        "resumed device should allow reads"
    );
}

/// Test: `reset()` clears tables and leaves the device unsuspended.
#[ktest]
fn reset_clears_tables() {
    ensure_initialized();

    let mock = MockBlockDevice::new("mock-reset", 256);
    let mut table = DmTable::new();
    table
        .add_target(0, 256, DmTarget::Linear(LinearTarget::new(mock, 0)))
        .unwrap();
    let mapped = MappedDevice::create("dm-reset-0", table).unwrap();

    assert!(mapped.table().is_some());
    assert!(!mapped.is_suspended());

    mapped.reset();

    assert!(mapped.table().is_none());
    assert!(mapped.inactive_table().is_none());
    assert!(!mapped.is_suspended());

    // I/O should be refused after reset because no table is loaded.
    let segment = BioSegment::alloc(1, BioDirection::FromDevice);
    let bio = Bio::new(BioType::Read, Sid::new(0), vec![segment], None);
    assert_eq!(
        bio.submit_and_wait(mapped.as_ref()),
        Err(BioEnqueueError::Refused),
    );
}

/// Test: the in-flight counter stays balanced when the backing device
/// refuses a bio after `MappedDevice::enqueue` has already counted it.
#[ktest]
fn enqueue_error_keeps_in_flight_balanced() {
    ensure_initialized();

    let backing = RefusingBlockDevice::new("mock-refuse", 256);
    let mut table = DmTable::new();
    table
        .add_target(0, 256, DmTarget::Linear(LinearTarget::new(backing, 0)))
        .unwrap();
    let mapped = MappedDevice::create("dm-refuse-0", table).unwrap();

    assert_eq!(mapped.in_flight(), 0);

    // The backing device refuses the bio, so `map_bio` returns an error.
    // `enqueue` must decrement the counter it incremented, because the
    // dropped bio never fires its completion callback.
    let segment = BioSegment::alloc(1, BioDirection::FromDevice);
    let bio = Bio::new(BioType::Read, Sid::new(0), vec![segment], None);
    assert_eq!(
        bio.submit_and_wait(mapped.as_ref()),
        Err(BioEnqueueError::Refused),
    );
    assert_eq!(
        mapped.in_flight(),
        0,
        "failed enqueue must balance the in-flight counter"
    );

    // Repeated failures must not underflow the counter either.
    let segment = BioSegment::alloc(1, BioDirection::FromDevice);
    let bio = Bio::new(BioType::Read, Sid::new(0), vec![segment], None);
    assert!(bio.submit_and_wait(mapped.as_ref()).is_err());
    assert_eq!(mapped.in_flight(), 0);

    // Bios issued while suspended are deferred (not counted as in-flight).
    // On resume the deferred bio is replayed: submit() increments the
    // counter, then the refusing backing fails map_bio and map_submitted_bio
    // calls io.finish() to balance it. We verify the counter is balanced
    // without waiting for the bio's completion (the refusing device consumes
    // it without completing, so wait_all would block).
    mapped.set_suspended(true);
    let segment = BioSegment::alloc(1, BioDirection::FromDevice);
    let bio = Bio::new(BioType::Read, Sid::new(0), vec![segment], None);
    let mut io_batch = IoBatch::new();
    assert!(bio.submit(mapped.as_ref(), &mut io_batch).is_ok());
    assert_eq!(mapped.deferred_len(), 1);
    assert_eq!(mapped.in_flight(), 0, "deferred bio must not be in-flight");
    mapped.set_suspended(false);
    assert_eq!(mapped.deferred_len(), 0, "deferred bio must be replayed on resume");
    assert_eq!(mapped.in_flight(), 0, "replayed bio that fails must balance the counter");
}

/// Test: `rename_by_name` updates the registry so the old name is not found
/// and the new name is.
#[ktest]
fn rename_updates_registry() {
    ensure_initialized();

    let mock = MockBlockDevice::new("mock-rename", 64);
    let mut table = DmTable::new();
    table
        .add_target(0, 64, DmTarget::Linear(LinearTarget::new(mock, 0)))
        .unwrap();
    let mapped = MappedDevice::create("dm-rename-0", table).unwrap();
    let id = mapped.device_id();

    // Rename the device.
    MappedDevice::rename_by_name("dm-rename-0", "dm-rename-1").unwrap();

    // Old name should not find the device.
    assert!(MappedDevice::lookup_by_name("dm-rename-0").is_none());
    // New name should find the same device (same id).
    let renamed = MappedDevice::lookup_by_name("dm-rename-1").unwrap();
    assert_eq!(renamed.device_id(), id);
    // The device's internal name field should also be updated.
    assert_eq!(renamed.name().as_ref(), "dm-rename-1");

    // Clean up.
    MappedDevice::remove_by_name("dm-rename-1").unwrap();
}

/// Test: renaming to a name that is already in use returns
/// `AlreadyRegistered` and leaves the original device unchanged.
#[ktest]
fn rename_to_existing_name_fails() {
    ensure_initialized();

    let mock_a = MockBlockDevice::new("mock-rn-a", 64);
    let mock_b = MockBlockDevice::new("mock-rn-b", 64);
    let mut table_a = DmTable::new();
    table_a
        .add_target(0, 64, DmTarget::Linear(LinearTarget::new(mock_a, 0)))
        .unwrap();
    let mut table_b = DmTable::new();
    table_b
        .add_target(0, 64, DmTarget::Linear(LinearTarget::new(mock_b, 0)))
        .unwrap();
    let dev_a = MappedDevice::create("dm-rn-a", table_a).unwrap();
    let dev_b = MappedDevice::create("dm-rn-b", table_b).unwrap();
    let id_a = dev_a.device_id();
    let id_b = dev_b.device_id();

    // Attempt to rename dev_a to "dm-rn-b" (already used by dev_b).
    let err = MappedDevice::rename_by_name("dm-rn-a", "dm-rn-b").unwrap_err();
    assert_eq!(format!("{:?}", err), "AlreadyRegistered");

    // dev_a should still be found under its original name.
    let still_a = MappedDevice::lookup_by_name("dm-rn-a").unwrap();
    assert_eq!(still_a.device_id(), id_a);
    assert_eq!(still_a.name().as_ref(), "dm-rn-a");
    // dev_b should be unaffected.
    let still_b = MappedDevice::lookup_by_name("dm-rn-b").unwrap();
    assert_eq!(still_b.device_id(), id_b);

    // Clean up.
    MappedDevice::remove_by_name("dm-rn-a").unwrap();
    MappedDevice::remove_by_name("dm-rn-b").unwrap();
}

/// Test: renaming a device to the same name is a no-op (returns the device).
#[ktest]
fn rename_to_same_name_is_noop() {
    ensure_initialized();

    let mock = MockBlockDevice::new("mock-rn-same", 64);
    let mut table = DmTable::new();
    table
        .add_target(0, 64, DmTarget::Linear(LinearTarget::new(mock, 0)))
        .unwrap();
    let mapped = MappedDevice::create("dm-rn-same-0", table).unwrap();
    let id = mapped.device_id();

    // Rename to the same name.
    MappedDevice::rename_by_name("dm-rn-same-0", "dm-rn-same-0").unwrap();

    // Device should still be found under the same name.
    let found = MappedDevice::lookup_by_name("dm-rn-same-0").unwrap();
    assert_eq!(found.device_id(), id);

    MappedDevice::remove_by_name("dm-rn-same-0").unwrap();
}

/// Test: `set_uuid` and `lookup_by_uuid` work correctly.
#[ktest]
fn uuid_lookup_works() {
    ensure_initialized();

    let mock = MockBlockDevice::new("mock-uuid", 64);
    let mut table = DmTable::new();
    table
        .add_target(0, 64, DmTarget::Linear(LinearTarget::new(mock, 0)))
        .unwrap();
    let mapped = MappedDevice::create("dm-uuid-0", table).unwrap();

    // No UUID initially.
    assert!(mapped.uuid().is_empty());
    assert!(MappedDevice::lookup_by_uuid("LVM-test").is_none());

    // Set UUID and look it up.
    mapped.set_uuid("LVM-test-uuid-1").unwrap();
    assert_eq!(mapped.uuid().as_ref(), "LVM-test-uuid-1");

    let found = MappedDevice::lookup_by_uuid("LVM-test-uuid-1").unwrap();
    assert_eq!(found.device_id(), mapped.device_id());

    // Empty UUID should not match.
    assert!(MappedDevice::lookup_by_uuid("").is_none());

    MappedDevice::remove_by_name("dm-uuid-0").unwrap();
}

// ===========================================================================
// Striped target tests
//
// These exercise RAID0-style striping: single stripe behaves like linear,
// multiple stripes distribute data round-robin, bios crossing stripe
// boundaries are split, and invalid stripe sizes are rejected.
// ===========================================================================

/// Test: a single-stripe striped target is equivalent to a linear target.
#[ktest]
fn striped_single_stripe_equivalent_to_linear() {
    ensure_initialized();

    let mock = MockBlockDevice::new("mock-stripe-1", 256);
    let target = StripedTarget::new(vec![(mock.clone(), 0)], 8);
    let mut table = DmTable::new();
    table.add_target(0, 256, DmTarget::Striped(target)).unwrap();
    let mapped = MappedDevice::create("dm-stripe-linear", table).unwrap();

    let pattern: Vec<u8> = (0..BLOCK_SIZE).map(|i| (i % 251) as u8).collect();
    write_blocks(mapped.as_ref(), Bid::new(0), &pattern);

    let direct = mock.read_bytes(0, BLOCK_SIZE);
    assert_eq!(direct, pattern, "single stripe should map to sector 0");

    let read_buf = read_blocks(mapped.as_ref(), Bid::new(0), BLOCK_SIZE);
    assert_eq!(read_buf, pattern);
}

/// Test: a two-stripe target distributes blocks round-robin.
#[ktest]
fn striped_two_stripes_round_robin() {
    ensure_initialized();

    let mock_a = MockBlockDevice::new("mock-stripe-a", 256);
    let mock_b = MockBlockDevice::new("mock-stripe-b", 256);
    let target = StripedTarget::new(
        vec![(mock_a.clone(), 0), (mock_b.clone(), 0)],
        (BLOCK_SIZE / SECTOR_SIZE) as u64,
    );
    let mut table = DmTable::new();
    // 4 stripes = 4 blocks = 32 sectors.
    table.add_target(0, 32, DmTarget::Striped(target)).unwrap();
    let mapped = MappedDevice::create("dm-stripe-rr", table).unwrap();

    let pattern_a: Vec<u8> = (0..BLOCK_SIZE).map(|i| (i % 251) as u8).collect();
    let pattern_b: Vec<u8> = (0..BLOCK_SIZE).map(|i| (i % 253) as u8).collect();
    let pattern_c: Vec<u8> = (0..BLOCK_SIZE).map(|i| (i % 247) as u8).collect();
    let pattern_d: Vec<u8> = (0..BLOCK_SIZE).map(|i| (i % 241) as u8).collect();

    write_blocks(mapped.as_ref(), Bid::new(0), &pattern_a);
    write_blocks(mapped.as_ref(), Bid::new(1), &pattern_b);
    write_blocks(mapped.as_ref(), Bid::new(2), &pattern_c);
    write_blocks(mapped.as_ref(), Bid::new(3), &pattern_d);

    assert_eq!(
        mock_a.read_bytes(0, BLOCK_SIZE),
        pattern_a,
        "stripe 0 -> dev_a"
    );
    assert_eq!(
        mock_b.read_bytes(0, BLOCK_SIZE),
        pattern_b,
        "stripe 1 -> dev_b"
    );
    assert_eq!(
        mock_a.read_bytes(BLOCK_SIZE, BLOCK_SIZE),
        pattern_c,
        "stripe 2 -> dev_a"
    );
    assert_eq!(
        mock_b.read_bytes(BLOCK_SIZE, BLOCK_SIZE),
        pattern_d,
        "stripe 3 -> dev_b"
    );
}

/// Test: a bio spanning a stripe boundary is split across devices.
#[ktest]
fn striped_split_at_stripe_boundary() {
    ensure_initialized();

    let mock_a = MockBlockDevice::new("mock-stripe-split-a", 256);
    let mock_b = MockBlockDevice::new("mock-stripe-split-b", 256);
    let target = StripedTarget::new(
        vec![(mock_a.clone(), 0), (mock_b.clone(), 0)],
        (BLOCK_SIZE / SECTOR_SIZE) as u64,
    );
    let mut table = DmTable::new();
    table.add_target(0, 32, DmTarget::Striped(target)).unwrap();
    let mapped = MappedDevice::create("dm-stripe-split", table).unwrap();

    let pattern_a: Vec<u8> = (0..BLOCK_SIZE).map(|i| (i % 251) as u8).collect();
    let pattern_b: Vec<u8> = (0..BLOCK_SIZE).map(|i| (i % 253) as u8).collect();
    let mut full_pattern = Vec::with_capacity(2 * BLOCK_SIZE);
    full_pattern.extend_from_slice(&pattern_a);
    full_pattern.extend_from_slice(&pattern_b);

    // A two-block write starting at sector 0 crosses the stripe boundary at
    // sector 8 (one block = 8 sectors).
    write_blocks(mapped.as_ref(), Bid::new(0), &full_pattern);

    assert_eq!(
        mock_a.read_bytes(0, BLOCK_SIZE),
        pattern_a,
        "first stripe half should land on dev_a"
    );
    assert_eq!(
        mock_b.read_bytes(0, BLOCK_SIZE),
        pattern_b,
        "second stripe half should land on dev_b"
    );
}

/// Test: the parser rejects a stripe size that is not a power of two.
#[ktest]
fn parser_rejects_non_power_of_two_stripe_size() {
    let table = "dm-bad: 0 16 striped 2 3 8:0 0 8:1 0";
    let result = crate::parser::parse_create_arg(table, 0);
    assert!(
        result.is_err(),
        "stripe size 3 is not a power of two and must be rejected"
    );
}

// ===========================================================================
// Flush propagation and table swap tests
//
// These exercise the control-plane paths: flush must reach every unique
// underlying device, and loading an inactive table followed by resume must
// atomically swap the active table.
// ===========================================================================

/// Test: a flush bio is forwarded to every unique underlying device.
#[ktest]
fn flush_reaches_all_underlying_devices() {
    ensure_initialized();

    let mock_a = MockBlockDevice::new("mock-flush-a", 64);
    let mock_b = MockBlockDevice::new("mock-flush-b", 64);
    let mut table = DmTable::new();
    table
        .add_target(
            0,
            32,
            DmTarget::Linear(LinearTarget::new(mock_a.clone(), 0)),
        )
        .unwrap();
    table
        .add_target(
            32,
            32,
            DmTarget::Linear(LinearTarget::new(mock_b.clone(), 0)),
        )
        .unwrap();
    let mapped = MappedDevice::create("dm-flush-multi", table).unwrap();

    assert_eq!(mock_a.flush_count(), 0);
    assert_eq!(mock_b.flush_count(), 0);

    let bio = Bio::new(BioType::Flush, Sid::new(0), vec![], None);
    let status = bio.submit_and_wait(mapped.as_ref()).unwrap();
    assert_eq!(status, BioStatus::Complete, "flush should complete");

    assert_eq!(mock_a.flush_count(), 1, "mock_a should receive one flush");
    assert_eq!(mock_b.flush_count(), 1, "mock_b should receive one flush");
}

/// Test: loading an inactive table and resuming swaps the active table.
#[ktest]
fn table_swap_via_load_and_resume() {
    ensure_initialized();

    let mock_a = MockBlockDevice::new("mock-swap-a", 256);
    let mock_b = MockBlockDevice::new("mock-swap-b", 256);

    let mut table_a = DmTable::new();
    table_a
        .add_target(
            0,
            256,
            DmTarget::Linear(LinearTarget::new(mock_a.clone(), 0)),
        )
        .unwrap();
    let mapped = MappedDevice::create("dm-table-swap", table_a).unwrap();

    // Suspend, then load a new table pointing to mock_b.
    mapped.set_suspended(true);
    let mut table_b = DmTable::new();
    table_b
        .add_target(
            0,
            256,
            DmTarget::Linear(LinearTarget::new(mock_b.clone(), 0)),
        )
        .unwrap();
    mapped.load_table(table_b).unwrap();

    // Before resume the active table is still table_a.
    let pattern_a: Vec<u8> = (0..BLOCK_SIZE).map(|i| (i % 251) as u8).collect();
    // While suspended, new I/O is deferred; just verify the device state.
    assert!(mapped.is_suspended());

    // Resume swaps the inactive table to active.
    mapped.set_suspended(false);

    // Writes should now land on mock_b, not mock_a.
    write_blocks(mapped.as_ref(), Bid::new(0), &pattern_a);
    assert!(
        mock_a.read_bytes(0, BLOCK_SIZE).iter().all(|&b| b == 0),
        "old target mock_a should not receive writes after swap"
    );
    assert_eq!(
        mock_b.read_bytes(0, BLOCK_SIZE),
        pattern_a,
        "new target mock_b should receive writes after swap"
    );
}

// ---------------------------------------------------------------------------
// Verity textual table parsing tests (`VerityTarget::from_table_args`)
// ---------------------------------------------------------------------------

/// Encodes bytes as lowercase hex, matching `parse_hex_bytes`/`format_params`.
fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

/// Registers a mock device in the block registry so that the textual table
/// parser can resolve it by name or `major:minor`.
fn register_mock(mock: &Arc<MockBlockDevice>) {
    aster_block::register(mock.clone()).expect("register mock block device");
}

/// Builds a verity fixture with the given name prefix and dm-verity version.
///
/// Version 1 hashes as `hash(salt || block)`; version 0 hashes as
/// `hash(block || salt)`. Returns the data device, hash device (tree written
/// top level first at block 0), root digest, and salt.
fn build_verity_fixture_versioned(
    data_name: &str,
    hash_name: &str,
    num_data_blocks: usize,
    version: u8,
) -> (Arc<MockBlockDevice>, Arc<MockBlockDevice>, Vec<u8>, Vec<u8>) {
    assert!(num_data_blocks >= 1, "num_data_blocks must be at least 1");
    let salt = vec![0x5a; 16];
    let digest_size = 32;
    let hashes_per_block = BLOCK_SIZE / digest_size;

    // Generate unique data blocks.
    let mut payloads: Vec<Vec<u8>> = Vec::with_capacity(num_data_blocks);
    for i in 0..num_data_blocks {
        let mut block = vec![0u8; BLOCK_SIZE];
        for (j, b) in block.iter_mut().enumerate() {
            *b = ((i + j) % 256) as u8;
        }
        payloads.push(block);
    }

    let data_sectors = (num_data_blocks * BLOCK_SIZE) / SECTOR_SIZE;
    let data_device = MockBlockDevice::new(data_name, data_sectors);
    {
        let mut data = data_device.data.lock();
        for (i, payload) in payloads.iter().enumerate() {
            let offset = i * BLOCK_SIZE;
            data[offset..offset + BLOCK_SIZE].copy_from_slice(payload);
        }
    }

    // Hash ordering differs between dm-verity versions.
    let leaf_hash = |payload: &[u8]| -> [u8; 32] {
        if version == 0 {
            crate::sha256::digest(&[payload, &salt])
        } else {
            crate::sha256::digest(&[&salt, payload])
        }
    };
    let block_hash = |block: &[u8]| -> [u8; 32] {
        if version == 0 {
            crate::sha256::digest(&[block, &salt])
        } else {
            crate::sha256::digest(&[&salt, block])
        }
    };

    // Compute leaf hashes.
    let mut current_hashes: Vec<[u8; 32]> = payloads.iter().map(|p| leaf_hash(p)).collect();

    // Build hash-tree levels bottom-up.
    let mut level_blocks: Vec<Vec<Vec<u8>>> = Vec::new();
    loop {
        let blocks = pack_hashes_into_blocks(&current_hashes, hashes_per_block, BLOCK_SIZE);
        level_blocks.push(blocks.clone());
        if blocks.len() == 1 {
            break;
        }
        current_hashes = blocks.iter().map(|b| block_hash(b)).collect();
    }

    let root_block = level_blocks.last().unwrap().first().unwrap().clone();
    let root_digest = block_hash(&root_block);

    // Write the hash device with the top level first.
    let total_hash_blocks: usize = level_blocks.iter().map(|l| l.len()).sum();
    let hash_sectors = (total_hash_blocks * BLOCK_SIZE) / SECTOR_SIZE;
    let hash_device = MockBlockDevice::new(hash_name, hash_sectors);
    {
        let mut hash = hash_device.data.lock();
        let mut offset = 0;
        for level in level_blocks.iter().rev() {
            for block in level {
                hash[offset..offset + BLOCK_SIZE].copy_from_slice(block);
                offset += BLOCK_SIZE;
            }
        }
    }

    (data_device, hash_device, root_digest.to_vec(), salt)
}

/// Test: a valid version 1 table parses, round-trips through `params()`, and
/// verifies reads; device references work in both name and `major:minor` form.
#[ktest]
fn verity_from_table_args_parses_valid_table() {
    ensure_initialized();

    let num_blocks = 2usize;
    let (data_dev, hash_dev, root, salt) =
        build_verity_fixture_versioned("vt-ok-data", "vt-ok-hash", num_blocks, 1);
    register_mock(&data_dev);
    register_mock(&hash_dev);
    let root_hex = to_hex(&root);
    let salt_hex = to_hex(&salt);

    let parse_with = |data_ref: &str| {
        let arg_strings = vec![
            "1".to_string(),
            data_ref.to_string(),
            hash_dev.name().to_string(),
            "4096".to_string(),
            "4096".to_string(),
            num_blocks.to_string(),
            "0".to_string(),
            "sha256".to_string(),
            root_hex.clone(),
            salt_hex.clone(),
        ];
        let args: Vec<&str> = arg_strings.iter().map(String::as_str).collect();
        VerityTarget::from_table_args(&args)
    };

    // Reference by device name.
    let target = parse_with(data_dev.name()).expect("valid table by name");
    let expected = format!(
        "1 {}:{} {}:{} 4096 4096 {} 0 sha256 {} {}",
        data_dev.id().major().get(),
        data_dev.id().minor().get(),
        hash_dev.id().major().get(),
        hash_dev.id().minor().get(),
        num_blocks,
        root_hex,
        salt_hex,
    );
    assert_eq!(target.params(), &expected, "params should round-trip");

    // Reference by major:minor.
    let mm_ref = format!(
        "{}:{}",
        data_dev.id().major().get(),
        data_dev.id().minor().get()
    );
    parse_with(&mm_ref).expect("valid table by major:minor");

    // An empty salt is spelled `-` and round-trips as such.
    let arg_strings = vec![
        "1".to_string(),
        data_dev.name().to_string(),
        hash_dev.name().to_string(),
        "4096".to_string(),
        "4096".to_string(),
        num_blocks.to_string(),
        "0".to_string(),
        "sha256".to_string(),
        root_hex.clone(),
        "-".to_string(),
    ];
    let args: Vec<&str> = arg_strings.iter().map(String::as_str).collect();
    let no_salt = VerityTarget::from_table_args(&args).expect("empty salt table");
    assert!(
        no_salt.params().ends_with(" -"),
        "empty salt should format as `-`"
    );

    // The parsed target verifies reads end-to-end.
    let size_sectors = ((num_blocks * BLOCK_SIZE) / SECTOR_SIZE) as u64;
    let mut table = DmTable::new();
    table
        .add_target(0, size_sectors, DmTarget::Verity(Arc::new(target)))
        .unwrap();
    let mapped = MappedDevice::create("dm-vt-args", table).unwrap();
    let segment = BioSegment::alloc(1, BioDirection::FromDevice);
    let bio = Bio::new(BioType::Read, Sid::new(0), vec![segment], None);
    let status = bio.submit_and_wait(mapped.as_ref()).unwrap();
    assert_eq!(
        status,
        BioStatus::Complete,
        "read through a parsed verity target should verify"
    );
}

/// Test: version 0 tables hash as `hash(block || salt)`. A version 0 tree
/// verifies when declared as version 0 and fails when declared as version 1.
#[ktest]
fn verity_from_table_args_version_zero_hash_order() {
    ensure_initialized();

    let num_blocks = 2usize;
    let (data_dev, hash_dev, root, salt) =
        build_verity_fixture_versioned("vt-v0-data", "vt-v0-hash", num_blocks, 0);
    register_mock(&data_dev);
    register_mock(&hash_dev);

    let read_first_block = |name: &str, target: VerityTarget| {
        let size_sectors = ((num_blocks * BLOCK_SIZE) / SECTOR_SIZE) as u64;
        let mut table = DmTable::new();
        table
            .add_target(0, size_sectors, DmTarget::Verity(Arc::new(target)))
            .unwrap();
        let mapped = MappedDevice::create(name, table).unwrap();
        let segment = BioSegment::alloc(1, BioDirection::FromDevice);
        let bio = Bio::new(BioType::Read, Sid::new(0), vec![segment], None);
        bio.submit_and_wait(mapped.as_ref()).unwrap()
    };

    let make_args = |version: &str| {
        let arg_strings = vec![
            version.to_string(),
            data_dev.name().to_string(),
            hash_dev.name().to_string(),
            "4096".to_string(),
            "4096".to_string(),
            num_blocks.to_string(),
            "0".to_string(),
            "sha256".to_string(),
            to_hex(&root),
            to_hex(&salt),
        ];
        let args: Vec<&str> = arg_strings.iter().map(String::as_str).collect();
        VerityTarget::from_table_args(&args)
    };

    // Declared as version 0: the `block || salt` ordering verifies.
    let target_v0 = make_args("0").expect("version 0 table");
    assert!(
        target_v0.params().starts_with("0 "),
        "params should record version 0"
    );
    assert_eq!(
        read_first_block("dm-vt-v0-decl", target_v0),
        BioStatus::Complete,
        "version 0 tree should verify when declared as version 0"
    );

    // The same tree declared as version 1 uses `salt || block` and must fail.
    let target_v1 = make_args("1").expect("version 1 parse of a v0 tree succeeds");
    assert_eq!(
        read_first_block("dm-vt-v1-decl", target_v1),
        BioStatus::IoError,
        "version 0 tree must fail verification when declared as version 1"
    );
}

/// Test: malformed verity tables are rejected at every validation step.
#[ktest]
fn verity_from_table_args_rejects_invalid_tables() {
    ensure_initialized();

    // A registered pair sized for exactly one 4096-byte data/hash block.
    let data_dev = MockBlockDevice::new("vt-rj-data", BLOCK_SIZE / SECTOR_SIZE);
    let hash_dev = MockBlockDevice::new("vt-rj-hash", BLOCK_SIZE / SECTOR_SIZE);
    register_mock(&data_dev);
    register_mock(&hash_dev);
    let hash_dev_small = MockBlockDevice::new("vt-rj-hash-small", 4);
    register_mock(&hash_dev_small);

    let root_hex = to_hex(&[0xabu8; 32]);
    let salt_hex = to_hex(&[0x11u8; 16]);
    let make_args = |overrides: &[(&str, &str)]| -> Vec<String> {
        let mut args = vec![
            "1".to_string(),
            data_dev.name().to_string(),
            hash_dev.name().to_string(),
            "4096".to_string(),
            "4096".to_string(),
            "1".to_string(),
            "0".to_string(),
            "sha256".to_string(),
            root_hex.clone(),
            salt_hex.clone(),
        ];
        for (idx, val) in overrides {
            args[idx.parse::<usize>().unwrap()] = val.to_string();
        }
        args
    };
    let parse = |args: &[String]| {
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        VerityTarget::from_table_args(&refs)
    };
    let expect_err = |args: Vec<String>, what: &str| {
        assert!(parse(&args).is_err(), "{what} should be rejected");
    };

    // Sanity: the base table is valid.
    assert!(parse(&make_args(&[])).is_ok(), "base table should parse");

    // Argument count and the optional parameters slot.
    let mut short = make_args(&[]);
    short.truncate(9);
    expect_err(short, "9 arguments");
    let mut long = make_args(&[]);
    long.push("0".to_string());
    long.push("0".to_string());
    expect_err(long, "12 arguments");
    let mut bad_opt = make_args(&[]);
    bad_opt.push("1".to_string());
    expect_err(bad_opt, "nonzero optional argument");
    let mut ok_opt = make_args(&[]);
    ok_opt.push("0".to_string());
    assert!(
        parse(&ok_opt).is_ok(),
        "explicit zero optional argument should parse"
    );

    // Version.
    expect_err(make_args(&[("0", "2")]), "version 2");
    expect_err(make_args(&[("0", "x")]), "non-numeric version");

    // Unknown device reference.
    expect_err(
        make_args(&[("1", "no-such-verity-device")]),
        "unknown device",
    );

    // Block sizes: zero, not a sector multiple, not a power of two.
    expect_err(make_args(&[("3", "0")]), "zero data block size");
    expect_err(
        make_args(&[("3", "1000")]),
        "data block size not a sector multiple",
    );
    expect_err(
        make_args(&[("3", "12288")]),
        "data block size not a power of two",
    );
    expect_err(
        make_args(&[("4", "256")]),
        "hash block size below one sector",
    );

    // Data block count: zero, non-numeric, exceeding the data device.
    expect_err(make_args(&[("5", "0")]), "zero data block count");
    expect_err(make_args(&[("5", "x")]), "non-numeric data block count");
    expect_err(
        make_args(&[("5", "2")]),
        "geometry larger than the data device",
    );

    // Hash start block.
    expect_err(make_args(&[("6", "x")]), "non-numeric hash start block");
    expect_err(make_args(&[("6", "-1")]), "negative hash start block");

    // Unknown algorithm.
    expect_err(make_args(&[("7", "sha1")]), "unsupported algorithm");

    // Root digest: invalid hex, odd length, wrong length.
    expect_err(make_args(&[("8", "zz")]), "non-hex root digest");
    expect_err(make_args(&[("8", "0")]), "odd-length root digest");
    expect_err(
        make_args(&[("8", &to_hex(&[0xccu8; 16]))]),
        "wrong-length root digest",
    );

    // Salt: invalid hex, odd length. `-` (empty) is accepted.
    expect_err(make_args(&[("9", "zz")]), "non-hex salt");
    expect_err(make_args(&[("9", "0")]), "odd-length salt");
    assert!(
        parse(&make_args(&[("9", "-")])).is_ok(),
        "empty salt should parse"
    );

    // Geometry exceeding the hash device.
    expect_err(
        make_args(&[("2", hash_dev_small.name())]),
        "geometry larger than the hash device",
    );
}

// ---------------------------------------------------------------------------
// Suspend drain/no-flush, nested dm, concurrent I/O, and error recovery tests
// ---------------------------------------------------------------------------

/// A block device that queues bios without completing them.
///
/// `enqueue` stores the submitted bio in a pending list; the test completes
/// them explicitly via `complete_pending`. This lets tests hold I/O in
/// flight, which synchronous mocks cannot do.
struct DeferredBlockDevice {
    id: DeviceId,
    name: String,
    nr_sectors: usize,
    pending: SpinLock<Vec<SubmittedBio>>,
}

impl DeferredBlockDevice {
    fn new(name: &str, nr_sectors: usize) -> Arc<Self> {
        let minor = MOCK_MINOR_COUNTER.fetch_add(1, Ordering::Relaxed);
        Arc::new(Self {
            id: DeviceId::new(MajorId::new(240), MinorId::new(minor)),
            name: String::from(name),
            nr_sectors,
            pending: SpinLock::new(Vec::new()),
        })
    }

    fn pending_count(&self) -> usize {
        self.pending.lock().len()
    }

    /// Completes every queued bio with `BioStatus::Complete`.
    fn complete_pending(&self) {
        let pending = core::mem::take(&mut *self.pending.lock());
        for bio in pending {
            bio.complete(BioStatus::Complete);
        }
    }
}

impl BlockDevice for DeferredBlockDevice {
    fn enqueue(&self, bio: SubmittedBio) -> Result<(), BioEnqueueError> {
        self.pending.lock().push(bio);
        Ok(())
    }

    fn metadata(&self) -> BlockDeviceMeta {
        BlockDeviceMeta {
            max_nr_segments_per_bio: usize::MAX,
            nr_sectors: self.nr_sectors,
        }
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn id(&self) -> DeviceId {
        self.id
    }
}

impl core::fmt::Debug for DeferredBlockDevice {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        f.debug_struct("DeferredBlockDevice")
            .field("name", &self.name)
            .field("nr_sectors", &self.nr_sectors)
            .finish()
    }
}

/// Test: `set_suspended(true)` blocks new I/O immediately but returns only
/// after all in-flight bios have completed (drain).
#[ktest]
fn suspend_waits_for_in_flight_io() {
    ensure_initialized();

    let backing = DeferredBlockDevice::new("mock-deferred", 256);
    let mut table = DmTable::new();
    table
        .add_target(
            0,
            256,
            DmTarget::Linear(LinearTarget::new(backing.clone(), 0)),
        )
        .unwrap();
    let mapped = MappedDevice::create("dm-drain-0", table).unwrap();

    // Submit a read asynchronously; the backing device holds it in flight.
    let segment = BioSegment::alloc(1, BioDirection::FromDevice);
    let bio = Bio::new(BioType::Read, Sid::new(0), vec![segment], None);
    let mut io_batch = IoBatch::new();
    bio.submit(mapped.as_ref(), &mut io_batch).unwrap();
    assert_eq!(mapped.in_flight(), 1);
    assert_eq!(backing.pending_count(), 1);

    // Suspend from another task: the flag flips immediately, but the call
    // must not return while the read is still in flight.
    let suspend_done = Arc::new(AtomicBool::new(false));
    let suspend_done_child = suspend_done.clone();
    let mapped_child = mapped.clone();
    TaskOptions::new(move || {
        mapped_child.set_suspended(true);
        suspend_done_child.store(true, Ordering::Release);
    })
    .data(())
    .spawn()
    .unwrap();

    // Wait until the suspending task has set the flag.
    let mut saw_suspended = false;
    for _ in 0..100 {
        Task::yield_now();
        if mapped.is_suspended() {
            saw_suspended = true;
            break;
        }
    }
    assert!(saw_suspended, "suspending task should set the flag");
    assert!(
        !suspend_done.load(Ordering::Acquire),
        "suspend must not return while I/O is in flight"
    );
    assert_eq!(mapped.in_flight(), 1);

    // Complete the pending read; the drain unblocks the suspend.
    backing.complete_pending();
    io_batch.wait_all().unwrap();
    assert_eq!(mapped.in_flight(), 0);

    let mut finished = false;
    for _ in 0..1000 {
        Task::yield_now();
        if suspend_done.load(Ordering::Acquire) {
            finished = true;
            break;
        }
    }
    assert!(finished, "suspend should return once in-flight I/O drains");
}

/// Test: `set_suspended_no_flush(true)` returns immediately without waiting
/// for in-flight I/O, refuses new I/O while suspended, and resume restores
/// service.
#[ktest]
fn suspend_no_flush_skips_drain() {
    ensure_initialized();

    let backing = DeferredBlockDevice::new("mock-noflush", 256);
    let mut table = DmTable::new();
    table
        .add_target(
            0,
            256,
            DmTarget::Linear(LinearTarget::new(backing.clone(), 0)),
        )
        .unwrap();
    let mapped = MappedDevice::create("dm-noflush-0", table).unwrap();

    // Hold a read in flight.
    let segment = BioSegment::alloc(1, BioDirection::FromDevice);
    let bio = Bio::new(BioType::Read, Sid::new(0), vec![segment], None);
    let mut io_batch = IoBatch::new();
    bio.submit(mapped.as_ref(), &mut io_batch).unwrap();
    assert_eq!(mapped.in_flight(), 1);

    // No-flush suspend returns immediately even though I/O is in flight.
    mapped.set_suspended_no_flush(true);
    assert!(mapped.is_suspended());
    assert_eq!(
        mapped.in_flight(),
        1,
        "no-flush suspend must not drain in-flight I/O"
    );

    // New I/O is deferred (not refused) while suspended.
    let segment = BioSegment::alloc(1, BioDirection::FromDevice);
    let bio2 = Bio::new(BioType::Read, Sid::new(8), vec![segment], None);
    let mut io_batch2 = IoBatch::new();
    assert!(
        bio2.submit(mapped.as_ref(), &mut io_batch2).is_ok(),
        "suspended device should defer I/O, not refuse it"
    );
    assert_eq!(mapped.deferred_len(), 1);

    // The still-pending read completes normally through the deferred device.
    backing.complete_pending();
    io_batch.wait_all().unwrap();
    assert_eq!(mapped.in_flight(), 0);

    // Resume replays the deferred bio; the device accepts new I/O again.
    mapped.set_suspended(false);
    assert!(!mapped.is_suspended());
    assert_eq!(mapped.deferred_len(), 0);
    // The replayed bio is now pending on the backing device.
    backing.complete_pending();
    io_batch2.wait_all().unwrap();

    let segment = BioSegment::alloc(1, BioDirection::FromDevice);
    let bio = Bio::new(BioType::Read, Sid::new(16), vec![segment], None);
    let mut io_batch = IoBatch::new();
    bio.submit(mapped.as_ref(), &mut io_batch).unwrap();
    assert_eq!(backing.pending_count(), 1);
    backing.complete_pending();
    io_batch.wait_all().unwrap();
}

/// Test: a dm device can back another dm device; sector addressing and
/// flush propagation compose correctly through both levels.
#[ktest]
fn nested_dm_linear_on_linear() {
    ensure_initialized();

    let bottom = MockBlockDevice::new("mock-bottom", 256);

    // inner: logical [0,128) -> bottom sectors [100,228).
    let mut inner_table = DmTable::new();
    inner_table
        .add_target(
            0,
            128,
            DmTarget::Linear(LinearTarget::new(bottom.clone(), 100)),
        )
        .unwrap();
    let inner = MappedDevice::create("dm-nested-inner", inner_table).unwrap();

    // outer: [0,64) -> bottom directly, [64,192) -> inner.
    let mut outer_table = DmTable::new();
    outer_table
        .add_target(
            0,
            64,
            DmTarget::Linear(LinearTarget::new(bottom.clone(), 0)),
        )
        .unwrap();
    outer_table
        .add_target(
            64,
            128,
            DmTarget::Linear(LinearTarget::new(inner.clone(), 0)),
        )
        .unwrap();
    let outer = MappedDevice::create("dm-nested-outer", outer_table).unwrap();

    // Write 24 blocks (192 sectors) covering the whole outer device.
    let pattern: Vec<u8> = (0..24 * BLOCK_SIZE).map(|i| (i % 253) as u8).collect();
    write_blocks(outer.as_ref(), Bid::new(0), &pattern);

    // First target: outer [0,64) lands on bottom [0,64).
    assert_eq!(
        bottom.read_bytes(0, 64 * SECTOR_SIZE),
        pattern[0..64 * SECTOR_SIZE],
        "outer [0,64) should map to bottom [0,64)"
    );

    // The gap between the two bottom ranges must stay untouched.
    assert!(
        bottom
            .read_bytes(64 * SECTOR_SIZE, 36 * SECTOR_SIZE)
            .iter()
            .all(|&b| b == 0),
        "bottom [64,100) must remain untouched"
    );

    // Second target: outer [64,192) -> inner [0,128) -> bottom [100,228).
    assert_eq!(
        bottom.read_bytes(100 * SECTOR_SIZE, 128 * SECTOR_SIZE),
        pattern[64 * SECTOR_SIZE..192 * SECTOR_SIZE],
        "outer [64,192) should map through inner to bottom [100,228)"
    );

    // Flush propagates through both levels: bottom receives one flush from
    // the outer table's direct target and one forwarded through inner.
    let flushes_before = bottom.flush_count();
    let bio = Bio::new(BioType::Flush, Sid::new(0), vec![], None);
    let status = bio.submit_and_wait(outer.as_ref()).unwrap();
    assert_eq!(status, BioStatus::Complete);
    assert_eq!(
        bottom.flush_count() - flushes_before,
        2,
        "flush should reach bottom via both the direct target and inner"
    );
}

/// Test: concurrent I/O from multiple kernel tasks to disjoint regions of
/// the same mapped device all complete with correct data, and the in-flight
/// counter returns to zero.
#[ktest]
fn concurrent_io_to_disjoint_regions() {
    ensure_initialized();

    const TASKS: usize = 8;
    const BLOCKS_PER_TASK: usize = 4;
    let sectors_per_block = BLOCK_SIZE / SECTOR_SIZE;
    let nr_sectors = TASKS * BLOCKS_PER_TASK * sectors_per_block;
    let mock = MockBlockDevice::new("mock-conc", nr_sectors);
    let mut table = DmTable::new();
    table
        .add_target(
            0,
            nr_sectors as u64,
            DmTarget::Linear(LinearTarget::new(mock.clone(), 0)),
        )
        .unwrap();
    let mapped = MappedDevice::create("dm-conc-0", table).unwrap();

    let mismatches = Arc::new(AtomicUsize::new(0));
    let done = Arc::new(AtomicUsize::new(0));
    for t in 0..TASKS {
        let mapped = mapped.clone();
        let mismatches = mismatches.clone();
        let done = done.clone();
        TaskOptions::new(move || {
            let base_bid = (t * BLOCKS_PER_TASK) as u64;
            let pattern: Vec<u8> = (0..BLOCKS_PER_TASK * BLOCK_SIZE)
                .map(|i| ((i + t) % 251) as u8)
                .collect();
            write_blocks(mapped.as_ref(), Bid::new(base_bid), &pattern);
            let read_back = read_blocks(
                mapped.as_ref(),
                Bid::new(base_bid),
                BLOCKS_PER_TASK * BLOCK_SIZE,
            );
            if read_back != pattern {
                mismatches.fetch_add(1, Ordering::AcqRel);
            }
            done.fetch_add(1, Ordering::AcqRel);
        })
        .data(())
        .spawn()
        .unwrap();
    }

    let mut all_done = false;
    for _ in 0..100_000 {
        Task::yield_now();
        if done.load(Ordering::Acquire) == TASKS {
            all_done = true;
            break;
        }
    }
    assert!(all_done, "all concurrent tasks should finish");
    assert_eq!(mismatches.load(Ordering::Acquire), 0);
    assert_eq!(
        mapped.in_flight(),
        0,
        "in-flight counter must return to zero"
    );

    // Data from every task landed in its own region.
    for t in 0..TASKS {
        let base = t * BLOCKS_PER_TASK * BLOCK_SIZE;
        let pattern: Vec<u8> = (0..BLOCKS_PER_TASK * BLOCK_SIZE)
            .map(|i| ((i + t) % 251) as u8)
            .collect();
        assert_eq!(
            mock.read_bytes(base, pattern.len()),
            pattern,
            "task {t} region should hold its pattern"
        );
    }
}

/// Test: a flush through a single-target dm whose backing device refuses
/// enqueue fails with `Refused`, and the in-flight counter stays balanced.
#[ktest]
fn flush_to_refusing_backing_is_refused() {
    ensure_initialized();

    let backing = RefusingBlockDevice::new("mock-flush-refuse", 256);
    let mut table = DmTable::new();
    table
        .add_target(0, 256, DmTarget::Linear(LinearTarget::new(backing, 0)))
        .unwrap();
    let mapped = MappedDevice::create("dm-flush-refuse", table).unwrap();

    let bio = Bio::new(BioType::Flush, Sid::new(0), vec![], None);
    assert_eq!(
        bio.submit_and_wait(mapped.as_ref()),
        Err(BioEnqueueError::Refused),
    );
    assert_eq!(mapped.in_flight(), 0);
}

/// Test: with multiple underlying devices, a flush fails with `IoError` if
/// any backing device refuses it; healthy devices are still flushed.
#[ktest]
fn flush_aggregates_partial_failure() {
    ensure_initialized();

    let ok = MockBlockDevice::new("mock-flush-ok", 128);
    let bad = RefusingBlockDevice::new("mock-flush-bad", 128);
    let mut table = DmTable::new();
    table
        .add_target(0, 128, DmTarget::Linear(LinearTarget::new(ok.clone(), 0)))
        .unwrap();
    table
        .add_target(128, 128, DmTarget::Linear(LinearTarget::new(bad, 0)))
        .unwrap();
    let mapped = MappedDevice::create("dm-flush-partial", table).unwrap();

    let bio = Bio::new(BioType::Flush, Sid::new(0), vec![], None);
    let status = bio.submit_and_wait(mapped.as_ref()).unwrap();
    assert_eq!(
        status,
        BioStatus::IoError,
        "flush must report IoError when any underlying device fails"
    );
    assert_eq!(mapped.in_flight(), 0);
    assert_eq!(
        ok.flush_count(),
        1,
        "healthy device should still be flushed"
    );
}

/// Test: a read past the verity data area completes with `IoError` instead
/// of returning unverified data.
#[ktest]
fn verity_read_beyond_data_blocks_fails() {
    ensure_initialized();

    let num_blocks = 1usize;
    let (data_dev, hash_dev, root, salt) =
        build_verity_fixture_versioned("vt-oob-data", "vt-oob-hash", num_blocks, 1);
    register_mock(&data_dev);
    register_mock(&hash_dev);

    let arg_strings = vec![
        "1".to_string(),
        data_dev.name().to_string(),
        hash_dev.name().to_string(),
        "4096".to_string(),
        "4096".to_string(),
        num_blocks.to_string(),
        "0".to_string(),
        "sha256".to_string(),
        to_hex(&root),
        to_hex(&salt),
    ];
    let args: Vec<&str> = arg_strings.iter().map(String::as_str).collect();
    let target = VerityTarget::from_table_args(&args).unwrap();

    // The table claims two blocks although the target only covers one.
    let mut table = DmTable::new();
    table
        .add_target(0, 16, DmTarget::Verity(Arc::new(target)))
        .unwrap();
    let mapped = MappedDevice::create("dm-vt-oob", table).unwrap();

    // Sector 0 (block 0) verifies normally.
    let segment = BioSegment::alloc(1, BioDirection::FromDevice);
    let bio = Bio::new(BioType::Read, Sid::new(0), vec![segment], None);
    assert_eq!(
        bio.submit_and_wait(mapped.as_ref()).unwrap(),
        BioStatus::Complete,
    );

    // Sector 8 (block 1) is past `num_data_blocks` and must fail.
    let segment = BioSegment::alloc(1, BioDirection::FromDevice);
    let bio = Bio::new(BioType::Read, Sid::new(8), vec![segment], None);
    assert_eq!(
        bio.submit_and_wait(mapped.as_ref()).unwrap(),
        BioStatus::IoError,
        "read past the verity data area must fail with IoError"
    );
}

// ===========================================================================
// Read-only enforcement tests (P0-2)
//
// A device marked read-only must refuse writes while still allowing reads
// and flushes. These cases previously had zero coverage.
// ===========================================================================

/// Test: a read-only mapped device refuses writes but allows reads and
/// flushes, and writability is restored after clearing the flag.
#[ktest]
fn readonly_device_refuses_writes() {
    ensure_initialized();

    let mock = MockBlockDevice::new("mock-ro", 256);
    let mut table = DmTable::new();
    table
        .add_target(0, 256, DmTarget::Linear(LinearTarget::new(mock.clone(), 0)))
        .unwrap();
    let mapped = MappedDevice::create("dm-ro-0", table).unwrap();

    // Mark the device read-only.
    mapped.set_readonly(true);
    assert!(mapped.is_readonly());

    // Writes must be refused at the dm enqueue layer (never reach the mock).
    let segment = BioSegment::alloc(1, BioDirection::ToDevice);
    let bio = Bio::new(BioType::Write, Sid::new(0), vec![segment], None);
    assert_eq!(
        bio.submit_and_wait(mapped.as_ref()),
        Err(BioEnqueueError::Refused),
        "read-only device must refuse writes"
    );

    // Reads are still allowed.
    let segment = BioSegment::alloc(1, BioDirection::FromDevice);
    let bio = Bio::new(BioType::Read, Sid::new(0), vec![segment], None);
    assert_eq!(
        bio.submit_and_wait(mapped.as_ref()),
        Ok(BioStatus::Complete),
        "read-only device must still allow reads"
    );

    // Flushes are still allowed (a read-only device may need to flush
    // already-persisted data).
    let bio = Bio::new(BioType::Flush, Sid::new(0), vec![], None);
    assert_eq!(
        bio.submit_and_wait(mapped.as_ref()),
        Ok(BioStatus::Complete),
        "read-only device must still allow flushes"
    );

    // Clearing the flag restores writability.
    mapped.set_readonly(false);
    assert!(!mapped.is_readonly());
    let pattern: Vec<u8> = (0..BLOCK_SIZE).map(|i| (i % 251) as u8).collect();
    write_blocks(mapped.as_ref(), Bid::new(0), &pattern);
    assert_eq!(
        mock.read_bytes(0, BLOCK_SIZE),
        pattern,
        "write after clearing read-only should reach the backing device"
    );
}

// ===========================================================================
// open/close counter tests
// ===========================================================================

/// Test: `open`/`close` maintain the open count, and an unbalanced `close`
/// saturates at zero instead of wrapping around to `u32::MAX`.
#[ktest]
fn open_close_count_is_balanced() {
    ensure_initialized();

    let mock = MockBlockDevice::new("mock-oc", 64);
    let mut table = DmTable::new();
    table
        .add_target(0, 64, DmTarget::Linear(LinearTarget::new(mock, 0)))
        .unwrap();
    let mapped = MappedDevice::create("dm-oc-0", table).unwrap();

    assert_eq!(mapped.open_count(), 0, "fresh device has zero open count");

    mapped.open().unwrap();
    assert_eq!(mapped.open_count(), 1);
    mapped.open().unwrap();
    assert_eq!(mapped.open_count(), 2);

    mapped.close();
    assert_eq!(mapped.open_count(), 1);
    mapped.close();
    assert_eq!(mapped.open_count(), 0);

    // Extra (unbalanced) closes must not wrap the counter to u32::MAX.
    mapped.close();
    assert_eq!(
        mapped.open_count(),
        0,
        "unbalanced close must saturate at 0"
    );
    mapped.close();
    assert_eq!(mapped.open_count(), 0);
}

// ===========================================================================
// Deferred remove (DM_DEFERRED_REMOVE) tests
// ===========================================================================

/// Test: a device marked for deferred removal stays registered while open,
/// and the last `close` completes the removal and recycles the minor.
#[ktest]
fn deferred_remove_completes_on_last_close() {
    ensure_initialized();

    let mock = MockBlockDevice::new("mock-dr", 64);
    let mut table = DmTable::new();
    table
        .add_target(0, 64, DmTarget::Linear(LinearTarget::new(mock, 0)))
        .unwrap();
    let mapped = MappedDevice::create("dm-dr-0", table).unwrap();
    let id = mapped.device_id();
    let minor = id.minor().get();

    mapped.open().unwrap();
    mapped.open().unwrap();

    // DM_DEV_REMOVE with DM_DEFERRED_REMOVE on a busy device registers the
    // removal; the device stays listed until the last close.
    mapped.mark_deferred_remove();
    assert!(mapped.is_deferred_remove_pending());
    assert!(
        MappedDevice::lookup_by_name("dm-dr-0").is_some(),
        "deferred-remove device must stay visible while open"
    );

    mapped.close();
    assert_eq!(mapped.open_count(), 1);
    assert!(
        MappedDevice::lookup_by_name("dm-dr-0").is_some(),
        "a non-final close must not remove the device"
    );

    mapped.close();
    assert_eq!(mapped.open_count(), 0);
    assert!(
        MappedDevice::lookup_by_name("dm-dr-0").is_none(),
        "the last close must complete the deferred removal"
    );
    assert!(
        aster_block::lookup(id).is_none(),
        "the device must be unregistered from the block layer"
    );

    // The minor is recycled: creating a device with the same explicit minor
    // must succeed.
    let recycled = crate::manager()
        .create("dm-dr-recycle", None::<Arc<str>>, Some(minor))
        .expect("the minor of the removed device must be recycled");
    assert_eq!(recycled.device_id().minor().get(), minor);
    MappedDevice::remove_by_name("dm-dr-recycle").unwrap();
}

/// Test: if the block layer still reports the device busy at the last
/// `close` (e.g. an outstanding lease), the deferred flag is kept and the
/// device survives until a later removal succeeds.
#[ktest]
fn deferred_remove_kept_when_block_layer_busy() {
    ensure_initialized();

    let mock = MockBlockDevice::new("mock-drb", 64);
    let mut table = DmTable::new();
    table
        .add_target(0, 64, DmTarget::Linear(LinearTarget::new(mock, 0)))
        .unwrap();
    let mapped = MappedDevice::create("dm-dr-busy", table).unwrap();

    mapped.open().unwrap();
    mapped.mark_deferred_remove();

    // Simulate a holder (e.g. a nested DM target) keeping a lease on the
    // device while the last userspace close happens.
    let lease = aster_block::lookup_lease(mapped.device_id()).unwrap();
    mapped.close();
    assert_eq!(mapped.open_count(), 0);
    assert!(
        mapped.is_deferred_remove_pending(),
        "the deferred flag must be kept when the removal cannot complete"
    );
    assert!(
        MappedDevice::lookup_by_name("dm-dr-busy").is_some(),
        "the device must survive when the block layer reports busy"
    );

    // Once the holder releases the lease, a later removal succeeds.
    drop(lease);
    MappedDevice::remove_by_name("dm-dr-busy").unwrap();
    assert!(MappedDevice::lookup_by_name("dm-dr-busy").is_none());
}

// ===========================================================================
// Table lookup and construction tests
// ===========================================================================

/// Test: `find_target` routes sectors to the correct entry via binary
/// search, including exact boundaries and the out-of-range case.
#[ktest]
fn find_target_binary_search_boundaries() {
    let mock = MockBlockDevice::new("mock-lookup", 256);
    let mut table = DmTable::new();
    // [0, 8) linear, [8, 16) zero, [16, 24) error.
    table
        .add_target(0, 8, DmTarget::Linear(LinearTarget::new(mock.clone(), 0)))
        .unwrap();
    table
        .add_target(
            8,
            8,
            DmTarget::Zero(crate::targets::zero::ZeroTarget::new(8)),
        )
        .unwrap();
    table
        .add_target(
            16,
            8,
            DmTarget::Error(crate::targets::error::ErrorTarget::new(8)),
        )
        .unwrap();

    // First target: start and last-sector-before-boundary.
    assert!(matches!(table.find_target(0), Some(DmTarget::Linear(_))));
    assert!(matches!(table.find_target(7), Some(DmTarget::Linear(_))));

    // Second target boundary (sector 8 is its first sector).
    assert!(matches!(table.find_target(8), Some(DmTarget::Zero(_))));
    assert!(matches!(table.find_target(15), Some(DmTarget::Zero(_))));

    // Third target.
    assert!(matches!(table.find_target(16), Some(DmTarget::Error(_))));
    assert!(matches!(table.find_target(23), Some(DmTarget::Error(_))));

    // Past the end of the table: no target.
    assert!(table.find_target(24).is_none());
    assert!(table.find_target(u64::MAX).is_none());
}

/// Test: `add_target` rejects zero-length entries, non-contiguous entries,
/// and total sector counts that overflow.
#[ktest]
fn add_target_rejects_invalid_tables() {
    let mock = MockBlockDevice::new("mock-invalid", 256);

    // Zero-length target.
    let mut table = DmTable::new();
    assert!(matches!(
        table.add_target(0, 0, DmTarget::Linear(LinearTarget::new(mock.clone(), 0))),
        Err(TableError::ZeroLength)
    ));

    // Non-contiguous: a gap (or overlap) between entries.
    let mut table = DmTable::new();
    table
        .add_target(0, 8, DmTarget::Linear(LinearTarget::new(mock.clone(), 0)))
        .unwrap();
    assert!(matches!(
        table.add_target(
            100,
            8,
            DmTarget::Zero(crate::targets::zero::ZeroTarget::new(8))
        ),
        Err(TableError::NotContiguous)
    ));

    // Overflow: two entries whose combined length exceeds u64::MAX.
    let half = 1u64 << 63;
    let mut table = DmTable::new();
    table
        .add_target(
            0,
            half,
            DmTarget::Linear(LinearTarget::new(mock.clone(), 0)),
        )
        .unwrap();
    assert!(
        matches!(
            table.add_target(half, half, DmTarget::Linear(LinearTarget::new(mock, 0))),
            Err(TableError::TooLarge)
        ),
        "combined length wrapping past u64::MAX must be TooLarge"
    );
}

// ===========================================================================
// P0/P1 regression tests (postponed replay, split completion order, failure
// injection, cross-mapper concurrency) — added 2026-09-15 per dm-overview.md
// appendix B/P3 item 22.
// ===========================================================================

/// Test (P1): a bio submitted while the device is Suspending/Suspended is
/// deferred (postponed) and replayed against the **new** active table after
/// resume. This is the core P1 semantic fix: Linux queues `dm_submit_bio`
/// into `md->deferred` and replays on resume; Asterinas matches this via
/// `DmIoState::push_deferred` + `take_deferred` in `set_suspended(false)`.
///
/// Without this test, regressions that silently re-introduce the old "Refused
/// during suspend" behavior (filesystem sees fake EIO) would not be caught.
#[ktest]
fn postponed_bio_replays_against_new_table_after_resume() {
    ensure_initialized();

    // Backing A: initial table. Backing B: post-resume table.
    let mock_a = MockBlockDevice::new("mock-post-a", 256);
    let mock_b = MockBlockDevice::new("mock-post-b", 256);

    let mut table_a = DmTable::new();
    table_a
        .add_target(0, 256, DmTarget::Linear(LinearTarget::new(mock_a.clone(), 0)))
        .unwrap();
    let mapped = MappedDevice::create("dm-post-0", table_a).unwrap();

    // Suspend (no in-flight I/O, so drain returns immediately).
    mapped.set_suspended(true);
    assert!(mapped.is_suspended());

    // Load a new inactive table pointing to mock_b.
    let mut table_b = DmTable::new();
    table_b
        .add_target(0, 256, DmTarget::Linear(LinearTarget::new(mock_b.clone(), 0)))
        .unwrap();
    mapped.load_table(table_b).unwrap();

    // Submit a write while suspended — it must be deferred, not refused.
    let segment = BioSegment::alloc(1, BioDirection::ToDevice);
    let pattern: Vec<u8> = (0..BLOCK_SIZE).map(|i| (i % 239) as u8).collect();
    segment
        .write(0, &mut VmReader::from(pattern.as_slice()).to_fallible())
        .unwrap();
    let bio = Bio::new(BioType::Write, Sid::new(0), vec![segment], None);
    let mut io_batch = IoBatch::new();
    bio.submit(mapped.as_ref(), &mut io_batch).unwrap();

    // No I/O has reached either backing device yet.
    assert!(
        mock_a.read_bytes(0, BLOCK_SIZE).iter().all(|&b| b == 0),
        "old target must not receive writes while suspended"
    );
    assert!(
        mock_b.read_bytes(0, BLOCK_SIZE).iter().all(|&b| b == 0),
        "new target must not receive writes until resume replays"
    );
    assert_eq!(
        mapped.deferred_len(),
        1,
        "bio should be in the deferred queue, not submitted to the active table"
    );

    // Resume: swaps inactive→active, then replays deferred bios against the
    // new table.
    mapped.set_suspended(false);
    io_batch.wait_all().unwrap();

    assert!(
        mock_a.read_bytes(0, BLOCK_SIZE).iter().all(|&b| b == 0),
        "old target must remain untouched after resume (deferred replay uses new table)"
    );
    assert_eq!(
        mock_b.read_bytes(0, BLOCK_SIZE),
        pattern,
        "deferred write must replay against the new active table"
    );
    assert_eq!(mapped.in_flight(), 0);
}

/// Test (P1): a split bio's parent completes correctly regardless of the
/// order in which its child sub-bios complete. The completion aggregator
/// must not assume in-order completion; any ordering must yield the same
/// final status (Complete when all succeed).
///
/// This guards the P1 fix where suspend TOCTOU was eliminated by packing
/// the suspended bit and in-flight count into a single `AtomicU32`; the
/// same atomic decrement path is used by split bio completion, so a
/// regression here would indicate the counter is not being updated
/// correctly under out-of-order completion.
#[ktest]
fn split_bio_completes_regardless_of_child_order() {
    ensure_initialized();

    // Two-backing-device table; a bio spanning both targets is split.
    let mock_a = DeferredBlockDevice::new("mock-split-a", 128);
    let mock_b = DeferredBlockDevice::new("mock-split-b", 128);
    let mut table = DmTable::new();
    table
        .add_target(0, 128, DmTarget::Linear(LinearTarget::new(mock_a.clone(), 0)))
        .unwrap();
    table
        .add_target(128, 128, DmTarget::Linear(LinearTarget::new(mock_b.clone(), 0)))
        .unwrap();
    let mapped = MappedDevice::create("dm-split-ord", table).unwrap();

    // Submit a read spanning both targets (BLOCK_SIZE = 8 sectors; submit
    // 32 sectors = 4 blocks). Start at sector 112 so the bio crosses the
    // 128-sector target boundary: [112,128) → target A, [128,144) → target B.
    let nblocks = 4;
    let segment = BioSegment::alloc(nblocks, BioDirection::FromDevice);
    let bio = Bio::new(BioType::Read, Sid::new(112), vec![segment], None);
    let mut io_batch = IoBatch::new();
    bio.submit(mapped.as_ref(), &mut io_batch).unwrap();

    // Both backings hold one sub-bio each.
    assert_eq!(mock_a.pending_count(), 1);
    assert_eq!(mock_b.pending_count(), 1);
    assert_eq!(
        mapped.in_flight(),
        1,
        "parent bio in-flight until all children done"
    );

    // Complete in reverse order (B first, then A). The parent must still
    // complete successfully.
    mock_b.complete_pending();
    // Parent not done yet; only one child has completed.
    assert_eq!(
        mapped.in_flight(),
        1,
        "parent still in-flight while one child pending"
    );

    mock_a.complete_pending();
    io_batch.wait_all().unwrap();
    assert_eq!(
        mapped.in_flight(),
        0,
        "parent must complete once all children complete"
    );
}

/// Test (P1): when one child of a split bio fails, the parent bio's status
/// becomes `IoError` even if other children succeed. This matches Linux's
/// `dm_end_io` semantics: a single sub-bio error propagates to the parent.
///
/// Combined with the previous test, this verifies the completion aggregator
/// handles both success and failure paths under arbitrary ordering.
#[ktest]
fn split_bio_propagates_child_failure_to_parent() {
    ensure_initialized();

    let mock_ok = DeferredBlockDevice::new("mock-fail-ok", 128);
    let mock_bad = RefusingBlockDevice::new("mock-fail-bad", 128);
    let mut table = DmTable::new();
    table
        .add_target(0, 128, DmTarget::Linear(LinearTarget::new(mock_ok.clone(), 0)))
        .unwrap();
    table
        .add_target(128, 128, DmTarget::Linear(LinearTarget::new(mock_bad, 0)))
        .unwrap();
    let mapped = MappedDevice::create("dm-fail-split", table).unwrap();

    // Submit a read spanning both targets. The first half lands on mock_ok
    // (deferred); the second half lands on mock_bad which refuses enqueue.
    // We cannot use submit_and_wait here because mock_ok's child is
    // deferred and will not complete until complete_pending() is called.
    // Instead, submit the bio, manually complete the deferred child, then
    // wait for the parent to finish.
    let nblocks = 4;
    let segment = BioSegment::alloc(nblocks, BioDirection::FromDevice);
    let bio = Bio::new(BioType::Read, Sid::new(112), vec![segment], None);
    let mut io_batch = IoBatch::new();
    bio.submit(mapped.as_ref(), &mut io_batch).unwrap();

    // Complete the deferred child on mock_ok. Now both children have
    // reported: mock_ok's child with Complete, mock_bad's child with
    // IoError (via complete_child in the DM split path).
    mock_ok.complete_pending();

    // The parent's aggregate status is IoError — the first non-Complete
    // child status wins, matching Linux's dm_end_io semantics.
    assert!(
        io_batch.wait_all().is_err(),
        "parent must report IoError when any child fails"
    );
    assert_eq!(
        mapped.in_flight(),
        0,
        "in-flight must balance even on failure"
    );
}

/// Test (P1): two mapped devices sharing the same underlying backing device
/// can issue concurrent I/O without interfering with each other. This is
/// the simplest cross-mapper concurrency regression: both mappers' in-flight
/// counters must be independent, and the underlying device must receive all
/// writes intact.
///
/// This guards against regressions where the in-flight counter or suspended
/// flag is accidentally shared (e.g., via a `static` instead of per-device
/// state).
#[ktest]
fn cross_mapper_concurrent_io_to_shared_backing() {
    ensure_initialized();

    let shared = MockBlockDevice::new("mock-shared", 256);

    // mapper_a: [0,128) -> shared [0,128)
    let mut table_a = DmTable::new();
    table_a
        .add_target(0, 128, DmTarget::Linear(LinearTarget::new(shared.clone(), 0)))
        .unwrap();
    let mapper_a = MappedDevice::create("dm-cross-a", table_a).unwrap();

    // mapper_b: [0,128) -> shared [128,256)
    let mut table_b = DmTable::new();
    table_b
        .add_target(0, 128, DmTarget::Linear(LinearTarget::new(shared.clone(), 128)))
        .unwrap();
    let mapper_b = MappedDevice::create("dm-cross-b", table_b).unwrap();

    // Two tasks, each writes a distinct pattern to its own mapper.
    let done = Arc::new(AtomicUsize::new(0));
    let mismatches = Arc::new(AtomicUsize::new(0));
    let mapper_a_child = mapper_a.clone();
    let mapper_b_child = mapper_b.clone();
    let done_a = done.clone();
    let done_b = done.clone();
    let mismatches_a = mismatches.clone();
    let mismatches_b = mismatches.clone();

    let pattern_a: Vec<u8> = (0..2 * BLOCK_SIZE).map(|i| (i % 251) as u8).collect();
    let pattern_b: Vec<u8> = (0..2 * BLOCK_SIZE)
        .map(|i| ((i + 17) % 251) as u8)
        .collect();
    // Keep copies for post-task assertions (the originals move into closures).
    let pattern_a_check = pattern_a.clone();
    let pattern_b_check = pattern_b.clone();

    TaskOptions::new(move || {
        write_blocks(mapper_a_child.as_ref(), Bid::new(0), &pattern_a);
        let read_back = read_blocks(mapper_a_child.as_ref(), Bid::new(0), 2 * BLOCK_SIZE);
        if read_back != pattern_a {
            mismatches_a.fetch_add(1, Ordering::AcqRel);
        }
        done_a.fetch_add(1, Ordering::AcqRel);
    })
    .data(())
    .spawn()
    .unwrap();

    TaskOptions::new(move || {
        write_blocks(mapper_b_child.as_ref(), Bid::new(0), &pattern_b);
        let read_back = read_blocks(mapper_b_child.as_ref(), Bid::new(0), 2 * BLOCK_SIZE);
        if read_back != pattern_b {
            mismatches_b.fetch_add(1, Ordering::AcqRel);
        }
        done_b.fetch_add(1, Ordering::AcqRel);
    })
    .data(())
    .spawn()
    .unwrap();

    let mut all_done = false;
    for _ in 0..0x10000 {
        Task::yield_now();
        if done.load(Ordering::Acquire) == 2 {
            all_done = true;
            break;
        }
    }
    assert!(all_done, "both cross-mapper tasks should finish");
    assert_eq!(
        mismatches.load(Ordering::Acquire),
        0,
        "data integrity must hold across mappers"
    );

    // Each mapper's in-flight counter is independent and returns to zero.
    assert_eq!(
        mapper_a.in_flight(),
        0,
        "mapper_a in-flight must be independent"
    );
    assert_eq!(
        mapper_b.in_flight(),
        0,
        "mapper_b in-flight must be independent"
    );

    // Shared backing received both writes at their correct offsets.
    assert_eq!(
        shared.read_bytes(0, 2 * BLOCK_SIZE),
        pattern_a_check,
        "shared [0,128) should hold mapper_a's pattern"
    );
    assert_eq!(
        shared.read_bytes(128 * SECTOR_SIZE, 2 * BLOCK_SIZE),
        pattern_b_check,
        "shared [128,256) should hold mapper_b's pattern"
    );
}
