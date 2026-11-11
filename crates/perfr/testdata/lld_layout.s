# Copyright (c) 2026 The perfr Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# A process with lld's segment layout, for the symbolizer's tests: lld does
# not pad the file between segments, it bumps the virtual address by a page,
# so the text mapping is a page further from its file offset than the
# headers' mapping is. GNU ld pads, which is why a test binary built with it
# cannot stand in for this one.
#
# The two binaries beside this file are it, linked with LLD 20.1.5 both ways:
#
#   as --64 -o lld_layout.o lld_layout.s
#   ld.lld --build-id=none      -o lld_layout_image_base lld_layout.o
#   ld.lld --build-id=none -pie -o lld_layout_pie        lld_layout.o
#
# (`ld.lld` is `rust-lld -flavor gnu` where only a Rust toolchain has one.)
# The first is loaded at lld's default image base, 0x200000; the second
# wherever the kernel likes. Neither needs libc or a dynamic loader.
#
# The process writes the address it sees `marker` at to stdout, as eight raw
# bytes, then sleeps until it is killed -- or for a minute, should the test
# that started it die first.

        .text
        .globl  _start
        .type   _start, @function
_start:
        mov     $37, %eax               # alarm(60)
        mov     $60, %edi
        syscall
        lea     marker(%rip), %rax
        push    %rax
        mov     $1, %eax                # write(1, &address, 8)
        mov     $1, %edi
        mov     %rsp, %rsi
        mov     $8, %edx
        syscall
1:      mov     $34, %eax               # pause()
        syscall
        jmp     1b
        .size   _start, .-_start

        .globl  before_marker
        .type   before_marker, @function
before_marker:
        ret
        .size   before_marker, .-before_marker

        .p2align 4
        .globl  marker
        .type   marker, @function
marker:
        nop
        nop
        nop
        nop
        ret
        .size   marker, .-marker

# Read-only data, so the first segment holds more than the headers.
        .section .rodata
        .globl  anchor
anchor: .asciz  "a read-only segment ahead of the text"
