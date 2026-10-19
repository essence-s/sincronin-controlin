fn main() {
    // Vigilar las carpetas enteras, no solo los .slint. Los íconos SVG se
    // embeben en tiempo de compilación, así que sin esto editar un SVG no
    // dispara rebuild y la imagen queda vieja.
    println!("cargo:rerun-if-changed=ui");
    println!("cargo:rerun-if-changed=assets");

    slint_build::compile("ui/app.slint").expect("no se pudo compilar ui/app.slint");
}
