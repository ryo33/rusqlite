//! [Session Extension](https://sqlite.org/sessionintro.html)
#![expect(non_camel_case_types)]

use std::cell::RefCell;
use std::ffi::{c_char, c_int, c_uchar, c_void, CStr};
use std::io::{Read, Write};
use std::marker::PhantomData;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr;
use std::slice::{from_raw_parts, from_raw_parts_mut};

use fallible_streaming_iterator::FallibleStreamingIterator;

use crate::error::{check, error_from_sqlite_code, Error};
use crate::ffi;
use crate::hooks::Action;
use crate::types::ValueRef;
use crate::{errmsg_to_string, Connection, Name, Result, MAIN_DB};

// https://sqlite.org/session.html

type Filter = Option<Box<RefCell<dyn FnMut(&str) -> bool>>>;

/// An instance of this object is a session that can be
/// used to record changes to a database.
/// Do not access or drop sessions from callbacks on the same connection.
/// Reentrant access panics; reentrant drop leaks the session and its filter
/// to avoid freeing callback state still in use by SQLite.
pub struct Session<'conn> {
    db: &'conn Connection,
    s: *mut ffi::sqlite3_session,
    filter: Filter,
}

impl Session<'_> {
    /// Create a new session object
    #[inline]
    pub fn new(db: &Connection) -> Result<Session<'_>> {
        Session::new_with_name(db, MAIN_DB)
    }

    /// Create a new session object
    #[inline]
    pub fn new_with_name<N: Name>(db: &Connection, name: N) -> Result<Session<'_>> {
        let name = name.as_cstr()?;

        let guard = db.db.borrow_mut();

        let mut s: *mut ffi::sqlite3_session = ptr::null_mut();
        check(unsafe { ffi::sqlite3session_create(guard.db, name.as_ptr(), &mut s) })?;

        Ok(Session {
            db,
            s,
            filter: None,
        })
    }

    fn config(&self, op: c_int, mut value: c_int) -> Result<bool> {
        let _guard = self.db.db.borrow_mut();
        check(unsafe {
            ffi::sqlite3session_object_config(self.s, op, (&mut value as *mut c_int).cast())
        })?;
        Ok(value != 0)
    }

    /// Track tables without an explicit primary key, using their rowid.
    /// Disabled by default. Returns SQLITE_MISUSE after a table is attached.
    pub fn set_rowid_tracking(&mut self, enabled: bool) -> Result<()> {
        self.config(ffi::SQLITE_SESSION_OBJCONFIG_ROWID, enabled.into())
            .map(|_| ())
    }

    /// Query whether tables without an explicit primary key are tracked.
    pub fn is_rowid_tracking_enabled(&self) -> Result<bool> {
        self.config(ffi::SQLITE_SESSION_OBJCONFIG_ROWID, -1)
    }

    /// Enable changeset size estimation before attaching any tables.
    /// Disabled by default. Returns SQLITE_MISUSE after a table is attached.
    pub fn set_size_tracking(&mut self, enabled: bool) -> Result<()> {
        self.config(ffi::SQLITE_SESSION_OBJCONFIG_SIZE, enabled.into())
            .map(|_| ())
    }

    /// Query whether changeset size estimation is enabled.
    pub fn is_size_tracking_enabled(&self) -> Result<bool> {
        self.config(ffi::SQLITE_SESSION_OBJCONFIG_SIZE, -1)
    }

    /// Upper bound on the changeset size in bytes; zero if size tracking is disabled.
    pub fn changeset_size(&self) -> u64 {
        let _guard = self.db.db.borrow_mut();
        unsafe { ffi::sqlite3session_changeset_size(self.s) as u64 }
    }

    /// Heap memory currently used by this session, in bytes.
    pub fn memory_used(&self) -> u64 {
        let _guard = self.db.db.borrow_mut();
        unsafe { ffi::sqlite3session_memory_used(self.s) as u64 }
    }

    /// Set a persistent table filter. Panics skip the table.
    /// The filter must not access the same connection or session. Unlike apply
    /// callbacks, it is retained by SQLite and therefore must be `'static`.
    pub fn table_filter<F>(&mut self, filter: Option<F>)
    where
        F: FnMut(&str) -> bool + 'static,
    {
        let _guard = self.db.db.borrow_mut();
        unsafe extern "C" fn call_boxed_closure<F>(
            p_arg: *mut c_void,
            tbl_str: *const c_char,
        ) -> c_int
        where
            F: FnMut(&str) -> bool,
        {
            unsafe {
                let tbl_name = CStr::from_ptr(tbl_str).to_str();
                c_int::from(
                    callback_result(|| {
                        let filter = &*p_arg.cast::<RefCell<F>>();
                        (filter.borrow_mut())(tbl_name.expect("non-utf8 table name"))
                    })
                    .unwrap_or_default(),
                )
            }
        }

        match filter {
            Some(filter) => {
                let boxed_filter = Box::new(RefCell::new(filter));
                unsafe {
                    ffi::sqlite3session_table_filter(
                        self.s,
                        Some(call_boxed_closure::<F>),
                        &*boxed_filter as *const RefCell<F> as *mut _,
                    );
                }
                self.filter = Some(boxed_filter);
            }
            _ => {
                unsafe { ffi::sqlite3session_table_filter(self.s, None, ptr::null_mut()) }
                self.filter = None;
            }
        }
    }

    /// Attach a table. `None` means all tables.
    pub fn attach<N: Name>(&mut self, table: Option<N>) -> Result<()> {
        let _guard = self.db.db.borrow_mut();
        let cs = table.as_ref().map(N::as_cstr).transpose()?;
        let table = cs.as_ref().map(|s| s.as_ptr()).unwrap_or(ptr::null());
        check(unsafe { ffi::sqlite3session_attach(self.s, table) })
    }

    /// Generate a Changeset
    pub fn changeset(&mut self) -> Result<Changeset> {
        let _guard = self.db.db.borrow_mut();
        let mut n = 0;
        let mut cs: *mut c_void = ptr::null_mut();
        check(unsafe { ffi::sqlite3session_changeset(self.s, &mut n, &mut cs) })?;
        Ok(Changeset { cs, n })
    }

    /// Write the set of changes represented by this session to `output`.
    #[inline]
    pub fn changeset_strm(&mut self, output: &mut dyn Write) -> Result<()> {
        let _guard = self.db.db.borrow_mut();
        let mut output_ref = output;
        check(unsafe {
            ffi::sqlite3session_changeset_strm(
                self.s,
                Some(x_output),
                &mut output_ref as *mut &mut dyn Write as *mut c_void,
            )
        })
    }

    /// Generate a Patchset
    #[inline]
    pub fn patchset(&mut self) -> Result<Changeset> {
        let _guard = self.db.db.borrow_mut();
        let mut n = 0;
        let mut ps: *mut c_void = ptr::null_mut();
        check(unsafe { ffi::sqlite3session_patchset(self.s, &mut n, &mut ps) })?;
        // TODO Validate: same struct
        Ok(Changeset { cs: ps, n })
    }

    /// Write the set of patches represented by this session to `output`.
    #[inline]
    pub fn patchset_strm(&mut self, output: &mut dyn Write) -> Result<()> {
        let _guard = self.db.db.borrow_mut();
        let mut output_ref = output;
        check(unsafe {
            ffi::sqlite3session_patchset_strm(
                self.s,
                Some(x_output),
                &mut output_ref as *mut &mut dyn Write as *mut c_void,
            )
        })
    }

    /// Load the difference between tables.
    pub fn diff<D: Name, N: Name>(&mut self, from: N, table: N) -> Result<()> {
        let _guard = self.db.db.borrow_mut();
        let from = from.as_cstr()?;
        let table = table.as_cstr()?;
        let table = table.as_ptr();
        unsafe {
            let mut errmsg = ptr::null_mut();
            let r =
                ffi::sqlite3session_diff(self.s, from.as_ptr(), table, &mut errmsg as *mut *mut _);
            if r != ffi::SQLITE_OK {
                let errmsg: *mut c_char = errmsg;
                let message = if errmsg.is_null() {
                    None
                } else {
                    Some(errmsg_to_string(&*errmsg))
                };
                ffi::sqlite3_free(errmsg as *mut c_void);
                return Err(error_from_sqlite_code(r, message));
            }
        }
        Ok(())
    }

    /// Test if a changeset has recorded any changes
    #[inline]
    pub fn is_empty(&self) -> bool {
        let _guard = self.db.db.borrow_mut();
        unsafe { ffi::sqlite3session_isempty(self.s) != 0 }
    }

    /// Query the current state of the session
    #[inline]
    pub fn is_enabled(&self) -> bool {
        let _guard = self.db.db.borrow_mut();
        unsafe { ffi::sqlite3session_enable(self.s, -1) != 0 }
    }

    /// Enable or disable the recording of changes
    #[inline]
    pub fn set_enabled(&mut self, enabled: bool) {
        let _guard = self.db.db.borrow_mut();
        unsafe {
            ffi::sqlite3session_enable(self.s, c_int::from(enabled));
        }
    }

    /// Query the current state of the indirect flag
    #[inline]
    pub fn is_indirect(&self) -> bool {
        let _guard = self.db.db.borrow_mut();
        unsafe { ffi::sqlite3session_indirect(self.s, -1) != 0 }
    }

    /// Set or clear the indirect change flag
    #[inline]
    pub fn set_indirect(&mut self, indirect: bool) {
        let _guard = self.db.db.borrow_mut();
        unsafe {
            ffi::sqlite3session_indirect(self.s, c_int::from(indirect));
        }
    }
}

