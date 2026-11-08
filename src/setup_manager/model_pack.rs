// src/setup_manager/model_pack.rs
// Focused downloader for the offline voice pack.
//
// The legacy platform setup installs LLVM, FFmpeg, Piper, SenseVoice and
// several optional models. The desktop assistant only needs three assets to
// boot offline, and they are all portable archives:
//
//   - NVIDIA Parakeet-TDT 0.6B v3 int8 (STT)
//   - Kokoro-82M multi-language (TTS)
//   - Silero VAD (endpointing)
//
// Everything is downloaded into `<pkg>/downloads`, extracted into
// `<pkg>/models/<engine>`, and validated before the status flips to ready.

use crate::setup_manager::SetupEvent;
use futures::StreamExt;
use reqwest::Client;
use serde::Serialize;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

#[derive(Clone, Copy, Debug)]
enum AssetKind {
    Direct(&'static str),
    TarBz2(&'static str),
}

struct ModelAsset {
    name: &'static str,
    url: &'static str,
    filename: &'static str,
    kind: AssetKind,
}

const MODEL_ASSETS: &[ModelAsset] = &[
    ModelAsset {
        name: "Moonshine STT (base)",
        url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-moonshine-base-en-int8.tar.bz2",
        filename: "moonshine.tar.bz2",
        kind: AssetKind::TarBz2("models/moonshine"),
    },
    ModelAsset {
        name: "Silero VAD",
        url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/silero_vad.onnx",
        filename: "silero_vad.onnx",
        kind: AssetKind::Direct("models/silero_vad.onnx"),
    },
    ModelAsset {
        name: "Parakeet STT",
        url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8.tar.bz2",
        filename: "parakeet.tar.bz2",
        kind: AssetKind::TarBz2("models/parakeet"),
    },
    ModelAsset {
        name: "Kokoro TTS",
        url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/tts-models/kokoro-multi-lang-v1_0.tar.bz2",
        filename: "kokoro.tar.bz2",
        kind: AssetKind::TarBz2("models/kokoro"),
    },
];

#[derive(Clone, Debug, Serialize)]
pub struct ModelPackStatus {
    pub pkg_dir: String,
    pub stt_backend: String,
    pub stt_ready: bool,
    pub tts_backend: String,
    pub tts_ready: bool,
    pub vad_ready: bool,
    pub ready: bool,
}

fn moonshine_ready(pkg: &Path) -> bool {
    let dir = pkg.join("models/moonshine");
    dir.join("preprocess.onnx").exists()
        && (dir.join("encode.int8.onnx").exists() || dir.join("encode.onnx").exists())
        && (dir.join("uncached_decode.int8.onnx").exists()
            || dir.join("uncached_decode.onnx").exists())
        && (dir.join("cached_decode.int8.onnx").exists() || dir.join("cached_decode.onnx").exists())
}

fn parakeet_ready(pkg: &Path) -> bool {
    let dir = pkg.join("models/parakeet");
    let encoder = dir.join("encoder.int8.onnx").exists() || dir.join("encoder.onnx").exists();
    let decoder = dir.join("decoder.int8.onnx").exists() || dir.join("decoder.onnx").exists();
    let joiner = dir.join("joiner.int8.onnx").exists() || dir.join("joiner.onnx").exists();
    encoder && decoder && joiner && dir.join("tokens.txt").exists()
}

fn kokoro_ready(pkg: &Path) -> bool {
    let dir = pkg.join("models/kokoro");
    let model = dir.join("model.onnx").exists() || dir.join("model.int8.onnx").exists();
    model && dir.join("voices.bin").exists() && dir.join("tokens.txt").exists()
}

fn silero_ready(pkg: &Path) -> bool {
    pkg.join("models/silero_vad.onnx").exists()
}

/// Inspect the installed model pack.
pub fn status() -> ModelPackStatus {
    status_in(&crate::core::paths::pkg_dir())
}

pub fn status_in(pkg: &Path) -> ModelPackStatus {
    let moon_ready = moonshine_ready(pkg);
    let stt_ready = moon_ready || parakeet_ready(pkg);
    let tts_ready = kokoro_ready(pkg);
    let vad_ready = silero_ready(pkg);

    ModelPackStatus {
        pkg_dir: pkg.to_string_lossy().into_owned(),
        stt_backend: if moon_ready {
            "moonshine-base-en-int8".to_string()
        } else if parakeet_ready(pkg) {
            "parakeet-tdt-0.6b-v3".to_string()
        } else {
            "missing".to_string()
        },
        stt_ready,
        tts_backend: if tts_ready {
            "kokoro-82m".to_string()
        } else {
            "missing".to_string()
        },
        tts_ready,
        vad_ready,
        ready: stt_ready && tts_ready && vad_ready,
    }
}

fn asset_ready(pkg: &Path, asset: &ModelAsset) -> bool {
    match asset.kind {
        AssetKind::Direct(relative) => pkg.join(relative).is_file(),
        AssetKind::TarBz2(relative) => {
            let dir = pkg.join(relative);
            match asset.filename {
                "moonshine.tar.bz2" => moonshine_ready(pkg),
                "parakeet.tar.bz2" => parakeet_ready(pkg),
                "kokoro.tar.bz2" => kokoro_ready(pkg),
                _ => dir.is_dir(),
            }
        }
    }
}

fn send(sender: &Arc<Mutex<mpsc::UnboundedSender<SetupEvent>>>, event: SetupEvent) {
    if let Ok(tx) = sender.lock() {
        let _ = tx.send(event);
    }
}

/// Install the offline voice pack, reporting progress through `SetupEvent`.
pub async fn download_model_pack(
    pkg_dir: PathBuf,
    event_sender: mpsc::UnboundedSender<SetupEvent>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let sender = Arc::new(Mutex::new(event_sender));
    let downloads = pkg_dir.join("downloads");
    fs::create_dir_all(&downloads)?;
    crate::core::paths::ensure_layout(&pkg_dir)?;

    let client = Client::builder()
        .user_agent("igris-v5-model-installer/0.1")
        .build()?;

    for asset in MODEL_ASSETS {
        if asset_ready(&pkg_dir, asset) {
            send(
                &sender,
                SetupEvent::Downloading {
                    name: asset.name.to_string(),
                    progress: 100,
                },
            );
            continue;
        }

        let archive = downloads.join(asset.filename);
        let needs_download = match fs::metadata(&archive) {
            Ok(meta) => meta.len() < 1024,
            Err(_) => true,
        };

        if needs_download {
            send(
                &sender,
                SetupEvent::Downloading {
                    name: asset.name.to_string(),
                    progress: 0,
                },
            );
            download_file(&client, asset, &archive, &sender).await?;
        }

        send(
            &sender,
            SetupEvent::Extracting {
                name: asset.name.to_string(),
                progress: 0,
            },
        );
        install_asset(&pkg_dir, asset, &archive)?;
        send(
            &sender,
            SetupEvent::Completed {
                name: asset.name.to_string(),
            },
        );
    }

    // Drop archives once the model files are in place.
    for asset in MODEL_ASSETS {
        if asset_ready(&pkg_dir, asset) {
            let _ = fs::remove_file(downloads.join(asset.filename));
        }
    }

    send(&sender, SetupEvent::AllComplete);
    Ok(())
}

/// Install the model pack without exposing the event channel.
pub async fn install_model_pack(
    pkg_dir: PathBuf,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let drain = tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            eprintln!("[ModelPack] {:?}", event);
        }
    });
    let result = download_model_pack(pkg_dir, tx).await;
    drain.abort();
    result
}

