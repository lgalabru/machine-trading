//! `mtx bot check`: a bot directory against the shape the grader runs, the
//! one `mtx bot` command the agent's build keeps: the others start from a
//! checkout, which an agent's image doesn't hold.

use std::path::Path;

/// What's wrong with a bot directory, and what's worth knowing: problems
/// stop `practice` and `pack`; notes don't.
pub struct Checked {
    pub problems: Vec<String>,
    pub notes: Vec<String>,
}

/// `mtx bot check`: the bot directory against the contract's shape. A bot
/// is a strategy component, `bot.wasm`; a directory without one is a
/// process bot (a `Justfile` whose `run` starts `bin/bot`), which only
/// practice runs.
pub fn check(dir: &Path) -> Checked {
    let mut problems = Vec::new();
    let mut notes = Vec::new();
    let wasm = dir.join("bot.wasm");
    if wasm.exists() {
        match std::fs::read(&wasm) {
            Err(e) => problems.push(format!("can't read bot.wasm: {e}")),
            // A component's preamble: the wasm magic, then layer 1. A core
            // module (version 1, layer 0) isn't one.
            Ok(bytes) if bytes.get(..8) == Some(b"\0asm\x0d\0\x01\0") => {}
            Ok(bytes) if bytes.starts_with(b"\0asm") => problems.push(
                "bot.wasm is a core wasm module, not a component: build it for wasm32-wasip2 (`just build`)".into(),
            ),
            Ok(_) => problems.push("bot.wasm isn't wasm".into()),
        }
        stale(&wasm, dir, "bot.wasm", &mut notes);
        return Checked { problems, notes };
    }
    if !["Justfile", "justfile", "run.sh"]
        .iter()
        .any(|name| dir.join(name).is_file())
    {
        problems.push("no bot.wasm: build the strategy first (`just build`)".into());
        return Checked { problems, notes };
    }
    notes.push(
        "a process bot (no bot.wasm): practice runs it, but the bench grades strategy components only".into(),
    );
    let bin = dir.join("bin/bot");
    match std::fs::read(&bin) {
        Err(_) => problems.push("no bin/bot: build the bot first (`just build`)".into()),
        Ok(bytes) => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if std::fs::metadata(&bin).is_ok_and(|m| m.permissions().mode() & 0o111 == 0) {
                    problems.push("bin/bot isn't executable".into());
                }
            }
            let magic = bytes.get(..4).unwrap_or_default();
            let elf = magic == b"\x7fELF";
            let mach_o = matches!(
                magic,
                [0xcf, 0xfa, 0xed, 0xfe] | [0xfe, 0xed, 0xfa, 0xcf] | [0xca, 0xfe, 0xba, 0xbe]
            );
            if mach_o {
                notes.push(
                    "bin/bot is a macOS binary: it runs here, but the bench grades on Linux (Debian bookworm), from a Linux build"
                        .into(),
                );
            } else if !elf && !bytes.starts_with(b"#!") {
                notes.push("bin/bot is neither a native executable nor a script".into());
            }
            stale(&bin, dir, "bin/bot", &mut notes);
        }
    }
    Checked { problems, notes }
}

/// A note if `src/` changed since `built` was.
fn stale(built: &Path, dir: &Path, name: &str, notes: &mut Vec<String>) {
    if let (Ok(built), Some(newest)) = (
        std::fs::metadata(built).and_then(|m| m.modified()),
        newest_source(&dir.join("src")),
    ) && newest > built
    {
        notes.push(format!(
            "src/ changed since {name} was built: run `just build`"
        ));
    }
}

fn newest_source(dir: &Path) -> Option<std::time::SystemTime> {
    let mut newest = None;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        let time = if path.is_dir() {
            newest_source(&path)
        } else {
            entry.metadata().and_then(|m| m.modified()).ok()
        };
        newest = newest.max(time);
    }
    newest
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh directory holding `bot.wasm` with these bytes, checked.
    fn checked_with(name: &str, wasm: Option<&[u8]>) -> Checked {
        let dir = std::env::temp_dir().join(format!("mtx-bot-check-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        if let Some(bytes) = wasm {
            std::fs::write(dir.join("bot.wasm"), bytes).unwrap();
        }
        let checked = check(&dir);
        std::fs::remove_dir_all(&dir).unwrap();
        checked
    }

    #[test]
    fn only_a_component_is_a_bot_the_bench_grades() {
        let component = checked_with("component", Some(b"\0asm\x0d\0\x01\0 and the rest"));
        assert!(component.problems.is_empty(), "{:?}", component.problems);

        let module = checked_with("module", Some(b"\0asm\x01\0\0\0 and the rest"));
        assert!(
            module.problems[0].contains("core wasm module"),
            "{:?}",
            module.problems
        );

        let text = checked_with("text", Some(b"not wasm at all"));
        assert_eq!(text.problems, ["bot.wasm isn't wasm"]);

        // Shorter than a preamble: still wasm by its magic, and no component.
        let short = checked_with("short", Some(b"\0asm"));
        assert!(
            short.problems[0].contains("core wasm module"),
            "{:?}",
            short.problems
        );

        let none = checked_with("none", None);
        assert!(
            none.problems[0].starts_with("no bot.wasm"),
            "{:?}",
            none.problems
        );
    }
}