impl Drop for Session<'_> {
    fn drop(&mut self) {
        // A session can be dropped from another callback on this connection.
        // Do not mutate SQLite's session list while SQLite is using it, and do
        // not free a filter whose pointer SQLite still owns. Leak in this
        // unsupported reentrant case instead of risking a use-after-free.
        let Ok(_guard) = self.db.db.try_borrow_mut() else {
            if let Some(filter) = self.filter.take() {
                std::mem::forget(filter);
            }
            return;
        };
        unsafe { ffi::sqlite3session_delete(self.s) };
    }
}

/// Invert a changeset
#[inline]
pub fn invert_strm(input: &mut dyn Read, output: &mut dyn Write) -> Result<()> {
    let mut input_ref = input;
    let mut output_ref = output;
    check(unsafe {
        ffi::sqlite3changeset_invert_strm(
            Some(x_input),
            &mut input_ref as *mut &mut dyn Read as *mut c_void,
            Some(x_output),
            &mut output_ref as *mut &mut dyn Write as *mut c_void,
        )
    })
}

/// Combine two changesets
#[inline]
pub fn concat_strm(
    input_a: &mut dyn Read,
    input_b: &mut dyn Read,
    output: &mut dyn Write,
) -> Result<()> {
    let mut input_a_ref = input_a;
    let mut input_b_ref = input_b;
    let mut output_ref = output;
    check(unsafe {
        ffi::sqlite3changeset_concat_strm(
            Some(x_input),
            &mut input_a_ref as *mut &mut dyn Read as *mut c_void,
            Some(x_input),
            &mut input_b_ref as *mut &mut dyn Read as *mut c_void,
            Some(x_output),
            &mut output_ref as *mut &mut dyn Write as *mut c_void,
        )
    })
}

/// Changeset or Patchset
pub struct Changeset {
    cs: *mut c_void,
    n: c_int,
}

impl Changeset {
    /// Copy serialized bytes using SQLite's allocator.
    /// Format validation is deferred to iteration or application. This accepts
    /// changesets and patchsets, including the empty byte string.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        let n = changeset_len(bytes)?;
        let cs = if bytes.is_empty() {
            ptr::null_mut()
        } else {
            let cs = unsafe { ffi::sqlite3_malloc(n) };
            if cs.is_null() {
                return Err(error_from_sqlite_code(ffi::SQLITE_NOMEM, None));
            }
            unsafe { ptr::copy_nonoverlapping(bytes.as_ptr(), cs.cast(), bytes.len()) };
            cs
        };
        Ok(Self { cs, n })
    }

    /// Borrow the serialized changeset or patchset.
    pub fn as_bytes(&self) -> &[u8] {
        if self.n == 0 {
            &[]
        } else {
            unsafe { from_raw_parts(self.cs.cast(), self.n as usize) }
        }
    }

    /// Copy the serialized changeset or patchset into a vector.
    pub fn to_vec(&self) -> Vec<u8> {
        self.as_bytes().to_vec()
    }

    /// Invert a changeset
    #[inline]
    pub fn invert(&self) -> Result<Changeset> {
        let mut n = 0;
        let mut cs = ptr::null_mut();
        check(unsafe {
            ffi::sqlite3changeset_invert(self.n, self.cs, &mut n, &mut cs as *mut *mut _)
        })?;
        Ok(Changeset { cs, n })
    }

    /// Create an iterator to traverse a changeset
    #[inline]
    pub fn iter(&self) -> Result<ChangesetIter<'_>> {
        let mut it = ptr::null_mut();
        check(unsafe { ffi::sqlite3changeset_start(&mut it as *mut *mut _, self.n, self.cs) })?;
        Ok(ChangesetIter {
            phantom: PhantomData,
            it,
            item: None,
            _input: None,
        })
    }

    /// Concatenate two changeset objects
    #[inline]
    pub fn concat(a: &Changeset, b: &Changeset) -> Result<Changeset> {
        let mut n = 0;
        let mut cs = ptr::null_mut();
        check(unsafe {
            ffi::sqlite3changeset_concat(a.n, a.cs, b.n, b.cs, &mut n, &mut cs as *mut *mut _)
        })?;
        Ok(Changeset { cs, n })
    }
}

