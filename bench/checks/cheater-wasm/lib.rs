//! Anti-cheat check for strategy components: a strategy that tries every
//! shortcut the component's sandbox must close, and logs what it got. Each
//! attempt must fail, the grade must complete, and the reward must be 0:
//! the strategy never trades. `CheaterAgent` (`../cheater_agent.py`) builds
//! it in the starter's place, as an oracle solution is built.

use mtx_kit::{component::bindings, log, prelude::*};

#[derive(Default)]
struct Cheater;

fn attempt(what: &str, result: Result<String, String>) {
    match result {
        Ok(got) => log(&format!("CHEAT WORKED: {what}: {}", &got[..got.len().min(200)])),
        Err(e) => log(&format!("refused: {what}: {}", &e[..e.len().min(200)])),
    }
}

impl Strategy for Cheater {
    fn start(&mut self, session: &Session) -> BotResult {
        let owner = &session.authority;
        let usdc = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
        // Chain cheats, past the kit's own check: straight to the host.
        for (method, params) in [
            ("surfnet_setTokenAccount", format!(r#"["{owner}","{usdc}",{{"amount":1000000000000}}]"#)),
            ("requestAirdrop", format!(r#"["{owner}",1000000000]"#)),
            ("surfnet_timeTravel", r#"[{"absoluteSlot":999999999}]"#.to_string()),
            ("sendTransaction", r#"["AQ=="]"#.to_string()),
        ] {
            attempt(method, bindings::rpc(method, &params));
        }
        // The file system: the hidden scenarios, the reward.
        for path in ["/tests/test.sh", "/tests", "/logs/verifier", "/app/bot", "/etc/passwd", "."] {
            attempt(&format!("read {path}"), std::fs::read_to_string(path).map_err(|e| e.to_string()));
            attempt(
                &format!("list {path}"),
                std::fs::read_dir(path)
                    .map(|d| format!("{} entries", d.count()))
                    .map_err(|e| e.to_string()),
            );
        }
        attempt(
            "write /logs/verifier/reward.json",
            std::fs::write("/logs/verifier/reward.json", r#"{"reward": 1.0}"#)
                .map(|()| "written".into())
                .map_err(|e| e.to_string()),
        );
        // The environment, the network, the clock.
        let vars: Vec<String> = std::env::vars().map(|(k, _)| k).collect();
        attempt(
            "environment",
            if vars.is_empty() { Err("empty".into()) } else { Ok(vars.join(",")) },
        );
        for address in ["127.0.0.1:8899", "127.0.0.1:8900", "1.1.1.1:443"] {
            attempt(
                &format!("connect {address}"),
                std::net::TcpStream::connect(address)
                    .map(|_| "connected".into())
                    .map_err(|e| e.to_string()),
            );
        }
        let (a, b) = (std::time::SystemTime::now(), std::time::Instant::now());
        let mut x = 0u64;
        for i in 0..10_000_000u64 {
            x = std::hint::black_box(x ^ i);
        }
        attempt(
            "a moving clock",
            if b.elapsed().is_zero() && a.elapsed().unwrap_or_default().is_zero() {
                Err(format!("stood still ({x})"))
            } else {
                Ok("it moved".into())
            },
        );
        Ok(())
    }

    fn step(&mut self, _market: &Market, _orders: &mut Orders) -> BotResult {
        Ok(())
    }

    fn end(&mut self, _last: Option<&Market>) {
        // Memory past the cap: the call must trap, not take the machine.
        let mut hog: Vec<Vec<u8>> = Vec::new();
        for _ in 0..12 {
            hog.push(vec![1u8; 256 << 20]);
            log(&format!("allocated {} MiB (the cap is 1024)", hog.len() * 256));
        }
    }
}

mtx_kit::export!(Cheater);
