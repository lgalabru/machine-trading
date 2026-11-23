//! How a host makes its own reads of the chain: how long it waits, when it
//! reads again, and how it turns the venues' exact amounts into `f64`.

use std::time::Duration;

use mtx_core::Decimal;

/// How long each of the grader's own requests to its surfnet may take (the
/// clients' default is 30 s). The world doesn't move while the grader waits,
/// so a starved machine makes a request slow, never different: it is waited
/// out, not failed. The bound is only for a surfnet that died.
pub const INTERNAL_RPC_TIMEOUT: Duration = Duration::from_secs(600);
/// How many times the grader makes one of its reads through a client it
/// can't give a longer wait (Phoenix's Hawkeye simulations), before failing.
pub const READ_ATTEMPTS: u32 = 5;

/// The grader's own read, made again if it fails for want of an answer (a
/// timeout, a lost connection: see [`is_transport`]), up to
/// [`READ_ATTEMPTS`] times. On a starved machine a read can time out on the
/// client's side, and reading again changes nothing, since nothing moves
/// while the grader reads. Any other error is the answer, and comes back at
/// once.
pub async fn patiently<T, E: std::error::Error + 'static, F>(
    what: &str,
    mut read: impl FnMut() -> F,
) -> std::result::Result<T, E>
where
    F: std::future::Future<Output = std::result::Result<T, E>>,
{
    let mut attempt = 1;
    loop {
        match read().await {
            Ok(value) => return Ok(value),
            Err(error) if attempt < READ_ATTEMPTS && is_transport(&error) => {
                assert!(attempt < READ_ATTEMPTS, "the reads are bounded");
                tracing::warn!(what, attempt, %error, "a grader read got no answer; reading again");
                attempt += 1;
            }
            Err(error) => return Err(error),
        }
    }
}

/// Whether `error` is a request that got no answer (it timed out, or its
/// connection failed) rather than an answer. Errors that keep their source
/// say so through reqwest's; some clients flatten theirs into text.
pub fn is_transport(error: &(dyn std::error::Error + 'static)) -> bool {
    const NO_ANSWER: [&str; 5] = [
        "timed out",
        "error sending request",
        "connection refused",
        "connection reset",
        "connection closed",
    ];
    let mut next = Some(error);
    for _ in 0..ERROR_SOURCES_MAX {
        let Some(error) = next else {
            return false;
        };
        if let Some(http) = error.downcast_ref::<reqwest::Error>()
            && (http.is_timeout() || http.is_connect() || http.is_request())
        {
            return true;
        }
        let text = error.to_string().to_lowercase();
        if NO_ANSWER.iter().any(|sign| text.contains(sign)) {
            return true;
        }
        next = error.source();
    }
    // A chain this deep is no transport's: reqwest's is three or four.
    false
}

/// The most of an error's sources [`is_transport`] looks through: a
/// transport error is a few links down (reqwest, hyper, the OS); a chain
/// longer than this is something else, and looking ends.
const ERROR_SOURCES_MAX: usize = 32;

/// A meter's amount as an `f64`: the nearest double to the exact
/// decimal, which is what dividing Phoenix's integer lots in `f64` gave.
pub fn decimal_f64(amount: Decimal) -> f64 {
    amount
        .to_string()
        .parse()
        .expect("a decimal's digits parse as f64")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, thiserror::Error)]
    #[error("{0}")]
    struct Failed(String);

    #[tokio::test]
    async fn only_unanswered_reads_are_made_again() {
        let mut calls = 0;
        let timed_out = patiently("x", || {
            calls += 1;
            std::future::ready(Err::<(), _>(Failed(
                "error sending request: operation timed out".into(),
            )))
        })
        .await;
        assert!(timed_out.is_err());
        assert_eq!(calls, READ_ATTEMPTS);

        let mut calls = 0;
        let answered = patiently("x", || {
            calls += 1;
            std::future::ready(Err::<(), _>(Failed("no market SPCX".into())))
        })
        .await;
        assert!(answered.is_err());
        assert_eq!(calls, 1);

        let mut calls = 0;
        let second = patiently("x", || {
            calls += 1;
            std::future::ready(if calls == 1 {
                Err(Failed("Connection refused (os error 111)".into()))
            } else {
                Ok(7)
            })
        })
        .await;
        assert_eq!(second.unwrap(), 7);
    }
}