impl Drop for Changeset {
    #[inline]
    fn drop(&mut self) {
        unsafe {
            ffi::sqlite3_free(self.cs);
        }
    }
}

/// Cursor for iterating over the elements of a changeset
/// or patchset.
pub struct ChangesetIter<'changeset> {
    phantom: PhantomData<&'changeset Changeset>,
    it: *mut ffi::sqlite3_changeset_iter,
    item: Option<ChangesetItem<'changeset>>,
    // Stable callback context, owned until SQLite finalizes the iterator.
    _input: Option<Box<&'changeset mut dyn Read>>,
}

impl ChangesetIter<'_> {
    /// Create an iterator borrowing `input` until the iterator is dropped.
    ///
    /// ```compile_fail
    /// use rusqlite::session::ChangesetIter;
    /// let mut input = std::io::Cursor::new(Vec::<u8>::new());
    /// let iter = ChangesetIter::start_strm(&mut input).unwrap();
    /// drop(input);
    /// drop(iter);
    /// ```
    #[inline]
    pub fn start_strm<'input>(input: &'input mut dyn Read) -> Result<ChangesetIter<'input>> {
        let mut input = Box::new(input);
        let mut it = ptr::null_mut();
        check(unsafe {
            ffi::sqlite3changeset_start_strm(
                &mut it as *mut *mut _,
                Some(x_input),
                (&mut *input as *mut &mut dyn Read).cast(),
            )
        })?;
        Ok(ChangesetIter {
            phantom: PhantomData,
            it,
            item: None,
            _input: Some(input),
        })
    }
}

impl<'changeset> FallibleStreamingIterator for ChangesetIter<'changeset> {
    type Error = Error;
    type Item = ChangesetItem<'changeset>;

    #[inline]
    fn advance(&mut self) -> Result<()> {
        self.item = None;
        let rc = unsafe { ffi::sqlite3changeset_next(self.it) };
        match rc {
            ffi::SQLITE_ROW => {
                self.item = Some(ChangesetItem {
                    it: self.it,
                    foreign_key_conflict: false,
                    phantom: PhantomData,
                });
                Ok(())
            }
            ffi::SQLITE_DONE => {
                self.item = None;
                Ok(())
            }
            code => Err(error_from_sqlite_code(code, None)),
        }
    }

    #[inline]
    fn get(&self) -> Option<&Self::Item> {
        self.item.as_ref()
    }
}

/// Operation
pub struct Operation<'item> {
    table_name: &'item str,
    number_of_columns: i32,
    code: Action,
    indirect: bool,
}

impl Operation<'_> {
    /// Returns the table name.
    #[inline]
    pub fn table_name(&self) -> &str {
        self.table_name
    }

    /// Returns the number of columns in table
    #[inline]
    pub fn number_of_columns(&self) -> i32 {
        self.number_of_columns
    }

    /// Returns the action code.
    #[inline]
    pub fn code(&self) -> Action {
        self.code
    }

    /// Returns `true` for an 'indirect' change.
    #[inline]
    pub fn indirect(&self) -> bool {
        self.indirect
    }
}

impl Drop for ChangesetIter<'_> {
    #[inline]
    fn drop(&mut self) {
        unsafe {
            ffi::sqlite3changeset_finalize(self.it);
        }
    }
}

/// An item passed to a conflict-handler by
/// [`Connection::apply`](Connection::apply), or an item generated by
/// [`ChangesetIter::next`](ChangesetIter::next).
///
/// A conflict item cannot be saved beyond its callback:
/// ```compile_fail
/// use rusqlite::{Connection, session::{ChangesetItem, Changeset, ConflictAction}};
/// let db = Connection::open_in_memory().unwrap();
/// let cs = Changeset::from_bytes(&[]).unwrap();
/// let mut saved: Option<ChangesetItem<'_>> = None;
/// db.apply(&cs, None::<fn(&str) -> bool>, |_, item| {
///     saved = Some(item);
///     ConflictAction::SQLITE_CHANGESET_ABORT
/// }).unwrap();
/// ```
// TODO enum ? Delete, Insert, Update, ...
pub struct ChangesetItem<'a> {
    foreign_key_conflict: bool,
    phantom: PhantomData<&'a ffi::sqlite3_changeset_iter>,
    it: *mut ffi::sqlite3_changeset_iter,
}

