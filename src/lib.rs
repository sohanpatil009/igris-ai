//! igris-ai — IGRIS voice assistant intelligence core + its own installer.
//!
//! Extracted from the IGRIS desktop companion so the assistant brain
//! (speech in/out, wake word, VAD, NLU, online reasoning) lives in its own
//! crate while the companion keeps owning the app shell, UI, and device
//! ecosystem. The companion installs [`bridge::install`] before starting
//! the assistant.
//!
//! Since several engines are still being stabilized upstream, dead code is
//! tolerated crate-wide.

#![allow(dead_code)]
#![allow(unused_imports)]
#![allow(unused_variables)]

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

/// One-way seam to the companion host (config values + plugin snapshots).
pub mod bridge;

/// Self-contained installer for the assistant: model downloads, extraction,
/// validation, per-platform tooling setup, and runtime permissions.
pub mod setup_manager;

/// Speech engines: STT (sherpa-onnx), TTS (piper/sherpa), VAD, wake word,
/// audio capture, assistant about-knowledge.
pub mod core;

/// Natural language understanding: intent engine, NER, SBERT semantic
/// matching, conversation context, sentence splitting.
pub mod nlu;

/// Online mode: NVIDIA NIM STT, reasoning, task planning, intent routing.
pub mod online;

// ---- Shared assistant control state (moved from the companion shell) ----

// Selected online LLM model for reasoning (label string), sourced from the
// NVIDIA_NIM_MODEL env default like the original companion shell.
pub static SELECTED_MODEL: once_cell::sync::Lazy<Arc<Mutex<String>>> =
    once_cell::sync::Lazy::new(|| Arc::new(Mutex::new(
        std::env::var("NVIDIA_NIM_MODEL").unwrap_or_else(|_| "meta/llama-3.1-70b-instruct".to_string())
    )));

pub fn set_selected_model(model: &str) {
    if let Ok(mut m) = SELECTED_MODEL.lock() {
        *m = model.to_string();
    }
}

pub fn get_selected_model() -> String {
    SELECTED_MODEL.lock().map(|m| m.clone()).unwrap_or_default()
}

// Selected provider (nvidia, openai, groq, google)
pub static SELECTED_PROVIDER: once_cell::sync::Lazy<Arc<Mutex<String>>> =
    once_cell::sync::Lazy::new(|| Arc::new(Mutex::new("nvidia".to_string())));

pub fn set_selected_provider(provider: &str) {
    if let Ok(mut p) = SELECTED_PROVIDER.lock() {
        *p = provider.to_string();
    }
}

pub fn get_selected_provider() -> String {
    SELECTED_PROVIDER
        .lock()
        .map(|p| p.clone())
        .unwrap_or_else(|_| "nvidia".to_string())
}

// Reset flag - set by global hotkey to restart voice loop from wake word detection
pub static RESET_FLAG: AtomicBool = AtomicBool::new(false);

// Force listen flag - set by global hotkey to skip wake word and enter continuous listening mode directly
pub static FORCE_LISTEN: AtomicBool = AtomicBool::new(false);

// TTS playing flag - set while TTS audio is playing, suppresses mic input to prevent feedback loop
pub static TTS_PLAYING: AtomicBool = AtomicBool::new(false);
