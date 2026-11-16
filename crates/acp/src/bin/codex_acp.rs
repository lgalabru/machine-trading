//! `mtx codex-acp` as a standalone binary, for tests.

fn main() -> anyhow::Result<()> {
    mtx_acp::codex::serve_stdio()
}
