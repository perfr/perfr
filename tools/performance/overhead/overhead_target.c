// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
//
// The program `overhead.py` measures: the same small function called in a
// loop, plain, through a function the driver hooks, and inside a manual
// scope. It times batches of calls and prints what each call cost, so the
// driver can ask the same question before a capture, during one, with a hook
// armed and after it is gone.
//
// One command per line on standard input, one JSON line back:
//
//   init                      perfr_init(); {"init": rc}
//   run <case> <rounds> <calls> <batches> <cpu>[,<cpu>...]
//                             one thread per cpu, pinned to it, all started
//                             together; each times <batches> batches of
//                             <calls> calls. {"case":..,"ns":[..]} with one
//                             ns-per-call figure per thread and batch.
//   quit
//
// Cases:
//   plain        overhead_plain(x, rounds); never instrumented
//   hooked       overhead_hooked(x, rounds); the function the driver hooks
//   scope        perfr_start("overhead_scope") + overhead_plain + perfr_stop
//   scope_long   the same under a 96-byte name, which takes four records
//
// The two functions have the same body: `rounds` turns of a xorshift, each
// waiting on the one before, so the time a call takes is set by `rounds` and
// not by how well the processor overlaps one call with the next.

#define _GNU_SOURCE
#include <errno.h>
#include <inttypes.h>
#include <pthread.h>
#include <sched.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>
#ifdef __linux__
#include <sys/prctl.h>
#endif

#include "perfr.h"

// Not inlined, and not folded into one: GCC merges functions with the same
// body at -O2, and the hook would then land on both loops.
#if defined(__GNUC__) && !defined(__clang__)
#define NOINLINE __attribute__((noinline, noclone, no_icf))
#else
#define NOINLINE __attribute__((noinline))
#endif

#define BODY                                \
  for (uint64_t i = 0; i < rounds; ++i) {   \
    x ^= x << 13;                           \
    x ^= x >> 7;                            \
    x ^= x << 17;                           \
  }                                         \
  return x

NOINLINE uint64_t overhead_plain(uint64_t x, uint64_t rounds) { BODY; }
NOINLINE uint64_t overhead_hooked(uint64_t x, uint64_t rounds) { BODY; }

#define LONG_NAME                                                               \
  "overhead_scope_long::a_name_as_long_as_a_cpp_signature<with, template, "   \
  "arguments>(and, parameters)"

enum Case { PLAIN, HOOKED, SCOPE, SCOPE_LONG };

static uint64_t now_ns(void) {
  struct timespec t;
  clock_gettime(CLOCK_MONOTONIC, &t);
  return (uint64_t)t.tv_sec * 1000000000ull + (uint64_t)t.tv_nsec;
}

// One batch. The loops are written out per case so that the case is not a
// branch inside the loop being timed.
static NOINLINE uint64_t batch(enum Case which, uint64_t rounds, uint64_t calls, uint64_t x) {
  switch (which) {
    case PLAIN:
      for (uint64_t i = 0; i < calls; ++i) x = overhead_plain(x, rounds);
      break;
    case HOOKED:
      for (uint64_t i = 0; i < calls; ++i) x = overhead_hooked(x, rounds);
      break;
    case SCOPE:
      for (uint64_t i = 0; i < calls; ++i) {
        perfr_scope s = perfr_start(PERFR_LIT("overhead_scope"));
        x = overhead_plain(x, rounds);
        perfr_stop(s);
      }
      break;
    case SCOPE_LONG:
      for (uint64_t i = 0; i < calls; ++i) {
        perfr_scope s = perfr_start(PERFR_LIT(LONG_NAME));
        x = overhead_plain(x, rounds);
        perfr_stop(s);
      }
      break;
  }
  return x;
}

struct Job {
  enum Case which;
  uint64_t rounds, calls, batches;
  int cpu;
  pthread_barrier_t* start;
  double* ns;  // one per batch
  uint64_t sink;
  int pinned;
};

static void* worker(void* arg) {
  struct Job* job = arg;
  cpu_set_t set;
  CPU_ZERO(&set);
  CPU_SET(job->cpu, &set);
  job->pinned = sched_setaffinity(0, sizeof set, &set) == 0;
  // Once through, untimed: the thread's first record takes its ring, and a
  // hook's first call on a thread sets up its side of the interceptor.
  uint64_t x = batch(job->which, job->rounds, job->calls < 64 ? job->calls : 64, 0x9e3779b97f4a7c15ull + (uint64_t)job->cpu);
  pthread_barrier_wait(job->start);
  for (uint64_t b = 0; b < job->batches; ++b) {
    uint64_t t0 = now_ns();
    x = batch(job->which, job->rounds, job->calls, x);
    uint64_t t1 = now_ns();
    job->ns[b] = (double)(t1 - t0) / (double)job->calls;
  }
  job->sink = x;
  return NULL;
}

