// src/core/stt.rs
// Offline speech-to-text.
//
// Primary engine: NVIDIA Parakeet-TDT 0.6B v3 (int8) through sherpa-onnx.
// Fallback engine: SenseVoice (multilingual), kept for installs that already
// have the older model pack.

use crate::core::paths;
use once_cell::sync::Lazy;
use sherpa_onnx::{
    OfflineMoonshineModelConfig, OfflineRecognizer, OfflineRecognizerConfig,
    OfflineSenseVoiceModelConfig, OfflineTransducerModelConfig,
};
use std::path::Path;
use std::sync::Mutex;

const SAMPLE_RATE: i32 = 16000;

/// Lazily initialized engine shared by the desktop loop. Reloading Parakeet on
/// every utterance costs hundreds of milliseconds, so it is initialized once.
static SHARED_STT: Lazy<Mutex<Option<SttEngine>>> = Lazy::new(|| Mutex::new(None));

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SttBackend {
    Moonshine,
    Parakeet,
    SenseVoice,
}

impl SttBackend {
    pub fn as_str(self) -> &'static str {
        match self {
            SttBackend::Moonshine => "moonshine-base-en-int8",
            SttBackend::Parakeet => "parakeet-tdt-0.6b-v3",
            SttBackend::SenseVoice => "sensevoice",
        }
    }
}

pub struct SttEngine {
    recognizer: OfflineRecognizer,
    backend: SttBackend,
}

impl SttEngine {
    pub fn backend(&self) -> SttBackend {
        self.backend
    }

    pub fn backend_name(&self) -> &'static str {
        self.backend.as_str()
    }

    pub fn transcribe(&self, audio_samples: &[f32]) -> Result<String, Box<dyn std::error::Error>> {
        let stream = self.recognizer.create_stream();
        stream.accept_waveform(SAMPLE_RATE, audio_samples);
        self.recognizer.decode(&stream);
        let text = stream.get_result().map(|r| r.text).unwrap_or_default();
        Ok(text.trim().to_string())
    }
}

/// Resolve the Moonshine v1 model files.
fn moonshine_files() -> Option<(
    std::path::PathBuf,
    std::path::PathBuf,
    std::path::PathBuf,
    std::path::PathBuf,
)> {
    let preprocess = paths::model_file("moonshine", &["preprocess.onnx"])?;
    let encoder = paths::model_file("moonshine", &["encode.int8.onnx", "encode.onnx"])?;
    let uncached = paths::model_file(
        "moonshine",
        &["uncached_decode.int8.onnx", "uncached_decode.onnx"],
    )?;
    let cached = paths::model_file(
        "moonshine",
        &["cached_decode.int8.onnx", "cached_decode.onnx"],
    )?;
    Some((preprocess, encoder, uncached, cached))
}

fn init_moonshine() -> Result<SttEngine, Box<dyn std::error::Error>> {
    let (preprocess, encoder, uncached, cached) = moonshine_files()
        .ok_or("Moonshine model not found (models/moonshine/{preprocess,encode,uncached_decode,cached_decode}.onnx)")?;
    let tokens = paths::model_file("moonshine", &["tokens.txt"]);

    println!(
        "[STT] Loading Moonshine base: {}",
        encoder.parent().unwrap_or(Path::new(".")).display()
    );

    let mut config = OfflineRecognizerConfig::default();
    config.model_config.moonshine = OfflineMoonshineModelConfig {
        preprocessor: Some(preprocess.to_string_lossy().into_owned()),
        encoder: Some(encoder.to_string_lossy().into_owned()),
        uncached_decoder: Some(uncached.to_string_lossy().into_owned()),
        cached_decoder: Some(cached.to_string_lossy().into_owned()),
        merged_decoder: None,
    };
    if let Some(tokens_path) = tokens {
        config.model_config.tokens = Some(tokens_path.to_string_lossy().into_owned());
    }
    config.model_config.num_threads = 2;
    config.model_config.provider = Some("cpu".into());
    config.decoding_method = Some("greedy_search".into());

    let recognizer = OfflineRecognizer::create(&config)
        .ok_or("Failed to create sherpa-onnx Moonshine recognizer")?;

    println!("[STT] Moonshine ready");
    Ok(SttEngine {
        recognizer,
        backend: SttBackend::Moonshine,
    })
}

/// Resolve the Parakeet model files.
///
/// sherpa archives keep the engine in a versioned directory; accept both the
/// flattened `models/parakeet` layout and the original archive folder.
fn parakeet_files() -> Option<(
    std::path::PathBuf,
    std::path::PathBuf,
    std::path::PathBuf,
    std::path::PathBuf,
)> {
    let encoder = paths::model_file("parakeet", &["encoder.int8.onnx", "encoder.onnx"])?;
    let decoder = paths::model_file("parakeet", &["decoder.int8.onnx", "decoder.onnx"])?;
    let joiner = paths::model_file("parakeet", &["joiner.int8.onnx", "joiner.onnx"])?;
    let tokens = paths::model_file("parakeet", &["tokens.txt"])?;
    Some((encoder, decoder, joiner, tokens))
}

