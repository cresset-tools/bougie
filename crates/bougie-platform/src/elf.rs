//! Minimal ELF64 LE parser, scoped to one job: given a PHP-extension
//! `.so`, identify the canonical extension name and whether it loads
//! as `extension=` or `zend_extension=`. This avoids shelling out to
//! PHP just to read a string the file already contains.
//!
//! ## References
//!
//! - "ELF-64 Object File Format" (Matz/Hubicka/Jaeger, Draft v1.5,
//!   1998). Defines the on-disk layout this module reads.
//! - System V Application Binary Interface (gABI), §4 "Object Files".
//! - PHP source `Zend/zend_modules.h` (`struct _zend_module_entry`)
//!   and `Zend/zend_extensions.h` (`struct _zend_extension`) for the
//!   struct layouts whose `name` field we walk to.
//!
//! Scope deliberately omitted: ELF32, big-endian, program headers,
//! relocations, dynamic-section parsing. PHP extensions in the wild
//! are 64-bit LE; ELF32 / BE builds aren't relevant. Mach-O / PE
//! aren't ELF and aren't handled here.

// File offsets, virtual addresses, and addends are u64 in the ELF64
// wire format. Bougie's supported targets (Linux/macOS/Windows on
// x86_64 / aarch64 — see CI matrix) are all 64-bit, so `u64 as usize`
// is lossless. `r_addend as u64` is gated on a `< 0` check above the
// cast. Section sizes we write back (`self.dynstr.len() as u32`) are
// our own buffers that never approach 4 GB.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
)]

use crate::binfmt::DetectedExt;
use eyre::{eyre, Result};

const ELFMAG: &[u8; 4] = b"\x7fELF";
const ELFCLASS64: u8 = 2;
const ELFDATA2LSB: u8 = 1;

const SHT_SYMTAB: u32 = 2;
const SHT_STRTAB: u32 = 3;
const SHT_RELA: u32 = 4;
const SHT_DYNSYM: u32 = 11;

/// `R_X86_64_RELATIVE`: the dynamic linker writes `base + r_addend`
/// at `r_offset`. In a shared library the link-time base is 0, so the
/// addend is the absolute link-time vaddr of the target — exactly the
/// value the file would have stored at that location if the build had
/// emitted absolute pointers. (System V AMD64 ABI, §B.1.)
const R_X86_64_RELATIVE: u32 = 8;
/// `R_AARCH64_RELATIVE`: same semantics, ARM64 ABI §5.7.4.
const R_AARCH64_RELATIVE: u32 = 1027;

/// On 64-bit, `zend_module_entry.name` lives at offset 32:
///
/// ```text
///   off  size  field
///   ---  ----  ------------------------
///     0    2   unsigned short size
///     2    2   (padding)
///     4    4   unsigned int   zend_api
///     8    1   unsigned char  zend_debug
///     9    1   unsigned char  zts
///    10    6   (padding to 8-byte align)
///    16    8   const zend_ini_entry  *ini_entry
///    24    8   const zend_module_dep *deps
///    32    8   const char            *name   <-- target
/// ```
const ZEND_MODULE_ENTRY_NAME_OFFSET: u64 = 32;

/// `zend_extension.name` is the very first field, so offset 0.
const ZEND_EXTENSION_ENTRY_NAME_OFFSET: u64 = 0;