static int parse_case(const char* name, enum Case* out) {
  if (strcmp(name, "plain") == 0) *out = PLAIN;
  else if (strcmp(name, "hooked") == 0) *out = HOOKED;
  else if (strcmp(name, "scope") == 0) *out = SCOPE;
  else if (strcmp(name, "scope_long") == 0) *out = SCOPE_LONG;
  else return 0;
  return 1;
}

#define MAX_THREADS 256

static void run(char* line) {
  char name[32], cpus[1024];
  uint64_t rounds, calls, batches;
  if (sscanf(line, "run %31s %" SCNu64 " %" SCNu64 " %" SCNu64 " %1023s", name, &rounds, &calls, &batches, cpus) != 5) {
    puts("{\"error\":\"run <case> <rounds> <calls> <batches> <cpus>\"}");
    return;
  }
  enum Case which;
  if (!parse_case(name, &which) || calls == 0 || batches == 0) {
    puts("{\"error\":\"unknown case, or no calls or batches\"}");
    return;
  }
  static struct Job jobs[MAX_THREADS];
  static pthread_t threads[MAX_THREADS];
  int n = 0;
  for (char* tok = strtok(cpus, ","); tok != NULL && n < MAX_THREADS; tok = strtok(NULL, ",")) {
    jobs[n].cpu = atoi(tok);
    ++n;
  }
  if (n == 0) {
    puts("{\"error\":\"no cpus\"}");
    return;
  }
  pthread_barrier_t start;
  pthread_barrier_init(&start, NULL, (unsigned)n);
  for (int i = 0; i < n; ++i) {
    jobs[i].which = which;
    jobs[i].rounds = rounds;
    jobs[i].calls = calls;
    jobs[i].batches = batches;
    jobs[i].start = &start;
    jobs[i].ns = calloc(batches, sizeof(double));
    jobs[i].pinned = 0;
    if (pthread_create(&threads[i], NULL, worker, &jobs[i]) != 0) {
      puts("{\"error\":\"pthread_create\"}");
      exit(4);
    }
  }
  uint64_t sink = 0;
  int pinned = 1;
  for (int i = 0; i < n; ++i) {
    pthread_join(threads[i], NULL);
    sink ^= jobs[i].sink;
    pinned &= jobs[i].pinned;
  }
  pthread_barrier_destroy(&start);
  printf("{\"case\":\"%s\",\"rounds\":%" PRIu64 ",\"calls\":%" PRIu64 ",\"batches\":%" PRIu64
         ",\"threads\":%d,\"pinned\":%s,\"sink\":%" PRIu64 ",\"ns\":[",
         name, rounds, calls, batches, n, pinned ? "true" : "false", sink);
  for (int i = 0; i < n; ++i) {
    for (uint64_t b = 0; b < batches; ++b) printf("%s%.4f", (i == 0 && b == 0) ? "" : ",", jobs[i].ns[b]);
    free(jobs[i].ns);
  }
  puts("]}");
}

int main(void) {
#ifdef __linux__
  // Lets a service that is not this program's parent inject its agent where
  // Yama restricts ptrace. EINVAL means the kernel has no Yama.
  if (prctl(PR_SET_PTRACER, PR_SET_PTRACER_ANY, 0, 0, 0) != 0 && errno != EINVAL) return 2;
#endif
  setvbuf(stdout, NULL, _IOLBF, 0);
  printf("{\"pid\":%d}\n", (int)getpid());
  char line[2048];
  while (fgets(line, sizeof line, stdin) != NULL) {
    if (strncmp(line, "quit", 4) == 0) break;
    if (strncmp(line, "init", 4) == 0) {
      int rc = perfr_init();
      printf("{\"init\":%d,\"available\":%d}\n", rc, perfr_available());
      continue;
    }
    if (strncmp(line, "run ", 4) == 0) {
      run(line);
      continue;
    }
    puts("{\"error\":\"unknown command\"}");
  }
  perfr_shutdown();
  return 0;
}
