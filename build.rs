use std::path::{Path, PathBuf};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=src/core/piper_ffi.rs");

    // Use vendored protoc so Windows builds don't require manual protoc installation.
    // The riva proto tree lives in the desktop project (owner of the assistant
    // setup/download flow); this crate only generates the client bindings.
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default();
    let manifest_path = PathBuf::from(&manifest_dir);
    let proto_root = manifest_path.join("../igris-ecosystem");
    // Riva bindings are feature-gated (with_riva); only generate + compile
    // when the feature is on, matching the old desktop build behavior.
    if std::env::var("CARGO_FEATURE_WITH_RIVA").is_ok() {
        let proto_file = proto_root.join("proto/riva/proto/riva_asr.proto");
        if proto_file.exists() {
            let protoc_path = protoc_bin_vendored::protoc_bin_path()
                .expect("protoc-bin-vendored failed to locate protoc binary");
            std::env::set_var("PROTOC", protoc_path);

            tonic_build::configure()
                .build_server(false)
                .compile_protos(
                    &[proto_root.join("proto/riva/proto/riva_asr.proto")],
                    &[proto_root.join("proto/")],
                )?;
        } else {
            println!(
                "cargo:warning=riva proto not found at {}, skipping tonic_build",
                proto_file.display()
            );
        }
    }

    #[cfg(target_os = "windows")]
    {
        // Link libpiper (source-built piper1-gpl) when the FFI install is
        // available in the desktop companion's pkg tree. When absent, the
        // piper_ffi module is compiled out and TTS falls back to spawning
        // piper.exe.
        let piper_dir = Path::new(&manifest_dir)
            .join("../igris-ecosystem/pkg/piper/piper1-gpl/libpiper/install");
        if piper_dir.exists() {
            println!("cargo:rustc-link-search=native={}", piper_dir.display());
            println!("cargo:rustc-link-lib=piper");
            println!("cargo:rustc-cfg=piper_ffi");
        }
    }
    // Declare the custom cfg so cargo's unexpected_cfgs lint stays quiet.
    println!("cargo::rustc-check-cfg=cfg(piper_ffi)");

    Ok(())
}
