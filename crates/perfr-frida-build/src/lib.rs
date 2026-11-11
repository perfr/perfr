// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Shared native devkit linking for the small Core and Gum ABI adapters.
//! Called from the build scripts of `perfr-lib` and
//! `perfr-hook`; the devkits come from `tools/frida/devkits.sh`.

/// Where the `component` devkit for `target` is expected, and whether the
/// root was named by `PERFR_FRIDA_DEVKIT_ROOT` rather than defaulted to
/// `target/frida-devkits` of the workspace.
fn devkit_dir(target: &str, component: &str) -> (std::path::PathBuf, bool) {
    let from_env = std::env::var_os("PERFR_FRIDA_DEVKIT_ROOT").map(std::path::PathBuf::from);
    let named = from_env.is_some();
    let root = from_env.unwrap_or_else(|| {
        std::path::PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap())
            .join("../../target/frida-devkits")
    });
    (root.join(target).join(component), named)
}

fn devkit_header(kit: &std::path::Path, component: &str) -> std::path::PathBuf {
    kit.join(format!("frida-{component}.h"))
}

/// What a build script that has a stub does about the devkit.
#[derive(Debug, PartialEq, Eq)]
enum Plan {
    Devkit,
    Stub,
    Fail,
}

/// The stub exists so that `cargo test --workspace` and `cargo clippy
/// --workspace` work on a checkout that never ran `tools/frida/devkits.sh`.
/// It must not be reachable from a build somebody ships or asked for by
/// name: a release profile, or a root given in the environment, still fails
/// when the devkit is not there, so a skipped download cannot turn into a
/// helper that injects nothing.
fn plan(devkit_present: bool, root_named: bool, profile: &str) -> Plan {
    if devkit_present {
        Plan::Devkit
    } else if !root_named && profile != "release" {
        Plan::Stub
    } else {
        Plan::Fail
    }
}

/// `build_frida`, except that a debug build with no devkit in the default
/// place compiles `stub` (C source text defining the same symbols as
/// `source`) and links no Frida at all. Returns whether the real adapter was
/// built.
pub fn build_frida_or_stub(component: &str, source: &str, stub: &str) -> bool {
    let target = std::env::var("TARGET").unwrap();
    let (kit, root_named) = devkit_dir(&target, component);
    let header = devkit_header(&kit, component);
    let profile = std::env::var("PROFILE").unwrap_or_default();
    if plan(header.is_file(), root_named, &profile) != Plan::Stub {
        build_frida(component, source);
        return true;
    }
    println!(
        "cargo:warning=no Frida {component} devkit at {}: building a stub that fails at run time. \
         Run tools/frida/devkits.sh {target} for the real one.",
        kit.display()
    );
    println!("cargo:rerun-if-env-changed=PERFR_FRIDA_DEVKIT_ROOT");
    // A path that does not exist makes cargo rerun this script on every
    // build, which is what picks the devkit up once it has been downloaded.
    println!("cargo:rerun-if-changed={}", header.display());
    let stub_path = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap())
        .join(format!("frida-{component}-stub.c"));
    std::fs::write(&stub_path, stub).expect("write the devkit stub");
    cc::Build::new()
        .file(&stub_path)
        .flag_if_supported("-fvisibility=hidden")
        .compile(&format!("perfr-frida-{component}-adapter"));
    false
}

/// Compiles `source` against the Frida `component` devkit ("core" or "gum")
/// for the build's target and emits the link lines for it. The devkit root
/// is `PERFR_FRIDA_DEVKIT_ROOT`, by default `target/frida-devkits` of the
/// workspace.
pub fn build_frida(component: &str, source: &str) {
    let target = std::env::var("TARGET").unwrap();
    let (kit, _) = devkit_dir(&target, component);
    assert!(
        devkit_header(&kit, component).is_file(),
        "missing Frida {component} devkit at {}; run tools/frida/devkits.sh {target}",
        kit.display()
    );
    println!("cargo:rerun-if-env-changed=PERFR_FRIDA_DEVKIT_ROOT");
    println!("cargo:rerun-if-changed={source}");
    cc::Build::new()
        .file(source)
        .include(&kit)
        .flag_if_supported("-fvisibility=hidden")
        .compile(&format!("perfr-frida-{component}-adapter"));
    println!("cargo:rustc-link-search=native={}", kit.display());
    println!("cargo:rustc-link-lib=static=frida-{component}");
    if target.contains("windows") {
        // The MSVC devkits are built against the static CRT (/MT) and lean
        // on these system libraries; the .lib carries no defaultlib
        // directives for them.
        for lib in [
            "ws2_32", "winmm", "shlwapi", "crypt32", "secur32", "iphlpapi", "psapi", "dnsapi", "setupapi",
            "ole32", "shell32", "user32", "gdi32", "advapi32", "bcrypt", "dbghelp", "ntdll", "userenv", "version",
        ] {
            println!("cargo:rustc-link-lib={lib}");
        }
        return;
    }
    if target.contains("apple") {
        // Do not make the target agent depend on unused dylibs/frameworks:
        // Darwin's injector resolves dependencies in the target process.
        println!("cargo:rustc-link-arg=-Wl,-dead_strip_dylibs");
        for lib in ["resolv", "m"] {
            println!("cargo:rustc-link-lib={lib}");
        }
        if component == "core" {
            for lib in ["bsm", "dl"] {
                println!("cargo:rustc-link-lib={lib}");
            }
            for framework in ["Foundation", "CoreFoundation", "AppKit"] {
                println!("cargo:rustc-link-lib=framework={framework}");
            }
        }
    } else {
        for lib in ["resolv", "dl", "m", "pthread", "rt"] {
            println!("cargo:rustc-link-lib={lib}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_devkit_that_is_there_is_always_used() {
        for named in [false, true] {
            for profile in ["debug", "release"] {
                assert_eq!(plan(true, named, profile), Plan::Devkit);
            }
        }
    }

    #[test]
    fn a_debug_build_of_a_checkout_without_devkits_gets_the_stub() {
        assert_eq!(plan(false, false, "debug"), Plan::Stub);
    }

    #[test]
    fn a_release_build_never_gets_the_stub() {
        assert_eq!(plan(false, false, "release"), Plan::Fail);
        assert_eq!(plan(false, true, "release"), Plan::Fail);
    }

    #[test]
    fn a_devkit_root_named_in_the_environment_must_hold_the_devkit() {
        assert_eq!(plan(false, true, "debug"), Plan::Fail);
    }
}
