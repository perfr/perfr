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
#include <time.h>
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
// The stress run ('s'): two threads call a tiny hooked function as fast as a
// hook lets them, far faster than its return journal can be read; a third
// runs the hooked recursion beside them, and a fourth runs code with no hook
// anywhere on its stack.
NOINLINE uint64_t perfr_unwind_hot(uint64_t x) {
  x = x * 6364136223846793005ULL + 1442695040888963407ULL;
  __asm__ volatile("" : "+r"(x));
  return x;
}
NOINLINE uint64_t perfr_unwind_hot_caller(uint64_t x) {
  struct timespec start, now;
  clock_gettime(CLOCK_MONOTONIC, &start);
  do {
    for (unsigned i = 0; i != 4096; ++i) x = perfr_unwind_hot(x);
    clock_gettime(CLOCK_MONOTONIC, &now);
  } while ((now.tv_sec - start.tv_sec) * 1000 + (now.tv_nsec - start.tv_nsec) / 1000000 < 700);
  return x;
}
NOINLINE uint64_t perfr_unwind_plain_leaf(uint64_t x) {
  for (unsigned i = 0; i != 4000000; ++i) {
    x = x * 6364136223846793005ULL + 1;
    __asm__ volatile("" : "+r"(x));
  }
  return x;
}
NOINLINE uint64_t perfr_unwind_plain(uint64_t x) {
  uint64_t result = perfr_unwind_plain_leaf(x);
  __asm__ volatile("" : "+r"(result));
  return result + 1;
}
NOINLINE void * perfr_unwind_stress_worker(void * arg) {
  uint64_t x = (uintptr_t)arg;
  if (x < 2) return (void *)(uintptr_t)perfr_unwind_hot_caller(x);
  for (unsigned n = 0; n != 96; ++n)
    x = (uintptr_t)arg == 2 ? perfr_unwind_beta(x) : perfr_unwind_plain(x);
  return (void *)(uintptr_t)x;
}
int main(void) {
  prctl(PR_SET_PTRACER, PR_SET_PTRACER_ANY, 0, 0, 0);
  printf("%d\n", getpid()); fflush(stdout);
  pthread_t threads[4];
  int command;
  while ((command = getchar()) != EOF) {
    if (command == 's') {
      for (uintptr_t i = 0; i != 4; ++i)
        pthread_create(&threads[i], NULL, perfr_unwind_stress_worker, (void *)i);
      for (unsigned i = 0; i != 4; ++i) pthread_join(threads[i], NULL);
      puts("done"); fflush(stdout);
      continue;
    }
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
