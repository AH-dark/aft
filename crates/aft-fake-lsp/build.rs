// The helper includes LSP sources from the agent-file-tools crate. Those
// sources carry unit tests that already run in that crate; this cfg keeps
// them out of the helper's own test build so each test is listed once.
fn main() {
    println!("cargo:rustc-check-cfg=cfg(aft_fake_lsp_helper)");
    println!("cargo:rustc-cfg=aft_fake_lsp_helper");
}
