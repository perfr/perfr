// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
// Linux x86-64 regression fixture. Build with -O2 -g -fomit-frame-pointer
// -fno-optimize-sibling-calls -pthread. No perfr SDK is linked.
#include <pthread.h>
#include <stdint.h>
#include <stdatomic.h>
#include <sched.h>
#include <stdio.h>
#include <sys/prctl.h>
#include <unistd.h>
#define NOINLINE __attribute__((noinline))
static atomic_uint hold, waiting;
NOINLINE uint64_t perfr_unwind_leaf(uint64_t x) {
  if (atomic_load(&hold)) {
    atomic_fetch_add(&waiting, 1);
    while (atomic_load(&hold)) __asm__ volatile("" ::: "memory");
  }
  for (unsigned i = 0; i != 4000000; ++i) {
    x = x * 6364136223846793005ULL + 1;
    __asm__ volatile("" : "+r"(x));
  }
  return x;
}
NOINLINE uint64_t perfr_unwind_recursive(unsigned depth, uint64_t x) {
  uint64_t result = depth ? perfr_unwind_recursive(depth - 1, x + depth) : perfr_unwind_leaf(x);
  __asm__ volatile("" : "+r"(result));
  return result + depth;
}
NOINLINE uint64_t perfr_unwind_alpha(uint64_t x) { return perfr_unwind_recursive(3, x); }
NOINLINE uint64_t perfr_unwind_beta(uint64_t x) { return perfr_unwind_recursive(3, x); }
NOINLINE void * perfr_unwind_worker(void * arg) {
  uint64_t x = (uintptr_t)arg;
  for (unsigned n = 0; n != 48; ++n)
    x = arg ? perfr_unwind_alpha(x) : perfr_unwind_beta(x);
  return (void *)(uintptr_t)x;
}
int main(void) {
  prctl(PR_SET_PTRACER, PR_SET_PTRACER_ANY, 0, 0, 0);
  printf("%d\n", getpid()); fflush(stdout);
  pthread_t threads[2];
  int command;
  while ((command = getchar()) != EOF) {
    if (command != 'r') {
      atomic_store(&waiting, 0);
      atomic_store(&hold, command == 'h');
      for (uintptr_t i = 0; i != 2; ++i)
        pthread_create(&threads[i], NULL, perfr_unwind_worker, (void *)i);
    } else { atomic_store(&hold, 0); }
    if (command == 'h') {
      while (atomic_load(&waiting) != 2) sched_yield();
      puts("held"); fflush(stdout);
      continue;
    }
    for (unsigned i = 0; i != 2; ++i) pthread_join(threads[i], NULL);
    puts("done"); fflush(stdout);
  }
}
