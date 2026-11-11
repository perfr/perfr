// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use perfr_frida_build::build_frida_or_stub;

/// Stands in for src/core.c when there is no Frida Core devkit to compile it
/// against (a fresh checkout running the workspace's tests). Same two
/// symbols; every injection fails with the reason.
const CORE_STUB: &str = r#"
#include <stddef.h>
#include <stdio.h>

void * perfr_core_inject(unsigned pid, const char * path, const char * data,
    char * message, size_t capacity, const void * loader, size_t loader_size) {
  (void)pid; (void)path; (void)data; (void)loader; (void)loader_size;
  snprintf(message, capacity, "%s",
      "this perfr-hook was built without the Frida Core devkit and cannot inject; "
      "run tools/frida/devkits.sh and rebuild it");
  return NULL;
}
void perfr_core_close(void * injector) { (void)injector; }
"#;

fn main() {
    build_frida_or_stub("core", "src/core.c", CORE_STUB);
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rerun-if-changed=src/loader.c");
        let output =
            std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap()).join("loader.dylib");
        let status = cc::Build::new()
            .get_compiler()
            .to_command()
            .args(["-dynamiclib", "-O2", "src/loader.c", "-o"])
            .arg(&output)
            .status()
            .expect("compile Darwin loader");
        assert!(status.success(), "Darwin loader compilation failed");
    }
}
