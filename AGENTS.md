# Foxhound agent guide

Foxhound is the native window substrate. Keep policy, model calls, workflows, recording, and
adapter registries out of this repository; those belong in clients such as Hound.

## Boundaries

- `foxhound-window`: target discovery and window grouping.
- `foxhound-capture`: capture covered target windows without foreground activation.
- `foxhound-input`: post input without moving the physical pointer or taking keyboard focus.
- `foxhound-helper`: localhost HTTP bridge over the three libraries.
- Windows is the implemented backend. Other platforms must fail explicitly rather than pretend to
  work.
- Never add physical input APIs or activate the target as a convenience fallback.

## Verify changes

Run `cargo fmt --all --check` and `cargo test --workspace`. For changes to focus or z-order behavior,
also run a real helper session while actively using a different foreground application.

Do not commit `target/`, credentials, user-specific paths, machine names, personal contact details,
or generated recordings.
