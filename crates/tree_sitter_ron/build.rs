fn main() {
    let grammar_dir = std::path::Path::new("grammar");
    cc::Build::new()
        .include(grammar_dir)
        .std("c11")
        .flag_if_supported("-Wno-unused-parameter")
        .flag_if_supported("-Wno-unused-but-set-variable")
        .flag_if_supported("-Wno-trigraphs")
        .file(grammar_dir.join("parser.c"))
        .file(grammar_dir.join("scanner.c"))
        .compile("tree-sitter-ron");
    println!("cargo:rerun-if-changed=grammar");
}
