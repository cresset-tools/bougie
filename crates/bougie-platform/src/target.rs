//! Host-target triple detection per CLI.md §7.2.

use bougie_errors::BougieError;
use eyre::{Result, WrapErr};
use std::fmt;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Arch {
    X86_64,
    Aarch64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Os {
    Linux,
    Darwin,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Vendor {
    Unknown,
    Apple,
}

/// libc family. `None` for darwin (the field is omitted from the triple).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Env {
    Gnu,
    Musl,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Triple {
    pub arch: Arch,
    pub vendor: Vendor,
    pub os: Os,
    pub env: Option<Env>,
}

impl fmt::Display for Arch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::X86_64 => "x86_64",
            Self::Aarch64 => "aarch64",
        })
    }
}

impl fmt::Display for Os {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Linux => "linux",
            Self::Darwin => "darwin",
        })
    }
}

impl fmt::Display for Vendor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Unknown => "unknown",
            Self::Apple => "apple",
        })
    }
}

impl fmt::Display for Env {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Gnu => "gnu",
            Self::Musl => "musl",
        })
    }
}

impl fmt::Display for Triple {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.env {
            Some(e) => write!(f, "{}-{}-{}-{}", self.arch, self.vendor, self.os, e),
            None => write!(f, "{}-{}-{}", self.arch, self.vendor, self.os),
        }
    }
}

impl Triple {
    /// Detect the host's triple. Compile-time OS/arch + runtime libc probe
    /// on Linux.
    pub fn detect() -> Result<Self> {
        let arch = detect_arch()?;
        match std::env::consts::OS {
            "linux" => {
                let env = detect_linux_libc()?;
                Ok(Self { arch, vendor: Vendor::Unknown, os: Os::Linux, env: Some(env) })
            }
            "macos" => Ok(Self { arch, vendor: Vendor::Apple, os: Os::Darwin, env: None }),
            other => Err(BougieError::UnknownTarget {
                triple: format!("os={other}"),
                hint: "bougie supports linux and macos; other operating systems would need new build authority targets".into(),
            }
            .into()),
        }
    }
}

fn detect_arch() -> Result<Arch> {
    match std::env::consts::ARCH {
        "x86_64" => Ok(Arch::X86_64),
        "aarch64" => Ok(Arch::Aarch64),
        other => Err(BougieError::UnknownTarget {
            triple: format!("arch={other}"),
            hint: "bougie supports x86_64 and aarch64".into(),
        }
        .into()),
    }
}

fn detect_linux_libc() -> Result<Env> {
    let interp = read_pt_interp(Path::new("/bin/sh"))
        .or_else(|_| read_pt_interp(Path::new("/usr/bin/env")))
        .wrap_err("could not read PT_INTERP from /bin/sh or /usr/bin/env to detect libc")?;
    classify_libc(&interp).ok_or_else(|| {
        BougieError::UnknownTarget {
            triple: format!("libc=unknown (PT_INTERP={interp})"),
            hint: "bougie classifies the libc family by reading /bin/sh's dynamic linker; expected ld-linux-* (gnu) or ld-musl-* (musl)".into(),
        }
        .into()
    })
}

/// Classify a dynamic linker path string per CLI.md §7.2.
pub(crate) fn classify_libc(interp: &str) -> Option<Env> {
    let stem = Path::new(interp).file_name()?.to_str()?;
    if stem.starts_with("ld-linux") {
        Some(Env::Gnu)
    } else if stem.starts_with("ld-musl") {
        Some(Env::Musl)
    } else {
        None
    }
}