fn init_parakeet() -> Result<SttEngine, Box<dyn std::error::Error>> {
    let (encoder, decoder, joiner, tokens) = parakeet_files()
        .ok_or("Parakeet model not found (models/parakeet/{encoder,decoder,joiner}.int8.onnx + tokens.txt)")?;

    println!(
        "[STT] Loading Parakeet-TDT 0.6B v3: {}",
        encoder.parent().unwrap_or(Path::new(".")).display()
    );

    let mut config = OfflineRecognizerConfig::default();
    config.model_config.transducer = OfflineTransducerModelConfig {
        encoder: Some(encoder.to_string_lossy().into_owned()),
        decoder: Some(decoder.to_string_lossy().into_owned()),
        joiner: Some(joiner.to_string_lossy().into_owned()),
    };
    config.model_config.tokens = Some(tokens.to_string_lossy().into_owned());
    config.model_config.model_type = Some("nemo_transducer".into());
    config.model_config.num_threads = 2;
    // CPU is the safe default on hybrid laptops (MX350 is slower than a modern
    // CPU for small int8 encoders and competing for it hurts UI compositing).
    config.model_config.provider = Some("cpu".into());
    config.decoding_method = Some("greedy_search".into());

    let recognizer = OfflineRecognizer::create(&config)
        .ok_or("Failed to create sherpa-onnx Parakeet recognizer")?;

    println!("[STT] Parakeet ready");
    Ok(SttEngine {
        recognizer,
        backend: SttBackend::Parakeet,
    })
}

fn init_sense_voice() -> Result<SttEngine, Box<dyn std::error::Error>> {
    let model_path = paths::find_file("models/sense-voice/model.onnx");
    let tokens_path = paths::find_file("models/sense-voice/tokens.txt");

    let (model_path, tokens_path) = match (model_path, tokens_path) {
        (Some(model), Some(tokens)) => (model, tokens),
        _ => {
            return Err(
                "No local STT model found. Expected models/parakeet/ (preferred) or models/sense-voice/."
                    .into(),
            )
        }
    };

    println!(
        "[STT] Falling back to SenseVoice model: {}",
        model_path.display()
    );

    let mut config = OfflineRecognizerConfig::default();
    config.model_config.sense_voice = OfflineSenseVoiceModelConfig {
        model: Some(model_path.to_string_lossy().into_owned()),
        language: Some("auto".into()),
        use_itn: true,
    };
    config.model_config.tokens = Some(tokens_path.to_string_lossy().into_owned());
    config.model_config.num_threads = 2;
    config.model_config.provider = Some("cpu".into());

    let recognizer = OfflineRecognizer::create(&config)
        .ok_or("Failed to create sherpa-onnx offline recognizer")?;

    println!("[STT] SenseVoice ready");
    Ok(SttEngine {
        recognizer,
        backend: SttBackend::SenseVoice,
    })
}

/// Initialize the best available offline STT engine.
pub fn init_stt_engine() -> Result<SttEngine, Box<dyn std::error::Error>> {
    // Preferred: Moonshine (small footprint, CPU-friendly, good for MX350 laptops).
    match init_moonshine() {
        Ok(engine) => Ok(engine),
        Err(primary_error) => {
            eprintln!("[STT] Moonshine unavailable: {}", primary_error);
            // Legacy fallbacks kept for older installs.
            match init_parakeet() {
                Ok(engine) => Ok(engine),
                Err(parakeet_error) => {
                    eprintln!("[STT] Parakeet unavailable: {}", parakeet_error);
                    init_sense_voice()
                }
            }
        }
    }
}

/// Initialize a specific backend, falling back to auto-detection.
pub fn init_stt_engine_prefer(
    backend: Option<SttBackend>,
) -> Result<SttEngine, Box<dyn std::error::Error>> {
    match backend {
        Some(SttBackend::Moonshine) => init_moonshine(),
        Some(SttBackend::Parakeet) => init_parakeet(),
        Some(SttBackend::SenseVoice) => init_sense_voice(),
        None => init_stt_engine(),
    }
}

pub fn transcribe_audio(
    audio_samples: &[f32],
    engine: &SttEngine,
) -> Result<String, Box<dyn std::error::Error>> {
    engine.transcribe(audio_samples)
}

/// Transcribe using the process-wide engine, initializing it on first use.
pub fn transcribe_with_shared(audio_samples: &[f32]) -> Result<String, Box<dyn std::error::Error>> {
    let mut guard = SHARED_STT.lock().map_err(|_| "STT engine lock poisoned")?;
    if guard.is_none() {
        *guard = Some(init_stt_engine()?);
    }
    guard
        .as_ref()
        .ok_or("STT engine unavailable")?
        .transcribe(audio_samples)
}

/// Backend name of the shared engine, if it has been initialized.
pub fn shared_stt_backend() -> Option<&'static str> {
    SHARED_STT
        .lock()
        .ok()
        .and_then(|guard| guard.as_ref().map(SttEngine::backend_name))
}

pub async fn hybrid_transcribe_audio(
    audio_samples: &[f32],
    engine: &SttEngine,
) -> Result<String, Box<dyn std::error::Error>> {
    engine.transcribe(audio_samples)
}