async fn download_file(
    client: &Client,
    asset: &ModelAsset,
    destination: &Path,
    sender: &Arc<Mutex<mpsc::UnboundedSender<SetupEvent>>>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let part = destination.with_extension(format!(
        "{}part",
        destination
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| format!("{e}."))
            .unwrap_or_default()
    ));

    let response = client.get(asset.url).send().await?.error_for_status()?;
    let total = response.content_length().unwrap_or(0);
    let mut file = fs::File::create(&part)?;
    let mut stream = response.bytes_stream();
    let mut downloaded = 0u64;
    let mut last_percent = 0u32;

    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        file.write_all(&chunk)?;
        downloaded += chunk.len() as u64;

        if total > 0 {
            let percent = ((downloaded as f64 / total as f64) * 100.0) as u32;
            if percent >= last_percent + 2 || percent == 100 {
                last_percent = percent;
                send(
                    sender,
                    SetupEvent::Downloading {
                        name: asset.name.to_string(),
                        progress: percent,
                    },
                );
            }
        }
    }

    file.flush()?;
    drop(file);
    fs::rename(&part, destination)?;
    Ok(())
}

fn install_asset(
    pkg_dir: &Path,
    asset: &ModelAsset,
    archive: &Path,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    match asset.kind {
        AssetKind::Direct(relative) => {
            let destination = pkg_dir.join(relative);
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(archive, &destination)?;
        }
        AssetKind::TarBz2(relative) => {
            let destination = pkg_dir.join(relative);
            fs::create_dir_all(&destination)?;

            let stripped = Command::new("tar")
                .args(["-xjf"])
                .arg(archive)
                .arg("-C")
                .arg(&destination)
                .arg("--strip-components")
                .arg("1")
                .status();

            let needs_flatten = match stripped {
                Ok(status) => !status.success(),
                Err(_) => true,
            };

            if needs_flatten {
                let status = Command::new("tar")
                    .args(["-xjf"])
                    .arg(archive)
                    .arg("-C")
                    .arg(&destination)
                    .status()?;
                if !status.success() {
                    return Err(format!(
                        "failed to extract {} (tar exited with {})",
                        archive.display(),
                        status
                    )
                    .into());
                }
                flatten_single_directory(&destination)?;
            }
        }
    }

    if !asset_ready(pkg_dir, asset) {
        return Err(format!("{} installed but model validation failed", asset.name).into());
    }
    Ok(())
}

fn flatten_single_directory(dir: &Path) -> std::io::Result<()> {
    let entries: Vec<PathBuf> = fs::read_dir(dir)?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .collect();

    if entries.len() != 1 || !entries[0].is_dir() {
        return Ok(());
    }

    let nested = &entries[0];
    for entry in fs::read_dir(nested)? {
        let entry = entry?;
        let target = dir.join(entry.file_name());
        if target.exists() {
            continue;
        }
        fs::rename(entry.path(), target)?;
    }
    fs::remove_dir(nested)?;
    Ok(())
}
