//! Minimal Mach-O parser, scoped to the same job as [`crate::elf`]:
//! given a PHP-extension `.so` (an `MH_BUNDLE` on macOS), return the
//! canonical extension name and whether it loads as `zend_extension=`
//! or `extension=`. Reads only what's necessary — segments and the
//! classic symbol table.
//!
//! ## References
//!
//! - Apple's `<mach-o/loader.h>` (load-command layout, `mach_header_64`
//!   field offsets, `MH_MAGIC_64`).
//! - Apple's `<mach-o/nlist.h>` (`struct nlist_64` — 16 bytes).
//! - LLVM `include/llvm/BinaryFormat/MachO.h` (cross-check of
//!   constant values).
//! - PHP source `Zend/zend_modules.h` / `Zend/zend_extensions.h` for
//!   the struct layouts whose `name` field we walk to. On Apple
//!   Silicon and `x86_64` macOS both structs follow the same LP64
//!   layout used in [`crate::elf`], so the field offsets are reused.
//!
//! ## Why no rebase opcode walker?
//!
//! `LC_DYLD_INFO_ONLY` carries an opcode stream describing which
//! pointers in `__DATA` need a slide added at load time. Unlike ELF's
//! `R_*_RELATIVE` (where the file stores zero and the addend carries
//! the value), Mach-O stores the link-time vmaddr directly in the
//! file — dyld just adds the load slide. The file already contains a
//! correct vmaddr we can resolve through segments. Hand-verified
//! against the Tideways arm64 bundle.

use crate::binfmt::DetectedExt;
use eyre::{eyre, Result};

const MH_MAGIC_64: [u8; 4] = [0xcf, 0xfa, 0xed, 0xfe];

const LC_SEGMENT_64: u32 = 0x19;
const LC_SYMTAB: u32 = 0x2;
/// `LC_REQ_DYLD` is OR'd into command IDs that must be understood by
/// dyld; we mask it off to compare base IDs.
const LC_REQ_DYLD: u32 = 0x8000_0000;

/// Field offsets in `zend_module_entry` / `zend_extension`. These are
/// the same as in [`crate::elf`] because both Mach-O targets we
/// support (arm64, `x86_64`) use the same LP64 struct layout PHP was
/// compiled against.
const ZEND_MODULE_ENTRY_NAME_OFFSET: u64 = 32;
const ZEND_EXTENSION_ENTRY_NAME_OFFSET: u64 = 0;

pub fn detect_from_bytes(buf: &[u8]) -> Result<DetectedExt> {
    let hdr = MachHeader::parse(buf)?;
    let (segments, symtab) = hdr.scan_load_commands(buf)?;

    let symtab = symtab.ok_or_else(|| eyre!("no LC_SYMTAB load command found"))?;
    let strtab = read_slice(buf, symtab.stroff, symtab.strsize)?;
    let symbols = read_slice(buf, symtab.symoff, symtab.nsyms.saturating_mul(16))?;

    // On Mach-O every external C identifier carries a leading
    // underscore in the symbol table — `int foo()` ends up as `_foo`.
    let mut zend_entry: Option<NList> = None;
    let mut module_entries: Vec<(String, NList)> = Vec::new();
    for chunk in symbols.chunks_exact(16) {
        let sym = NList::parse(chunk);
        if sym.n_value == 0 {
            // Undefined / external import.
            continue;
        }
        let name = read_cstring(strtab, sym.n_strx as usize)?;
        if name == "_zend_extension_entry" {
            zend_entry = Some(sym);
        } else if name.ends_with("_module_entry")
            && name.starts_with('_')
            && name != "__module_entry"
        {
            module_entries.push((name.to_owned(), sym));
        }
    }

    if let Some(sym) = zend_entry {
        let name = read_indirect_string(
            buf,
            &segments,
            sym.n_value,
            ZEND_EXTENSION_ENTRY_NAME_OFFSET,
        )?;
        return Ok(DetectedExt { name, zend: true });
    }
    if let Some((_, sym)) = module_entries.into_iter().next() {
        let name = read_indirect_string(
            buf,
            &segments,
            sym.n_value,
            ZEND_MODULE_ENTRY_NAME_OFFSET,
        )?;
        return Ok(DetectedExt { name, zend: false });
    }
    Err(eyre!(
        "no `_zend_extension_entry` or `_*_module_entry` symbol found — \
         this Mach-O bundle doesn't look like a PHP extension"
    ))
}

