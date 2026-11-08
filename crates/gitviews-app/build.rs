fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=assets/windows/gitviews.rc");
    println!("cargo:rerun-if-changed=assets/icons/gitviews.ico");

    // Deep native element trees and font shaping use the main thread on Windows.
    // Match the stack reserve commonly available to desktop UI threads on Unix.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo:rustc-link-arg-bin=gitviews=/STACK:8388608");
        embed_resource::compile_for(
            "assets/windows/gitviews.rc",
            ["gitviews"],
            embed_resource::ParamsIncludeDirs(["assets/icons"]),
        )
        .manifest_required()
        .expect("the GitViews application icon must be embedded");
    }
}
