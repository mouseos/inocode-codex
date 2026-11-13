// libcef.dll lives in the runtime-fetched CEF directory, not next to the exe, so on Windows it is
// delay-loaded and the directory is added with SetDllDirectory before the first CEF call.
fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo:rustc-link-lib=delayimp");
        println!("cargo:rustc-link-arg-bins=/DELAYLOAD:libcef.dll");
    }
}