fn read_indirect_string(
    buf: &[u8],
    segments: &[Segment],
    struct_vmaddr: u64,
    field_offset: u64,
) -> Result<String> {
    let ptr_vmaddr = struct_vmaddr
        .checked_add(field_offset)
        .ok_or_else(|| eyre!("overflow computing field vmaddr"))?;
    let ptr_off = vmaddr_to_file_off(segments, ptr_vmaddr, 8)?;
    let mut p = [0u8; 8];
    p.copy_from_slice(&buf[ptr_off..ptr_off + 8]);
    let name_vmaddr = u64::from_le_bytes(p);
    if name_vmaddr == 0 {
        return Err(eyre!(
            "name pointer at vmaddr {ptr_vmaddr:#x} is NULL — unexpected \
             shape for a PHP extension Mach-O bundle"
        ));
    }
    let name_off = vmaddr_to_file_off(segments, name_vmaddr, 1)?;
    Ok(read_cstring(buf, name_off)?.to_owned())
}

fn vmaddr_to_file_off(segments: &[Segment], vmaddr: u64, len: u64) -> Result<usize> {
    for seg in segments {
        // `__LINKEDIT` has filesize > 0 but doesn't hold addressable
        // content past the symbol/string tables we already located
        // explicitly. Including it in the scan is harmless because
        // its vmaddr range doesn't overlap with `__TEXT`/`__DATA`,
        // so a struct-pointer lookup never lands there.
        let end_vm = seg.vmaddr.saturating_add(seg.vmsize);
        if vmaddr >= seg.vmaddr && vmaddr.saturating_add(len) <= end_vm {
            let delta = vmaddr - seg.vmaddr;
            // Cap at filesize: a segment can have vmsize > filesize
            // for the zero-fill tail (think .bss). We refuse to read
            // past the on-disk extent.
            if delta + len > seg.filesize {
                return Err(eyre!(
                    "vmaddr {vmaddr:#x} falls into segment `{}` but past its \
                     on-disk extent (delta {delta}, filesize {})",
                    seg.name, seg.filesize
                ));
            }
            return Ok((seg.fileoff + delta) as usize);
        }
    }
    Err(eyre!(
        "vmaddr {vmaddr:#x} ({len} bytes) not within any LC_SEGMENT_64"
    ))
}

fn read_cstring(buf: &[u8], off: usize) -> Result<&str> {
    let slice = buf.get(off..)
        .ok_or_else(|| eyre!("string offset {off} out of range"))?;
    let nul = slice.iter().position(|&b| b == 0)
        .ok_or_else(|| eyre!("unterminated string at offset {off}"))?;
    std::str::from_utf8(&slice[..nul])
        .map_err(|e| eyre!("non-UTF8 string at offset {off}: {e}"))
}

fn read_slice(buf: &[u8], off: u32, len: u32) -> Result<&[u8]> {
    let start = off as usize;
    let end = start.checked_add(len as usize)
        .ok_or_else(|| eyre!("range overflow"))?;
    buf.get(start..end)
        .ok_or_else(|| eyre!("range {start}..{end} out of bounds (buf={})", buf.len()))
}

/// `mach_header_64` (32 bytes). We only care about `ncmds`/`sizeofcmds`
/// — the rest (`cputype`, `flags`, etc.) doesn't influence parsing.
#[derive(Debug)]
struct MachHeader {
    ncmds: u32,
    sizeofcmds: u32,
}

impl MachHeader {
    fn parse(buf: &[u8]) -> Result<Self> {
        if buf.len() < 32 {
            return Err(eyre!("file too small for a Mach-O 64 header"));
        }
        if buf[0..4] != MH_MAGIC_64 {
            return Err(eyre!("not an MH_MAGIC_64 Mach-O file"));
        }
        let ncmds      = u32::from_le_bytes(buf[16..20].try_into().unwrap());
        let sizeofcmds = u32::from_le_bytes(buf[20..24].try_into().unwrap());
        Ok(Self { ncmds, sizeofcmds })
    }

