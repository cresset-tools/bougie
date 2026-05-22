//! Binary-format dispatcher: given a `.so` file, pick the right
//! parser (ELF on Linux, Mach-O on macOS) and return the canonical
//! PHP extension name + `zend_extension=` vs `extension=` kind.
//!
//! The format-specific work lives in [`crate::elf`] and
//! [`crate::macho`]; this module just sniffs the first four magic
//! bytes and routes to the right `detect_from_bytes`.

use eyre::{eyre, Result, WrapErr};
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectedExt {
    /// The canonical extension name as `phpinfo()` reports it — the
    /// string the runtime registers under and the platform check
    /// compares against `ext-<name>`.
    pub name: String,
    /// `true` iff the extension loads as `zend_extension=` (versus
    /// `extension=`). Determined by the symbol it exports.
    pub zend: bool,
}

pub fn detect_php_extension(path: &Path) -> Result<DetectedExt> {
    let bytes = std::fs::read(path)
        .wrap_err_with(|| format!("reading {}", path.display()))?;
    detect_from_bytes(&bytes)
}

pub fn detect_from_bytes(buf: &[u8]) -> Result<DetectedExt> {
    let head = buf.get(..4).ok_or_else(|| eyre!("file too small to identify"))?;
    match head {
        b"\x7fELF" => crate::elf::detect_from_bytes(buf),
        // MH_MAGIC_64 (little-endian on disk): 0xfeedfacf
        [0xcf, 0xfa, 0xed, 0xfe] => crate::macho::detect_from_bytes(buf),
        // MH_MAGIC (32-bit) — not supported.
        [0xce, 0xfa, 0xed, 0xfe] => Err(eyre!(
            "32-bit Mach-O is not supported; modern PHP extensions \
             are universally 64-bit"
        )),
        // Big-endian Mach-O (MH_CIGAM_64 on disk = 0xcffaedfe BE).
        [0xfe, 0xed, 0xfa, 0xcf | 0xce] => Err(eyre!(
            "big-endian Mach-O is not supported"
        )),
        // FAT/universal binaries (big-endian magic on disk = 0xCAFEBABE).
        // The 64-bit variant uses 0xCAFEBABF.
        [0xca, 0xfe, 0xba, 0xbe | 0xbf] => Err(eyre!(
            "this is a FAT/universal binary; extract a single-arch \
             slice first, e.g. `lipo <path> -thin arm64 -output <out>.so` \
             (or `x86_64` on Intel macOS)"
        )),
        // Likely PE/COFF or something else entirely.
        _ => Err(eyre!(
            "unrecognised file format (magic = {head:02x?}); expected \
             ELF or Mach-O"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fat_binary_suggests_lipo() {
        let err = detect_from_bytes(&[0xca, 0xfe, 0xba, 0xbe, 0, 0, 0, 0]).unwrap_err();
        assert!(err.to_string().contains("lipo"), "got: {err}");
    }

    #[test]
    fn rejects_random_bytes() {
        let err = detect_from_bytes(b"hello world, not a binary").unwrap_err();
        assert!(err.to_string().contains("unrecognised"), "got: {err}");
    }

    #[test]
    fn rejects_too_short() {
        assert!(detect_from_bytes(b"\x7fEL").is_err());
    }
}
