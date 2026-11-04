// clew patch: compiles the generated parser the release ships. The release's
// build script also ran the `tree-sitter` CLI to generate one when `src/parser.c`
// is missing, which it never is here.
fn main() {
    let src_dir = std::path::Path::new("src");
    println!("cargo:rerun-if-changed=src/parser.c");
    println!("cargo:rerun-if-changed=src/scanner.c");
    let mut build = cc::Build::new();
    build
        .include(src_dir)
        .flag_if_supported("-Wno-unused-parameter")
        .flag_if_supported("-Wno-unused-but-set-variable")
        .flag_if_supported("-Wno-trigraphs")
        .opt_level(2);
    // clew patch: node names such as "\u03bb" (λ) must be UTF-8 in the
    // compiled grammar, as the queries spell them. GCC and Clang encode a
    // string literal so; MSVC encodes it in the system's code page unless
    // told otherwise, and on Windows the highlight query then named a node
    // the grammar did not have, and Lean was not highlighted at all.
    if build.get_compiler().is_like_msvc() {
        build.flag("/utf-8");
    }
    build
        .file(src_dir.join("parser.c"))
        .file(src_dir.join("scanner.c"))
        .compile("parser");
}
