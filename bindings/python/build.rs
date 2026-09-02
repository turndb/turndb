//! A PyO3 extension module leaves the interpreter's symbols unresolved until Python loads it.
//! maturin passes the linker flags that allow that on macOS (`-undefined dynamic_lookup`); a bare
//! `cargo build -p turndb-python` — which the CI binding jobs and the conformance tests use — does
//! not, and on macOS the link fails with every `_Py*` symbol undefined. This is PyO3's own remedy:
//! it emits exactly those flags, only on macOS, only for an extension module. Found by the first
//! macOS run of the Python binding job.
fn main() {
    pyo3_build_config::add_extension_module_link_args();
}