impl ChangesetItem<'_> {
    /// Obtain conflicting row values
    ///
    /// May only be called with an `SQLITE_CHANGESET_DATA` or
    /// `SQLITE_CHANGESET_CONFLICT` conflict handler callback.
    #[inline]
    pub fn conflict(&self, col: usize) -> Result<ValueRef<'_>> {
        if self.foreign_key_conflict {
            return Err(error_from_sqlite_code(ffi::SQLITE_MISUSE, None));
        }
        unsafe {
            let mut p_value: *mut ffi::sqlite3_value = ptr::null_mut();
            check(ffi::sqlite3changeset_conflict(
                self.it,
                col as i32,
                &mut p_value,
            ))?;
            if p_value.is_null() {
                Err(Error::InvalidColumnIndex(col))
            } else {
                Ok(ValueRef::from_value(p_value))
            }
        }
    }

    /// Determine the number of foreign key constraint violations
    ///
    /// May only be called with an `SQLITE_CHANGESET_FOREIGN_KEY` conflict
    /// handler callback.
    #[inline]
    pub fn fk_conflicts(&self) -> Result<i32> {
        unsafe {
            let mut p_out = 0;
            check(ffi::sqlite3changeset_fk_conflicts(self.it, &mut p_out))?;
            Ok(p_out)
        }
    }

    /// Obtain new.* Values
    ///
    /// May only be called if the type of change is either `SQLITE_UPDATE` or
    /// `SQLITE_INSERT`.
    #[inline]
    pub fn new_value(&self, col: usize) -> Result<ValueRef<'_>> {
        if self.foreign_key_conflict {
            return Err(error_from_sqlite_code(ffi::SQLITE_MISUSE, None));
        }
        unsafe {
            let mut p_value: *mut ffi::sqlite3_value = ptr::null_mut();
            check(ffi::sqlite3changeset_new(self.it, col as i32, &mut p_value))?;
            if p_value.is_null() {
                Err(Error::InvalidColumnIndex(col))
            } else {
                Ok(ValueRef::from_value(p_value))
            }
        }
    }

    /// Obtain old.* Values
    ///
    /// May only be called if the type of change is either `SQLITE_DELETE` or
    /// `SQLITE_UPDATE`.
    #[inline]
    pub fn old_value(&self, col: usize) -> Result<ValueRef<'_>> {
        if self.foreign_key_conflict {
            return Err(error_from_sqlite_code(ffi::SQLITE_MISUSE, None));
        }
        unsafe {
            let mut p_value: *mut ffi::sqlite3_value = ptr::null_mut();
            check(ffi::sqlite3changeset_old(self.it, col as i32, &mut p_value))?;
            if p_value.is_null() {
                Err(Error::InvalidColumnIndex(col))
            } else {
                Ok(ValueRef::from_value(p_value))
            }
        }
    }

    /// Obtain the current operation
    #[inline]
    pub fn op(&self) -> Result<Operation<'_>> {
        if self.foreign_key_conflict {
            return Err(error_from_sqlite_code(ffi::SQLITE_MISUSE, None));
        }
        let mut number_of_columns = 0;
        let mut code = 0;
        let mut indirect = 0;
        let tab = unsafe {
            let mut pz_tab: *const c_char = ptr::null();
            check(ffi::sqlite3changeset_op(
                self.it,
                &mut pz_tab,
                &mut number_of_columns,
                &mut code,
                &mut indirect,
            ))?;
            if pz_tab.is_null() {
                return Err(error_from_sqlite_code(ffi::SQLITE_MISUSE, None));
            }
            CStr::from_ptr(pz_tab)
        };
        let table_name = tab.to_str()?;
        Ok(Operation {
            table_name,
            number_of_columns,
            code: Action::from(code),
            indirect: indirect != 0,
        })
    }

    /// Obtain the primary key definition of a table
    #[inline]
    pub fn pk(&self) -> Result<&[u8]> {
        if self.foreign_key_conflict {
            return Err(error_from_sqlite_code(ffi::SQLITE_MISUSE, None));
        }
        let mut number_of_columns = 0;
        unsafe {
            let mut pks: *mut c_uchar = ptr::null_mut();
            check(ffi::sqlite3changeset_pk(
                self.it,
                &mut pks,
                &mut number_of_columns,
            ))?;
            if number_of_columns == 0 {
                return Ok(&[]);
            }
            if pks.is_null() || number_of_columns < 0 {
                return Err(error_from_sqlite_code(ffi::SQLITE_MISUSE, None));
            }
            Ok(from_raw_parts(pks, number_of_columns as usize))
        }
    }
}

/// Used to combine two or more changesets or
/// patchsets
pub struct Changegroup {
    cg: *mut ffi::sqlite3_changegroup,
}

impl Changegroup {
    /// Create a new change group.
    #[inline]
    pub fn new() -> Result<Self> {
        let mut cg = ptr::null_mut();
        check(unsafe { ffi::sqlite3changegroup_new(&mut cg) })?;
        Ok(Changegroup { cg })
    }

    /// Add a changeset
    #[inline]
    pub fn add(&mut self, cs: &Changeset) -> Result<()> {
        check(unsafe { ffi::sqlite3changegroup_add(self.cg, cs.n, cs.cs) })
    }

    /// Add a changeset read from `input` to this change group.
    #[inline]
    pub fn add_stream(&mut self, input: &mut dyn Read) -> Result<()> {
        let mut input_ref = input;
        check(unsafe {
            ffi::sqlite3changegroup_add_strm(
                self.cg,
                Some(x_input),
                &mut input_ref as *mut &mut dyn Read as *mut c_void,
            )
        })
    }

    /// Obtain a composite Changeset
    #[inline]
    pub fn output(&mut self) -> Result<Changeset> {
        let mut n = 0;
        let mut output: *mut c_void = ptr::null_mut();
        check(unsafe { ffi::sqlite3changegroup_output(self.cg, &mut n, &mut output) })?;
        Ok(Changeset { cs: output, n })
    }

    /// Write the combined set of changes to `output`.
    #[inline]
    pub fn output_strm(&mut self, output: &mut dyn Write) -> Result<()> {
        let mut output_ref = output;
        check(unsafe {
            ffi::sqlite3changegroup_output_strm(
                self.cg,
                Some(x_output),
                &mut output_ref as *mut &mut dyn Write as *mut c_void,
            )
        })
    }
}

impl Drop for Changegroup {
    #[inline]
    fn drop(&mut self) {
        unsafe {
            ffi::sqlite3changegroup_delete(self.cg);
        }
    }
}

bitflags::bitflags! {
    /// Options for applying a changeset. No options are enabled by default.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct ApplyFlags: c_int {
        /// Treat foreign key actions as NO ACTION. Foreign key checks remain enabled.
        const FKNOACTION = ffi::SQLITE_CHANGESETAPPLY_FKNOACTION;
        /// Apply the inverse changeset. Not valid for patchsets.
        const INVERT = ffi::SQLITE_CHANGESETAPPLY_INVERT;
        /// Omit SQLite's savepoint. The caller must roll back partial changes on error.
        const NOSAVEPOINT = ffi::SQLITE_CHANGESETAPPLY_NOSAVEPOINT;
        /// Suppress conflicts for changes that would have no effect.
        const IGNORENOOP = ffi::SQLITE_CHANGESETAPPLY_IGNORENOOP;
    }
}

fn changeset_len(bytes: &[u8]) -> Result<c_int> {
    c_int::try_from(bytes.len()).map_err(|_| error_from_sqlite_code(ffi::SQLITE_TOOBIG, None))
}

