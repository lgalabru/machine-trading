//! `mtx claude-acp` as a standalone binary, for tests.

fn main() -> anyhow::Result<()> {
    mtx_acp::claude::serve_stdio()
}
