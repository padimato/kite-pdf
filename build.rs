//! Intègre l'icône et les informations de version (Propriétés > Détails) dans l'exécutable Windows.
fn main() {
    println!("cargo:rerun-if-changed=assets/icon/kite.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let mut res = winresource::WindowsResource::new();
    res.set_icon("assets/icon/kite.ico")
        .set("ProductName", "Kite PDF")
        .set("FileDescription", "Kite PDF")
        .set("InternalName", "kite-pdf")
        .set("OriginalFilename", "kite-pdf.exe")
        .set("LegalCopyright", "Licence MIT");
    if let Err(e) = res.compile() {
        // Sans le SDK Windows (rc.exe), l'exécutable fonctionne quand même, simplement sans icône.
        println!("cargo:warning=icône non intégrée : {e}");
    }
}
