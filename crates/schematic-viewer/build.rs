#[cfg(feature = "renderer-kicad-cli")]
fn main() {
    tauri_build::build();
}

#[cfg(not(feature = "renderer-kicad-cli"))]
fn main() {}
