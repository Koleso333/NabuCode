fn main() {
    println!("cargo:rerun-if-env-changed=NABUCODE_SERVER_EXE");
    tauri_build::build()
}
