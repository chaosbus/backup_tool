use std::{env, process::Command};

fn main() {
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set by cargo");

    println!("cargo:rerun-if-changed=ui");

    match Command::new("node")
        .arg("ui/check-gui.js")
        .current_dir(&manifest_dir)
        .status()
    {
        Ok(status) if status.success() => {}
        Ok(status) => panic!("ui guard failed: node ui/check-gui.js exited with {status}"),
        Err(err) => {
            println!("cargo:warning=node not found; skipping ui guard: {err}");
        }
    }

    tauri_build::build()
}