pub fn detect_from_bytes(buf: &[u8]) -> Result<DetectedExt> {
    let hdr = ElfHeader::parse(buf)?;
    let sections = hdr.sections(buf)?;

    // Prefer .dynsym: it's the dynamic-link symbol table and survives
    // `strip`, while .symtab is routinely removed in release builds.
    let sym_section = sections.iter()
        .find(|s| s.sh_type == SHT_DYNSYM)
        .or_else(|| sections.iter().find(|s| s.sh_type == SHT_SYMTAB))
        .ok_or_else(|| eyre!("no .dynsym or .symtab section found"))?;

    let strtab = sections.get(sym_section.sh_link as usize)
        .filter(|s| s.sh_type == SHT_STRTAB)
        .ok_or_else(|| {
            eyre!(
                "symbol table's sh_link={} doesn't point at a strtab",
                sym_section.sh_link
            )
        })?;
    let strtab_bytes = strtab.data(buf)?;

    if sym_section.sh_entsize != 24 {
        return Err(eyre!(
            "unexpected ELF64 sym entry size {} (expected 24)",
            sym_section.sh_entsize
        ));
    }
    let sym_data = sym_section.data(buf)?;

    let mut zend_entry: Option<Symbol> = None;
    let mut module_entries: Vec<(String, Symbol)> = Vec::new();
    for chunk in sym_data.chunks_exact(24) {
        let sym = Symbol::parse(chunk);
        if sym.st_value == 0 {
            // Undefined / external symbol — no struct lives in this
            // .so at that address. Always true for the index-0 NULL
            // entry; sometimes true for unresolved imports.
            continue;
        }
        let name = read_cstring(strtab_bytes, sym.st_name as usize)?;
        if name == "zend_extension_entry" {
            zend_entry = Some(sym);
            // Don't break — let the loop finish so a synthetic test
            // with both symbols still works; Zend wins below.
        } else if name.ends_with("_module_entry") && name != "_module_entry" {
            module_entries.push((name.to_owned(), sym));
        }
    }

    if let Some(sym) = zend_entry {
        let name = read_indirect_string(
            buf,
            &sections,
            sym.st_value,
            ZEND_EXTENSION_ENTRY_NAME_OFFSET,
        )?;
        return Ok(DetectedExt { name, zend: true });
    }
    // Most extensions have exactly one `<foo>_module_entry`. If we
    // see several, take the first — we can't disambiguate without a
    // full disassembly of get_module(), and the alternatives are
    // typically support structs that wouldn't be valid module entries.
    if let Some((_, sym)) = module_entries.into_iter().next() {
        let name = read_indirect_string(
            buf,
            &sections,
            sym.st_value,
            ZEND_MODULE_ENTRY_NAME_OFFSET,
        )?;
        return Ok(DetectedExt { name, zend: false });
    }
    Err(eyre!(
        "no `zend_extension_entry` or `*_module_entry` symbol found — \
         this `.so` doesn't look like a PHP extension, or its module \
         entry is a static (file-scoped) symbol that was stripped"
    ))
}

/// Read the C string pointed to by `*(struct_vaddr + field_offset)`.
/// Resolves the two vaddr→file-offset lookups one after the other:
/// first to fetch the pointer field, then to follow it to the string.
///
/// In a PIC shared library the file's on-disk pointer bytes are often
/// zero: the dynamic linker writes the real value at load time using
/// `R_*_RELATIVE` relocations in `.rela.dyn`. We therefore consult
/// the relocation table when the in-file pointer comes back NULL.
fn read_indirect_string(
    buf: &[u8],
    sections: &[Section],
    struct_vaddr: u64,
    field_offset: u64,
) -> Result<String> {
    let ptr_vaddr = struct_vaddr
        .checked_add(field_offset)
        .ok_or_else(|| eyre!("overflow computing field vaddr"))?;
    let name_vaddr = read_pointer_at_vaddr(buf, sections, ptr_vaddr)?;
    if name_vaddr == 0 {
        return Err(eyre!(
            "name pointer at vaddr {ptr_vaddr:#x} is NULL even after \
             walking relocations — extension `.so` is unexpected shape"
        ));
    }
    let name_off = vaddr_to_file_off(sections, name_vaddr, 1)?;
    Ok(read_cstring(buf, name_off)?.to_owned())
}

