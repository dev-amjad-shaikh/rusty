# Rusty local capability pack

This packaged document proves that an agent can read a workspace-scoped file
without receiving arbitrary filesystem access. The reader canonicalizes every
requested path, refuses traversal and symlink escapes, accepts only supported
text document formats, and enforces a response-size boundary.

The same run records the exact file request and returned bytes in its journal.
