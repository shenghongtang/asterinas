// SPDX-License-Identifier: MPL-2.0

//! Parser for the `dm_mod.create=` and `dm_mod.create_mandatory=` kernel
//! command-line parameters.
//!
//! Mirrors Linux's `dm-mod.create=` syntax, allowing device-mapper devices to
//! be created at boot time without userspace involvement. Each value has the
//! form:
//!
//! ```text
//! "<name>: <start_sector> <length> <target_type> <target_args>[; ...]"
//! ```
//!
//! Multiple segments are separated by `;`. The value is usually quoted on
//! the command line because it contains spaces, e.g.:
//!
//! ```text
//! dm_mod.create="rootfs: 0 4096 linear 8:0 0"
//! dm_mod.create="roothash: 0 8 verity 1 8:1 8:2 4096 4096 1 0 sha256 <hex> -"
//! ```
//!
//! `dm_mod.create_mandatory=` uses the same syntax, but if parsing or device
//! creation fails for any entry, the kernel panics immediately. This is
//! intended for devices that are required for boot (for example, the rootfs)
//! where continuing without the device would lead to an unusable system.

use alloc::{
    boxed::Box,
    string::{String, ToString},
    vec::Vec,
};
use core::str::FromStr;

use aster_cmdline::parse::ParamError;

use crate::{
    DmError, DmTable, DmTarget, MappedDevice,
    targets::{
        error::ErrorTarget, linear::LinearTarget, striped::StripedTarget, verity::VerityTarget,
        zero::ZeroTarget,
    },
};

/// One `dm_mod.create=` value from the kernel command line.
///
/// The raw value preserves the surrounding double quotes stripped by the
/// command-line tokenizer, mirroring how Linux's `lib/cmdline.c` unquotes
/// kernel parameter values.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DmCreateArg(String);

impl DmCreateArg {
    /// Returns the raw argument string (quotes already stripped).
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for DmCreateArg {
    type Err = ParamError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // A dm table value contains spaces, so on the kernel command line it
        // is written as `dm_mod.create="<name>: <start> <len> <target> ..."`.
        // The command-line tokenizer keeps the wrapping double quotes in the
        // value, so strip one matching pair here.
        let unquoted = strip_matching_quotes(s.trim());
        if unquoted.is_empty() {
            Err(ParamError::InvalidValue)
        } else {
            Ok(Self(unquoted.to_string()))
        }
    }
}

fn strip_matching_quotes(value: &str) -> &str {
    value
        .strip_prefix('"')
        .and_then(|inner| inner.strip_suffix('"'))
        .unwrap_or(value)
}

/// The parsed result of one `dm_mod.create=` entry.
#[derive(Debug)]
pub struct ParsedDmCreate {
    pub name: String,
    pub table: DmTable,
}

/// Parses a single `dm_mod.create=` value into a [`ParsedDmCreate`].
///
/// `fallback_index` is used to generate a default name (`dm-<index>`) when
/// the entry's name field is `-` or empty, matching Linux behavior.
pub fn parse_create_arg(arg: &str, fallback_index: usize) -> Result<ParsedDmCreate, DmError> {
    let (raw_name, table_text) = split_name_and_table(arg)
        .ok_or(DmError::InvalidParameters("missing name/table separator"))?;
    let name = normalize_name(raw_name.trim(), fallback_index);

    let mut table = DmTable::new();
    for line in table_text.split(';') {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let (start, length, target) = parse_segment(line)?;
        table
            .add_target(start, length, target)
            .map_err(DmError::InvalidTable)?;
    }

    if table.num_targets() == 0 {
        return Err(DmError::InvalidParameters("table has no targets"));
    }

    Ok(ParsedDmCreate { name, table })
}

