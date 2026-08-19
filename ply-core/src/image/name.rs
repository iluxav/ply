//! Image filename grammar: `<name>-<semver>-<os>-<arch>.img`
//!
//! Filename = identity claim; lockfile hash = identity proof.

use std::fmt;

use semver::Version;

use crate::error::{Error, Result};
use crate::manifest::validate_package_name;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    Linux,
}

impl Os {
    pub fn as_str(&self) -> &'static str {
        match self {
            Os::Linux => "linux",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arch {
    X64,
    Arm64,
}

impl Arch {
    pub fn as_str(&self) -> &'static str {
        match self {
            Arch::X64 => "x64",
            Arch::Arm64 => "arm64",
        }
    }

    pub fn host() -> Arch {
        #[cfg(target_arch = "x86_64")]
        {
            Arch::X64
        }
        #[cfg(target_arch = "aarch64")]
        {
            Arch::Arm64
        }
    }
}

/// A parsed image filename.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageName {
    pub name: String,
    pub version: Version,
    pub os: Os,
    pub arch: Arch,
}

impl ImageName {
    pub fn new(name: &str, version: Version, os: Os, arch: Arch) -> Result<Self> {
        validate_package_name(name)?;
        Ok(ImageName {
            name: name.to_string(),
            version,
            os,
            arch,
        })
    }

    /// Parse a filename like `ffmpeg-6.1.0-linux-x64.img`.
    pub fn parse(filename: &str) -> Result<Self> {
        let bad = |why: &str| {
            Error::ImageName(format!(
                "invalid image filename `{filename}`: {why} (expected <name>-<semver>-<os>-<arch>.img, e.g. ffmpeg-6.1.0-linux-x64.img)"
            ))
        };

        let stem = filename
            .strip_suffix(".img")
            .ok_or_else(|| bad("missing .img extension"))?;

        // Split from the right: <arch>, <os>, then the version starts at the
        // first `-<digit>` boundary (names may not contain `-<digit>`).
        let (rest, arch) = stem.rsplit_once('-').ok_or_else(|| bad("missing arch"))?;
        let arch = match arch {
            "x64" => Arch::X64,
            "arm64" => Arch::Arm64,
            other => return Err(bad(&format!("unknown arch `{other}`"))),
        };
        let (rest, os) = rest.rsplit_once('-').ok_or_else(|| bad("missing os"))?;
        let os = match os {
            "linux" => Os::Linux,
            other => return Err(bad(&format!("unknown os `{other}`"))),
        };

        let bytes = rest.as_bytes();
        let version_dash = (0..bytes.len().saturating_sub(1))
            .find(|&i| bytes[i] == b'-' && bytes[i + 1].is_ascii_digit())
            .ok_or_else(|| bad("missing version"))?;
        let (name, version) = (&rest[..version_dash], &rest[version_dash + 1..]);
        let version = Version::parse(version)
            .map_err(|e| bad(&format!("invalid semver `{version}`: {e}")))?;

        validate_package_name(name).map_err(|e| bad(&e.to_string()))?;
        Ok(ImageName {
            name: name.to_string(),
            version,
            os,
            arch,
        })
    }
}

impl fmt::Display for ImageName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}-{}-{}-{}.img",
            self.name,
            self.version,
            self.os.as_str(),
            self.arch.as_str()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let n = ImageName::parse("ffmpeg-6.1.0-linux-x64.img").unwrap();
        assert_eq!(n.name, "ffmpeg");
        assert_eq!(n.version, Version::new(6, 1, 0));
        assert_eq!(n.to_string(), "ffmpeg-6.1.0-linux-x64.img");
    }

    #[test]
    fn multi_dash_name() {
        let n = ImageName::parse("chrome-headless-120.0.1-linux-arm64.img").unwrap();
        assert_eq!(n.name, "chrome-headless");
        assert_eq!(n.arch, Arch::Arm64);
    }

    #[test]
    fn prerelease_version() {
        let n = ImageName::parse("node-22.1.0-rc.1-linux-x64.img").unwrap();
        assert_eq!(n.version.to_string(), "22.1.0-rc.1");
    }

    #[test]
    fn rejects_garbage() {
        assert!(ImageName::parse("noversion-linux-x64.img").is_err());
        assert!(ImageName::parse("app-1.0.0-linux-x64.tar").is_err());
        assert!(ImageName::parse("app-1.0.0-windows-x64.img").is_err());
        assert!(ImageName::parse("app-1.0.0-linux-riscv.img").is_err());
    }
}
