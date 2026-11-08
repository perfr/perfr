/* Copyright (c) 2026 The perfr Authors. All rights reserved.
 * Use of this source code is governed by a BSD-style license that can be
 * found in the LICENSE file. */

/* JNI glue. perfr.h loads libperfr; this library does not link it.
 * Methods are dev.perfr.Perfr.native*. */

#include <jni.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>

#include "perfr.h"

#if !defined(_WIN32)
#include <pthread.h>
#endif

static uint64_t start_bytes(JNIEnv *env, jbyteArray utf8, int async) {
  jsize n;
  jbyte *bytes;
  uint64_t handle;
  if (!utf8) return 0;
  n = (*env)->GetArrayLength(env, utf8);
  if (n == 0) return async ? perfr_start_async("", 0) : perfr_start("", 0);
  bytes = (*env)->GetByteArrayElements(env, utf8, NULL);
  if (!bytes) return 0;
  handle = async ? perfr_start_async((const char *)bytes, (size_t)n)
                 : perfr_start((const char *)bytes, (size_t)n);
  (*env)->ReleaseByteArrayElements(env, utf8, bytes, JNI_ABORT);
  return handle;
}

JNIEXPORT jint JNICALL Java_dev_perfr_Perfr_nativeInit(JNIEnv *env, jclass cls) {
  (void)env;
  (void)cls;
  return perfr_init();
}

JNIEXPORT void JNICALL Java_dev_perfr_Perfr_nativeShutdown(JNIEnv *env, jclass cls) {
  (void)env;
  (void)cls;
  perfr_shutdown();
}

JNIEXPORT jboolean JNICALL Java_dev_perfr_Perfr_nativeAvailable(JNIEnv *env, jclass cls) {
  (void)env;
  (void)cls;
  return perfr_available() ? JNI_TRUE : JNI_FALSE;
}

JNIEXPORT jstring JNICALL Java_dev_perfr_Perfr_nativeLibraryPath(JNIEnv *env, jclass cls) {
  FILE *file;
  char line[1024];
  (void)cls;
  file = fopen("/proc/self/maps", "r");
  if (!file) return NULL;
  while (fgets(line, sizeof line, file)) {
    char *start;
    char *end;
    if (!strstr(line, "libperfr.so")) continue;
    start = strchr(line, '/');
    if (!start) continue;
    end = strchr(start, '\n');
    if (end) *end = 0;
    fclose(file);
    return (*env)->NewStringUTF(env, start);
  }
  fclose(file);
  return NULL;
}

JNIEXPORT jlong JNICALL Java_dev_perfr_Perfr_nativeStart(JNIEnv *env, jclass cls, jbyteArray utf8) {
  (void)cls;
  return (jlong)start_bytes(env, utf8, 0);
}

JNIEXPORT jlong JNICALL Java_dev_perfr_Perfr_nativeStartAsync(JNIEnv *env, jclass cls, jbyteArray utf8) {
  (void)cls;
  return (jlong)start_bytes(env, utf8, 1);
}

JNIEXPORT void JNICALL Java_dev_perfr_Perfr_nativeStop(JNIEnv *env, jclass cls, jlong handle) {
  (void)env;
  (void)cls;
  perfr_stop((uint64_t)handle);
}

JNIEXPORT jlong JNICALL Java_dev_perfr_Perfr_nativeInstant(JNIEnv *env, jclass cls, jbyteArray utf8) {
  jsize n;
  jbyte *bytes;
  uint64_t handle;
  (void)cls;
  if (!utf8) return 0;
  n = (*env)->GetArrayLength(env, utf8);
  if (n == 0) return (jlong)perfr_instant("", 0);
  bytes = (*env)->GetByteArrayElements(env, utf8, NULL);
  if (!bytes) return 0;
  handle = perfr_instant((const char *)bytes, (size_t)n);
  (*env)->ReleaseByteArrayElements(env, utf8, bytes, JNI_ABORT);
  return (jlong)handle;
}

JNIEXPORT void JNICALL Java_dev_perfr_Perfr_nativeLink(JNIEnv *env, jclass cls, jlong from, jlong to) {
  (void)env;
  (void)cls;
  perfr_link((uint64_t)from, (uint64_t)to);
}

JNIEXPORT void JNICALL Java_dev_perfr_Perfr_nativeValue(JNIEnv *env, jclass cls, jbyteArray utf8, jdouble value) {
  jsize n;
  jbyte *bytes;
  (void)cls;
  if (!utf8) return;
  n = (*env)->GetArrayLength(env, utf8);
  if (n == 0) {
    perfr_value("", 0, value);
    return;
  }
  bytes = (*env)->GetByteArrayElements(env, utf8, NULL);
  if (!bytes) return;
  perfr_value((const char *)bytes, (size_t)n, value);
  (*env)->ReleaseByteArrayElements(env, utf8, bytes, JNI_ABORT);
}

JNIEXPORT jlong JNICALL Java_dev_perfr_Perfr_nativeNowNs(JNIEnv *env, jclass cls) {
  (void)env;
  (void)cls;
  return (jlong)perfr_now_ns();
}

static void span_bytes(JNIEnv *env, jbyteArray utf8, jlong start_ns, jlong end_ns, int async) {
  jsize n;
  jbyte *bytes;
  if (!utf8) return;
  n = (*env)->GetArrayLength(env, utf8);
  bytes = n == 0 ? NULL : (*env)->GetByteArrayElements(env, utf8, NULL);
  if (n != 0 && !bytes) return;
  if (async)
    perfr_span_async(n ? (const char *)bytes : "", (size_t)n, (uint64_t)start_ns, (uint64_t)end_ns);
  else
    perfr_span(n ? (const char *)bytes : "", (size_t)n, (uint64_t)start_ns, (uint64_t)end_ns);
  if (n) (*env)->ReleaseByteArrayElements(env, utf8, bytes, JNI_ABORT);
}

JNIEXPORT void JNICALL Java_dev_perfr_Perfr_nativeSpan(JNIEnv *env, jclass cls, jbyteArray utf8, jlong start_ns, jlong end_ns) {
  (void)cls;
  span_bytes(env, utf8, start_ns, end_ns, 0);
}

JNIEXPORT void JNICALL Java_dev_perfr_Perfr_nativeSpanAsync(JNIEnv *env, jclass cls, jbyteArray utf8, jlong start_ns, jlong end_ns) {
  (void)cls;
  span_bytes(env, utf8, start_ns, end_ns, 1);
}

JNIEXPORT jboolean JNICALL Java_dev_perfr_Perfr_nativeNameThread(JNIEnv *env, jclass cls, jstring name) {
  const char *chars;
  char buf[16];
  size_t len;
  int rc;
  (void)cls;
#if defined(_WIN32)
  (void)env;
  (void)name;
  return JNI_FALSE;
#else
  if (!name) return JNI_FALSE;
  chars = (*env)->GetStringUTFChars(env, name, NULL);
  if (!chars) return JNI_FALSE;
  len = strlen(chars);
  if (len > 15) len = 15;
  memcpy(buf, chars, len);
  buf[len] = 0;
  (*env)->ReleaseStringUTFChars(env, name, chars);
#if defined(__APPLE__)
  rc = pthread_setname_np(buf);
#else
  rc = pthread_setname_np(pthread_self(), buf);
#endif
  return rc == 0 ? JNI_TRUE : JNI_FALSE;
#endif
}
