fn main() {
    // Icon and version information for the Windows executable.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo:rerun-if-changed=assets/xdisplay.rc");
        println!("cargo:rerun-if-changed=assets/xdisplay.ico");
        embed_resource::compile("assets/xdisplay.rc", embed_resource::NONE)
            .manifest_optional()
            .expect("compiling Windows resources");
    }
}
