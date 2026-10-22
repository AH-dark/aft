The macOS worker's private `mktemp` shim is now published only after its staged executable is closed, preventing a child launch from racing an in-place write.