/// Return the runtime value of an 8-byte pointer at `ptr_vaddr`. If
/// the on-disk bytes are non-zero, that's the answer (common when
/// the toolchain emits absolute addresses). Otherwise scan `.rela.dyn`
/// (and friends) for a `R_*_RELATIVE` reloc at this offset and use
/// its addend.
fn read_pointer_at_vaddr(buf: &[u8], sections: &[Section], ptr_vaddr: u64) -> Result<u64> {
    let ptr_off = vaddr_to_file_off(sections, ptr_vaddr, 8)?;
    let end = ptr_off
        .checked_add(8)
        .ok_or_else(|| eyre!("pointer file offset {ptr_off} overflows"))?;
    let bytes = buf
        .get(ptr_off..end)
        .ok_or_else(|| eyre!("pointer at file offset {ptr_off} out of range (buf={})", buf.len()))?;
    let mut p = [0u8; 8];
    p.copy_from_slice(bytes);
    let direct = u64::from_le_bytes(p);
    if direct != 0 {
        return Ok(direct);
    }
    if let Some(addend) = find_relative_reloc(buf, sections, ptr_vaddr)? {
        return Ok(addend);
    }
    Ok(0)
}

/// Walk every `SHT_RELA` section, looking for a relocation whose
/// `r_offset` matches `ptr_vaddr` and whose type is one of the
/// "relative" forms. Returns the addend (== runtime target vaddr in
/// a shared lib whose link-time base is 0).
fn find_relative_reloc(buf: &[u8], sections: &[Section], ptr_vaddr: u64) -> Result<Option<u64>> {
    for s in sections {
        if s.sh_type != SHT_RELA {
            continue;
        }
        if s.sh_entsize != 24 {
            // ELF64 Rela is always 24 bytes; anything else means
            // either a corrupt file or we're looking at SHT_REL by
            // mistake.
            continue;
        }
        let data = s.data(buf)?;
        for chunk in data.chunks_exact(24) {
            let r_offset = u64::from_le_bytes(chunk[0..8].try_into().unwrap());
            if r_offset != ptr_vaddr {
                continue;
            }
            let r_info   = u64::from_le_bytes(chunk[8..16].try_into().unwrap());
            let r_addend = i64::from_le_bytes(chunk[16..24].try_into().unwrap());
            // ELF64_R_TYPE: low 32 bits of r_info.
            let r_type = (r_info & 0xffff_ffff) as u32;
            if r_type == R_X86_64_RELATIVE || r_type == R_AARCH64_RELATIVE {
                if r_addend < 0 {
                    return Err(eyre!(
                        "RELATIVE reloc at {ptr_vaddr:#x} has negative addend {r_addend} — refusing"
                    ));
                }
                return Ok(Some(r_addend as u64));
            }
        }
    }
    Ok(None)
}

