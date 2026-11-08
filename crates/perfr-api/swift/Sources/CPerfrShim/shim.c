/* Copyright (c) 2026 The perfr Authors. All rights reserved.
 * Use of this source code is governed by a BSD-style license that can be
 * found in the LICENSE file. */

/* glibc declares pthread_setname_np only under _GNU_SOURCE. */
#if !defined(_WIN32) && !defined(_GNU_SOURCE)
#define _GNU_SOURCE
#endif

#include "CPerfrShim.h"

#if !defined(_WIN32)
#include <pthread.h>
#endif

static const void* slot = 0;

const void* perfr_shim_load(void) { return __atomic_load_n(&slot, __ATOMIC_ACQUIRE); }

void perfr_shim_store(const void* library) { __atomic_store_n(&slot, library, __ATOMIC_RELEASE); }

int perfr_shim_set_thread_name(const char* name) {
#if defined(_WIN32)
  (void)name;
  return -1;
#elif defined(__APPLE__)
  return pthread_setname_np(name);
#else
  return pthread_setname_np(pthread_self(), name);
#endif
}
