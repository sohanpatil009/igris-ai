// Offline voice smoke test.
//
// Run with:
//   cargo run --example voice_smoke
//
// It loads the installed Parakeet/Silero/Kokoro pack and writes a sample
// utterance to pkg/audio/kokoro_smoke.wav. It does not open the microphone.

use std::path::PathBuf;

fn main() {
    if let Err(error) = run() {
        eprintln!("voice_smoke failed: {}", error);
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    println!("pkg: {}", igris_ai::core::paths::pkg_dir().display());
    println!(
        "vad backend: {}",
        if igris_ai::core::audio_capture::silero_vad_available() {
            "silero-onnx"
        } else {
            "energy-fallback"
        }
    );
    println!(
        "model pack: {:?}",
        igris_ai::setup_manager::model_pack_status()
    );

    let stt = igris_ai::core::stt::init_stt_engine().map_err(|e| e.to_string())?;
    println!("stt backend: {}", stt.backend_name());

    let wave_path = igris_ai::core::paths::pkg_dir().join("models/parakeet/test.wav");
    if wave_path.exists() {
        let wave = sherpa_onnx::Wave::read(&wave_path.to_string_lossy())
            .ok_or_else(|| "failed to read test wav".to_string())?;
        let text = stt.transcribe(wave.samples()).map_err(|e| e.to_string())?;
        println!("stt sample: {}", text);
    } else {
        println!("stt sample: skipped (no test wav)");
    }

    igris_ai::core::tts::init_tts().map_err(|e| e.to_string())?;
    println!("tts backend: {}", igris_ai::core::tts::backend_name());

    let output = PathBuf::from("pkg/audio/kokoro_smoke.wav");
    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    igris_ai::core::tts::synthesize_to_file("IGRIS V5 is online and ready.", &output)
        .map_err(|e| e.to_string())?;
    let size = std::fs::metadata(&output).map_err(|e| e.to_string())?.len();
    println!("wrote: {} ({} bytes)", output.display(), size);

    Ok(())
}