    fn scan_load_commands(
        &self,
        buf: &[u8],
    ) -> Result<(Vec<Segment>, Option<Symtab>)> {
        let mut segments = Vec::new();
        let mut symtab = None;
        let mut cursor: usize = 32; // load commands start right after the header
        let cmd_end = 32usize
            .checked_add(self.sizeofcmds as usize)
            .ok_or_else(|| eyre!("sizeofcmds overflow"))?;
        if cmd_end > buf.len() {
            return Err(eyre!("load commands extend past end of file"));
        }
        for _ in 0..self.ncmds {
            if cursor + 8 > cmd_end {
                return Err(eyre!("load command at {cursor} runs off end"));
            }
            let cmd = u32::from_le_bytes(buf[cursor..cursor + 4].try_into().unwrap());
            let cmdsize = u32::from_le_bytes(buf[cursor + 4..cursor + 8].try_into().unwrap()) as usize;
            if cmdsize < 8 || cursor + cmdsize > cmd_end {
                return Err(eyre!(
                    "load command at {cursor} has invalid cmdsize {cmdsize}"
                ));
            }
            let body = &buf[cursor..cursor + cmdsize];
            match cmd & !LC_REQ_DYLD {
                LC_SEGMENT_64 => segments.push(Segment::parse(body)?),
                LC_SYMTAB => symtab = Some(Symtab::parse(body)?),
                _ => {}
            }
            cursor += cmdsize;
        }
        Ok((segments, symtab))
    }
}

#[derive(Debug)]
struct Segment {
    name: String,
    vmaddr: u64,
    vmsize: u64,
    fileoff: u64,
    filesize: u64,
}

impl Segment {
    fn parse(body: &[u8]) -> Result<Self> {
        // segment_command_64 = 72 bytes, plus nsects * section_64
        // (which we ignore). Field offsets within the load command:
        //   0..4   cmd
        //   4..8   cmdsize
        //   8..24  segname[16]
        //  24..32  vmaddr
        //  32..40  vmsize
        //  40..48  fileoff
        //  48..56  filesize
        if body.len() < 72 {
            return Err(eyre!("LC_SEGMENT_64 body too small"));
        }
        let segname = &body[8..24];
        let name_end = segname.iter().position(|&b| b == 0).unwrap_or(16);
        let name = std::str::from_utf8(&segname[..name_end])
            .map_err(|e| eyre!("non-UTF8 segname: {e}"))?
            .to_owned();
        Ok(Self {
            name,
            vmaddr:   u64::from_le_bytes(body[24..32].try_into().unwrap()),
            vmsize:   u64::from_le_bytes(body[32..40].try_into().unwrap()),
            fileoff:  u64::from_le_bytes(body[40..48].try_into().unwrap()),
            filesize: u64::from_le_bytes(body[48..56].try_into().unwrap()),
        })
    }
}

#[derive(Debug)]
struct Symtab {
    symoff: u32,
    nsyms: u32,
    stroff: u32,
    strsize: u32,
}

impl Symtab {
    fn parse(body: &[u8]) -> Result<Self> {
        // symtab_command = 24 bytes:
        //   0..4   cmd
        //   4..8   cmdsize
        //   8..12  symoff
        //  12..16  nsyms
        //  16..20  stroff
        //  20..24  strsize
        if body.len() < 24 {
            return Err(eyre!("LC_SYMTAB body too small"));
        }
        Ok(Self {
            symoff:  u32::from_le_bytes(body[8..12].try_into().unwrap()),
            nsyms:   u32::from_le_bytes(body[12..16].try_into().unwrap()),
            stroff:  u32::from_le_bytes(body[16..20].try_into().unwrap()),
            strsize: u32::from_le_bytes(body[20..24].try_into().unwrap()),
        })
    }
}

#[derive(Debug, Clone, Copy)]
struct NList {
    n_strx: u32,
    n_value: u64,
}

