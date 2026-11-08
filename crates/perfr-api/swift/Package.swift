// swift-tools-version:5.10
// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

import PackageDescription

let package = Package(
    name: "Perfr",
    products: [
        .library(name: "Perfr", targets: ["Perfr"]),
        .executable(name: "PerfrSample", targets: ["PerfrSample"]),
    ],
    targets: [
        .target(name: "Perfr"),
        .executableTarget(name: "PerfrSample", dependencies: ["Perfr"]),
        .testTarget(name: "PerfrTests", dependencies: ["Perfr"]),
    ]
)
