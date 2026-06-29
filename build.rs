//! Build script: guarantee an embeddable documentation site exists.
//!
//! The documentation is built with `mkdocs build` into `site/` and embedded into
//! the binary (see `src/web/docs.rs`). So that a plain `cargo build` always
//! compiles — even on a machine without Python/mkdocs — this script writes a
//! tiny, fully offline placeholder into `site/index.html` when no real site has
//! been built yet. CI runs `mkdocs build` *before* `cargo build`, so there the
//! real site is already present and is left untouched.

use std::fs;
use std::path::Path;

const PLACEHOLDER: &str = "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
<title>YANuget documentation</title>\
<style>body{font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,Helvetica,Arial,\
sans-serif;max-width:40rem;margin:4rem auto;padding:0 1rem;line-height:1.5;color:#222}\
code{background:#f2f2f2;padding:.1em .35em;border-radius:4px}</style></head><body>\
<h1>Documentation not bundled</h1>\
<p>This build of YANuget did not include the rendered documentation site. \
Build it with <code>mkdocs build</code> (see <code>requirements-docs.txt</code>) and rebuild, \
or use an official release binary, which ships the full docs.</p>\
<p>Meanwhile the Markdown sources live in the <code>docs/</code> directory of the repository.</p>\
</body></html>";

fn main() {
    let index = Path::new("site/index.html");
    if !index.exists() {
        fs::create_dir_all("site").expect("create site/ directory");
        fs::write(index, PLACEHOLDER).expect("write placeholder docs index");
    }
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=site");
}