/// Map a virtual address to a file offset by finding the section
/// whose `[sh_addr, sh_addr+sh_size)` range contains it. Non-loaded
/// sections (`sh_addr` == 0) are skipped — those are link-time
/// artifacts like .symtab / .strtab that don't get a runtime mapping.
fn vaddr_to_file_off(sections: &[Section], vaddr: u64, len: u64) -> Result<usize> {
    for s in sections {
        if s.sh_addr == 0 {
            continue;
        }
        let end = s.sh_addr.saturating_add(s.sh_size);
        if vaddr >= s.sh_addr && vaddr.saturating_add(len) <= end {
            let delta = vaddr - s.sh_addr;
            let off = s
                .sh_offset
                .checked_add(delta)
                .ok_or_else(|| eyre!("file offset overflow mapping vaddr {vaddr:#x}"))?;
            return usize::try_from(off)
                .map_err(|_| eyre!("file offset {off} too large for this platform"));
        }
    }
    Err(eyre!(
        "vaddr {vaddr:#x} ({len} bytes) not within any loaded section"
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

#[derive(Debug)]
struct ElfHeader {
    e_shoff: u64,
    e_shentsize: u16,
    e_shnum: u16,
}

impl ElfHeader {
    fn parse(buf: &[u8]) -> Result<Self> {
        if buf.len() < 64 {
            return Err(eyre!("file too small for an ELF64 header"));
        }
        if &buf[0..4] != ELFMAG {
            return Err(eyre!("not an ELF file (bad magic {:02x?})", &buf[0..4]));
        }
        if buf[4] != ELFCLASS64 {
            return Err(eyre!("only ELF64 is supported (got EI_CLASS={})", buf[4]));
        }
        if buf[5] != ELFDATA2LSB {
            return Err(eyre!("only little-endian ELF is supported (got EI_DATA={})", buf[5]));
        }
        let e_shoff     = u64::from_le_bytes(buf[40..48].try_into().unwrap());
        let e_shentsize = u16::from_le_bytes(buf[58..60].try_into().unwrap());
        let e_shnum     = u16::from_le_bytes(buf[60..62].try_into().unwrap());
        if e_shentsize != 64 {
            return Err(eyre!("unexpected e_shentsize {e_shentsize} (expected 64)"));
        }
        Ok(Self { e_shoff, e_shentsize, e_shnum })
    }

    fn sections(&self, buf: &[u8]) -> Result<Vec<Section>> {
        let start = self.e_shoff as usize;
        let count = self.e_shnum as usize;
        let stride = self.e_shentsize as usize;
        let total = stride.checked_mul(count)
            .ok_or_else(|| eyre!("section-table size overflow"))?;
        let end = start.checked_add(total)
            .ok_or_else(|| eyre!("section-table extent overflow"))?;
        if end > buf.len() {
            return Err(eyre!("section table extends past end of file"));
        }
        let mut out = Vec::with_capacity(count);
        for i in 0..count {
            let off = start + i * stride;
            out.push(Section::parse(&buf[off..off + 64]));
        }
        Ok(out)
    }
}

#[derive(Debug, Clone, Copy)]
struct Section {
    sh_type: u32,
    sh_addr: u64,
    sh_offset: u64,
    sh_size: u64,
    sh_link: u32,
    sh_entsize: u64,
}

impl Section {
    fn parse(b: &[u8]) -> Self {
        Self {
            // sh_name (b[0..4]) is unused — we look up by sh_type, not
            // by name, so .shstrtab parsing isn't needed.
            sh_type:    u32::from_le_bytes(b[4..8].try_into().unwrap()),
            // sh_flags (b[8..16]) unused.
            sh_addr:    u64::from_le_bytes(b[16..24].try_into().unwrap()),
            sh_offset:  u64::from_le_bytes(b[24..32].try_into().unwrap()),
            sh_size:    u64::from_le_bytes(b[32..40].try_into().unwrap()),
            sh_link:    u32::from_le_bytes(b[40..44].try_into().unwrap()),
            // sh_info, sh_addralign unused.
            sh_entsize: u64::from_le_bytes(b[56..64].try_into().unwrap()),
        }
    }

    fn data<'a>(&self, buf: &'a [u8]) -> Result<&'a [u8]> {
        let start = self.sh_offset as usize;
        let end = start.checked_add(self.sh_size as usize)
            .ok_or_else(|| eyre!("section size overflow"))?;
        buf.get(start..end)
            .ok_or_else(|| eyre!("section data {start}..{end} out of bounds"))
    }
}

#[derive(Debug, Clone, Copy)]
struct Symbol {
    st_name: u32,
    st_value: u64,
}

impl Symbol {
    fn parse(b: &[u8]) -> Self {
        Self {
            st_name:  u32::from_le_bytes(b[0..4].try_into().unwrap()),
            // st_info(1), st_other(1), st_shndx(2) at 4..8 — unused.
            st_value: u64::from_le_bytes(b[8..16].try_into().unwrap()),
            // st_size (16..24) unused.
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hand-roll a minimal ELF64 LE that contains exactly what the
    /// parser needs: one loaded section holding a fake module/extension
    /// struct + its name string, plus a dynsym + dynstr describing the
    /// struct's symbol. The file isn't a valid loadable object — there
    /// are no program headers — but it satisfies every check in
    /// `detect_from_bytes`.
    struct ElfBuilder {
        data_vaddr: u64,
        data: Vec<u8>,
        dynstr: Vec<u8>,
        symbols: Vec<(u32, u64)>, // (st_name_offset, st_value)
        /// (`r_offset`, `r_addend`) for `R_X86_64_RELATIVE` entries.
        relocs: Vec<(u64, u64)>,
    }

    impl ElfBuilder {
        fn new() -> Self {
            // ELF convention: index 0 in a strtab is the empty string.
            Self {
                data_vaddr: 0x1000,
                data: Vec::new(),
                dynstr: vec![0],
                symbols: vec![(0, 0)], // index-0 NULL symbol
                relocs: Vec::new(),
            }
        }

        fn add_relative_reloc(&mut self, r_offset: u64, r_addend: u64) {
            self.relocs.push((r_offset, r_addend));
        }

        fn add_string(&mut self, s: &str) -> u32 {
            let off = self.dynstr.len() as u32;
            self.dynstr.extend_from_slice(s.as_bytes());
            self.dynstr.push(0);
            off
        }

        /// Append `bytes` to the .data section. Returns the vaddr of
        /// the start of those bytes.
        fn add_data(&mut self, bytes: &[u8]) -> u64 {
            let vaddr = self.data_vaddr + self.data.len() as u64;
            self.data.extend_from_slice(bytes);
            vaddr
        }

        fn add_symbol(&mut self, name: &str, value: u64) {
            let off = self.add_string(name);
            self.symbols.push((off, value));
        }

        fn build(self) -> Vec<u8> {
            let mut buf = vec![0u8; 64]; // ELF header — fill at the end.

            let data_offset = buf.len() as u64;
            buf.extend_from_slice(&self.data);

            let dynstr_offset = buf.len() as u64;
            buf.extend_from_slice(&self.dynstr);
            let dynstr_size = self.dynstr.len() as u64;

            let dynsym_offset = buf.len() as u64;
            for (st_name, st_value) in &self.symbols {
                let mut entry = [0u8; 24];
                entry[0..4].copy_from_slice(&st_name.to_le_bytes());
                entry[8..16].copy_from_slice(&st_value.to_le_bytes());
                buf.extend_from_slice(&entry);
            }
            let dynsym_size = (self.symbols.len() * 24) as u64;

            let rela_offset = buf.len() as u64;
            for (r_offset, r_addend) in &self.relocs {
                let mut entry = [0u8; 24];
                entry[0..8].copy_from_slice(&r_offset.to_le_bytes());
                let r_info: u64 = u64::from(R_X86_64_RELATIVE); // sym=0, type in low bits
                entry[8..16].copy_from_slice(&r_info.to_le_bytes());
                entry[16..24].copy_from_slice(&(*r_addend as i64).to_le_bytes());
                buf.extend_from_slice(&entry);
            }
            let rela_size = (self.relocs.len() * 24) as u64;
            let has_relocs = !self.relocs.is_empty();

            // Section header table: 4 (or 5) entries × 64 bytes.
            // [0] SHN_UNDEF, [1] .data PROGBITS, [2] .dynstr STRTAB,
            // [3] .dynsym DYNSYM, [4] .rela.dyn RELA (only when present).
            let shoff = buf.len() as u64;
            buf.extend_from_slice(&[0u8; 64]); // shdr[0] all zero

            let mut shdr = |sh_type: u32, sh_addr: u64, sh_offset: u64, sh_size: u64, sh_link: u32, sh_entsize: u64| {
                let mut e = [0u8; 64];
                e[4..8].copy_from_slice(&sh_type.to_le_bytes());
                e[16..24].copy_from_slice(&sh_addr.to_le_bytes());
                e[24..32].copy_from_slice(&sh_offset.to_le_bytes());
                e[32..40].copy_from_slice(&sh_size.to_le_bytes());
                e[40..44].copy_from_slice(&sh_link.to_le_bytes());
                e[56..64].copy_from_slice(&sh_entsize.to_le_bytes());
                buf.extend_from_slice(&e);
            };

            // [1] .data — loaded, holds the struct + name string.
            shdr(1, self.data_vaddr, data_offset, self.data.len() as u64, 0, 0);
            // [2] .dynstr — strtab, not loaded.
            shdr(3, 0, dynstr_offset, dynstr_size, 0, 0);
            // [3] .dynsym — dynsym, sh_link points at .dynstr (index 2).
            shdr(11, 0, dynsym_offset, dynsym_size, 2, 24);
            if has_relocs {
                // [4] .rela.dyn — SHT_RELA, not loaded.
                shdr(SHT_RELA, 0, rela_offset, rela_size, 0, 24);
            }

            // ELF header.
            buf[0..4].copy_from_slice(ELFMAG);
            buf[4] = ELFCLASS64;
            buf[5] = ELFDATA2LSB;
            buf[6] = 1; // EI_VERSION
            // e_shoff @ 40..48
            buf[40..48].copy_from_slice(&shoff.to_le_bytes());
            // e_shentsize @ 58..60 = 64
            buf[58..60].copy_from_slice(&64u16.to_le_bytes());
            // e_shnum
            let shnum: u16 = if has_relocs { 5 } else { 4 };
            buf[60..62].copy_from_slice(&shnum.to_le_bytes());
            // e_shstrndx @ 62..64 = 0 (we don't reference it)
            buf
        }
    }

    #[test]
    fn detects_regular_module_extension() {
        let mut b = ElfBuilder::new();
        // Layout in .data:
        //   [0..32]  zend_module_entry head (zeros — values irrelevant)
        //   [32..40] name pointer
        //   [40..]   "redis\0"
        let mut struct_bytes = vec![0u8; 32];
        // Reserve 8 bytes for the pointer; we'll patch after placing
        // the string so we know its vaddr.
        struct_bytes.extend_from_slice(&[0u8; 8]);
        let struct_vaddr = b.add_data(&struct_bytes);
        let name_vaddr = b.add_data(b"redis\0");
        // Patch the name pointer in-place.
        let ptr_idx = (struct_vaddr - b.data_vaddr) as usize + 32;
        b.data[ptr_idx..ptr_idx + 8].copy_from_slice(&name_vaddr.to_le_bytes());

        b.add_symbol("redis_module_entry", struct_vaddr);

        let bytes = b.build();
        let got = detect_from_bytes(&bytes).unwrap();
        assert_eq!(got, DetectedExt { name: "redis".into(), zend: false });
    }

    #[test]
    fn detects_zend_extension() {
        let mut b = ElfBuilder::new();
        // zend_extension struct: name pointer at offset 0.
        let mut struct_bytes = vec![0u8; 8];
        // Reserve 24 more bytes of zeros so the struct looks plausibly
        // sized — the parser only touches the first 8.
        struct_bytes.extend_from_slice(&[0u8; 24]);
        let struct_vaddr = b.add_data(&struct_bytes);
        let name_vaddr = b.add_data(b"Xdebug\0");
        let ptr_idx = (struct_vaddr - b.data_vaddr) as usize;
        b.data[ptr_idx..ptr_idx + 8].copy_from_slice(&name_vaddr.to_le_bytes());

        b.add_symbol("zend_extension_entry", struct_vaddr);

        let bytes = b.build();
        let got = detect_from_bytes(&bytes).unwrap();
        assert_eq!(got, DetectedExt { name: "Xdebug".into(), zend: true });
    }

    #[test]
    fn zend_wins_when_both_present() {
        // A few extensions (notably xdebug-style hybrids) export both
        // a module_entry AND zend_extension_entry; PHP loads them as
        // Zend extensions, so our detection should agree.
        let mut b = ElfBuilder::new();
        let mut zend_struct = vec![0u8; 8];
        zend_struct.extend_from_slice(&[0u8; 24]);
        let zend_vaddr = b.add_data(&zend_struct);
        let zend_name_vaddr = b.add_data(b"xdebug\0");
        let idx = (zend_vaddr - b.data_vaddr) as usize;
        b.data[idx..idx + 8].copy_from_slice(&zend_name_vaddr.to_le_bytes());

        let mut mod_struct = vec![0u8; 40];
        let mod_vaddr = b.add_data(&mod_struct);
        let _ = &mut mod_struct;
        let mod_name_vaddr = b.add_data(b"xdebug-as-module\0");
        let idx2 = (mod_vaddr - b.data_vaddr) as usize + 32;
        b.data[idx2..idx2 + 8].copy_from_slice(&mod_name_vaddr.to_le_bytes());

        b.add_symbol("zend_extension_entry", zend_vaddr);
        b.add_symbol("xdebug_module_entry", mod_vaddr);

        let got = detect_from_bytes(&b.build()).unwrap();
        assert_eq!(got, DetectedExt { name: "xdebug".into(), zend: true });
    }

    #[test]
    fn resolves_name_pointer_via_rela_dyn() {
        // Mirrors the real-world stripped-+-PIC layout: the struct's
        // name field is zero on disk; `ld.so` would patch it at load
        // time using a R_X86_64_RELATIVE entry whose addend is the
        // vaddr of the name string. Parser must consult `.rela.dyn`
        // when the in-file pointer is NULL.
        let mut b = ElfBuilder::new();
        let mut zend_struct = vec![0u8; 32]; // 8-byte pointer field stays zero
        let struct_vaddr = b.add_data(&zend_struct);
        let name_vaddr = b.add_data(b"Xdebug\0");
        let _ = &mut zend_struct;
        // No patching of struct bytes — they remain zero.
        b.add_relative_reloc(struct_vaddr, name_vaddr);
        b.add_symbol("zend_extension_entry", struct_vaddr);

        let got = detect_from_bytes(&b.build()).unwrap();
        assert_eq!(got, DetectedExt { name: "Xdebug".into(), zend: true });
    }

    #[test]
    fn rejects_non_elf() {
        // 64 bytes so we get past the length gate and hit the magic check.
        let buf = b"not an ELF at all, just text padded to sixty-four bytes ........";
        assert_eq!(buf.len(), 64);
        let err = detect_from_bytes(buf).unwrap_err();
        assert!(err.to_string().contains("bad magic"), "got: {err}");
    }

    #[test]
    fn rejects_elf32() {
        // Build a header that's ELF magic but EI_CLASS=1 (ELF32).
        let mut buf = vec![0u8; 64];
        buf[0..4].copy_from_slice(ELFMAG);
        buf[4] = 1; // ELFCLASS32
        buf[5] = ELFDATA2LSB;
        let err = detect_from_bytes(&buf).unwrap_err();
        assert!(err.to_string().contains("ELF64"), "got: {err}");
    }

    #[test]
    fn rejects_big_endian() {
        let mut buf = vec![0u8; 64];
        buf[0..4].copy_from_slice(ELFMAG);
        buf[4] = ELFCLASS64;
        buf[5] = 2; // ELFDATA2MSB
        let err = detect_from_bytes(&buf).unwrap_err();
        assert!(err.to_string().contains("little-endian"), "got: {err}");
    }

    #[test]
    fn errors_when_no_module_symbol() {
        let mut b = ElfBuilder::new();
        let v = b.add_data(&[0u8; 40]);
        b.add_symbol("some_unrelated_symbol", v);
        let err = detect_from_bytes(&b.build()).unwrap_err();
        assert!(
            err.to_string().contains("_module_entry") || err.to_string().contains("zend_extension_entry"),
            "got: {err}"
        );
    }
}
