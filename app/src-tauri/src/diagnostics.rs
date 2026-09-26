//! "Copy diagnostics": a plain-text report a user can paste into a bug
//! report. Everything goes through [`redact`] as one block at the end, so
//! nothing added here can skip it: known secrets (Vast key, CivitAI key,
//! launch secrets) are masked by value, anything key-shaped by pattern, and
//! the user's home folder becomes `%USERPROFILE%` (it holds their Windows
//! user name).

use std::fmt::Write as _;

use crate::config::Settings;
use crate::persist::ActiveRecord;
use crate::redact::redact;
use crate::session::SessionState;
use crate::sync::SyncReport;

pub struct Inputs<'a> {
    pub app_version: &'a str,
    pub mode: &'a str,
    pub os: String,
    pub webview: Option<String>,
    pub generated_unix: u64,
    pub state: &'a SessionState,
    pub history: &'a [String],
    pub log: &'a [String],
    pub settings: &'a Settings,
    pub catalog: String,
    pub sync: Option<&'a SyncReport>,
    pub has_vast_key: bool,
    pub has_civitai_key: bool,
    pub record: Option<&'a ActiveRecord>,
    pub credit: Option<f64>,
    pub update: Option<String>,
    pub image: &'a str,
    pub assets_url: &'a str,
    pub assets_sha256: &'a str,
}

/// Short enough that the token pattern doesn't mask it; long enough to tell
/// pins apart.
fn short_hash(h: &str) -> String {
    let h = h.rsplit(':').next().unwrap_or(h);
    format!("{}…", &h[..h.len().min(12)])
}

fn image_without_digest(image: &str) -> &str {
    image.split('@').next().unwrap_or(image)
}

fn yes_no(b: bool) -> &'static str {
    if b {
        "stored"
    } else {
        "missing"
    }
}

/// The report, redacted. `known` are secret values to mask exactly; `home`
/// is the user's profile folder.
pub fn report(i: &Inputs, known: &[&str], home: Option<&str>) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "SlopTweak diagnostics");
    let _ = writeln!(s, "app {} ({} mode)", i.app_version, i.mode);
    let _ = writeln!(s, "os {}", i.os);
    let _ = writeln!(s, "webview2 {}", i.webview.as_deref().unwrap_or("unknown"));
    let _ = writeln!(s, "generated {} (unix)", i.generated_unix);
    let _ = writeln!(
        s,
        "image {} (digest {})",
        image_without_digest(i.image),
        short_hash(i.image)
    );
    let _ = writeln!(
        s,
        "instance assets {} (sha256 {})",
        i.assets_url,
        short_hash(i.assets_sha256)
    );
    if let Some(u) = &i.update {
        let _ = writeln!(s, "update {u}");
    }

    let _ = writeln!(s, "\n== keys ==");
    let _ = writeln!(s, "vast key {}", yes_no(i.has_vast_key));
    let _ = writeln!(s, "civitai key {}", yes_no(i.has_civitai_key));
    match i.credit {
        Some(c) => {
            let _ = writeln!(s, "vast credit ${c:.2}");
        }
        None => {
            let _ = writeln!(s, "vast credit unknown");
        }
    }

    let _ = writeln!(s, "\n== session ==");
    let _ = writeln!(
        s,
        "state {}",
        serde_json::to_string(i.state).unwrap_or_else(|e| format!("<{e}>"))
    );
    match i.record {
        Some(r) => {
            let _ = writeln!(
                s,
                "active record: instance {} offer {} {} ${:.4}/hr model {} created {} ({})",
                r.instance_id,
                r.offer_id,
                r.gpu_name,
                r.hourly,
                r.model_id,
                r.created_unix,
                r.provider
            );
        }
        None => {
            let _ = writeln!(s, "active record: none");
        }
    }
    if let Some(r) = i.sync {
        let _ = writeln!(
            s,
            "output sync {:?}: {} saved, {} canvas, {} missing, last error {}",
            r.phase,
            r.saved,
            r.canvas_saved,
            r.missing,
            r.last_error.as_deref().unwrap_or("none")
        );
    }

    let st = i.settings;
    let _ = writeln!(s, "\n== settings ==");
    let _ = writeln!(
        s,
        "model {} · max ${:.2}/hr · idle {} min · heartbeat {} min · max session {} min · min credit ${:.2}",
        st.model_id.as_deref().unwrap_or("none"),
        st.max_dph,
        st.idle_minutes,
        st.heartbeat_minutes,
        st.max_session_minutes,
        st.min_credit
    );
    let _ = writeln!(
        s,
        "offer floors: reliability {} · inet {} Mbps · disk {} MB/s · compute {} · ready timeout {} min · attempts {}",
        st.min_reliability,
        st.min_inet_down_mbps,
        st.min_disk_bw_mbps,
        st.min_compute_cap,
        st.ready_timeout_minutes,
        st.max_attempts
    );
    let _ = writeln!(
        s,
        "output folder {} · tutorial done {}",
        st.output_dir.as_deref().unwrap_or("default"),
        st.tutorial_done
    );
    for l in &st.loras {
        let _ = writeln!(
            s,
            "lora {} / {} (version {}, {}, {})",
            l.name,
            l.version_name,
            l.version_id,
            l.base_model,
            if l.enabled { "on" } else { "off" }
        );
    }
    let _ = writeln!(s, "catalog {}", i.catalog);

    let _ = writeln!(s, "\n== state history ==");
    for line in i.history {
        let _ = writeln!(s, "{line}");
    }
    let _ = writeln!(s, "\n== log ==");
    for line in i.log {
        let _ = writeln!(s, "{line}");
    }

    let s = match home {
        Some(h) => mask_home(&s, h),
        None => s,
    };
    redact(&s, known)
}

