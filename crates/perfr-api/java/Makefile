# Copyright (c) 2026 The perfr Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

CC ?= cc
CFLAGS ?= -O2 -fPIC -Wall -Wextra -Werror -D_GNU_SOURCE
JAVA_HOME ?= /usr/lib/jvm/default-java
JNI_CFLAGS := -I$(JAVA_HOME)/include -I$(JAVA_HOME)/include/linux
KOTLINC ?= kotlinc
KOTLIN_STDLIB ?= $(shell dirname $$(dirname $$(command -v $(KOTLINC))))/lib/kotlin-stdlib.jar
CP := build/classes:$(KOTLIN_STDLIB)

.PHONY: all test clean
all: build/libperfr_jni.so build/classes/dev/perfr/Perfr.class

build/libperfr_jni.so: src/main/c/perfr_jni.c ../include/perfr.h
	mkdir -p build
	$(CC) $(CFLAGS) $(JNI_CFLAGS) -I../include -shared -o $@ src/main/c/perfr_jni.c

build/classes/dev/perfr/Perfr.class: src/main/java/dev/perfr/Perfr.java build/libperfr_jni.so
	mkdir -p build/classes
	javac --release 17 -d build/classes src/main/java/dev/perfr/Perfr.java src/sample/java/dev/perfr/Sample.java src/test/java/dev/perfr/PerfrTest.java

build/classes/dev/perfr/ScopesKt.class: build/classes/dev/perfr/Perfr.class src/main/kotlin/dev/perfr/Scopes.kt src/test/kotlin/dev/perfr/ScopeTest.kt
	$(KOTLINC) -cp build/classes -d build/classes src/main/kotlin/dev/perfr/Scopes.kt src/test/kotlin/dev/perfr/ScopeTest.kt

test: build/classes/dev/perfr/Perfr.class build/classes/dev/perfr/ScopesKt.class
	PERFR_JNI=/no/such/libperfr_jni.so PERFR_LIB=/no/such/libperfr.so java -cp $(CP) dev.perfr.PerfrTest
	PERFR_JNI=$(CURDIR)/build/libperfr_jni.so PERFR_LIB=/no/such/libperfr.so java -cp $(CP) dev.perfr.PerfrTest
	PERFR_JNI=$(CURDIR)/build/libperfr_jni.so PERFR_LIB=/no/such/libperfr.so java -cp $(CP) dev.perfr.ScopeTestKt
	@if [ -f "$(LIB)" ]; then \
	  PERFR_JNI=$(CURDIR)/build/libperfr_jni.so PERFR_LIB="$(LIB)" java -cp $(CP) dev.perfr.PerfrTest && \
	  PERFR_JNI=$(CURDIR)/build/libperfr_jni.so PERFR_LIB="$(LIB)" java -cp $(CP) dev.perfr.ScopeTestKt && \
	  PERFR_JNI=$(CURDIR)/build/libperfr_jni.so PERFR_LIB="$(LIB)" java -cp $(CP) dev.perfr.Sample ; \
	else echo "no libperfr at $(LIB); skipped the capture test"; fi

clean:
	rm -rf build
