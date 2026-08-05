//! Build script: stage an embeddable documentation site inside `OUT_DIR`.
//!
//! The documentation is built with `mkdocs build` into `site/` and baked into
//! the binary (see `src/web/docs.rs`). `rust_embed` needs that directory to
//! exist at compile time, but a plain `cargo build` must work on a machine
//! without Python/mkdocs — and `cargo publish` refuses to package a crate whose
//! build script writes anywhere outside `OUT_DIR`.
//!
//! So the embedded copy always lives in `$OUT_DIR/site`: the real site is copied
//! there when mkdocs has run, and a tiny fully-offline placeholder is written
//! there when it has not. The source tree is only ever read.

use std::fs;
use std::path::Path;

const PLACEHOLDER: &str = "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
<title>YANuget documentation</title>\
<style>body{font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,Helvetica,Arial,\
sans-serif;max-width:40rem;margin:4rem auto;padding:0 1rem;line-height:1.5;color:#222}\
code{background:#f2f2f2;padding:.1em .35em;border-radius:4px}\
a{color:#0b6bcb}</style></head><body>\
<h1>Documentation not bundled</h1>\
<p>This build of YANuget did not include the rendered documentation site. \
Build it with <code>mkdocs build</code> (see <code>requirements-docs.txt</code>) and rebuild, \
or use an official release binary or container image, which ship the full docs.</p>\
<p>The Markdown sources live in the <code>docs/</code> directory of \
<a href=\"https://github.com/thoscut/yanuget\">the repository</a>.</p>\
</body></html>";

fn main() {
    // Cargo guarantees `OUT_DIR` for build scripts.
    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR set by cargo");
    let staged = Path::new(&out_dir).join("site");

    // Start from a clean slate: a stale file from an earlier build (say, the
    // placeholder from before mkdocs was run) would otherwise be embedded
    // alongside the real site and shadow a page with the same name.
    if staged.exists() {
        fs::remove_dir_all(&staged).expect("clear staged docs directory");
    }
    fs::create_dir_all(&staged).expect("create staged docs directory");

    let source = Path::new("site");
    if source.join("index.html").exists() {
        copy_tree(source, &staged);
    } else {
        fs::write(staged.join("index.html"), PLACEHOLDER).expect("write placeholder docs index");
    }

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=site");
}

/// Recursively copy `from` into `to`, which must already exist.
fn copy_tree(from: &Path, to: &Path) {
    for entry in fs::read_dir(from).expect("read docs directory") {
        let entry = entry.expect("read docs directory entry");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("stat docs entry").is_dir() {
            fs::create_dir_all(&target).expect("create staged docs subdirectory");
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target).expect("copy docs file");
        }
    }
}