impl Connection {
    /// Apply a changeset using an internal savepoint and no optional flags.
    /// See [`Self::apply_bytes_with_flags`] for callback and reentrancy rules.
    pub fn apply<F, C>(&self, cs: &Changeset, filter: Option<F>, conflict: C) -> Result<()>
    where
        F: FnMut(&str) -> bool,
        C: for<'a> FnMut(ConflictType, ChangesetItem<'a>) -> ConflictAction,
    {
        self.apply_with_flags(cs, ApplyFlags::empty(), filter, conflict)
    }

    /// Apply a changeset with explicit flags.
    /// See [`Self::apply_bytes_with_flags`] for callback and reentrancy rules.
    pub fn apply_with_flags<F, C>(
        &self,
        cs: &Changeset,
        flags: ApplyFlags,
        filter: Option<F>,
        conflict: C,
    ) -> Result<()>
    where
        F: FnMut(&str) -> bool,
        C: for<'a> FnMut(ConflictType, ChangesetItem<'a>) -> ConflictAction,
    {
        self.apply_bytes_with_flags(cs.as_bytes(), flags, filter, conflict)
    }

    /// Apply borrowed bytes without copying them into a `Changeset`.
    /// SQLite validates the format during application. Rebase output is discarded.
    ///
    /// Callbacks run synchronously and may borrow local state. Conflict items
    /// cannot escape their callback. Do not access this connection from callbacks:
    /// its internal mutable borrow is held throughout application, so reentry
    /// panics. Conflict callback panics abort application; filter panics skip
    /// that table and cause this method to return SQLITE_ABORT (other tables may
    /// have been applied). Use an outer transaction if this must be atomic.
    /// With panic=abort, callback panics terminate the process.
    pub fn apply_bytes_with_flags<F, C>(
        &self,
        bytes: &[u8],
        flags: ApplyFlags,
        filter: Option<F>,
        conflict: C,
    ) -> Result<()>
    where
        F: FnMut(&str) -> bool,
        C: for<'a> FnMut(ConflictType, ChangesetItem<'a>) -> ConflictAction,
    {
        let n = changeset_len(bytes)?;
        let db = self.db.borrow_mut();
        let filtered = filter.is_some();
        let mut ctx = (filter, conflict, false);
        let rc = unsafe {
            ffi::sqlite3changeset_apply_v2(
                db.db,
                n,
                bytes.as_ptr().cast_mut().cast(),
                if filtered {
                    Some(call_filter::<F, C>)
                } else {
                    None
                },
                Some(call_conflict::<F, C>),
                (&mut ctx as *mut (Option<F>, C, bool)).cast(),
                ptr::null_mut(),
                ptr::null_mut(),
                flags.bits(),
            )
        };
        check(if ctx.2 { ffi::SQLITE_ABORT } else { rc })
    }

    /// Apply a streamed changeset without optional flags.
    pub fn apply_strm<F, C>(
        &self,
        input: &mut dyn Read,
        filter: Option<F>,
        conflict: C,
    ) -> Result<()>
    where
        F: FnMut(&str) -> bool,
        C: for<'a> FnMut(ConflictType, ChangesetItem<'a>) -> ConflictAction,
    {
        self.apply_strm_with_flags(input, ApplyFlags::empty(), filter, conflict)
    }

    /// Apply a streamed changeset with explicit flags.
    /// See [`Self::apply_bytes_with_flags`] for callback and reentrancy rules.
    /// Reader panics are translated to SQLITE_IOERR_READ.
    pub fn apply_strm_with_flags<F, C>(
        &self,
        input: &mut dyn Read,
        flags: ApplyFlags,
        filter: Option<F>,
        conflict: C,
    ) -> Result<()>
    where
        F: FnMut(&str) -> bool,
        C: for<'a> FnMut(ConflictType, ChangesetItem<'a>) -> ConflictAction,
    {
        let mut input_ref = input;
        let db = self.db.borrow_mut();
        let filtered = filter.is_some();
        let mut ctx = (filter, conflict, false);
        let rc = unsafe {
            ffi::sqlite3changeset_apply_v2_strm(
                db.db,
                Some(x_input),
                (&mut input_ref as *mut &mut dyn Read).cast(),
                if filtered {
                    Some(call_filter::<F, C>)
                } else {
                    None
                },
                Some(call_conflict::<F, C>),
                (&mut ctx as *mut (Option<F>, C, bool)).cast(),
                ptr::null_mut(),
                ptr::null_mut(),
                flags.bits(),
            )
        };
        check(if ctx.2 { ffi::SQLITE_ABORT } else { rc })
    }
}

/// Constants passed to the conflict handler
/// See [here](https://sqlite.org/session.html#SQLITE_CHANGESET_CONFLICT) for details.
#[allow(missing_docs)]
#[repr(i32)]
#[derive(Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ConflictType {
    UNKNOWN = -1,
    SQLITE_CHANGESET_DATA = ffi::SQLITE_CHANGESET_DATA,
    SQLITE_CHANGESET_NOTFOUND = ffi::SQLITE_CHANGESET_NOTFOUND,
    SQLITE_CHANGESET_CONFLICT = ffi::SQLITE_CHANGESET_CONFLICT,
    SQLITE_CHANGESET_CONSTRAINT = ffi::SQLITE_CHANGESET_CONSTRAINT,
    SQLITE_CHANGESET_FOREIGN_KEY = ffi::SQLITE_CHANGESET_FOREIGN_KEY,
}
impl From<i32> for ConflictType {
    fn from(code: i32) -> ConflictType {
        match code {
            ffi::SQLITE_CHANGESET_DATA => ConflictType::SQLITE_CHANGESET_DATA,
            ffi::SQLITE_CHANGESET_NOTFOUND => ConflictType::SQLITE_CHANGESET_NOTFOUND,
            ffi::SQLITE_CHANGESET_CONFLICT => ConflictType::SQLITE_CHANGESET_CONFLICT,
            ffi::SQLITE_CHANGESET_CONSTRAINT => ConflictType::SQLITE_CHANGESET_CONSTRAINT,
            ffi::SQLITE_CHANGESET_FOREIGN_KEY => ConflictType::SQLITE_CHANGESET_FOREIGN_KEY,
            _ => ConflictType::UNKNOWN,
        }
    }
}

/// Constants returned by the conflict handler
/// See [here](https://sqlite.org/session.html#SQLITE_CHANGESET_ABORT) for details.
#[allow(missing_docs)]
#[repr(i32)]
#[derive(Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ConflictAction {
    SQLITE_CHANGESET_OMIT = ffi::SQLITE_CHANGESET_OMIT,
    SQLITE_CHANGESET_REPLACE = ffi::SQLITE_CHANGESET_REPLACE,
    SQLITE_CHANGESET_ABORT = ffi::SQLITE_CHANGESET_ABORT,
}