/// Replace the profile folder (any case, either slash) with `%USERPROFILE%`.
fn mask_home(text: &str, home: &str) -> String {
    let home = home.trim_end_matches(['\\', '/']);
    if home.chars().count() < 4 {
        return text.to_string();
    }
    let norm = |c: char| if c == '/' { '\\' } else { c };
    let same = |a: char, b: char| norm(a) == norm(b) || a.to_lowercase().eq(b.to_lowercase());
    let needle: Vec<char> = home.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    // Char by char, so case folding never shifts byte offsets.
    while let Some(first) = rest.chars().next() {
        let mut end = 0;
        let mut chars = rest.char_indices();
        let hit = needle.iter().all(|&n| match chars.next() {
            Some((i, c)) if same(c, n) => {
                end = i + c.len_utf8();
                true
            }
            _ => false,
        });
        if hit {
            out.push_str("%USERPROFILE%");
            rest = &rest[end..];
        } else {
            out.push(first);
            rest = &rest[first.len_utf8()..];
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::OfferSummary;

    const VAST: &str = "0f1e2d3c4b5a69788796a5b4c3d2e1f00f1e2d3c4b5a69788796a5b4c3d2e1f0";
    const CIVITAI: &str = "a1b2c3d4e5f60718293a4b5c6d7e8f90";
    const LAUNCH: &str = "q3Jx_9bK-2mPzL8vR4tY7wN1cF6hD0sA5eG3uI9oX2k";
    /// A secret that doesn't look like one (letters only, so no pattern
    /// catches it): only the known-value layer can mask it.
    const ODD: &str = "correcthorsebatterystaple";

    fn ready() -> SessionState {
        SessionState::Ready {
            instance_id: 52630531,
            offer: OfferSummary {
                offer_id: 49992718,
                gpu_name: "RTX 3060".into(),
                hourly: 0.1012,
                download_cost: 0.02,
                location: Some("Utah, US".into()),
            },
            ready_unix: 1_790_000_000,
            deadlines: None,
        }
    }

    fn build(log: &[String], settings: &Settings, known: &[&str]) -> String {
        let state = ready();
        let history = vec![
            "1790000000 Idle -> Renting attempt 1/3".to_string(),
            format!("1790000100 Provisioning -> Ready: instance 52630531 RTX 3060 t={LAUNCH}"),
        ];
        let rec = ActiveRecord {
            instance_id: 52630531,
            offer_id: 49992718,
            gpu_name: "RTX 3060".into(),
            hourly: 0.1012,
            model_id: "banana-splitz-xxl".into(),
            created_unix: 1_790_000_000,
            provider: "vast".into(),
        };
        report(
            &Inputs {
                app_version: "0.2.0",
                mode: "vast",
                os: "windows x86_64".into(),
                webview: Some("140.0.3485.54".into()),
                generated_unix: 1_790_000_200,
                state: &state,
                history: &history,
                log,
                settings,
                catalog: "online, 3 models".into(),
                sync: None,
                has_vast_key: true,
                has_civitai_key: true,
                record: Some(&rec),
                credit: Some(11.2),
                update: None,
                image: crate::config::IMAGE,
                assets_url: crate::config::ASSETS_URL,
                assets_sha256: crate::config::ASSETS_SHA256,
            },
            known,
            Some(r"C:\Users\Jane Doe"),
        )
    }

    fn noisy_log() -> Vec<String> {
        vec![
            format!("1790000001 create: Authorization: Bearer {VAST}"),
            format!("1790000002 civitai token={CIVITAI}&x=1"),
            format!("1790000003 opening https://thunder-west.trycloudflare.com/__auth?t={LAUNCH}"),
            format!("1790000004 heartbeat \"Bearer {LAUNCH}\""),
            format!("1790000005 odd secret {ODD} here"),
            r"1790000006 saved C:\Users\Jane Doe\Pictures\SlopTweak\a.png".to_string(),
            "1790000007 saved c:/users/jane doe/Pictures/b.png".to_string(),
        ]
    }

    #[test]
    fn no_secret_survives() {
        let mut settings = Settings {
            output_dir: Some(r"C:\Users\Jane Doe\Pictures\Out".into()),
            ..Default::default()
        };
        settings.model_id = Some("banana-splitz-xxl".into());
        let text = build(&noisy_log(), &settings, &[VAST, CIVITAI, LAUNCH, ODD]);
        for secret in [VAST, CIVITAI, LAUNCH, ODD, "Jane Doe", "jane doe"] {
            assert!(!text.contains(secret), "{secret} leaked:\n{text}");
        }
        assert!(text.contains(r"%USERPROFILE%\Pictures\SlopTweak\a.png"));
        assert!(text.contains("%USERPROFILE%/Pictures/b.png"));
        assert!(text.contains(r"output folder %USERPROFILE%\Pictures\Out"));
    }

    #[test]
    fn patterns_catch_secrets_the_app_did_not_know_about() {
        // Even with no known values, key-shaped strings are masked.
        let text = build(&noisy_log(), &Settings::default(), &[]);
        for secret in [VAST, CIVITAI, LAUNCH] {
            assert!(!text.contains(secret), "{secret} leaked:\n{text}");
        }
    }

    #[test]
    fn useful_facts_remain() {
        let text = build(&noisy_log(), &Settings::default(), &[VAST]);
        for want in [
            "app 0.2.0 (vast mode)",
            "webview2 140.0.3485.54",
            "vast key stored",
            "civitai key stored",
            "vast credit $11.20",
            "instance 52630531",
            "offer 49992718",
            "RTX 3060",
            "thunder-west.trycloudflare.com",
            "Idle -> Renting attempt 1/3",
            "== state history ==",
            "== log ==",
            "ghcr.io/invoke-ai/invokeai:v6.14.1-cuda (digest 39a7e3b182c4…)",
            "sha256 ea4bd0bc22da…",
            "Bearer [REDACTED]",
        ] {
            assert!(text.contains(want), "missing {want:?}:\n{text}");
        }
    }

    #[test]
    fn home_masking_edge_cases() {
        assert_eq!(mask_home("C:\\x", "C:\\"), "C:\\x");
        assert_eq!(
            mask_home(r"C:\USERS\ANA\x", r"C:\Users\Ana\"),
            r"%USERPROFILE%\x"
        );
        // Non-ASCII names, and text whose lowercase changes length.
        assert_eq!(
            mask_home(r"İ C:\Users\JOSÉ\a c:/users/josé/b", r"C:\Users\José"),
            r"İ %USERPROFILE%\a %USERPROFILE%/b"
        );
    }
}
