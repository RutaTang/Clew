// clew patch: compiles the generated parser the release ships. The release's
// build script also ran the `tree-sitter` CLI to generate one when `src/parser.c`
// is missing, which it never is here.
fn main() {
    let src_dir = std::path::Path::new("src");
    println!("cargo:rerun-if-changed=src/parser.c");
    println!("cargo:rerun-if-changed=src/scanner.c");
    cc::Build::new()
        .include(src_dir)
        .flag_if_supported("-Wno-unused-parameter")
        .flag_if_supported("-Wno-unused-but-set-variable")
        .flag_if_supported("-Wno-trigraphs")
        .opt_level(2)
        .file(src_dir.join("parser.c"))
        .file(src_dir.join("scanner.c"))
        .compile("parser");
}
