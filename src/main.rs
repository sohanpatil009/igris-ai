use moonshine_rs::audio::load_audio_for_transcription;
use moonshine_rs::{
    ModelArch, SttDependenciesOptions, Transcriber, get_stt_dependencies_with_options,
};
use serde_json::Value;
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::time::Instant;

mod assist;
mod hotkey;
mod llm;
mod live;
mod tts;

const MODELS: &[(&str, ModelArch, &str)] = &[
    (
        "small-streaming",
        ModelArch::SmallStreaming,
        "models/small-streaming",
    ),
    (
        "medium-streaming",
        ModelArch::MediumStreaming,
        "models/medium-streaming",
    ),
];

fn manifest_name(file: &Value) -> Result<&str, Box<dyn std::error::Error>> {
    let name = file["name"].as_str().ok_or("file missing name")?;
    let mut components = Path::new(name).components();
    if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
        return Err(format!("unsafe model manifest filename: {name}").into());
    }
    Ok(name)
}

pub(crate) fn ensure_model(dir: &str, arch: ModelArch) -> Result<(), Box<dyn std::error::Error>> {
    /* Only operate on directories we know belong to this tool; reject paths
     * that could reach outside a dedicated model tree. */
    let normalized = dir.trim();
    if normalized.is_empty() {
        return Err("model directory is empty".into());
    }
    let out = PathBuf::from(normalized);
    let root = PathBuf::from("models");
    fs::create_dir_all(&root)?;
    /* Require exactly one normal child below the dedicated model root. This
     * rejects absolute paths, `..`, nested symlink paths, and `--model .`. */
    let rel = out.strip_prefix(&root).map_err(|_| {
        format!(
            "model directory must be under {} (got {})",
            root.display(),
            out.display()
        )
    })?;
    if rel.components().count() != 1
        || !matches!(rel.components().next(), Some(Component::Normal(_)))
    {
        return Err("model directory must be one direct child of models".into());
    }
    if let Ok(metadata) = fs::symlink_metadata(&out) {
        if !metadata.is_dir() {
            return Err("model path exists but is not a directory".into());
        }
        let existing = out.canonicalize()?;
        let root_canonical = root.canonicalize()?;
        if !existing.starts_with(&root_canonical) {
            return Err("model directory resolves outside models".into());
        }
    }
    fs::create_dir_all(&out)?;
    let root_canonical = root.canonicalize()?;
    let out_canonical = out.canonicalize()?;
    if !out_canonical.starts_with(&root_canonical) {
        return Err("model directory resolves outside models".into());
    }

    let manifest_json =
        get_stt_dependencies_with_options("en", &SttDependenciesOptions::new().with_arch(arch))?;
    let manifest: Value = serde_json::from_str(&manifest_json)?;
    let groups = manifest["groups"]
        .as_array()
        .ok_or("manifest missing groups")?;

    /* If any on-disk file is missing or has the wrong size (e.g. files from
     * an older model revision), rebuild the manifest-listed files so the
     * required file set always matches what this SDK build expects. */
    let mut stale = false;
    for group in groups {
        let files = group["files"].as_array().ok_or("group missing files")?;
        for file in files {
            let name = manifest_name(file)?;
            let expected = file["size"].as_u64().unwrap_or(0);
            let ok = if let Ok(meta) = fs::metadata(out.join(name)) {
                expected == 0 || meta.len() == expected
            } else {
                false
            };
            if !ok {
                stale = true;
            }
        }
    }
    if stale {
        println!(
            "  revision mismatch detected; rebuilding {} from scratch",
            out.display()
        );
        /* Delete only files named in the manifest, not the whole directory.
         * This avoids wiping unrelated files if the manifest or path is
         * misconfigured. */
        let mut to_remove = std::collections::HashSet::new();
        for group in groups {
            for file in group["files"].as_array().ok_or("group missing files")? {
                let name = manifest_name(file)?;
                to_remove.insert(name.to_string());
            }
        }
        let mut cleaned = 0usize;
        for entry in fs::read_dir(&out)? {
            let entry = entry?;
            let p = entry.path();
            if p.is_file()
                && let Some(name) = p.file_name().and_then(|n| n.to_str())
                && to_remove.contains(name)
            {
                fs::remove_file(&p)?;
                cleaned += 1;
            }
        }
        println!("  removed {} stale files", cleaned);
    }

    for group in groups {
        let files = group["files"].as_array().ok_or("group missing files")?;
        for file in files {
            let name = manifest_name(file)?;
            let url = file["url"].as_str().ok_or("file missing url")?;
            let expected = file["size"].as_u64().unwrap_or(0);
            let dest = out.join(name);
            if dest.exists() {
                let have = fs::metadata(&dest)?.len();
                if expected == 0 || have == expected {
                    continue;
                }
            }
            println!(
                "    fetch {} ({:.1} MB) <- {}",
                name,
                expected as f64 / 1e6,
                url
            );
            let resp = ureq::get(url).call()?;
            let mut bytes = Vec::new();
            resp.into_body().into_reader().read_to_end(&mut bytes)?;
            fs::write(&dest, &bytes)?;
            println!("          wrote {} bytes", bytes.len());
        }
    }
    Ok(())
}

