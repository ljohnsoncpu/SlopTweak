//! Does a LoRA file fit MOSS-Image / Kromoss?
//!
//! ComfyUI loads a LoRA whose tensor names it can't place on the model with only a log
//! line, and then draws as if no LoRA were set. An SDXL file added to the MOSS panel would
//! look like it worked and do nothing. So the app reads the file's header (names and
//! shapes, no weights) before it spends time uploading it, and says so.
//!
//! "Fits" means ComfyUI's own key map for the model (`comfy/lora.py`, `model_lora_keys_unet`
//! plus the MOSS block for fused qkv, checked at the pinned commit a639f187) would place
//! the tensor. That gives two styles, both seen in real files:
//! * kohya: `lora_unet_layers_0_proj_ffn_down.lora_down.weight` (the Kohaku trainer's
//!   export, fused `proj_qkv`), and
//! * generic: `diffusion_model.layers.0.proj.k.lora_down.weight` (lodestones' DMD2 LoRA,
//!   separate q/k/v).
//!
//! The modules are the model's Linear layers, listed from the real Kromoss header
//! (`real_base_modules_are_all_covered` re-checks that against the file).

use std::collections::BTreeMap;

use serde_json::Value;

/// Which tensor-name style a LoRA uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyStyle {
    /// `lora_unet_<module with underscores>`.
    Kohya,
    /// `diffusion_model.<module path>`.
    Generic,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Nearly all of it (at least 90 % of its modules) lands on the model.
    Fits,
    /// Some of it does; the rest would be ignored.
    Partial,
    /// None of it does: a LoRA for another model, or another naming scheme.
    Foreign { example: String },
    /// No LoRA weights in the file at all.
    Empty,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    pub verdict: Verdict,
    /// Modules the file addresses, and how many of them the model has.
    pub modules: usize,
    pub matched: usize,
    /// The style of the modules that matched.
    pub style: Option<KeyStyle>,
    /// The first `lora_down`'s inner size, when the file says.
    pub rank: Option<u32>,
}

/// Per-module tensors a LoRA, LoHa, LoKr or DoRA export carries (ComfyUI's `load_lora`).
const SUFFIXES: &[&str] = &[
    ".alpha",
    ".lora_up.weight",
    ".lora_down.weight",
    ".lora_A.weight",
    ".lora_B.weight",
    ".lora_mid.weight",
    ".dora_scale",
    ".diff",
    ".diff_b",
    ".hada_w1_a",
    ".hada_w1_b",
    ".hada_w2_a",
    ".hada_w2_b",
    ".hada_t1",
    ".hada_t2",
    ".lokr_w1",
    ".lokr_w2",
    ".lokr_w1_a",
    ".lokr_w1_b",
    ".lokr_w2_a",
    ".lokr_w2_b",
    ".lokr_t2",
];

/// The model's Linear modules, with `.` written as `_` and `#` standing for a number.
/// `q`, `k` and `v` are the halves of the fused `qkv`, which ComfyUI splits for LoRAs.
const MODULES: &[&str] = &[
    "layers_#_proj_qkv",
    "layers_#_proj_q",
    "layers_#_proj_k",
    "layers_#_proj_v",
    "layers_#_proj_o",
    "layers_#_proj_ffn_down",
    "layers_#_proj_ffn_up_gate",
    "final_linear_video",
    "shared_block_modulation",
    "video_embedder",
    "video_head_modulation",
    "text_embedder_proj",
    "text_embedder_blocks_#_proj",
    "text_embedder_blocks_#_qkv",
    "text_embedder_blocks_#_ffn_down",
    "text_embedder_blocks_#_ffn_gate",
    "text_embedder_blocks_#_ffn_up",
    "time_embedding_#",
];

/// `#` in the pattern stands for one or more digits; everything else is literal.
fn pattern_matches(pat: &str, s: &str) -> bool {
    let mut parts = pat.split('#');
    let first = parts.next().unwrap_or("");
    let Some(mut rest) = s.strip_prefix(first) else {
        return false;
    };
    for lit in parts {
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            return false;
        }
        let Some(r) = rest[digits..].strip_prefix(lit) else {
            return false;
        };
        rest = r;
    }
    rest.is_empty()
}

fn module_exists(underscored: &str) -> bool {
    MODULES.iter().any(|p| pattern_matches(p, underscored))
}

