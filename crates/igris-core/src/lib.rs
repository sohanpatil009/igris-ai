//! IGRIS agent core: Moonshine STT, LFM3-Omni-2B / Qwen3.5-4B LLM through llama.cpp, piper
//! TTS, the ZeroClaw-derived tool parser, the dynamic tool registry, and
//! the agent loop that ties them together.
//!
//! Front-ends link this crate and drive its building blocks — the CLI
//! (`igris-cli`, binary `igris-ai`) is the reference front-end; a future
//! GUI (dioxus / tauri crate) would consume the same API without the
//! terminal-only pieces.
//!
//! Runtime assets (llama.cpp builds, piper runtime, STT/TTS models) live
//! in the gitignored `assets/` tree at the workspace root and resolve
//! through [`paths::asset`], independent of the process working directory.

pub mod assist;
pub mod budget;
pub mod gating;
pub mod hotkey;
pub mod llm;
pub mod live;
pub mod memory;
pub mod models;
pub mod paths;
pub mod skills;
pub mod toolgrammar;
pub mod toolparse;
pub mod tools;
pub mod tts;
