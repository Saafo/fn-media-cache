fn main() {
    println!("cargo:rerun-if-changed=icons/menu-bar-template@2x.png");
    tauri_build::build()
}
