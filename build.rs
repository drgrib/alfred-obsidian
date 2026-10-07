fn main() {
    println!("cargo:rustc-link-lib=framework=Foundation");
    println!("cargo:rustc-link-lib=framework=Vision");
    println!("cargo:rustc-link-lib=framework=CoreGraphics");
    println!("cargo:rustc-link-lib=framework=ImageIO");
    
    cc::Build::new()
        .file("src/ocr.m")
        .flag("-fobjc-arc")
        .compile("ocr");
}