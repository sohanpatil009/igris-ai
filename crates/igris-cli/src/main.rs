use igris_core::models::ensure_model;
use igris_core::paths;
use igris_core::tools;
use igris_core::{assist::cmd_assist, hotkey::cmd_hotkey, llm::cmd_chat, live::cmd_live, tts::cmd_speak};
use moonshine_rs::audio::load_audio_for_transcription;
use moonshine_rs::{ModelArch, Transcriber};
use std::fs;
use std::path::PathBuf;
use std::time::Instant;

fn models() -> Vec<(&'static str, ModelArch, String)> {
    vec![
        (
            "small-streaming",
            ModelArch::SmallStreaming,
            paths::asset("stt/small-streaming").to_string_lossy().into_owned(),
        ),
        (
            "medium-streaming",
            ModelArch::MediumStreaming,
            paths::asset("stt/medium-streaming").to_string_lossy().into_owned(),
        ),
    ]
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
    /* Config-defined tools must be staged before the first registry use
     * (the registry is built lazily at first prompt/dispatch); a broken
     * tools.toml is reported and ignored, never fatal. */
    tools::load_config_tools();
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: {} <AUDIO_FILE> [REFERENCE_TXT]", args[0]);
        eprintln!("       {} live [WAV] [--model DIR]", args[0]);
        eprintln!("       {} hotkey [--model DIR]", args[0]);
        eprintln!("       {} chat [PROMPT]", args[0]);
        eprintln!("       {} assist [WAV] [--model DIR] [--hotkey] [--no-tts] [--voice ID] [--cpu] [--no-tools]", args[0]);
        eprintln!("       {} speak \"TEXT\" [--voice ID]", args[0]);
        eprintln!();
        eprintln!("  live             live microphone transcription");
        eprintln!("  hotkey           hold CTRL+SPACE to record, release to transcribe");
        eprintln!("  assist           voice assistant: live STT feeds partial text into");
        eprintln!("                   the LLM while you speak; the reply streams live");
        eprintln!("  assist --hotkey  push-to-talk assistant: the mic opens only while");
        eprintln!("                   CTRL+SPACE is held anywhere; on release the");
        eprintln!("                   answer streams (press again to barge-in)");
        eprintln!("  chat             prompt the local LLM via llama.cpp;");
        eprintln!("                   without PROMPT it runs an interactive loop");
        eprintln!("                   MOON_LLM_MODEL=lfm3|qwen|qwen-moe picks the GGUF");
        eprintln!("  speak            say TEXT out loud through piper TTS (assets/piper);");
        eprintln!("                   the voice in assets/tts downloads on first run;");
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
        return cmd_live(&args[2..]);
    }
    /* `hotkey` subcommand: Ctrl+Space push-to-talk */
    if args[1] == "hotkey" {
        return cmd_hotkey(&args[2..]);
    }
    /* `chat` subcommand: prompt the local LLM (llama.cpp, no Ollama) */
    if args[1] == "chat" {
        return cmd_chat(&args[2..]);
    }
    /* `assist` subcommand: live STT -> LLM voice assistant */
    if args[1] == "assist" {
        return cmd_assist(&args[2..]);
    }
    /* `speak` subcommand: synthesize + play a phrase through piper TTS */
    if args[1] == "speak" {
        return cmd_speak(&args[2..]);
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

    for (label, arch, dir) in models() {
        println!("\n=== {} ===", label);
        let model_dir = ensure_model(&dir, arch)?;
        let t_load = Instant::now();
        let transcriber = Transcriber::from_files(&model_dir, arch, None)?;
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