/// `(style, module with underscores)` for a module name, or None for a name no
/// supported style produces.
fn style_of(module: &str) -> Option<(KeyStyle, String)> {
    if let Some(rest) = module.strip_prefix("lora_unet_") {
        return Some((KeyStyle::Kohya, rest.to_string()));
    }
    module
        .strip_prefix("diffusion_model.")
        .map(|rest| (KeyStyle::Generic, rest.replace('.', "_")))
}

fn split_suffix(name: &str) -> Option<&str> {
    SUFFIXES.iter().find_map(|s| name.strip_suffix(s))
}

/// A tensor name as something short and printable for a message.
fn example(name: &str) -> String {
    name.chars()
        .filter(|c| !c.is_control())
        .take(48)
        .collect::<String>()
}

/// The header's JSON object: `u64` little-endian length, then that many bytes.
fn header(bytes: &[u8]) -> Result<serde_json::Map<String, Value>, String> {
    let bad = || "That doesn't look like a LoRA file (.safetensors).".to_string();
    let len: [u8; 8] = bytes
        .get(..8)
        .ok_or_else(bad)?
        .try_into()
        .map_err(|_| bad())?;
    let n = usize::try_from(u64::from_le_bytes(len)).map_err(|_| bad())?;
    let raw = bytes
        .get(8..8usize.checked_add(n).ok_or_else(bad)?)
        .ok_or_else(bad)?;
    match serde_json::from_slice::<Value>(raw) {
        Ok(Value::Object(m)) => Ok(m),
        _ => Err(bad()),
    }
}

/// Read a LoRA's header and say whether the model has the modules it addresses.
pub fn probe(bytes: &[u8]) -> Result<Probe, String> {
    let h = header(bytes)?;
    // module (as written in the file) -> does the model have it
    let mut modules: BTreeMap<&str, Option<KeyStyle>> = BTreeMap::new();
    let mut first_foreign: Option<&str> = None;
    let mut rank = None;
    for (name, info) in &h {
        if name == "__metadata__" {
            continue;
        }
        let Some(module) = split_suffix(name) else {
            continue;
        };
        let style = style_of(module)
            .filter(|(_, path)| module_exists(path))
            .map(|(s, _)| s);
        modules.entry(module).or_insert(style);
        if style.is_none() && first_foreign.is_none() {
            first_foreign = Some(name);
        }
        if rank.is_none()
            && (name.ends_with(".lora_down.weight") || name.ends_with(".lora_A.weight"))
        {
            rank = info["shape"]
                .get(0)
                .and_then(Value::as_u64)
                .and_then(|r| u32::try_from(r).ok());
        }
    }
    let total = modules.len();
    let matched = modules.values().filter(|s| s.is_some()).count();
    let style = modules.values().flatten().copied().next();
    let verdict = if total == 0 {
        Verdict::Empty
    } else if matched == 0 {
        Verdict::Foreign {
            example: example(first_foreign.unwrap_or("")),
        }
    } else if matched * 10 >= total * 9 {
        Verdict::Fits
    } else {
        Verdict::Partial
    };
    Ok(Probe {
        verdict,
        modules: total,
        matched,
        style,
        rank,
    })
}

impl Probe {
    /// Why the file must not be added, in plain words.
    pub fn refusal(&self) -> Option<String> {
        match &self.verdict {
            Verdict::Fits | Verdict::Partial => None,
            Verdict::Empty => Some("That file has no LoRA weights in it.".into()),
            Verdict::Foreign { example } => Some(format!(
                "That LoRA is for a different model (its parts are named like \"{example}\"), so \
                 it would do nothing here. Use a LoRA made for MOSS-Image or Kromoss."
            )),
        }
    }

    /// What to tell the user about a file that is added but only partly fits.
    pub fn note(&self) -> Option<String> {
        (self.verdict == Verdict::Partial).then(|| {
            format!(
                "Added, but only {} of the {} parts of that LoRA fit this model; the rest are ignored.",
                self.matched, self.modules
            )
        })
    }
}