impl NList {
    fn parse(b: &[u8]) -> Self {
        Self {
            n_strx:  u32::from_le_bytes(b[0..4].try_into().unwrap()),
            // n_type(1), n_sect(1), n_desc(2) at 4..8 — unused.
            n_value: u64::from_le_bytes(b[8..16].try_into().unwrap()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hand-roll a minimal Mach-O 64-bit bundle. Lays out:
    ///   - `mach_header_64` (32 bytes)
    ///   - `LC_SEGMENT_64` (72 bytes, 0 sections) covering __DATA
    ///   - `LC_SYMTAB` (24 bytes)
    ///   - segment payload (the struct + name string)
    ///   - symbol table + string table
    struct MachOBuilder {
        seg_vmaddr: u64,
        seg_data: Vec<u8>,
        strtab: Vec<u8>,
        symbols: Vec<(u32, u64)>, // (n_strx, n_value)
    }

    impl MachOBuilder {
        fn new() -> Self {
            Self {
                seg_vmaddr: 0x1000,
                seg_data: Vec::new(),
                // Mach-O strtab index 0 is conventionally a single
                // null byte (the empty string).
                strtab: vec![0],
                symbols: Vec::new(),
            }
        }

        fn add_string(&mut self, s: &str) -> u32 {
            let off = self.strtab.len() as u32;
            self.strtab.extend_from_slice(s.as_bytes());
            self.strtab.push(0);
            off
        }

        fn add_data(&mut self, bytes: &[u8]) -> u64 {
            let vmaddr = self.seg_vmaddr + self.seg_data.len() as u64;
            self.seg_data.extend_from_slice(bytes);
            vmaddr
        }

        fn add_symbol(&mut self, name: &str, value: u64) {
            let off = self.add_string(name);
            self.symbols.push((off, value));
        }

        fn build(self) -> Vec<u8> {
            // Plan the layout up-front so we can fill load commands
            // with final file offsets.
            //
            //   [0..32)    mach_header_64
            //   [32..104)  LC_SEGMENT_64
            //   [104..128) LC_SYMTAB
            //   [128..)    segment payload (seg_data)
            //   then       symbols (16 bytes each)
            //   then       strtab
            let mut buf = vec![0u8; 32 + 72 + 24];
            let seg_fileoff = buf.len() as u64;
            buf.extend_from_slice(&self.seg_data);
            let symoff = buf.len() as u32;
            for (n_strx, n_value) in &self.symbols {
                let mut entry = [0u8; 16];
                entry[0..4].copy_from_slice(&n_strx.to_le_bytes());
                entry[8..16].copy_from_slice(&n_value.to_le_bytes());
                buf.extend_from_slice(&entry);
            }
            let nsyms = self.symbols.len() as u32;
            let stroff = buf.len() as u32;
            buf.extend_from_slice(&self.strtab);
            let strsize = self.strtab.len() as u32;

            // mach_header_64.
            buf[0..4].copy_from_slice(&MH_MAGIC_64);
            // cputype/cpusubtype/filetype/flags/reserved left as zeros.
            buf[16..20].copy_from_slice(&2u32.to_le_bytes()); // ncmds = 2
            buf[20..24].copy_from_slice(&(72u32 + 24).to_le_bytes()); // sizeofcmds

            // LC_SEGMENT_64 at offset 32 (72 bytes, 0 sections).
            let mut seg = [0u8; 72];
            seg[0..4].copy_from_slice(&LC_SEGMENT_64.to_le_bytes());
            seg[4..8].copy_from_slice(&72u32.to_le_bytes());
            // segname "__DATA\0"...
            seg[8..14].copy_from_slice(b"__DATA");
            seg[24..32].copy_from_slice(&self.seg_vmaddr.to_le_bytes());
            seg[32..40].copy_from_slice(&(self.seg_data.len() as u64).to_le_bytes());
            seg[40..48].copy_from_slice(&seg_fileoff.to_le_bytes());
            seg[48..56].copy_from_slice(&(self.seg_data.len() as u64).to_le_bytes());
            buf[32..104].copy_from_slice(&seg);

            // LC_SYMTAB at offset 104 (24 bytes).
            let mut sym = [0u8; 24];
            sym[0..4].copy_from_slice(&LC_SYMTAB.to_le_bytes());
            sym[4..8].copy_from_slice(&24u32.to_le_bytes());
            sym[8..12].copy_from_slice(&symoff.to_le_bytes());
            sym[12..16].copy_from_slice(&nsyms.to_le_bytes());
            sym[16..20].copy_from_slice(&stroff.to_le_bytes());
            sym[20..24].copy_from_slice(&strsize.to_le_bytes());
            buf[104..128].copy_from_slice(&sym);

            buf
        }
    }

    #[test]
    fn detects_regular_module_extension() {
        let mut b = MachOBuilder::new();
        let mut struct_bytes = vec![0u8; 32];
        struct_bytes.extend_from_slice(&[0u8; 8]); // pointer slot
        let struct_vmaddr = b.add_data(&struct_bytes);
        let name_vmaddr = b.add_data(b"redis\0");
        let idx = (struct_vmaddr - b.seg_vmaddr) as usize + 32;
        b.seg_data[idx..idx + 8].copy_from_slice(&name_vmaddr.to_le_bytes());

        b.add_symbol("_redis_module_entry", struct_vmaddr);
        let got = detect_from_bytes(&b.build()).unwrap();
        assert_eq!(got, DetectedExt { name: "redis".into(), zend: false });
    }

    #[test]
    fn detects_zend_extension() {
        let mut b = MachOBuilder::new();
        let mut struct_bytes = vec![0u8; 8]; // pointer slot
        struct_bytes.extend_from_slice(&[0u8; 24]);
        let struct_vmaddr = b.add_data(&struct_bytes);
        let name_vmaddr = b.add_data(b"Xdebug\0");
        let idx = (struct_vmaddr - b.seg_vmaddr) as usize;
        b.seg_data[idx..idx + 8].copy_from_slice(&name_vmaddr.to_le_bytes());

        b.add_symbol("_zend_extension_entry", struct_vmaddr);
        let got = detect_from_bytes(&b.build()).unwrap();
        assert_eq!(got, DetectedExt { name: "Xdebug".into(), zend: true });
    }

    #[test]
    fn zend_wins_when_both_present() {
        let mut b = MachOBuilder::new();
        let mut zend_struct = vec![0u8; 8];
        zend_struct.extend_from_slice(&[0u8; 24]);
        let zend_vmaddr = b.add_data(&zend_struct);
        let zend_name = b.add_data(b"xdebug\0");
        let idx = (zend_vmaddr - b.seg_vmaddr) as usize;
        b.seg_data[idx..idx + 8].copy_from_slice(&zend_name.to_le_bytes());

        let mod_struct = vec![0u8; 40];
        let mod_vmaddr = b.add_data(&mod_struct);
        let mod_name = b.add_data(b"xdebug_as_mod\0");
        let idx2 = (mod_vmaddr - b.seg_vmaddr) as usize + 32;
        b.seg_data[idx2..idx2 + 8].copy_from_slice(&mod_name.to_le_bytes());

        b.add_symbol("_zend_extension_entry", zend_vmaddr);
        b.add_symbol("_xdebug_module_entry", mod_vmaddr);
        let got = detect_from_bytes(&b.build()).unwrap();
        assert_eq!(got, DetectedExt { name: "xdebug".into(), zend: true });
    }

    #[test]
    fn rejects_non_macho() {
        // Bytes that are clearly not Mach-O magic. We bypass binfmt's
        // dispatch here because we're unit-testing the macho parser
        // directly — binfmt has its own tests for the magic switch.
        let buf = vec![0u8; 32];
        let err = detect_from_bytes(&buf).unwrap_err();
        assert!(err.to_string().contains("Mach-O"), "got: {err}");
    }

    #[test]
    fn errors_when_no_module_symbol() {
        let mut b = MachOBuilder::new();
        let v = b.add_data(&[0u8; 40]);
        b.add_symbol("_some_unrelated_symbol", v);
        let err = detect_from_bytes(&b.build()).unwrap_err();
        assert!(
            err.to_string().contains("_module_entry")
                || err.to_string().contains("zend_extension_entry"),
            "got: {err}"
        );
    }
}