/// Splits an argument into `(name, table_text)` on the first `:` or `,`.
///
/// Linux dm-init accepts both `name: ...` and `name, ...` forms. A
/// whitespace-only value without a separator is treated as an anonymous
/// table named `-`.
fn split_name_and_table(arg: &str) -> Option<(&str, &str)> {
    if let Some((name, table)) = arg.split_once(':') {
        return Some((name, table));
    }
    if let Some((name, table)) = arg.split_once(',') {
        return Some((name, table));
    }
    Some(("-", arg))
}

/// Parses one table segment line:
/// `<start_sector> <length> <target_type> [target_args...]`
fn parse_segment(line: &str) -> Result<(u64, u64, DmTarget), DmError> {
    let mut fields = line.split_whitespace();
    let start_sector = parse_u64(fields.next(), "segment start sector")?;
    let len_sectors = parse_u64(fields.next(), "segment length")?;
    let target_name = fields
        .next()
        .ok_or(DmError::InvalidParameters("missing target type"))?;
    let args: Vec<&str> = fields.collect();

    let target: DmTarget = match target_name {
        "linear" => DmTarget::Linear(parse_linear_target(&args, len_sectors)?),
        "striped" => {
            // LVM uses "striped" even for single-stripe (linear) LVs.
            // Format: "<num_stripes> <stripe_size> <dev1> <start1> [<dev2> <start2> ...]"
            // A single-stripe striped target is equivalent to linear; multi-stripe
            // targets distribute I/O across the underlying devices (RAID0).
            if args.len() < 4 {
                return Err(DmError::InvalidParameters(
                    "striped target requires at least <num_stripes> <stripe_size> <dev> <start>",
                ));
            }
            let num_stripes: usize = args[0]
                .parse()
                .map_err(|_| DmError::InvalidParameters("invalid stripe count"))?;
            if num_stripes == 0 {
                return Err(DmError::InvalidParameters("stripe count must be non-zero"));
            }
            let stripe_size = parse_u64(Some(args[1]), "striped stripe size")?;
            if stripe_size == 0 {
                return Err(DmError::InvalidParameters("stripe size must be non-zero"));
            }
            if stripe_size.count_ones() != 1 {
                return Err(DmError::InvalidParameters(
                    "stripe size must be a power of two",
                ));
            }
            if args.len() != 2 + 2 * num_stripes {
                return Err(DmError::InvalidParameters(
                    "striped target has mismatched device/start pair count",
                ));
            }
            let mut stripes = Vec::new();
            let mut idx = 2;
            for _ in 0..num_stripes {
                let dev = crate::lookup_block_device(args[idx])?;
                let start = parse_u64(Some(args[idx + 1]), "striped start sector")?;
                stripes.push((dev, start));
                idx += 2;
            }

            // Validate that every stripe device is large enough for the mapped
            // region. With RAID0 wrap-around, absolute stripe `a` lands on
            // physical device `a % num_stripes` at round `a / num_stripes`.
            let num_abs_stripes = len_sectors.div_ceil(stripe_size);
            for (i, (dev, start)) in stripes.iter().enumerate() {
                let count =
                    (num_abs_stripes + num_stripes as u64 - 1 - i as u64) / num_stripes as u64;
                if count == 0 {
                    continue;
                }
                let a_last = i as u64 + (count - 1) * num_stripes as u64;
                let last_stripe_sectors = if a_last == num_abs_stripes - 1 {
                    len_sectors - a_last * stripe_size
                } else {
                    stripe_size
                };
                let end_sector = start
                    .checked_add((count - 1) * stripe_size)
                    .and_then(|s| s.checked_add(last_stripe_sectors))
                    .ok_or(DmError::InvalidParameters(
                        "striped target extends past the end of an underlying device",
                    ))?;
                if end_sector > dev.metadata().nr_sectors as u64 {
                    return Err(DmError::InvalidParameters(
                        "striped target extends past the end of an underlying device",
                    ));
                }
            }

            DmTarget::Striped(StripedTarget::new(stripes, stripe_size))
        }
        "zero" => {
            if !args.is_empty() {
                return Err(DmError::InvalidParameters(
                    "zero target takes no parameters",
                ));
            }
            DmTarget::Zero(ZeroTarget::new(len_sectors))
        }
        "error" => {
            if !args.is_empty() {
                return Err(DmError::InvalidParameters(
                    "error target takes no parameters",
                ));
            }
            DmTarget::Error(ErrorTarget::new(len_sectors))
        }
        "verity" => DmTarget::Verity(Box::new(parse_verity_target(&args)?)),
        _ => return Err(DmError::InvalidParameters("unsupported target type")),
    };

    Ok((start_sector, len_sectors, target))
}

