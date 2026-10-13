# Fork maintenance and session API

This fork starts from upstream `v0.40.2`, commit
`e88f112bef7899234a497baed5cc3c3d553deeb8` (rusqlite 0.40.2,
libsqlite3-sys 0.38.2, bundled SQLite 3.53.2). Package names are unchanged.
Keep session implementation changes separate from CI/dependency policy changes
when merging upstream. Do not regenerate bindings or modify SQLite to implement
these APIs.

## Dependency policy

`.cargo/config.toml` enables Cargo's `min-publish-age` feature and a 14-day
minimum publication age, with incompatible versions denied. CI uses the dated
`nightly-2026-07-12` toolchain; the library itself does not require nightly.
Older stable Cargo versions do not enforce this unstable setting. Use a Cargo
version that supports it for dependency updates.

The policy filters newly resolved registry versions. It does not recheck versions
already in Cargo.lock, cover Git dependencies, or establish that a package is
trustworthy. Registries without publication timestamps cannot enforce the age
filter. Review dependency and checksum changes before accepting a new lockfile.
Do not bypass the age policy automatically, including for security updates.

This fork tracks Cargo.lock. CI uses `--locked`, does not update dependencies,
and does not use remote shell installers, third-party toolchain/cache Actions,
or coverage upload credentials. Checkout is pinned to a full commit and does
not persist credentials. Rust distributions, crates.io, the pinned Action, and
GitHub-hosted runner images remain part of the trusted build infrastructure;
version pinning is not a guarantee against supply-chain attacks.

For consuming applications, pin both packages to the same reviewed **fork commit**, not a branch
or mutable tag, and retain the application's Cargo.lock. For example, substitute the actual
40-character commit for `REVIEWED_FORK_COMMIT`:

```toml
[dependencies]
rusqlite = { git = "https://github.com/ryo33/rusqlite", rev = "REVIEWED_FORK_COMMIT", features = ["bundled", "session"] }

[patch.crates-io]
rusqlite = { git = "https://github.com/ryo33/rusqlite", rev = "REVIEWED_FORK_COMMIT" }
libsqlite3-sys = { git = "https://github.com/ryo33/rusqlite", rev = "REVIEWED_FORK_COMMIT" }
```

Patches must satisfy the consumer's version requirements; a 0.39 requirement
must be migrated to 0.40 separately. Migrate the consuming application independently.
Cargo does not inherit this repository's config or lockfile when it is used as
a dependency: apply the same policy in the consumer's workspace.

## Supported configurations

Native bundled SQLite uses `features = ["bundled", "session"]`, with no bindgen
or custom `LIBSQLITE3_FLAGS` required. The separate `preupdate_hook` Rust API can
also be enabled without bindgen. For wasm32-unknown-unknown, keep the default
`ffi-sqlite-wasm-rs` feature and enable `session`.

CI runs native tests on Linux and macOS, and builds test binaries for iOS device,
iOS simulator, and wasm32-unknown-unknown. Cross-target jobs are build checks,
not runtime tests. Every matrix entry checks that its normal/build dependency
graph has neither bindgen nor buildtime_bindgen enabled. System SQLite is outside
the supported matrix: it requires explicit buildtime_bindgen and a sufficiently
recent SQLite library built with session/preupdate support. Bindgen cannot add
missing extension support to a system library. SQLCipher and in_gecko are not
validated by this fork's CI.

CI also runs Clippy with warnings denied, documentation builds with
`RUSTDOCFLAGS=-Dwarnings`, and Linux session tests under AddressSanitizer.
The sanitizer instruments Rust (including the standard library) and bundled
SQLite C. Leak detection is disabled because callback panic payloads are
intentionally leaked; invalid accesses and stack-use-after-return are checked.
Session tests are excluded under `cfg(miri)`, so Miri is not used as evidence
for the safety of this FFI code.

The smaller CI matrix intentionally omits upstream's Windows, SQLCipher,
loadable-extension, and macros configurations: they are outside this fork's
validated scope. The direct-minimal-versions job is omitted because it changes
the dependency resolution instead of checking the reviewed lockfile. Coverage
uploads and remote installer scripts are omitted to reduce external execution
and credential use. Scheduled runs are omitted; CI runs on master pushes,
pull requests targeting master, and manual dispatch. Work-branch names are not
part of the trigger policy.

## Session behavior and migration

These session APIs intentionally differ from upstream 0.40.2, including breaking
signature changes. The package versions identify the upstream baseline; this
fork is consumed by reviewed Git revision and is not intended to be published
to crates.io under those versions. Reassess versioning before any package release.

- Configure ROWID and SIZE tracking before attaching tables. Both are off by
  default. Changing either after a table has been attached returns SQLite's
  SQLITE_MISUSE. ROWID tracking treats `_rowid_` as the implicit primary key for
  tables without an explicit primary key. `changeset_size` is an upper bound,
  and is zero while size tracking is disabled.
- `apply` and `apply_strm` delegate to the v2 API with empty flags. Their new
  `*_with_flags` equivalents take flags immediately after the input.
  FKNOACTION suppresses foreign-key actions, **not foreign-key checking**.
  Select it explicitly for synchronization. NOSAVEPOINT permits partial changes
  on error; callers must provide and roll back their own transaction.
- `Changeset::from_bytes` copies with SQLite's allocator and checks the C length
  limit. It deliberately defers format validation until iteration/application.
  `as_bytes` borrows and `to_vec` copies. `apply_bytes_with_flags` applies borrowed
  bytes without allocating an intermediate Changeset. Empty bytes are supported.
- Apply filters and conflict handlers accept borrowed `FnMut` closures without
  Send or 'static. `ChangesetItem<'a>` cannot escape a conflict callback; its
  values are borrowed from the item. FOREIGN_KEY conflicts have no row item:
  row accessors return SQLITE_MISUSE, while `fk_conflicts` remains available.
- `ChangesetIter::start_strm` now takes `&mut dyn Read` directly. Its boxed
  callback context owns that borrow until the iterator is finalized. Replace
  `start_strm(&input)` with `start_strm(input)` when input is already `&mut dyn Read`.
- Session operations and apply hold the connection's internal borrow during
  SQLite calls. Callbacks must not access the same connection/session or step
  previously prepared statements on it. Such access can panic. Reentrant session
  destruction conservatively leaks the session/filter rather than freeing state
  SQLite might still use. This is not a supported lifecycle.
- Unwinding panics in conflict handlers abort application. Reader/writer panics
  become SQLITE_IOERR_READ/WRITE. Apply-filter panics skip the table and return
  SQLITE_ABORT after the SQLite call; other tables may already have been applied,
  so use an outer transaction when atomic failure is required. Persistent session
  filter panics skip the table. That filter is stored, so it still requires 'static.
  Panic payloads are intentionally forgotten at the C boundary because their
  destructors could panic again. With panic=abort, panics terminate the process.

References: [session configuration](https://sqlite.org/session/c_session_objconfig_rowid.html),
[apply flags](https://sqlite.org/session/c_changesetapply_fknoaction.html),
[Cargo minimum publication age](https://doc.rust-lang.org/nightly/cargo/reference/config.html#registryglobal-min-publish-age).
