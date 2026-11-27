//! Packs the Chrome extension into the server: the server puts it where Chrome loads it from and keeps it current.
//! Its manifest gets `version_name` with a hash of the files, so the loaded extension can tell it was replaced.

use std::io::Write;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

fn main() {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../extension");
    println!("cargo:rerun-if-changed={}", source.display());
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("extension.zip");
    let mut zip = zip::ZipWriter::new(std::fs::File::create(&out).unwrap());
    // Without the built side panel the extension would be broken: the server then leaves the installed one alone
    if !source.join("panel/index.html").is_file() {
        println!("cargo:warning=extension/panel is not built (npm run build:panel): the server carries no extension");
        zip.finish().unwrap();
        return;
    }
    let mut files = Vec::new();
    collect(&source, &source, &mut files);
    files.sort();
    let mut hash = Sha256::new();
    for (name, bytes) in &files {
        hash.update(name.as_bytes());
        hash.update(bytes);
    }
    let id: String = hash.finalize().iter().take(6).map(|byte| format!("{byte:02x}")).collect();
    let options = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    for (name, bytes) in files {
        let bytes = if name == "manifest.json" { with_build(&bytes, &id) } else { bytes };
        zip.start_file(name, options).unwrap();
        zip.write_all(&bytes).unwrap();
    }
    zip.finish().unwrap();
}

fn collect(root: &Path, dir: &Path, files: &mut Vec<(String, Vec<u8>)>) {
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(root, &path, files);
        } else {
            let name = path.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
            files.push((name, std::fs::read(&path).unwrap()));
        }
    }
}

fn with_build(manifest: &[u8], id: &str) -> Vec<u8> {
    let mut manifest: serde_json::Value = serde_json::from_slice(manifest).expect("extension/manifest.json is not JSON");
    let version = manifest["version"].as_str().unwrap_or("0").to_owned();
    manifest["version_name"] = format!("{version} {id}").into();
    serde_json::to_vec_pretty(&manifest).unwrap()
}
