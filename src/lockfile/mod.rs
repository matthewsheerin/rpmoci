//! Module for operations involving a lockfile
//!
//! Copyright (C) Microsoft Corporation.
//!
//! This program is free software: you can redistribute it and/or modify
//! it under the terms of the GNU General Public License as published by
//! the Free Software Foundation, either version 3 of the License, or
//! (at your option) any later version.
//!
//! This program is distributed in the hope that it will be useful,
//! but WITHOUT ANY WARRANTY; without even the implied warranty of
//! MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
//! GNU General Public License for more details.
//!
//! You should have received a copy of the GNU General Public License
//! along with this program.  If not, see <https://www.gnu.org/licenses/>.
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::io::Write;
use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::write;
use crate::{NAME, config::Config};

mod build;
mod download;
mod resolve;

/// Represents an rpmoci lockfile
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Lockfile {
    pkg_specs: Vec<String>,
    packages: BTreeSet<Package>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    local_packages: BTreeSet<LocalPackage>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    repo_gpg_config: BTreeMap<String, RepoKeyInfo>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    global_key_specs: Vec<url::Url>,
}

/// A package that the user has specified locally
/// Note that we don't store the package version or path in the lockfile,
/// but instead re-do our search for local packages at install time.
///
/// This enables the version of local RPMs to change without breaking compatibility
/// with the lockfile. In particular, the local RPM's version can change without
/// re-resolving the lockfile.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, PartialOrd, Eq, Ord)]
pub struct LocalPackage {
    /// The path to the package
    name: String,
    /// The RPM requires
    requires: Vec<String>,
}

/// Format of dnf resolve script output
#[derive(Debug, Serialize, Deserialize)]
struct DnfOutput {
    /// The resolved remote packages
    packages: Vec<Package>,
    /// Local packages
    local_packages: Vec<LocalPackage>,
    /// Repository GPG configuration
    repo_gpg_config: BTreeMap<String, RepoKeyInfo>,
}

/// GPG key configuration for a specified repository
#[derive(Debug, Serialize, Deserialize, Clone)]
struct RepoKeyInfo {
    /// Is GPG checking enabled for this repository
    gpgcheck: bool,
    /// contents of any keys specified via repository configuration
    keys: Vec<String>,
}

/// A resolved package
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, PartialOrd, Eq, Ord)]
pub struct Package {
    /// The package name
    pub name: String,
    /// The package epoch-version-release
    pub evr: String,
    /// The package checksum
    pub checksum: Checksum,
    /// The id of the package's repository
    pub repoid: String,
    /// The architecture of the package
    /// Optional to support older lockfiles. If a new lockfile format is introduced
    /// that requires this field, it should be made mandatory.
    #[serde(default)]
    pub arch: Option<String>,
}

/// Checksum of RPM package
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, PartialOrd, Eq, Ord)]
pub struct Checksum {
    /// The algorithm of the checksum
    algorithm: Algorithm,
    /// The checksum value
    checksum: String,
}

/// Algorithms supported by RPM for checksums
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, PartialOrd, Eq, Ord)]
#[serde(rename_all = "lowercase")]
pub enum Algorithm {
    /// The MD5 algorithm
    MD5, //Devskim: ignore DS126858
    /// The SHA1 algorithm
    SHA1, //Devskim: ignore DS126858
    /// The SHA256 algorithm
    SHA256,
    /// The SHA384 algorithm
    SHA384,
    /// The SHA512 algorithm
    SHA512,
}

impl fmt::Display for Algorithm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Algorithm::MD5 => write!(f, "md5"),
            Algorithm::SHA1 => write!(f, "sha1"),
            Algorithm::SHA256 => write!(f, "sha256"),
            Algorithm::SHA384 => write!(f, "sha384"),
            Algorithm::SHA512 => write!(f, "sha512"),
        }
    }
}

impl Lockfile {
    /// Returns true if the lockfile is compatible with the
    /// given configuration, false otherwise
    ///
    /// Local RPMs are not considered in this check, so this check can be performed
    /// without them being present
    #[must_use]
    pub fn is_compatible_excluding_local_rpms(&self, cfg: &Config) -> bool {
        self.pkg_specs == cfg.contents.packages && self.global_key_specs == cfg.contents.gpgkeys
    }

    /// Returns true if the lockfile is compatible with the
    /// given configuration, false otherwise
    ///
    /// This check also verifies that dependencies of local RPMs match those in the lockfile,
    /// so requires the local RPMs to be present
    pub fn is_compatible_including_local_rpms(&self, cfg: &Config) -> Result<bool> {
        let local_package_deps: BTreeSet<String> = self
            .local_packages
            .clone()
            .into_iter()
            .flat_map(|p| p.requires)
            .collect();

        Ok(self.is_compatible_excluding_local_rpms(cfg)
            && Self::read_local_rpm_deps(cfg)? == local_package_deps)
    }

    /// Write the lockfile to a file on disk
    pub fn write_to_file(&self, path: impl AsRef<Path>) -> Result<()> {
        let mut lock = std::fs::File::create(path.as_ref())?;
        lock.write_all(
            format!(
                "# This file is @generated by {}\n# It is not intended for manual editing.\n",
                NAME.to_ascii_uppercase(),
            )
            .as_bytes(),
        )?;
        lock.write_all(toml::to_string_pretty(&self)?.as_bytes())?;
        Ok(())
    }

    /// Print messages to stderr showing changes from a previous lockfile.
    pub fn print_updates(&self, previous: Option<&Lockfile>) -> Result<()> {
        let mut new = self
            .packages
            .iter()
            .map(|pkg| (&pkg.name, &pkg.evr))
            .collect::<BTreeMap<_, _>>();
        let old = previous
            .map(|previous| {
                previous
                    .packages
                    .iter()
                    .map(|pkg| (&pkg.name, &pkg.evr))
                    .collect::<BTreeMap<_, _>>()
            })
            .unwrap_or_default();

        for (name, evr) in old {
            if let Some(new_evr) = new.remove(name) {
                if new_evr != evr {
                    write::ok("Updating", format!("{name} {evr} -> {new_evr}"))?;
                }
            } else {
                write::ok("Removing", format!("{name} {evr}"))?;
            }
        }
        for (name, evr) in new {
            write::ok("Adding", format!("{name} {evr}"))?;
        }

        Ok(())
    }

    /// Returns an iterator over the packages in the Lockfile
    pub fn iter_packages(&self) -> impl Iterator<Item = &Package> {
        self.packages.iter()
    }
}