// Never drop an arbitrary panic payload on the C side: its destructor may panic.
fn callback_result<T>(f: impl FnOnce() -> T) -> Option<T> {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(value) => Some(value),
        Err(payload) => {
            std::mem::forget(payload);
            None
        }
    }
}

unsafe extern "C" fn call_filter<F, C>(p_ctx: *mut c_void, tbl_str: *const c_char) -> c_int
where
    F: FnMut(&str) -> bool,
    C: for<'a> FnMut(ConflictType, ChangesetItem<'a>) -> ConflictAction,
{
    let ctx = unsafe { &mut *p_ctx.cast::<(Option<F>, C, bool)>() };
    match callback_result(|| {
        let name = unsafe { CStr::from_ptr(tbl_str) }
            .to_str()
            .expect("illegal table name");
        ctx.0.as_mut().is_none_or(|filter| filter(name))
    }) {
        Some(value) => value.into(),
        None => {
            ctx.2 = true;
            0
        }
    }
}

unsafe extern "C" fn call_conflict<F, C>(
    p_ctx: *mut c_void,
    e_conflict: c_int,
    p: *mut ffi::sqlite3_changeset_iter,
) -> c_int
where
    F: FnMut(&str) -> bool,
    C: for<'a> FnMut(ConflictType, ChangesetItem<'a>) -> ConflictAction,
{
    callback_result(|| {
        let ctx = unsafe { &mut *p_ctx.cast::<(Option<F>, C, bool)>() };
        (ctx.1)(
            ConflictType::from(e_conflict),
            ChangesetItem {
                it: p,
                foreign_key_conflict: e_conflict == ffi::SQLITE_CHANGESET_FOREIGN_KEY,
                phantom: PhantomData,
            },
        ) as c_int
    })
    .unwrap_or(ffi::SQLITE_CHANGESET_ABORT)
}

unsafe extern "C" fn x_input(p_in: *mut c_void, data: *mut c_void, len: *mut c_int) -> c_int {
    if p_in.is_null() || len.is_null() {
        return ffi::SQLITE_MISUSE;
    }
    callback_result(|| unsafe {
        if *len < 0 {
            return ffi::SQLITE_MISUSE;
        }
        let bytes = if *len == 0 {
            &mut []
        } else {
            from_raw_parts_mut(data.cast::<u8>(), *len as usize)
        };
        let input = &mut *p_in.cast::<&mut dyn Read>();
        match input.read(bytes) {
            Ok(n) if n <= bytes.len() => {
                *len = n as c_int;
                ffi::SQLITE_OK
            }
            _ => ffi::SQLITE_IOERR_READ,
        }
    })
    .unwrap_or(ffi::SQLITE_IOERR_READ)
}

unsafe extern "C" fn x_output(p_out: *mut c_void, data: *const c_void, len: c_int) -> c_int {
    if p_out.is_null() || len < 0 {
        return ffi::SQLITE_MISUSE;
    }
    callback_result(|| unsafe {
        let bytes = if len == 0 {
            &[]
        } else {
            from_raw_parts(data.cast::<u8>(), len as usize)
        };
        let output = &mut *p_out.cast::<&mut dyn Write>();
        match output.write_all(bytes) {
            Ok(()) => ffi::SQLITE_OK,
            Err(_) => ffi::SQLITE_IOERR_WRITE,
        }
    })
    .unwrap_or(ffi::SQLITE_IOERR_WRITE)
}

#[cfg(all(test, not(miri)))]
mod test {
    #[cfg(all(target_family = "wasm", target_os = "unknown"))]
    use wasm_bindgen_test::wasm_bindgen_test as test;

    use fallible_streaming_iterator::FallibleStreamingIterator as _;
    use std::io::Read;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::{Changeset, ChangesetIter, ConflictAction, ConflictType, Session};
    use crate::hooks::Action;
    use crate::{Connection, Result};

    fn one_changeset_insert() -> Result<Changeset> {
        let db = Connection::open_in_memory()?;
        db.execute_batch("CREATE TABLE foo(t TEXT PRIMARY KEY NOT NULL);")?;

        let mut session = Session::new(&db)?;
        assert!(session.is_empty());

        session.attach::<&str>(None)?;
        db.execute("INSERT INTO foo (t) VALUES (?1);", ["bar"])?;

        session.changeset()
    }

    fn one_changeset_update() -> Result<Changeset> {
        let db = Connection::open_in_memory()?;
        db.execute_batch(
            "CREATE TABLE foo(t TEXT PRIMARY KEY NOT NULL, i INTEGER NOT NULL DEFAULT 0);",
        )?;
        db.execute_batch("INSERT INTO foo (t) VALUES ('bar');")?;

        let mut session = Session::new(&db)?;
        session.attach::<&str>(None)?;
        db.execute("UPDATE foo SET i=100 WHERE t='bar';", [])?;

        session.changeset()
    }

    fn one_changeset_strm() -> Result<Vec<u8>> {
        let db = Connection::open_in_memory()?;
        db.execute_batch("CREATE TABLE foo(t TEXT PRIMARY KEY NOT NULL);")?;

        let mut session = Session::new(&db)?;
        assert!(session.is_empty());

        session.attach::<&str>(None)?;
        db.execute("INSERT INTO foo (t) VALUES (?1);", ["bar"])?;

        let mut output = Vec::new();
        session.changeset_strm(&mut output)?;
        Ok(output)
    }

    #[test]
    fn test_changeset() -> Result<()> {
        let changeset = one_changeset_insert()?;
        let mut iter = changeset.iter()?;
        let item = iter.next()?;
        assert!(item.is_some());

        let item = item.unwrap();
        let op = item.op()?;
        assert_eq!("foo", op.table_name());
        assert_eq!(1, op.number_of_columns());
        assert_eq!(Action::SQLITE_INSERT, op.code());
        assert!(!op.indirect());

        let pk = item.pk()?;
        assert_eq!(&[1], pk);

        let new_value = item.new_value(0)?;
        assert_eq!(Ok("bar"), new_value.as_str());
        Ok(())
    }

    #[test]
    fn test_changeset_strm() -> Result<()> {
        let output = one_changeset_strm()?;
        assert!(!output.is_empty());
        assert_eq!(14, output.len());

        let input: &mut dyn Read = &mut output.as_slice();
        let mut iter = ChangesetIter::start_strm(input)?;
        let item = iter.next()?;
        assert!(item.is_some());
        Ok(())
    }

