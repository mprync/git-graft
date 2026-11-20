# Graft design

Graft creates a standalone repository for each graft. Private refs, the index, reflogs and the object database travel with that graft; integration explicitly transfers the selected commit to the original repository. This permits ordinary Git operations while keeping task histories separate.

On a compatible filesystem, copy-on-write can share verified source files and immutable Git packs without sharing writable file identities. The chosen storage and fallback depend on the source, checkout settings and volume. Dependency sharing is a separate, explicitly trusted operation: the caller attests to the compatibility and relocatability of an installed npm layer.

Retirement archives the repository and its local files. Restoration reopens it; undoing a land appends a reversal commit subject to the destination still matching the recorded result. External processes are outside Graft's coordination and should be closed before retirement. See [operations](operations.md) and [safety](safety.md).
