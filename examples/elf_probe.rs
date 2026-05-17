//! One-shot helper: run the ELF parser against a `.so` and print the
//! detected name + extension kind. Kept as an example rather than a
//! subcommand because it's a developer tool for validating the parser
//! against real-world PHP extensions, not user-facing functionality.

fn main() {
    let path = std::env::args().nth(1).expect("usage: elf_probe <path-to-.so>");
    match bougie::elf::detect_php_extension(std::path::Path::new(&path)) {
        Ok(d) => println!("name={} zend={}", d.name, d.zend),
        Err(e) => {
            eprintln!("ERROR: {e:#}");
            std::process::exit(1);
        }
    }
}
