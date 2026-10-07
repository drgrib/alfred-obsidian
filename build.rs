fn main() {
    println!("cargo:rustc-link-lib=framework=Foundation");
    println!("cargo:rustc-link-lib=framework=Vision");
    println!("cargo:rustc-link-lib=framework=CoreGraphics");
    println!("cargo:rustc-link-lib=framework=ImageIO");
    println!("cargo:rustc-link-lib=framework=PDFKit");

    println!("cargo:rerun-if-changed=src/ocr.m");
    
    cc::Build::new()
        .file("src/ocr.m")
        .flag("-fobjc-arc")
        .compile("ocr");
}