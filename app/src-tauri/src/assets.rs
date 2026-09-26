//! Pre-rent check of the instance asset bundle. An instance that can't fetch
//! it never starts its sidecar and destroys itself ~10 minutes later, and
//! every retry fails the same way, so check once from the PC before renting.

use std::time::Duration;

use sha2::{Digest, Sha256};

/// The real bundle is ~40 KB.
const MAX_BYTES: usize = 16 * 1024 * 1024;

const NOTHING_RENTED: &str = "No GPU was rented.";

/// Download `url` the way `onstart.sh` does and check it against `sha256`.
pub async fn check(url: &str, sha256: &str) -> Result<(), String> {
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .user_agent(concat!("SlopTweak/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| e.to_string())?;
    let unreachable = |e: reqwest::Error| {
        format!(
            "Couldn't check SlopTweak's GPU setup files on GitHub ({}). {NOTHING_RENTED}",
            e.without_url()
        )
    };
    let resp = http.get(url).send().await.map_err(unreachable)?;
    let status = resp.status();
    if !status.is_success() {
        return Err(http_error(status.as_u16()));
    }
    if resp
        .content_length()
        .is_some_and(|n| n as usize > MAX_BYTES)
    {
        return Err(mismatch());
    }
    let bytes = resp.bytes().await.map_err(unreachable)?;
    verify(&bytes, sha256)
}

fn http_error(code: u16) -> String {
    let hint = if code == 404 {
        " This version's GitHub release may not be published yet."
    } else {
        ""
    };
    format!(
        "SlopTweak's GPU setup files can't be downloaded (HTTP {code}), so a GPU \
         couldn't finish starting.{hint} {NOTHING_RENTED}"
    )
}

fn mismatch() -> String {
    format!(
        "SlopTweak's GPU setup files on GitHub don't match this version, so a GPU \
         would refuse them. {NOTHING_RENTED}"
    )
}

fn verify(bytes: &[u8], sha256: &str) -> Result<(), String> {
    if bytes.len() > MAX_BYTES || !hex::encode(Sha256::digest(bytes)).eq_ignore_ascii_case(sha256) {
        return Err(mismatch());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_checks_the_hash() {
        // sha256("abc")
        let abc = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        assert!(verify(b"abc", abc).is_ok());
        assert!(verify(b"abc", &abc.to_uppercase()).is_ok());
        let e = verify(b"abd", abc).unwrap_err();
        assert!(
            e.contains("don't match") && e.contains(NOTHING_RENTED),
            "{e}"
        );
    }

    #[test]
    fn http_errors_say_nothing_was_rented() {
        let e = http_error(404);
        assert!(
            e.contains("HTTP 404") && e.contains("not be published"),
            "{e}"
        );
        assert!(e.ends_with(NOTHING_RENTED), "{e}");
        let e = http_error(500);
        assert!(!e.contains("published"), "{e}");
    }
}