fn parse_linear_target(args: &[&str], len_sectors: u64) -> Result<LinearTarget, DmError> {
    if args.len() != 2 {
        return Err(DmError::InvalidParameters(
            "linear target expects <device> <start_sector>",
        ));
    }
    let device = crate::lookup_block_device(args[0])?;
    let start_sector = parse_u64(Some(args[1]), "linear target start sector")?;
    let end_sector = start_sector
        .checked_add(len_sectors)
        .ok_or(DmError::InvalidParameters(
            "linear target extends past the end of the underlying device",
        ))?;
    if end_sector > device.metadata().nr_sectors as u64 {
        return Err(DmError::InvalidParameters(
            "linear target extends past the end of the underlying device",
        ));
    }
    Ok(LinearTarget::new(device, start_sector))
}

fn parse_verity_target(args: &[&str]) -> Result<VerityTarget, DmError> {
    VerityTarget::from_table_args(args)
}

fn parse_u64(value: Option<&str>, what: &'static str) -> Result<u64, DmError> {
    value
        .ok_or(DmError::InvalidParameters(what))?
        .parse::<u64>()
        .map_err(|_| DmError::InvalidParameters(what))
}

/// Normalizes a device name, falling back to `dm-<index>` when the name
/// is `-` or empty (matching Linux's dm-init convention).
fn normalize_name(raw: &str, fallback_index: usize) -> String {
    if raw == "-" || raw.is_empty() {
        alloc::format!("dm-{}", fallback_index)
    } else {
        raw.to_string()
    }
}

/// Creates all boot-time DM devices from the parsed command-line entries.
///
/// Called from `init_in_first_process` for both `dm_mod.create=` and
/// `dm_mod.create_mandatory=`. Each successfully parsed entry is registered
/// with the [`MappedDevice`] registry and resumed immediately.
///
/// If `mandatory` is `true`, a parsing or creation failure causes an
/// immediate `panic!` so that boot cannot continue with a required device
/// missing. If `mandatory` is `false`, failures are only logged and boot
/// proceeds.
pub fn create_boot_devices(args: &[DmCreateArg], mandatory: bool) {
    for (index, arg) in args.iter().enumerate() {
        match parse_create_arg(arg.as_str(), index) {
            Ok(parsed) => match MappedDevice::create(&*parsed.name, parsed.table) {
                Ok(device) => {
                    ostd::info!(
                        "created boot dm device '{}' ({:?})",
                        device.name(),
                        device.device_id(),
                    );
                }
                Err(err) => {
                    if mandatory {
                        panic!(
                            "mandatory dm_mod.create_mandatory entry '{}' failed to create device '{}': {:?}",
                            arg.as_str(),
                            parsed.name,
                            err
                        );
                    }
                    ostd::error!(
                        "failed to create boot dm device '{}': {:?}",
                        parsed.name,
                        err
                    );
                }
            },
            Err(err) => {
                if mandatory {
                    panic!(
                        "mandatory dm_mod.create_mandatory entry '{}' failed to parse: {:?}",
                        arg.as_str(),
                        err
                    );
                }
                ostd::error!(
                    "failed to parse dm_mod.create entry '{}': {:?}",
                    arg.as_str(),
                    err
                );
            }
        }
    }
}