fn wer(reference: &str, hypothesis: &str) -> f64 {
    let norm = |s: &str| -> Vec<String> {
        s.split_whitespace()
            .map(|w| {
                w.trim_matches(|c: char| !c.is_alphanumeric())
                    .to_lowercase()
            })
            .filter(|w| !w.is_empty())
            .collect()
    };
    let a = norm(reference);
    let b = norm(hypothesis);
    if a.is_empty() {
        return if b.is_empty() { 0.0 } else { 1.0 };
    }
    let mut dp_prev: Vec<usize> = (0..=b.len()).collect();
    let mut dp_cur = vec![0usize; b.len() + 1];
    for wa in &a {
        dp_cur[0] = dp_prev[0] + 1;
        for (j, wb) in b.iter().enumerate() {
            let cost = if wa == wb { 0 } else { 1 };
            dp_cur[j + 1] = (dp_prev[j + 1] + 1)
                .min(dp_cur[j] + 1)
                .min(dp_prev[j] + cost);
        }
        std::mem::swap(&mut dp_prev, &mut dp_cur);
    }
    dp_prev[b.len()] as f64 / a.len() as f64
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: {} <AUDIO_FILE> [REFERENCE_TXT]", args[0]);
        eprintln!("       {} live [WAV] [--model DIR]", args[0]);
        eprintln!("       {} hotkey [--model DIR]", args[0]);
        eprintln!("       {} chat [PROMPT]", args[0]);
        eprintln!("       {} assist [WAV] [--model DIR] [--hotkey] [--no-tts] [--voice ID] [--cpu]", args[0]);
        eprintln!("       {} speak \"TEXT\" [--voice ID]", args[0]);
        eprintln!();
        eprintln!("  live             live microphone transcription");
        eprintln!("  hotkey           hold CTRL+SPACE to record, release to transcribe");
        eprintln!("  assist           voice assistant: live STT feeds partial text into");
        eprintln!("                   the LLM while you speak; the reply streams live");
        eprintln!("  assist --hotkey  push-to-talk assistant: the mic opens only while");
        eprintln!("                   CTRL+SPACE is held anywhere; on release the");
        eprintln!("                   answer streams (press again to barge-in)");
        eprintln!("  chat             prompt the local LLM (LFM3-1.7B via llama.cpp);");
        eprintln!("                   without PROMPT it runs an interactive loop");
        eprintln!("  speak            say TEXT out loud through piper TTS (models/piper);");
        eprintln!("                   the voice in models/tts downloads on first run;");
        eprintln!("                   --voice en_US-amy-low picks the smaller/faster voice");
        eprintln!("  live WAV         stream a WAV file in real time (test mode)");
        eprintln!("  AUDIO_FILE       wav/mp3/flac/ogg/m4a to transcribe");
        eprintln!("  REFERENCE_TXT    optional ground-truth text file for WER");
        eprintln!();
        eprintln!("Models (small-streaming, medium-streaming) are downloaded on first run.");
        std::process::exit(2);
    }
    /* `live` subcommand: real-time transcription */
    if args[1] == "live" {
        return live::cmd_live(&args[2..]);
    }
    /* `hotkey` subcommand: Ctrl+Space push-to-talk */
    if args[1] == "hotkey" {
        return hotkey::cmd_hotkey(&args[2..]);
    }
    /* `chat` subcommand: prompt the local LLM (llama.cpp, no Ollama) */
    if args[1] == "chat" {
        return llm::cmd_chat(&args[2..]);
    }
    /* `assist` subcommand: live STT -> LLM voice assistant */
    if args[1] == "assist" {
        return assist::cmd_assist(&args[2..]);
    }
    /* `speak` subcommand: synthesize + play a phrase through piper TTS */
    if args[1] == "speak" {
        return tts::cmd_speak(&args[2..]);
    }
    let audio_path = PathBuf::from(&args[1]);
    let reference = args
        .get(2)
        .map(|p| fs::read_to_string(p).unwrap_or_default());

    println!("Audio: {}", audio_path.display());
    let t0 = Instant::now();
    let pcm = load_audio_for_transcription(&audio_path)?;
    let load_audio_ms = t0.elapsed().as_millis();
    let audio_seconds = pcm.len() as f64 / 16_000.0;
    println!(
        "Decoded {} samples ({:.2}s audio) in {} ms",
        pcm.len(),
        audio_seconds,
        load_audio_ms
    );

    let mut results: Vec<(String, String, u128, f64)> = Vec::new();

    for (label, arch, dir) in MODELS {
        println!("\n=== {} ===", label);
        ensure_model(dir, *arch)?;
        let t_load = Instant::now();
        let transcriber = Transcriber::from_files(dir, *arch, None)?;
        let load_ms = t_load.elapsed().as_millis();
        println!("  model load: {} ms", load_ms);

        let t_inf = Instant::now();
        let mut stream = transcriber.create_stream()?;
        for chunk in pcm.chunks(1600) {
            stream.add_audio(chunk, 16_000)?;
        }
        let transcript = stream.finalize()?;
        let inf_ms = t_inf.elapsed().as_millis();
        let text = transcript.text();
        let rtf = if inf_ms == 0 {
            f64::INFINITY
        } else {
            audio_seconds / (inf_ms as f64 / 1000.0)
        };
        println!("  inference: {} ms (RTF {:.2}x)", inf_ms, rtf);
        println!("  text: {}", text);
        if let Some(ref r) = reference {
            let e = wer(r, &text);
            println!(
                "  WER: {:.2}% (ref {} words, hyp {} words)",
                e * 100.0,
                r.split_whitespace().count(),
                text.split_whitespace().count()
            );
        }
        results.push((label.to_string(), text, inf_ms, rtf));
    }

    println!("\n=== SUMMARY ===");
    println!("{:<20} {:>10} {:>8}", "model", "inf_ms", "RTF");
    for (label, _text, inf_ms, rtf) in &results {
        println!("{:<20} {:>10} {:>8.2}", label, inf_ms, rtf);
    }
    if let Some(ref r) = reference {
        println!("\nreference words: {}", r.split_whitespace().count());
        for (label, text, _, _) in &results {
            println!("{} WER: {:.2}%", label, wer(r, text) * 100.0);
        }
    }

    Ok(())
}
