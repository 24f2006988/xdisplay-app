fn main() {
    let target = std::env::var("TARGET").unwrap_or_default();
    if target.contains("android") {
        println!("cargo:rustc-link-lib=mediandk");
        println!("cargo:rustc-link-lib=android");
        println!("cargo:rustc-link-lib=log");
    }
}
