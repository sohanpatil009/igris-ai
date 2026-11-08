// src/core/mod.rs - Core voice processing modules

pub mod about;
pub mod audio_capture;
pub mod paths;
#[cfg(all(target_os = "windows", piper_ffi))]
pub mod piper_ffi;
pub mod stt;
pub mod tts;
pub mod vad;
pub mod wake_word;

// Re-exports for convenience
pub use about::{is_about_query, wants_detailed_info, AboutSection, IgrisAbout};
pub use audio_capture::{capture_audio_vad, CaptureConfig, CaptureMode, CaptureResult};
pub use stt::{
    hybrid_transcribe_audio, init_stt_engine, init_stt_engine_prefer, shared_stt_backend,
    transcribe_audio, transcribe_with_shared, SttBackend, SttEngine,
};
pub use tts::{backend_name, init_tts, speak, speak_compat, synthesize_to_file, TTS_ENGINE};
pub use wake_word::{listen_for_wake_word, listen_for_wake_word_async};
