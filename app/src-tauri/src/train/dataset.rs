//! Dataset audit: pair images with captions, hash everything, and predict the
//! aspect-ratio bucket each image trains at, so problems show up before any
//! GPU is rented.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use serde::Serialize;
use sha2::{Digest, Sha256};

use super::settings::{BUCKET_MAX, BUCKET_MIN, BUCKET_STEP};

const IMAGE_EXTS: [&str; 2] = ["png", "jpg"];
/// Refuse absurd inputs before reading them into memory.
const MAX_IMAGE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_IMAGES: usize = 500;
/// Rough tag count past which captions risk truncation (CLIP window is 75
/// tokens per chunk, 150 with `max_token_length`).
const LONG_CAPTION_TAGS: usize = 60;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ImageInfo {
    pub id: String,
    pub file: String,
    pub sha256: String,
    pub caption_sha256: String,
    pub width: u32,
    pub height: u32,
    pub bucket: (u32, u32),
    pub tags: usize,
    pub first_tag: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// Training would fail or be wasted.
    Error,
    /// Probably a mistake; the user decides.
    Warning,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Issue {
    pub severity: Severity,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Report {
    pub images: Vec<ImageInfo>,
    pub issues: Vec<Issue>,
    /// "832x1216" -> how many images land there.
    pub buckets: BTreeMap<String, usize>,
}

impl Report {
    pub fn has_errors(&self) -> bool {
        self.issues.iter().any(|i| i.severity == Severity::Error)
    }
}

fn hex_sha256(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// Pixel size from the header of a PNG or baseline/progressive JPEG.
pub fn image_size(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() >= 24 && bytes.starts_with(b"\x89PNG\r\n\x1a\n") && &bytes[12..16] == b"IHDR" {
        let w = u32::from_be_bytes(bytes[16..20].try_into().ok()?);
        let h = u32::from_be_bytes(bytes[20..24].try_into().ok()?);
        return Some((w, h));
    }
    if bytes.starts_with(&[0xFF, 0xD8]) {
        let mut i = 2;
        while i + 9 < bytes.len() {
            if bytes[i] != 0xFF {
                i += 1;
                continue;
            }
            let marker = bytes[i + 1];
            if marker == 0xFF || marker == 0x01 || (0xD0..=0xD8).contains(&marker) {
                i += if marker == 0xFF { 1 } else { 2 };
                continue;
            }
            let len = usize::from(u16::from_be_bytes([bytes[i + 2], bytes[i + 3]]));
            let is_sof = (0xC0..=0xCF).contains(&marker) && !matches!(marker, 0xC4 | 0xC8 | 0xCC);
            if is_sof {
                let h = u32::from(u16::from_be_bytes([bytes[i + 5], bytes[i + 6]]));
                let w = u32::from(u16::from_be_bytes([bytes[i + 7], bytes[i + 8]]));
                return Some((w, h));
            }
            i += 2 + len;
        }
    }
    None
}

/// The bucket resolutions kohya builds for a target area: for each width, the
/// tallest height that keeps the area within `res`², plus the transpose.
fn bucket_resolutions(res: u32) -> Vec<(u32, u32)> {
    let max_area = res * res;
    let mut out = Vec::new();
    let mut w = BUCKET_MIN;
    while w <= BUCKET_MAX {
        let h = ((max_area / w) / BUCKET_STEP * BUCKET_STEP).min(BUCKET_MAX);
        if h >= BUCKET_MIN {
            out.push((w, h));
            out.push((h, w));
        }
        w += BUCKET_STEP;
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// The bucket kohya picks for an image: nearest aspect ratio (log scale).
pub fn bucket_for(width: u32, height: u32, res: u32) -> (u32, u32) {
    let aspect = (f64::from(width) / f64::from(height)).ln();
    bucket_resolutions(res)
        .into_iter()
        .min_by(|a, b| {
            let da = (aspect - (f64::from(a.0) / f64::from(a.1)).ln()).abs();
            let db = (aspect - (f64::from(b.0) / f64::from(b.1)).ln()).abs();
            da.total_cmp(&db)
        })
        .expect("bucket list is never empty")
}

fn tags(caption: &str) -> Vec<&str> {
    caption
        .split(',')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .collect()
}

/// Audit a folder of `name.png|jpg` + `name.txt` pairs (non-recursive).
pub fn audit(dir: &Path, res: u32) -> Result<Report, String> {
    let entries = std::fs::read_dir(dir).map_err(|e| format!("Can't read that folder: {e}"))?;
    let mut images: HashMap<String, String> = HashMap::new();
    let mut captions: HashMap<String, String> = HashMap::new();
    let mut issues = Vec::new();
    let mut warn = |severity, message: String| issues.push(Issue { severity, message });
    for entry in entries.flatten() {
        let path = entry.path();
        let (Some(stem), Some(ext)) = (
            path.file_stem().and_then(|s| s.to_str()),
            path.extension().and_then(|s| s.to_str()),
        ) else {
            continue;
        };
        let ext = ext.to_ascii_lowercase();
        // `.e621.txt` and `.nl.txt` siblings are alternate captions, not ours.
        if ext == "txt" && !stem.contains('.') {
            captions.insert(stem.to_owned(), entry.file_name().to_string_lossy().into());
        } else if IMAGE_EXTS.contains(&ext.as_str()) && !stem.contains('.') {
            images.insert(stem.to_owned(), entry.file_name().to_string_lossy().into());
        } else if matches!(ext.as_str(), "webp" | "jpeg" | "bmp" | "gif") {
            warn(
                Severity::Warning,
                format!(
                    "{} is skipped: use PNG or JPG.",
                    entry.file_name().to_string_lossy()
                ),
            );
        }
    }
    if images.len() > MAX_IMAGES {
        return Err(format!("That folder has more than {MAX_IMAGES} images."));
    }
    for stem in captions.keys().filter(|s| !images.contains_key(*s)) {
        warn(
            Severity::Warning,
            format!("Caption {stem}.txt has no image."),
        );
    }

    let mut stems: Vec<&String> = images.keys().collect();
    stems.sort();
    let mut out = Vec::new();
    let mut seen_hash: HashMap<String, String> = HashMap::new();
    for stem in stems {
        let file = &images[stem];
        let path = dir.join(file);
        let meta = std::fs::metadata(&path).map_err(|e| format!("{file}: {e}"))?;
        if meta.len() > MAX_IMAGE_BYTES {
            warn(Severity::Error, format!("{file} is larger than 64 MB."));
            continue;
        }
        let bytes = std::fs::read(&path).map_err(|e| format!("{file}: {e}"))?;
        let Some((w, h)) = image_size(&bytes) else {
            warn(
                Severity::Error,
                format!("{file} isn't a readable PNG or JPG."),
            );
            continue;
        };
        let Some(cap_file) = captions.get(stem) else {
            warn(
                Severity::Error,
                format!("{file} has no caption ({stem}.txt)."),
            );
            continue;
        };
        let cap_bytes =
            std::fs::read(dir.join(cap_file)).map_err(|e| format!("{cap_file}: {e}"))?;
        let Ok(caption) = String::from_utf8(cap_bytes.clone()) else {
            warn(Severity::Error, format!("{cap_file} isn't UTF-8 text."));
            continue;
        };
        let t = tags(&caption);
        if t.is_empty() {
            warn(Severity::Error, format!("{cap_file} is empty."));
            continue;
        }
        if w.min(h) < 512 {
            warn(
                Severity::Warning,
                format!("{file} is {w}x{h}; the short side is under 512 and will be upscaled."),
            );
        }
        if t.len() > LONG_CAPTION_TAGS {
            warn(
                Severity::Warning,
                format!("{cap_file} has {} tags and may be truncated.", t.len()),
            );
        }
        let sha = hex_sha256(&bytes);
        if let Some(first) = seen_hash.insert(sha.clone(), file.clone()) {
            warn(
                Severity::Warning,
                format!("{file} is identical to {first}."),
            );
        }
        out.push(ImageInfo {
            id: stem.clone(),
            file: file.clone(),
            sha256: sha,
            caption_sha256: hex_sha256(&cap_bytes),
            width: w,
            height: h,
            bucket: bucket_for(w, h, res),
            tags: t.len(),
            first_tag: t[0].to_owned(),
        });
    }

    if out.is_empty() {
        warn(
            Severity::Error,
            "No usable image + caption pairs found.".into(),
        );
    } else if out.len() < 8 {
        warn(
            Severity::Warning,
            format!(
                "Only {} usable images; results are usually poor below ~15.",
                out.len()
            ),
        );
    }
    let mut first_tags: HashMap<&str, usize> = HashMap::new();
    for i in &out {
        *first_tags.entry(i.first_tag.as_str()).or_default() += 1;
    }
    if first_tags.len() > 1 {
        warn(
            Severity::Warning,
            "Captions don't all start with the same tag. The first tag is the trigger word \
             and is kept fixed during training."
                .into(),
        );
    }
    let mut buckets = BTreeMap::new();
    for i in &out {
        *buckets
            .entry(format!("{}x{}", i.bucket.0, i.bucket.1))
            .or_default() += 1;
    }
    Ok(Report {
        images: out,
        issues,
        buckets,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(w: u32, h: u32, salt: u8) -> Vec<u8> {
        let mut b = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        b.extend(w.to_be_bytes());
        b.extend(h.to_be_bytes());
        b.extend([8, 2, 0, 0, 0, salt]);
        b
    }

    fn jpeg(w: u16, h: u16) -> Vec<u8> {
        let mut b = vec![0xFF, 0xD8, 0xFF, 0xE0, 0, 4, 0, 0];
        b.extend([0xFF, 0xC0, 0, 11, 8]);
        b.extend(h.to_be_bytes());
        b.extend(w.to_be_bytes());
        b.extend([3, 1, 0x22, 0, 0]);
        b
    }

    fn put(dir: &Path, name: &str, bytes: &[u8]) {
        std::fs::write(dir.join(name), bytes).unwrap();
    }

    #[test]
    fn reads_png_and_jpeg_sizes() {
        assert_eq!(image_size(&png(1033, 1522, 0)), Some((1033, 1522)));
        assert_eq!(image_size(&jpeg(800, 1200)), Some((800, 1200)));
        assert_eq!(image_size(b"GIF89a"), None);
        assert_eq!(image_size(&[]), None);
        assert_eq!(image_size(&[0xFF, 0xD8, 0xFF]), None);
    }

    #[test]
    fn buckets_match_the_validated_run() {
        // Every Thalia image (1033x1522) trained at 832x1216.
        assert_eq!(bucket_for(1033, 1522, 1024), (832, 1216));
        assert_eq!(bucket_for(1024, 1024, 1024), (1024, 1024));
        assert_eq!(bucket_for(1536, 1024, 1024), (1248, 832));
        let (w, h) = bucket_for(100, 4000, 1024);
        assert!(w >= BUCKET_MIN && h <= BUCKET_MAX);
    }

    #[test]
    fn a_clean_dataset_has_no_issues() {
        let d = tempfile::tempdir().unwrap();
        for i in 0..10u8 {
            put(d.path(), &format!("a{i}.png"), &png(1033, 1522, i));
            put(d.path(), &format!("a{i}.txt"), b"trig, solo, front_view");
            // Alternate captions next to the active one are ignored.
            put(d.path(), &format!("a{i}.e621.txt"), b"other, tags");
        }
        let r = audit(d.path(), 1024).unwrap();
        assert_eq!(r.images.len(), 10);
        assert!(r.issues.is_empty(), "{:?}", r.issues);
        assert_eq!(r.buckets["832x1216"], 10);
        assert_eq!(r.images[0].tags, 3);
    }

    #[test]
    fn flags_missing_and_bad_files() {
        let d = tempfile::tempdir().unwrap();
        for i in 0..8u8 {
            put(d.path(), &format!("ok{i}.png"), &png(1024, 1024, i));
            put(d.path(), &format!("ok{i}.txt"), b"trig, a");
        }
        put(d.path(), "nocap.png", &png(1024, 1024, 100));
        put(d.path(), "orphan.txt", b"trig, a");
        put(d.path(), "empty.png", &png(1024, 1024, 101));
        put(d.path(), "empty.txt", b" , ");
        put(d.path(), "junk.png", b"not an image");
        put(d.path(), "junk.txt", b"trig");
        put(d.path(), "small.png", &png(400, 600, 102));
        put(d.path(), "small.txt", b"trig, a");
        put(d.path(), "dupe.png", &png(1024, 1024, 0));
        put(d.path(), "dupe.txt", b"trig, a");
        put(d.path(), "pic.webp", b"x");
        put(d.path(), "drift.png", &png(1024, 1024, 103));
        put(d.path(), "drift.txt", b"other, a");
        let r = audit(d.path(), 1024).unwrap();
        let msgs: Vec<&str> = r.issues.iter().map(|i| i.message.as_str()).collect();
        let has = |needle: &str| msgs.iter().any(|m| m.contains(needle));
        assert!(has("nocap.png has no caption"));
        assert!(has("orphan.txt has no image"));
        assert!(has("empty.txt is empty"));
        assert!(has("junk.png isn't a readable"));
        assert!(has("small.png is 400x600"));
        assert!(has(".png is identical to"));
        assert!(has("pic.webp is skipped"));
        assert!(has("don't all start with the same tag"));
        assert!(r.has_errors());
        // Unusable files are excluded from the images that would train.
        assert!(r
            .images
            .iter()
            .all(|i| !matches!(i.id.as_str(), "nocap" | "empty" | "junk")));
    }

    #[test]
    fn empty_and_missing_folders() {
        let d = tempfile::tempdir().unwrap();
        let r = audit(d.path(), 1024).unwrap();
        assert!(r.has_errors());
        assert!(audit(&d.path().join("nope"), 1024).is_err());
    }

    #[test]
    fn warns_on_tiny_datasets() {
        let d = tempfile::tempdir().unwrap();
        for i in 0..3u8 {
            put(d.path(), &format!("a{i}.png"), &png(1024, 1024, i));
            put(d.path(), &format!("a{i}.txt"), b"trig, a");
        }
        let r = audit(d.path(), 1024).unwrap();
        assert!(!r.has_errors());
        assert!(r.issues.iter().any(|i| i.message.contains("Only 3 usable")));
    }

    /// Acceptance check against the real Thalia dataset (23 images, all
    /// 1033x1522 -> 832x1216). Point `SLOPTWEAK_TEST_DATASET` at its folder.
    #[test]
    #[ignore = "needs the Thalia dataset on disk"]
    fn thalia_dataset_audits_as_in_the_validated_run() {
        let dir = std::env::var("SLOPTWEAK_TEST_DATASET").expect("SLOPTWEAK_TEST_DATASET");
        let r = audit(Path::new(&dir), 1024).unwrap();
        assert!(!r.has_errors(), "{:?}", r.issues);
        assert_eq!(r.buckets.len(), 1);
        assert_eq!(r.buckets["832x1216"], r.images.len());
        println!("{} images, issues: {:?}", r.images.len(), r.issues);
    }
}
