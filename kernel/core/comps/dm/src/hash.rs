// SPDX-License-Identifier: MPL-2.0

//! Cryptographic hash algorithms for dm-verity.
//!
//! This module defines the [`HashAlgorithm`] trait used by `VerityTarget` to
//! abstract over concrete digest functions. SHA-256 and SHA-512 are supported;
//! adding another algorithm only requires implementing [`HashAlgorithm`] and
//! registering it in [`lookup_algorithm`].

use alloc::{sync::Arc, vec::Vec};
use core::fmt;

use crate::{sha256, sha512};

/// A cryptographic hash algorithm used by dm-verity.
///
/// Implementations must be `Send + Sync` because `VerityTarget` may be used
/// from multiple threads, and must be `'static` because they are held in an
/// `Arc`.
pub trait HashAlgorithm: Send + Sync + fmt::Debug {
    /// The algorithm name as it appears in the dm-verity table line
    /// (e.g. `"sha256"`).
    fn name(&self) -> &str;

    /// The size of a digest in bytes.
    fn digest_size(&self) -> usize;

    /// Computes the digest over the concatenation of `chunks`.
    ///
    /// The output is written into `out`, which must be exactly
    /// `digest_size()` bytes long. This method performs no heap
    /// allocations.
    fn digest(&self, chunks: &[&[u8]], out: &mut [u8]);
}

/// SHA-256 hash algorithm.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Sha256;

impl HashAlgorithm for Sha256 {
    fn name(&self) -> &str {
        "sha256"
    }

    fn digest_size(&self) -> usize {
        32
    }

    fn digest(&self, chunks: &[&[u8]], out: &mut [u8]) {
        out.copy_from_slice(&sha256::digest(chunks));
    }
}

/// SHA-512 hash algorithm.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Sha512;

impl HashAlgorithm for Sha512 {
    fn name(&self) -> &str {
        "sha512"
    }

    fn digest_size(&self) -> usize {
        64
    }

    fn digest(&self, chunks: &[&[u8]], out: &mut [u8]) {
        out.copy_from_slice(&sha512::digest(chunks));
    }
}

/// Looks up a built-in hash algorithm by name.
///
/// Returns `None` if the algorithm is not supported.
pub fn lookup_algorithm(name: &str) -> Option<Arc<dyn HashAlgorithm>> {
    match name {
        "sha256" => Some(Arc::new(Sha256)),
        "sha512" => Some(Arc::new(Sha512)),
        _ => None,
    }
}

/// Returns the list of supported algorithm names.
pub fn supported_algorithms() -> Vec<&'static str> {
    Vec::from(["sha256", "sha512"])
}