/// Read `PT_INTERP` from an ELF64 file. Returns the null-stripped path.
pub(crate) fn read_pt_interp(path: &Path) -> Result<String> {
    let mut f = File::open(path).wrap_err_with(|| format!("opening {}", path.display()))?;
    let mut ehdr = [0u8; 64];
    f.read_exact(&mut ehdr).wrap_err("reading ELF header")?;
    if &ehdr[0..4] != b"\x7fELF" {
        return Err(eyre::eyre!("{} is not ELF", path.display()));
    }
    if ehdr[4] != 2 {
        return Err(eyre::eyre!("{} is not 64-bit ELF", path.display()));
    }
    if ehdr[5] != 1 {
        return Err(eyre::eyre!("{} is not little-endian", path.display()));
    }
    let e_phoff = u64::from_le_bytes(ehdr[32..40].try_into().unwrap());
    let e_phentsize = u64::from(u16::from_le_bytes(ehdr[54..56].try_into().unwrap()));
    let e_phnum = u64::from(u16::from_le_bytes(ehdr[56..58].try_into().unwrap()));

    if e_phentsize < 56 {
        return Err(eyre::eyre!("ELF e_phentsize {e_phentsize} too small"));
    }

    for i in 0..e_phnum {
        f.seek(SeekFrom::Start(e_phoff + i * e_phentsize))
            .wrap_err("seeking program headers")?;
        let mut ph = [0u8; 56];
        f.read_exact(&mut ph).wrap_err("reading program header")?;
        let p_type = u32::from_le_bytes(ph[0..4].try_into().unwrap());
        if p_type != 3 {
            // PT_INTERP
            continue;
        }
        let p_offset = u64::from_le_bytes(ph[8..16].try_into().unwrap());
        let p_filesz = u64::from_le_bytes(ph[32..40].try_into().unwrap());
        if p_filesz == 0 || p_filesz > 4096 {
            return Err(eyre::eyre!("implausible PT_INTERP size {p_filesz}"));
        }
        f.seek(SeekFrom::Start(p_offset)).wrap_err("seeking PT_INTERP")?;
        let size = usize::try_from(p_filesz).wrap_err("PT_INTERP size overflow")?;
        let mut buf = vec![0u8; size];
        f.read_exact(&mut buf).wrap_err("reading PT_INTERP")?;
        // Strip trailing NUL.
        while buf.last() == Some(&0) {
            buf.pop();
        }
        return String::from_utf8(buf).wrap_err("PT_INTERP is not UTF-8");
    }
    Err(eyre::eyre!("no PT_INTERP found in {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_known_glibc_paths() {
        assert_eq!(classify_libc("/lib64/ld-linux-x86-64.so.2"), Some(Env::Gnu));
        assert_eq!(classify_libc("/lib/ld-linux-aarch64.so.1"), Some(Env::Gnu));
    }

    #[test]
    fn classify_known_musl_paths() {
        assert_eq!(classify_libc("/lib/ld-musl-x86_64.so.1"), Some(Env::Musl));
        assert_eq!(classify_libc("/lib/ld-musl-aarch64.so.1"), Some(Env::Musl));
    }

    #[test]
    fn classify_unknown_returns_none() {
        assert_eq!(classify_libc("/lib/ld-bsd.so.1"), None);
        assert_eq!(classify_libc(""), None);
    }

    #[test]
    fn triple_format_linux() {
        let t = Triple {
            arch: Arch::X86_64,
            vendor: Vendor::Unknown,
            os: Os::Linux,
            env: Some(Env::Gnu),
        };
        assert_eq!(t.to_string(), "x86_64-unknown-linux-gnu");
    }

    #[test]
    fn triple_format_darwin_omits_env() {
        let t = Triple {
            arch: Arch::Aarch64,
            vendor: Vendor::Apple,
            os: Os::Darwin,
            env: None,
        };
        assert_eq!(t.to_string(), "aarch64-apple-darwin");
    }

    #[test]
    fn read_pt_interp_on_bin_sh() {
        // This test only runs on Linux where /bin/sh is ELF.
        if cfg!(target_os = "linux") {
            let interp = read_pt_interp(Path::new("/bin/sh")).expect("read /bin/sh interp");
            assert!(
                classify_libc(&interp).is_some(),
                "expected gnu or musl, got {interp}"
            );
        }
    }
}
