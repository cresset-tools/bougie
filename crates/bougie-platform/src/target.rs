//! Host-target triple detection per CLI.md §7.2.

use bougie_errors::BougieError;
use eyre::Result;
#[cfg(target_os = "linux")]
use eyre::WrapErr;
use std::fmt;
#[cfg(target_os = "linux")]
use std::fs::File;
#[cfg(target_os = "linux")]
use std::io::{Read, Seek, SeekFrom};
#[cfg(target_os = "linux")]
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
    Windows,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Vendor {
    Unknown,
    Apple,
    Pc,
}

/// libc / ABI family. `None` for darwin (the field is omitted from
/// the triple); `Msvc` for windows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Env {
    Gnu,
    Musl,
    Msvc,
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
            Self::Windows => "windows",
        })
    }
}

impl fmt::Display for Vendor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Unknown => "unknown",
            Self::Apple => "apple",
            Self::Pc => "pc",
        })
    }
}

impl fmt::Display for Env {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Gnu => "gnu",
            Self::Musl => "musl",
            Self::Msvc => "msvc",
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
    /// Detect the host's triple. Compile-time OS/arch + runtime libc
    /// probe on Linux. Windows hosts always classify as
    /// `pc-windows-msvc`; the libc-detection helpers are
    /// `cfg(target_os = "linux")`-gated because both macOS and Windows
    /// pin the C runtime by platform convention rather than by
    /// `PT_INTERP`-style discovery, and leaving them as `cfg(unix)`
    /// would tip a `dead_code` error on macOS under `-D warnings`.
    pub fn detect() -> Result<Self> {
        let arch = detect_arch()?;
        match std::env::consts::OS {
            #[cfg(target_os = "linux")]
            "linux" => {
                let env = detect_linux_libc()?;
                Ok(Self { arch, vendor: Vendor::Unknown, os: Os::Linux, env: Some(env) })
            }
            #[cfg(target_os = "macos")]
            "macos" => Ok(Self { arch, vendor: Vendor::Apple, os: Os::Darwin, env: None }),
            #[cfg(target_os = "windows")]
            "windows" => Ok(Self { arch, vendor: Vendor::Pc, os: Os::Windows, env: Some(Env::Msvc) }),
            other => Err(BougieError::UnknownTarget {
                triple: format!("os={other}"),
                hint: "bougie supports linux, macos, and windows; other operating systems would need new build authority targets".into(),
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

#[cfg(target_os = "linux")]
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
#[cfg(target_os = "linux")]
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
#[cfg(target_os = "linux")]
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

    #[cfg(target_os = "linux")]
    #[test]
    fn classify_known_glibc_paths() {
        assert_eq!(classify_libc("/lib64/ld-linux-x86-64.so.2"), Some(Env::Gnu));
        assert_eq!(classify_libc("/lib/ld-linux-aarch64.so.1"), Some(Env::Gnu));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn classify_known_musl_paths() {
        assert_eq!(classify_libc("/lib/ld-musl-x86_64.so.1"), Some(Env::Musl));
        assert_eq!(classify_libc("/lib/ld-musl-aarch64.so.1"), Some(Env::Musl));
    }

    #[cfg(target_os = "linux")]
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
    fn triple_format_windows_emits_msvc_env() {
        let t = Triple {
            arch: Arch::X86_64,
            vendor: Vendor::Pc,
            os: Os::Windows,
            env: Some(Env::Msvc),
        };
        assert_eq!(t.to_string(), "x86_64-pc-windows-msvc");
        let arm = Triple {
            arch: Arch::Aarch64,
            vendor: Vendor::Pc,
            os: Os::Windows,
            env: Some(Env::Msvc),
        };
        assert_eq!(arm.to_string(), "aarch64-pc-windows-msvc");
    }

    /// On Windows hosts, `Triple::detect()` returns
    /// `<arch>-pc-windows-msvc` unconditionally — no libc probe (Windows
    /// has no `PT_INTERP`-equivalent), no Developer-Mode preflight.
    #[cfg(target_os = "windows")]
    #[test]
    fn detect_on_windows_classifies_as_pc_windows_msvc() {
        let t = Triple::detect().expect("detect on Windows host");
        assert_eq!(t.os, Os::Windows);
        assert_eq!(t.vendor, Vendor::Pc);
        assert_eq!(t.env, Some(Env::Msvc));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn read_pt_interp_on_bin_sh() {
        let interp = read_pt_interp(Path::new("/bin/sh")).expect("read /bin/sh interp");
        assert!(
            classify_libc(&interp).is_some(),
            "expected gnu or musl, got {interp}"
        );
    }
}