    #[test]
    fn test_changeset_values() -> Result<()> {
        let changeset = one_changeset_update()?;
        let mut iter = changeset.iter()?;
        let item = iter.next()?.unwrap();

        let new_value = item.new_value(0); // unchanged
        assert_eq!(Err(crate::Error::InvalidColumnIndex(0)), new_value);
        let new_value = item.new_value(1)?; // updated
        assert_eq!(Ok(100), new_value.as_i64());
        Ok(())
    }

    #[test]
    fn test_changeset_apply() -> Result<()> {
        let changeset = one_changeset_insert()?;

        let db = Connection::open_in_memory()?;
        db.execute_batch("CREATE TABLE foo(t TEXT PRIMARY KEY NOT NULL);")?;

        static CALLED: AtomicBool = AtomicBool::new(false);
        db.apply(
            &changeset,
            None::<fn(&str) -> bool>,
            |_conflict_type, _item| {
                CALLED.store(true, Ordering::Relaxed);
                ConflictAction::SQLITE_CHANGESET_OMIT
            },
        )?;

        assert!(!CALLED.load(Ordering::Relaxed));
        let check = db.query_row("SELECT 1 FROM foo WHERE t = ?1", ["bar"], |row| {
            row.get::<_, i32>(0)
        })?;
        assert_eq!(1, check);

        // conflict expected when same changeset applied again on the same db
        db.apply(
            &changeset,
            None::<fn(&str) -> bool>,
            |conflict_type, item| {
                CALLED.store(true, Ordering::Relaxed);
                assert_eq!(ConflictType::SQLITE_CHANGESET_CONFLICT, conflict_type);
                let conflict = item.conflict(0).unwrap();
                assert_eq!(Ok("bar"), conflict.as_str());
                ConflictAction::SQLITE_CHANGESET_OMIT
            },
        )?;
        assert!(CALLED.load(Ordering::Relaxed));
        Ok(())
    }

    #[test]
    fn test_changeset_apply_strm() -> Result<()> {
        let output = one_changeset_strm()?;

        let db = Connection::open_in_memory()?;
        db.execute_batch("CREATE TABLE foo(t TEXT PRIMARY KEY NOT NULL);")?;

        let mut input = output.as_slice();
        db.apply_strm(
            &mut input,
            None::<fn(&str) -> bool>,
            |_conflict_type, _item| ConflictAction::SQLITE_CHANGESET_OMIT,
        )?;

        let check = db.query_row("SELECT 1 FROM foo WHERE t = ?1", ["bar"], |row| {
            row.get::<_, i32>(0)
        })?;
        assert_eq!(1, check);
        Ok(())
    }

    #[test]
    fn test_session_empty() -> Result<()> {
        let db = Connection::open_in_memory()?;
        db.execute_batch("CREATE TABLE foo(t TEXT PRIMARY KEY NOT NULL);")?;

        let mut session = Session::new(&db)?;
        assert!(session.is_empty());

        session.attach::<&str>(None)?;
        db.execute("INSERT INTO foo (t) VALUES (?1);", ["bar"])?;

        assert!(!session.is_empty());
        Ok(())
    }

    #[test]
    fn test_session_set_enabled() -> Result<()> {
        let db = Connection::open_in_memory()?;

        let mut session = Session::new(&db)?;
        assert!(session.is_enabled());
        session.set_enabled(false);
        assert!(!session.is_enabled());
        Ok(())
    }

    #[test]
    fn test_session_set_indirect() -> Result<()> {
        let db = Connection::open_in_memory()?;

        let mut session = Session::new(&db)?;
        assert!(!session.is_indirect());
        session.set_indirect(true);
        assert!(session.is_indirect());
        Ok(())
    }

    #[test]
    fn rowid_crud_and_configuration() -> Result<()> {
        let source = Connection::open_in_memory()?;
        let target = Connection::open_in_memory()?;
        for db in [&source, &target] {
            db.execute_batch("CREATE TABLE t(value TEXT);")?;
        }
        let mut default_session = Session::new(&source)?;
        default_session.attach(Some("t"))?;
        let mut session = Session::new(&source)?;
        assert!(!session.is_rowid_tracking_enabled()?);
        assert!(!session.is_size_tracking_enabled()?);
        session.set_rowid_tracking(true)?;
        session.set_size_tracking(true)?;
        session.attach(Some("t"))?;
        assert!(session.is_rowid_tracking_enabled()?);
        assert!(session.is_size_tracking_enabled()?);
        for result in [
            session.set_rowid_tracking(false),
            session.set_size_tracking(false),
        ] {
            assert_eq!(
                result.unwrap_err().sqlite_error().unwrap().extended_code,
                crate::ffi::SQLITE_MISUSE
            );
        }
        source.execute_batch("INSERT INTO t(rowid, value) VALUES(7, 'a');")?;
        assert!(default_session.changeset()?.as_bytes().is_empty());
        let changes = session.changeset()?;
        assert!(session.changeset_size() >= changes.as_bytes().len() as u64);
        assert!(session.memory_used() > 0);
        target.apply(&changes, None::<fn(&str) -> bool>, |_, _| {
            ConflictAction::SQLITE_CHANGESET_ABORT
        })?;
        drop(session);
        for sql in [
            "UPDATE t SET value='b' WHERE rowid=7",
            "DELETE FROM t WHERE rowid=7",
        ] {
            let mut session = Session::new(&source)?;
            session.set_rowid_tracking(true)?;
            session.attach(Some("t"))?;
            source.execute_batch(sql)?;
            target.apply(&session.changeset()?, None::<fn(&str) -> bool>, |_, _| {
                ConflictAction::SQLITE_CHANGESET_ABORT
            })?;
            let rows = |db: &Connection| -> Result<Vec<(i64, String)>> {
                db.prepare("SELECT rowid, value FROM t ORDER BY rowid")?
                    .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                    .collect()
            };
            assert_eq!(rows(&source)?, rows(&target)?);
        }
        Ok(())
    }