/// A small, valid-looking kohya-style LoRA (three modules, rank 4) for tests that need a
/// file the probe accepts. Only the header is real; the data is zeros.
#[cfg(test)]
pub fn sample_lora_bytes() -> Vec<u8> {
    let mut off = 0u64;
    let mut h = serde_json::Map::new();
    h.insert(
        "__metadata__".into(),
        serde_json::json!({"ss_network_dim": "4"}),
    );
    for m in [
        "lora_unet_layers_0_proj_o",
        "lora_unet_layers_0_proj_qkv",
        "lora_unet_layers_1_proj_o",
    ] {
        for (suffix, shape) in [
            (".lora_down.weight", vec![4, 8]),
            (".lora_up.weight", vec![8, 4]),
            (".alpha", vec![]),
        ] {
            let n: u64 = 4 * shape.iter().product::<u64>().max(1);
            h.insert(
                format!("{m}{suffix}"),
                serde_json::json!({"dtype": "F32", "shape": shape, "data_offsets": [off, off + n]}),
            );
            off += n;
        }
    }
    let head = serde_json::to_vec(&h).unwrap();
    let mut v = (head.len() as u64).to_le_bytes().to_vec();
    v.extend_from_slice(&head);
    v.extend(std::iter::repeat_n(0u8, off as usize));
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A file with just these tensor names (shape `[4, 8]`, no data).
    fn file(names: &[&str]) -> Vec<u8> {
        let mut h = serde_json::Map::new();
        h.insert("__metadata__".into(), json!({}));
        for n in names {
            h.insert(
                (*n).into(),
                json!({"dtype": "F32", "shape": [4, 8], "data_offsets": [0, 0]}),
            );
        }
        let head = serde_json::to_vec(&h).unwrap();
        let mut v = (head.len() as u64).to_le_bytes().to_vec();
        v.extend_from_slice(&head);
        v
    }

    /// `n` modules in the generic style, `n_bad` more in a foreign one.
    fn mixed(n: usize, n_bad: usize) -> Vec<u8> {
        let mut names: Vec<String> = (0..n)
            .map(|i| format!("diffusion_model.layers.{i}.proj.o.lora_down.weight"))
            .collect();
        names
            .extend((0..n_bad).map(|i| {
                format!("lora_te_text_model_encoder_layers_{i}_mlp_fc1.lora_down.weight")
            }));
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        file(&refs)
    }

    #[test]
    fn a_kohaku_export_fits_with_its_rank() {
        let p = probe(&sample_lora_bytes()).unwrap();
        assert_eq!(p.verdict, Verdict::Fits);
        assert_eq!((p.modules, p.matched), (3, 3));
        assert_eq!(p.style, Some(KeyStyle::Kohya));
        assert_eq!(p.rank, Some(4));
        assert!(p.refusal().is_none() && p.note().is_none());
    }

    #[test]
    fn the_dmd2_style_fits() {
        let p = probe(&file(&[
            "diffusion_model.layers.0.proj.k.lora_down.weight",
            "diffusion_model.layers.0.proj.k.lora_up.weight",
            "diffusion_model.layers.0.proj.k.alpha",
            "diffusion_model.layers.41.proj.ffn_up_gate.lora_down.weight",
            "diffusion_model.layers.41.proj.ffn_down.lora_up.weight",
        ]))
        .unwrap();
        assert_eq!(p.verdict, Verdict::Fits);
        assert_eq!((p.modules, p.style), (3, Some(KeyStyle::Generic)));
    }

    #[test]
    fn the_modules_other_than_the_layers_projections_count_too() {
        let p = probe(&file(&[
            "lora_unet_final_linear_video.lora_down.weight",
            "lora_unet_text_embedder_blocks_1_ffn_gate.lora_up.weight",
            "lora_unet_time_embedding_0.alpha",
            "diffusion_model.shared_block_modulation.lora_A.weight",
        ]))
        .unwrap();
        assert_eq!((p.verdict, p.matched), (Verdict::Fits, 4));
    }

    #[test]
    fn an_sdxl_lora_is_foreign_and_refused_with_an_example_name() {
        let p = probe(&file(&[
            "lora_unet_input_blocks_1_1_transformer_blocks_0_attn1_to_q.lora_down.weight",
            "lora_unet_input_blocks_1_1_transformer_blocks_0_attn1_to_q.lora_up.weight",
            "lora_te1_text_model_encoder_layers_0_mlp_fc1.lora_down.weight",
        ]))
        .unwrap();
        let Verdict::Foreign { example } = &p.verdict else {
            panic!("{p:?}")
        };
        assert!(
            example.starts_with("lora_te1_") || example.starts_with("lora_unet_input_blocks"),
            "{example}"
        );
        assert!(example.chars().count() <= 48);
        let why = p.refusal().unwrap();
        assert!(
            why.contains("different model") && why.contains(example.as_str()),
            "{why}"
        );
    }

    #[test]
    fn other_naming_schemes_are_not_guessed_at() {
        // Diffusers/PEFT and LyCORIS-prefixed names are not in ComfyUI's key map for MOSS.
        for name in [
            "transformer.layers.0.proj.o.lora_A.weight",
            "base_model.model.layers.0.proj.o.lora_A.weight",
            "lycoris_layers_0_proj_o.lora_down.weight",
            "layers.0.proj.o.lora_down.weight",
        ] {
            let p = probe(&file(&[name])).unwrap();
            assert!(
                matches!(p.verdict, Verdict::Foreign { .. }),
                "{name}: {p:?}"
            );
        }
    }

    #[test]
    fn a_file_with_no_lora_tensors_is_empty() {
        for names in [&[][..], &["layers.0.weight", "some_other_tensor"][..]] {
            let p = probe(&file(names)).unwrap();
            assert_eq!(p.verdict, Verdict::Empty);
            assert!(p.refusal().unwrap().contains("no LoRA weights"));
        }
    }

    #[test]
    fn ninety_percent_is_the_line_between_fits_and_partial() {
        assert_eq!(probe(&mixed(9, 1)).unwrap().verdict, Verdict::Fits);
        let p = probe(&mixed(8, 2)).unwrap();
        assert_eq!(
            (p.verdict.clone(), p.matched, p.modules),
            (Verdict::Partial, 8, 10)
        );
        assert!(p.refusal().is_none());
        let note = p.note().unwrap();
        assert!(note.contains("8 of the 10"), "{note}");
        // Partial down to a single module is still allowed (and noted); none is foreign.
        assert_eq!(probe(&mixed(1, 9)).unwrap().verdict, Verdict::Partial);
        assert!(matches!(
            probe(&mixed(0, 3)).unwrap().verdict,
            Verdict::Foreign { .. }
        ));
    }

    #[test]
    fn several_tensors_of_one_module_count_once() {
        let p = probe(&file(&[
            "lora_unet_layers_3_proj_o.lora_down.weight",
            "lora_unet_layers_3_proj_o.lora_up.weight",
            "lora_unet_layers_3_proj_o.alpha",
        ]))
        .unwrap();
        assert_eq!((p.modules, p.matched), (1, 1));
    }

    #[test]
    fn unreadable_files_are_errors_not_verdicts() {
        let mut array = 2u64.to_le_bytes().to_vec();
        array.extend_from_slice(b"[]");
        let mut short = 100u64.to_le_bytes().to_vec();
        short.extend_from_slice(b"{}");
        for bad in [
            &b""[..],
            b"GGUF....",
            &u64::MAX.to_le_bytes(),
            &array,
            &short,
        ] {
            assert!(probe(bad).is_err());
        }
    }

    #[test]
    fn module_patterns_match_whole_names_with_numbers() {
        assert!(pattern_matches("layers_#_proj_qkv", "layers_12_proj_qkv"));
        assert!(!pattern_matches("layers_#_proj_q", "layers_12_proj_qkv"));
        assert!(!pattern_matches("layers_#_proj_o", "layers__proj_o"));
        assert!(!pattern_matches("layers_#_proj_o", "layers_1_proj_o_extra"));
        assert!(pattern_matches(
            "text_embedder_blocks_#_ffn_up",
            "text_embedder_blocks_0_ffn_up"
        ));
        assert!(!pattern_matches(
            "text_embedder_blocks_#_ffn_up",
            "text_embedder_blocks_0_ffn_up_gate"
        ));
    }

    fn read_header_of(path: &str) -> Option<Vec<u8>> {
        use std::io::Read;
        let mut f = std::fs::File::open(path).ok()?;
        let mut len = [0u8; 8];
        f.read_exact(&mut len).ok()?;
        let n = u64::from_le_bytes(len) as usize;
        let mut buf = len.to_vec();
        buf.resize(8 + n, 0);
        f.read_exact(&mut buf[8..]).ok()?;
        Some(buf)
    }

    /// The tensor names of a Kohaku export: `<module>.{alpha,lora_down.weight,lora_up.weight}`
    /// for every module in `modules`, on each of the model's 42 layers. This is exactly the name
    /// set of the real `km-mix-800` (504 names, fused qkv) and `km-smoke-lycoris` (756, split
    /// q/k/v); `real_local_loras_fit` checks the real files against it.
    fn kohaku_names(modules: &[&str]) -> Vec<String> {
        let mut v = Vec::new();
        for layer in 0..42 {
            for m in modules {
                for suffix in [".alpha", ".lora_down.weight", ".lora_up.weight"] {
                    v.push(format!("lora_unet_layers_{layer}_{m}{suffix}"));
                }
            }
        }
        v
    }

    #[test]
    fn the_name_sets_of_the_real_kohaku_exports_fit() {
        let fused = kohaku_names(&["proj_o", "proj_qkv", "proj_ffn_down", "proj_ffn_up_gate"]);
        let split = kohaku_names(&[
            "proj_q",
            "proj_k",
            "proj_v",
            "proj_o",
            "proj_ffn_down",
            "proj_ffn_up_gate",
        ]);
        assert_eq!((fused.len(), split.len()), (504, 756));
        for (names, modules) in [(&fused, 168), (&split, 252)] {
            let refs: Vec<&str> = names.iter().map(String::as_str).collect();
            let p = probe(&file(&refs)).unwrap();
            assert_eq!(
                (p.verdict, p.modules, p.matched, p.style),
                (Verdict::Fits, modules, modules, Some(KeyStyle::Kohya))
            );
        }
    }

    /// The real files from this PC (headers only): every Kohaku export in the ComfyUI
    /// loras folder, the `-lycoris` export, and lodestones' DMD2. They all load in ComfyUI,
    /// so any other verdict means the probe is wrong.
    #[test]
    #[ignore = "needs the LoRAs in C:/ai/ComfyUI/models/loras and D:/ai/KohakuMossTrainer"]
    fn real_local_loras_fit() {
        let dir = "C:/ai/ComfyUI/models/loras";
        let mut files: Vec<(String, KeyStyle, u32)> = std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("km-") && n.ends_with(".safetensors"))
            .map(|n| (format!("{dir}/{n}"), KeyStyle::Kohya, 32))
            .collect();
        files.push((
            "D:/ai/KohakuMossTrainer/runs/moss/km-smoke/export/km-smoke-lycoris.safetensors".into(),
            KeyStyle::Kohya,
            32,
        ));
        files.push((
            format!("{dir}/8-steps-dmd2-lora.safetensors"),
            KeyStyle::Generic,
            128,
        ));
        assert!(files.len() >= 8, "{files:?}");
        let fused: std::collections::BTreeSet<String> =
            kohaku_names(&["proj_o", "proj_qkv", "proj_ffn_down", "proj_ffn_up_gate"])
                .into_iter()
                .collect();
        for (path, style, rank) in files {
            let bytes = read_header_of(&path).unwrap_or_else(|| panic!("{path}"));
            let p = probe(&bytes).unwrap();
            assert_eq!(p.verdict, Verdict::Fits, "{path}: {p:?}");
            assert_eq!((p.style, p.rank), (Some(style), Some(rank)), "{path}");
            if path.contains("km-mix-800") {
                // The fixture above is the real file's name set, not a guess.
                let real: std::collections::BTreeSet<String> = header(&bytes)
                    .unwrap()
                    .keys()
                    .filter(|k| *k != "__metadata__")
                    .cloned()
                    .collect();
                assert_eq!(real, fused);
            }
        }
    }

    /// Every 2-D weight in the real Kromoss file is a module the probe knows, so a LoRA
    /// on any real layer is not wrongly called foreign.
    #[test]
    #[ignore = "needs C:/ai/ComfyUI/models/diffusion_models/kromoss_bf16.safetensors"]
    fn real_base_modules_are_all_covered() {
        let bytes =
            read_header_of("C:/ai/ComfyUI/models/diffusion_models/kromoss_bf16.safetensors")
                .unwrap();
        let h = header(&bytes).unwrap();
        let mut linear = 0;
        for (name, info) in &h {
            let shape = info["shape"].as_array();
            let two_d = shape.is_some_and(|s| s.len() == 2);
            if let (true, Some(module)) = (two_d, name.strip_suffix(".weight")) {
                linear += 1;
                assert!(module_exists(&module.replace('.', "_")), "{name}");
            }
        }
        assert!(linear > 150, "{linear}");
    }
}
