# Working on Sarab

## Comments

Comments live only at the top of a file, never between the lines of code.

- In Rust, that is the `//!` module header. Put no `//` or `///` comments on
  items, inside functions or in tests.
- In shell scripts, YAML and other files, the `#` block right after the
  shebang is the header.
- Whatever a function, constant or test needs explained goes into the file's
  header, which names it in backticks (`native_libs`, `PINS`). Write it as
  prose about the whole file, not as a list of items.
- Files copied from the Android image (`overlay/`, such as `ueventd.rc`) keep
  their upstream comments, so they still diff cleanly against the original.
- When you change code, update the header in the same change so it still
  describes the code.

To check, run this. It must print nothing:

```sh
git grep -n -E '^\s*//([^!]|$)' -- '*.rs'
```
