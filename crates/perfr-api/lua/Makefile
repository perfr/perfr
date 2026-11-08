# Copyright (c) 2026 The perfr Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

CC ?= cc
CFLAGS ?= -O2 -fPIC -Wall -Wextra -Werror -D_GNU_SOURCE
LUA_CFLAGS := $(shell pkg-config --cflags lua5.4)
LUA_LIBS := $(shell pkg-config --libs lua5.4)

.PHONY: all clean
all: perfr_native.so

perfr_native.so: src/perfr_native.c ../include/perfr.h
	$(CC) $(CFLAGS) $(LUA_CFLAGS) -I../include -shared -o $@ src/perfr_native.c $(LUA_LIBS)

clean:
	rm -f perfr_native.so
