/* Copyright (c) 2026 The perfr Authors. All rights reserved.
 * Use of this source code is governed by a BSD-style license that can be
 * found in the LICENSE file. */

/* The two things the Swift package cannot say in Swift 5 without a
 * dependency: an acquire/release pointer slot for the loaded library (the
 * Synchronization module needs macOS 15 and swift-atomics is a package), and
 * pthread_setname_np, whose signature differs per platform and which the
 * Glibc overlay does not re-export. */

#ifndef CPERFR_SHIM_H
#define CPERFR_SHIM_H

#ifdef __cplusplus
extern "C" {
#endif

/* The published library object, or NULL. Acquire, so a thread that never
 * called initialize() sees every field the publishing thread wrote. */
const void* perfr_shim_load(void);

/* Publishes the library object with release. Called once, under a lock. */
void perfr_shim_store(const void* library);

/* Names the calling thread. 0 on success. Linux keeps 15 bytes; the caller
 * has already cut the name. Returns -1 where the OS has no call. */
int perfr_shim_set_thread_name(const char* name);

#ifdef __cplusplus
}
#endif

#endif /* CPERFR_SHIM_H */
