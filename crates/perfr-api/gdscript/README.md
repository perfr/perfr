# GDScript

A Godot GDExtension is not part of this change.

The other language bindings are thin callers of `libperfr`: they compile
with the language's own toolchain and talk to the C API. A GDExtension is
not that shape. Godot loads a shared library that was built against one
Godot version's `gdextension_interface.h` (or against `godot-cpp`, which is
that header plus a large generated binding). This repository does not vendor
Godot, and the Linux CI image has no Godot binary that could load an
extension. Shipping the extension without building it and without a Godot
to open it would be an untested binary interface pinned to a version we did
not choose.

What a later extension would be, if someone has Godot 4.x locally:

- One native library that includes `crates/perfr-api/include/perfr.h` and
  does not link `libperfr`. `perfr_init` still searches `$PERFR_LIB`, then
  beside `perfr`, then beside the game, then the system loader.
- A `RefCounted` (or `Object`) named `Perfr` registered with the engine:
  `init`, `start` / `stop`, `scope` as a `RefCounted` whose destructor calls
  `perfr_stop`, `value`, `instant`, `link`, `now_ns`, `span`, and
  `name_thread` via `pthread_setname_np` (Linux `comm` is 15 bytes;
  `libperfr` has no thread-name call).
- A `.gdextension` file pointing at that library. GDScript would then be
  `var frame = Perfr.scope("frame")` and free it at the end of the block,
  which is as close as GDScript gets to a context manager.

Until that exists, a GDScript game can shell out to a small native helper
linked the same way as the C sample, or call the helper through Godot's
existing native-script path. That is a workaround, not a binding.
