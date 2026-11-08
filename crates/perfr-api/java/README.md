# perfr for Java and Kotlin

Manual instrumentation for the JVM. `libperfr_jni.so` is a small JNI
library that includes `perfr.h` and does not link `libperfr`. The header
loads `libperfr` at run time. Kotlin calls the same Java classes and adds
`scope(name) { ... }` / `scopeAsync(name) { ... }`.

```java
try (Perfr.Scope frame = Perfr.scope("frame")) {
    Perfr.value("fps", 59.9);
}
```

```kotlin
scope("frame") {
    Perfr.value("fps", 59.9)
}
```

`Perfr.use { }` also works: `Scope` is `AutoCloseable`.

`nameThread` sets the OS thread name. `libperfr` has no thread-name call.
On Linux the name is the 15-byte `comm`. The JNI method takes a Java string
and truncates with `strlen` of the modified-UTF-8 form, which matches UTF-8
for the ASCII names the tests use.

Set `PERFR_JNI` to the absolute path of `libperfr_jni.so`; otherwise the
class loads `perfr_jni` from `java.library.path`. When neither finds the
shim, the class still initialises: `init` returns `E_NOLIB`, `shimLoaded()`
is false and every call is a no-op, the same as a missing `libperfr`.
`PERFR_LIB` is the `libperfr` search override, exclusive when set.

## Build

```sh
make
make test LIB=/path/to/libperfr.so
```

`kotlinc` must be on `PATH`. The test target puts `kotlin-stdlib.jar` from
that compiler on the classpath. There is no Gradle or Maven build and no
JUnit jar: `PerfrTest` and `ScopeTestKt` are self-checking mains.

## What was tested

On Linux x86-64 with OpenJDK 21, Kotlin 2.1.10, and `libperfr.so` from
`cargo build -p perfr-lib`:

- A missing `PERFR_LIB`: `init` returns `E_NOLIB` and calls are no-ops, from
  Java and from the Kotlin wrappers.
- With the library: `init` returns 0, and `java-probe` / `java-fps` are in
  `/dev/shm/perfr-scopes-<pid>` only after the capturing word at byte 28 is
  set. `nameThread` matches `/proc/thread-self/comm`. The sample prints the
  library path.
- macOS and Windows were not run. The JNI `nameThread` returns false on
  Windows.
