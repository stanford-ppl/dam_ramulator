use cmake;
use std::io::Result;
fn main() -> Result<()> {
    let lib_path = "/home/ubuntu/dam_ramulator/external/ramulator2_wrapper/ext/ramulator2/";
    // let dst = cmake::Config::new(lib_path).build();
    println!("cargo:rustc-link-search=native={}", lib_path);
    // println!("cargo:rustc-link-search=native={}", lib_path);
    println!("cargo:rustc-link-lib=ramulator");
    // println!("cargo:rustc-link-lib=ramulator");
    // println!("cargo:rustc-link-lib=ramulator");
    Ok(())
}