    #[test]
    fn bytes_flags_and_borrowed_callbacks() -> Result<()> {
        use super::ApplyFlags;
        let changes = one_changeset_insert()?;
        let bytes = changes.to_vec();
        let owned = Changeset::from_bytes(&bytes)?;
        assert_eq!(owned.as_bytes(), bytes);
        drop(changes);
        let db = Connection::open_in_memory()?;
        db.execute_batch("CREATE TABLE foo(t TEXT PRIMARY KEY NOT NULL)")?;
        let mut tables = Vec::new();
        db.apply_bytes_with_flags(
            &bytes,
            ApplyFlags::empty(),
            Some(|table: &str| {
                tables.push(table.to_owned());
                true
            }),
            |_, _| ConflictAction::SQLITE_CHANGESET_ABORT,
        )?;
        assert_eq!(tables, ["foo"]);
        let mut conflicts = 0;
        db.apply_with_flags(
            &owned,
            ApplyFlags::IGNORENOOP,
            None::<fn(&str) -> bool>,
            |_, _| {
                conflicts += 1;
                ConflictAction::SQLITE_CHANGESET_ABORT
            },
        )?;
        assert_eq!(conflicts, 0);
        db.apply_strm_with_flags(
            &mut bytes.as_slice(),
            ApplyFlags::INVERT,
            None::<fn(&str) -> bool>,
            |_, _| ConflictAction::SQLITE_CHANGESET_ABORT,
        )?;
        assert_eq!(
            db.query_row("SELECT count(*) FROM foo", [], |r| r.get::<_, i64>(0))?,
            0
        );
        let tx = db.unchecked_transaction()?;
        tx.apply_with_flags(
            &owned,
            ApplyFlags::NOSAVEPOINT,
            None::<fn(&str) -> bool>,
            |_, _| ConflictAction::SQLITE_CHANGESET_ABORT,
        )?;
        tx.rollback()?;
        assert_eq!(
            db.query_row("SELECT count(*) FROM foo", [], |r| r.get::<_, i64>(0))?,
            0
        );
        Ok(())
    }

    #[test]
    fn empty_and_malformed_bytes() -> Result<()> {
        let db = Connection::open_in_memory()?;
        let empty = Changeset::from_bytes(&[])?;
        assert!(empty.as_bytes().is_empty());
        assert!(empty.iter()?.next()?.is_none());
        db.apply(&empty, None::<fn(&str) -> bool>, |_, _| {
            ConflictAction::SQLITE_CHANGESET_ABORT
        })?;
        let malformed = Changeset::from_bytes(b"not a changeset")?;
        assert!(db
            .apply(&malformed, None::<fn(&str) -> bool>, |_, _| {
                ConflictAction::SQLITE_CHANGESET_ABORT
            })
            .is_err());
        let mut iter = malformed.iter()?;
        assert!(iter.advance().is_err());
        assert!(iter.get().is_none());
        Ok(())
    }

    #[test]
    fn foreign_key_actions_and_checks() -> Result<()> {
        use super::ApplyFlags;
        let source = Connection::open_in_memory()?;
        let target = Connection::open_in_memory()?;
        for db in [&source, &target] {
            db.execute_batch("PRAGMA foreign_keys=ON;
                CREATE TABLE parent(id INTEGER PRIMARY KEY);
                CREATE TABLE child(id INTEGER PRIMARY KEY, parent INTEGER REFERENCES parent(id) ON DELETE CASCADE);
                INSERT INTO parent VALUES(1); INSERT INTO child VALUES(1,1);")?;
        }
        let mut session = Session::new(&source)?;
        session.attach::<&str>(None)?;
        source.execute_batch("DELETE FROM parent")?;
        target.apply_with_flags(
            &session.changeset()?,
            ApplyFlags::FKNOACTION,
            None::<fn(&str) -> bool>,
            |_, _| panic!("unexpected conflict"),
        )?;
        assert_eq!(
            target.query_row("SELECT count(*) FROM child", [], |r| r.get::<_, i64>(0))?,
            0
        );
        drop(session);
        source.execute_batch("PRAGMA foreign_keys=OFF")?;
        let mut session = Session::new(&source)?;
        session.attach(Some("child"))?;
        source.execute_batch("INSERT INTO child VALUES(2,999)")?;
        let mut fk_conflicts = 0;
        let result = target.apply_with_flags(
            &session.changeset()?,
            ApplyFlags::FKNOACTION,
            None::<fn(&str) -> bool>,
            |kind, item| {
                assert_eq!(kind, ConflictType::SQLITE_CHANGESET_FOREIGN_KEY);
                assert!(item.op().is_err());
                assert!(item.pk().is_err());
                assert!(item.new_value(0).is_err());
                assert!(item.old_value(0).is_err());
                assert!(item.conflict(0).is_err());
                fk_conflicts += item.fk_conflicts().unwrap();
                ConflictAction::SQLITE_CHANGESET_ABORT
            },
        );
        assert!(result.is_err());
        assert_eq!(fk_conflicts, 1);
        assert_eq!(
            target.query_row("SELECT count(*) FROM child", [], |r| r.get::<_, i64>(0))?,
            0
        );
        Ok(())
    }

    // wasm32-unknown-unknown normally uses panic=abort, which cannot be caught.
    #[cfg(panic = "unwind")]
    #[test]
    fn callback_panics_and_reentry() -> Result<()> {
        struct PanickingIo;
        impl std::io::Read for PanickingIo {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                panic!("read")
            }
        }
        impl std::io::Write for PanickingIo {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                panic!("write")
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let db = Connection::open_in_memory()?;
        db.execute_batch(
            "CREATE TABLE foo(t TEXT PRIMARY KEY NOT NULL); INSERT INTO foo VALUES('bar')",
        )?;
        let changes = one_changeset_insert()?;
        assert!(db
            .apply(&changes, None::<fn(&str) -> bool>, |_, _| panic!(
                "conflict"
            ))
            .is_err());
        assert!(db
            .apply(
                &changes,
                Some(|_: &str| -> bool { panic!("filter") }),
                |_, _| ConflictAction::SQLITE_CHANGESET_ABORT
            )
            .is_err());
        assert!(db
            .apply(&changes, None::<fn(&str) -> bool>, |_, _| {
                db.execute_batch("SELECT 1").unwrap();
                ConflictAction::SQLITE_CHANGESET_OMIT
            })
            .is_err());
        assert!(db
            .apply_strm(&mut PanickingIo, None::<fn(&str) -> bool>, |_, _| {
                ConflictAction::SQLITE_CHANGESET_ABORT
            })
            .is_err());
        assert!(super::invert_strm(&mut changes.as_bytes(), &mut PanickingIo).is_err());
        let mut session = Session::new(&db)?;
        session.attach(Some("foo"))?;
        db.execute_batch("INSERT INTO foo VALUES('other')")?;
        assert!(session.changeset_strm(&mut PanickingIo).is_err());
        // Connection remains usable after the caught callbacks.
        db.execute_batch("SELECT 1")?;
        Ok(())
    }
}
