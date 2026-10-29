//! C ABI wrapper around Seashell. Public surface is `include/seashell.h`.
//!
//! ## ABI conventions
//!
//! - All handles are returned as opaque `*mut T`. They must be freed with the
//!   matching `*_free` function. Passing null to a `*_free` fn is allowed and
//!   is a no-op.
//! - Pubkeys are passed as 32-byte buffers (no length param).
//! - Status returns: `0` = ok, non-zero = error. On error the thread-local
//!   error string is set and can be read with [`seashell_last_error_copy`].
//!   Specific non-zero codes are implementation details, not a stable contract.
//! - Rust panics are caught at every FFI boundary and converted to a non-zero
//!   status (or null) plus a descriptive error message. They are never unwound
//!   across the C ABI.
//!
//! ## Other notes
//!
//! - Handles aren't thread-safe.
//! - Callers are responsible for ensuring safety given the bounds of the API.
//!   If there's any ambiguity here, check the safety comment in this file for more info.

#![deny(clippy::all)]
#![warn(unsafe_op_in_unsafe_fn)]

use openssl as _;
use {
    bincode::serialize,
    seashell_core::{
        scenario::Scenario, InstructionProcessingError, InstructionProcessingResult, Seashell,
    },
    solana_account::Account,
    solana_instruction::{error::InstructionError, AccountMeta, Instruction},
    solana_keypair::Keypair,
    solana_program_error::ProgramError,
    solana_pubkey::Pubkey,
    solana_signer::Signer,
    solana_svm_log_collector::LogCollector,
    std::{
        cell::RefCell,
        panic::{catch_unwind, AssertUnwindSafe},
        path::PathBuf,
        ptr, slice,
        str::from_utf8,
    },
};

// todo @joey ok bro

thread_local! {
    static LAST_ERROR: RefCell<String> = const { RefCell::new(String::new()) };
}

fn set_error(msg: impl Into<String>) {
    LAST_ERROR.with(|e| *e.borrow_mut() = msg.into());
}

fn clear_error() {
    LAST_ERROR.with(|e| e.borrow_mut().clear());
}

fn quiet_panics() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        if std::env::var_os("RUST_BACKTRACE").is_none() {
            std::panic::set_hook(Box::new(|_| {}));
        }
    });
}

fn guard<T>(on_panic: T, f: impl FnOnce() -> T) -> T {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(v) => v,
        Err(payload) => {
            let msg = if let Some(s) = payload.downcast_ref::<&str>() {
                (*s).to_string()
            } else if let Some(s) = payload.downcast_ref::<String>() {
                s.clone()
            } else {
                "panic across FFI".to_string()
            };
            set_error(format!("panic: {msg}"));
            on_panic
        }
    }
}

/// # Safety
///
/// If `len > 0`, `ptr` must be aligned for `T`, point to `len` initialized
/// elements, and those elements must not be mutated for the returned lifetime
unsafe fn slice_from_c<'a, T>(ptr: *const T, len: usize, label: &str) -> Option<&'a [T]> {
    if len == 0 {
        Some(&[])
    } else if ptr.is_null() {
        set_error(format!("null {label} pointer"));
        None
    } else {
        // SAFETY: ptr is non-null and caller guarantees the rest of the
        // from_raw_parts contract (see this function's `# Safety`).
        Some(unsafe { slice::from_raw_parts(ptr, len) })
    }
}

/// # Safety
///
/// If `buf_len > 0` and `buf` is non-null, `buf` must be valid for writes of
/// at least `buf_len` bytes
unsafe fn copy_probe(bytes: &[u8], buf: *mut u8, buf_len: usize) -> usize {
    if buf_len > 0 && !buf.is_null() && !bytes.is_empty() {
        let copy_len = bytes.len().min(buf_len);
        // SAFETY: caller guarantees `buf` is writable for `buf_len` bytes;
        // we copy at most `buf_len` of `bytes`.
        unsafe { ptr::copy_nonoverlapping(bytes.as_ptr(), buf, copy_len) };
    }
    bytes.len()
}

/// # Safety
///
/// `p` must be either null or a pointer to at least 32 readable bytes
unsafe fn pubkey_from_ptr(p: *const u8) -> Option<Pubkey> {
    if p.is_null() {
        set_error("null pubkey pointer");
        return None;
    }
    // SAFETY: non-null and 32 readable bytes per caller's contract.
    let bytes = unsafe { slice::from_raw_parts(p, 32) };
    Some(Pubkey::try_from(bytes).expect("32-byte slice always fits Pubkey"))
}

/// # Safety
///
/// `out` must be null or valid for 32 writable bytes.
unsafe fn write_pubkey(key: &Pubkey, out: *mut u8) {
    if !out.is_null() {
        // SAFETY: non-null, 32 writable bytes per caller's contract.
        unsafe { ptr::copy_nonoverlapping(key.as_ref().as_ptr(), out, 32) };
    }
}

/// Copies the last error string into `buf` (up to `buf_len` bytes, no NUL).
/// Returns the *full* length of the error. `buf` may be null if `buf_len` is 0.
///
/// # Safety
///
/// If `buf_len > 0` and `buf` is non-null, `buf` must be valid for writes of
/// `buf_len` bytes.
#[no_mangle]
pub unsafe extern "C" fn seashell_last_error_copy(buf: *mut u8, buf_len: usize) -> usize {
    guard(0, || {
        LAST_ERROR.with(|e| {
            let err = e.borrow();
            // SAFETY: forwarded precondition on (buf, buf_len).
            unsafe { copy_probe(err.as_bytes(), buf, buf_len) }
        })
    })
}

/// Static NUL-terminated mollusk-cxx version. Do not free
#[no_mangle]
pub extern "C" fn seashell_version() -> *const u8 {
    concat!(env!("CARGO_PKG_VERSION"), "\0").as_ptr()
}

/// One account reference in a [`SeashellInstruction`]
#[repr(C)]
pub struct SeashellAccountMeta {
    pub pubkey: *const u8,
    pub is_signer: bool,
    pub is_writable: bool,
}

/// An instruction described with plain C data. Pointers are borrowed for
/// the duration of the call only.
#[repr(C)]
pub struct SeashellInstruction {
    pub program_id: *const u8,
    pub accounts: *const SeashellAccountMeta,
    pub accounts_len: usize,
    pub data: *const u8,
    pub data_len: usize,
}

/// An account and its address, as plain C data. Pointers are borrowed for
/// the duration of the call only.
#[repr(C)]
pub struct SeashellKeyedAccount {
    pub pubkey: *const u8,
    pub lamports: u64,
    pub data: *const u8,
    pub data_len: usize,
    pub owner: *const u8,
    pub executable: bool,
    pub rent_epoch: u64,
}

/// # Safety
///
/// `raw` must satisfy the [`SeashellInstruction`] contract.
unsafe fn instruction_from_c(raw: &SeashellInstruction) -> Option<Instruction> {
    // SAFETY: forwarded caller contract.
    let program_id = unsafe { pubkey_from_ptr(raw.program_id) }?;
    // SAFETY: forwarded caller contract.
    let metas = unsafe { slice_from_c(raw.accounts, raw.accounts_len, "accounts") }?;
    // SAFETY: forwarded caller contract.
    let data = unsafe { slice_from_c(raw.data, raw.data_len, "data") }?;
    let mut accounts = Vec::with_capacity(metas.len());
    for m in metas {
        // SAFETY: forwarded caller contract.
        let key = unsafe { pubkey_from_ptr(m.pubkey) }?;
        accounts.push(if m.is_writable {
            AccountMeta::new(key, m.is_signer)
        } else {
            AccountMeta::new_readonly(key, m.is_signer)
        });
    }
    Some(Instruction {
        program_id,
        accounts,
        data: data.to_vec(),
    })
}

/// # Safety
///
/// `raw` must satisfy the [`SeashellKeyedAccount`] contract.
unsafe fn keyed_account_from_c(raw: &SeashellKeyedAccount) -> Option<(Pubkey, Account)> {
    // SAFETY: forwarded caller contract.
    let key = unsafe { pubkey_from_ptr(raw.pubkey) }?;
    // SAFETY: forwarded caller contract.
    let owner = unsafe { pubkey_from_ptr(raw.owner) }?;
    // SAFETY: forwarded caller contract.
    let data = unsafe { slice_from_c(raw.data, raw.data_len, "account data") }?;
    Some((
        key,
        Account {
            lamports: raw.lamports,
            data: data.to_vec(),
            owner,
            executable: raw.executable,
            rent_epoch: raw.rent_epoch,
        },
    ))
}


pub const SEASHELL_ERR_NONE: i32 = 0;
pub const SEASHELL_ERR_INSTRUCTION: i32 = 1;
pub const SEASHELL_ERR_TRANSACTION: i32 = 2;

/// Same layout as `LiteSvmTxError` in litesvm-cxx and `MolluskError` in mollusk-cxx.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SeashellError {
    pub kind: i32,
    pub code: u32,
    pub instruction_index: u8,
    pub program_error: u64,
}

const NO_ERROR: SeashellError = SeashellError {
    kind: SEASHELL_ERR_NONE,
    code: 0,
    instruction_index: 0,
    program_error: 0,
};

/// bincode encodes an enum as a u32 LE variant index, then its fields.
fn variant_index<T: serde::Serialize>(v: &T) -> u32 {
    serialize(v)
        .ok()
        .and_then(|b| b.get(..4).map(|s| u32::from_le_bytes(s.try_into().unwrap())))
        .unwrap_or(0)
}

fn instruction_error(index: usize, err: &InstructionError) -> SeashellError {
    SeashellError {
        kind: SEASHELL_ERR_INSTRUCTION,
        code: variant_index(err),
        instruction_index: u8::try_from(index).unwrap_or(u8::MAX),
        program_error: ProgramError::try_from(err.clone())
            .map(u64::from)
            .unwrap_or(0),
    }
}

/// The outcome of one [`seashell_process_instruction`] call.
pub struct SeashellResult {
    ok: bool,
    compute_units: u64,
    error: SeashellError,
    error_text: String,
    return_data: Vec<u8>,
    accounts: Vec<(Pubkey, Account)>,
    logs: Vec<String>,
}

impl SeashellResult {
    fn new(r: InstructionProcessingResult, logs: Vec<String>) -> Self {
        let (error, error_text) = match &r.error {
            None => (NO_ERROR, String::new()),
            Some(InstructionProcessingError::InstructionError(e)) => {
                (instruction_error(0, e), format!("{e:?}"))
            }
            Some(e @ InstructionProcessingError::ProgramError) => (
                SeashellError {
                    kind: SEASHELL_ERR_INSTRUCTION,
                    ..NO_ERROR
                },
                format!("{e:?}"),
            ),
        };
        Self {
            ok: error.kind == SEASHELL_ERR_NONE,
            compute_units: r.compute_units_consumed,
            error,
            error_text,
            return_data: r.return_data,
            accounts: r.post_execution_accounts,
            logs,
        }
    }
}

fn boxed(r: SeashellResult) -> *mut SeashellResult {
    Box::into_raw(Box::new(r))
}

/// # Safety
///
/// `h` must be either null or a valid pointer to a `SeashellResult`.
unsafe fn result_ref<'a>(h: *const SeashellResult) -> Option<&'a SeashellResult> {
    if h.is_null() {
        set_error("null result handle");
        None
    } else {
        // SAFETY: non-null and a valid pointee per caller's contract.
        Some(unsafe { &*h })
    }
}

/// Free a result. Null is a no-op.
///
/// # Safety
///
/// `handle` must be null or a pointer from [`seashell_process_instruction`], not yet freed.
#[no_mangle]
pub unsafe extern "C" fn seashell_result_free(handle: *mut SeashellResult) {
    if handle.is_null() {
        return;
    }
    guard((), || {
        // SAFETY: non-null; the pointer came from `Box::into_raw`.
        drop(unsafe { Box::from_raw(handle) });
    });
}

/// 1 if it succeeded, 0 if not, -1 on a bad handle.
///
/// # Safety
///
/// See module-level safety model.
#[no_mangle]
pub unsafe extern "C" fn seashell_result_is_ok(handle: *const SeashellResult) -> i32 {
    guard(-1, || {
        clear_error();
        // SAFETY: forwarded caller contract.
        unsafe { result_ref(handle) }.map_or(-1, |r| r.ok as i32)
    })
}

/// # Safety
///
/// See module-level safety model.
#[no_mangle]
pub unsafe extern "C" fn seashell_result_compute_units(handle: *const SeashellResult) -> u64 {
    guard(0, || {
        clear_error();
        // SAFETY: forwarded caller contract.
        unsafe { result_ref(handle) }.map_or(0, |r| r.compute_units)
    })
}

/// Always 0: Seashell doesn't time executions.
///
/// Fills `out` (kind = SEASHELL_ERR_NONE on success). Returns 0, non-zero on
/// bad arguments.
///
/// # Safety
///
/// See module-level safety model. `out` must be null or valid for writes.
#[no_mangle]
pub unsafe extern "C" fn seashell_result_error_info(
    handle: *const SeashellResult,
    out: *mut SeashellError,
) -> i32 {
    guard(-1, || {
        clear_error();
        // SAFETY: forwarded caller contract.
        let Some(r) = (unsafe { result_ref(handle) }) else {
            return 1;
        };
        if out.is_null() {
            set_error("null error-info out pointer");
            return 2;
        }
        // SAFETY: non-null and valid for writes per caller's contract.
        unsafe { ptr::write_unaligned(out, r.error) };
        0
    })
}

/// Debug text of the error (empty on success), probe-then-copy.
///
/// # Safety
///
/// See module-level safety model and [`copy_probe`].
#[no_mangle]
pub unsafe extern "C" fn seashell_result_error_copy(
    handle: *const SeashellResult,
    buf: *mut u8,
    buf_len: usize,
) -> usize {
    guard(0, || {
        clear_error();
        // SAFETY: forwarded caller contract.
        unsafe { result_ref(handle) }
            // SAFETY: forwarded caller contract.
            .map_or(0, |r| unsafe { copy_probe(r.error_text.as_bytes(), buf, buf_len) })
    })
}

/// Return data, probe-then-copy.
///
/// # Safety
///
/// See module-level safety model and [`copy_probe`].
#[no_mangle]
pub unsafe extern "C" fn seashell_result_return_data_copy(
    handle: *const SeashellResult,
    buf: *mut u8,
    buf_len: usize,
) -> usize {
    guard(0, || {
        clear_error();
        // SAFETY: forwarded caller contract.
        unsafe { result_ref(handle) }
            // SAFETY: forwarded caller contract.
            .map_or(0, |r| unsafe { copy_probe(&r.return_data, buf, buf_len) })
    })
}

/// # Safety
///
/// See module-level safety model.
#[no_mangle]
pub unsafe extern "C" fn seashell_result_logs_count(handle: *const SeashellResult) -> usize {
    guard(0, || {
        clear_error();
        // SAFETY: forwarded caller contract.
        unsafe { result_ref(handle) }.map_or(0, |r| r.logs.len())
    })
}

/// Log line `index`, probe-then-copy (0 when out of range).
///
/// # Safety
///
/// See module-level safety model and [`copy_probe`].
#[no_mangle]
pub unsafe extern "C" fn seashell_result_log_copy(
    handle: *const SeashellResult,
    index: usize,
    buf: *mut u8,
    buf_len: usize,
) -> usize {
    guard(0, || {
        clear_error();
        // SAFETY: forwarded caller contract.
        let Some(r) = (unsafe { result_ref(handle) }) else {
            return 0;
        };
        // SAFETY: forwarded caller contract.
        r.logs
            .get(index)
            .map_or(0, |l| unsafe { copy_probe(l.as_bytes(), buf, buf_len) })
    })
}

/// Number of resulting accounts (every account the instruction touched;
/// none on failure).
///
/// # Safety
///
/// See module-level safety model.
#[no_mangle]
pub unsafe extern "C" fn seashell_result_accounts_count(handle: *const SeashellResult) -> usize {
    guard(0, || {
        clear_error();
        // SAFETY: forwarded caller contract.
        unsafe { result_ref(handle) }.map_or(0, |r| r.accounts.len())
    })
}

/// Resulting account `index` as a new account handle; writes its address to
/// `pubkey_out` (may be null). Null when out of range.
///
/// # Safety
///
/// See module-level safety model. `pubkey_out` must be null or 32 writable bytes.
#[no_mangle]
pub unsafe extern "C" fn seashell_result_account_at(
    handle: *const SeashellResult,
    index: usize,
    pubkey_out: *mut u8,
) -> *mut SeashellAccount {
    guard(ptr::null_mut(), || {
        clear_error();
        // SAFETY: forwarded caller contract.
        let Some(r) = (unsafe { result_ref(handle) }) else {
            return ptr::null_mut();
        };
        let Some((key, acct)) = r.accounts.get(index) else {
            set_error("account index out of range");
            return ptr::null_mut();
        };
        // SAFETY: forwarded caller contract.
        unsafe { write_pubkey(key, pubkey_out) };
        account_box(acct.clone())
    })
}

/// The resulting account at `pubkey`, or null if the result has none.
///
/// # Safety
///
/// See module-level safety model.
#[no_mangle]
pub unsafe extern "C" fn seashell_result_get_account(
    handle: *const SeashellResult,
    pubkey: *const u8,
) -> *mut SeashellAccount {
    guard(ptr::null_mut(), || {
        clear_error();
        // SAFETY: forwarded caller contract.
        let Some(r) = (unsafe { result_ref(handle) }) else {
            return ptr::null_mut();
        };
        // SAFETY: forwarded caller contract.
        let Some(key) = (unsafe { pubkey_from_ptr(pubkey) }) else {
            return ptr::null_mut();
        };
        r.accounts
            .iter()
            .find(|(k, _)| *k == key)
            .map_or(ptr::null_mut(), |(_, a)| account_box(a.clone()))
    })
}

pub struct SeashellHandle {
    inner: Seashell,
}

/// # Safety
///
/// `h` must be either null or a valid pointer to a `SeashellHandle`.
unsafe fn handle_ref<'a>(h: *const SeashellHandle) -> Option<&'a SeashellHandle> {
    if h.is_null() {
        set_error("null handle");
        None
    } else {
        // SAFETY: non-null and a valid pointee per caller's contract.
        Some(unsafe { &*h })
    }
}

/// # Safety
///
/// `h` must be either null or a valid pointer to a `SeashellHandle` with no
/// other live references.
unsafe fn handle_mut<'a>(h: *mut SeashellHandle) -> Option<&'a mut SeashellHandle> {
    if h.is_null() {
        set_error("null handle");
        None
    } else {
        // SAFETY: non-null and a valid, exclusively-owned pointee per caller.
        Some(unsafe { &mut *h })
    }
}

/// # Safety
///
/// `(p, len)` must be readable or `(null, 0)`.
unsafe fn str_from_c<'a>(p: *const u8, len: usize, label: &str) -> Option<&'a str> {
    // SAFETY: forwarded caller contract.
    let raw = unsafe { slice_from_c(p, len, label) }?;
    match from_utf8(raw) {
        Ok(s) => Some(s),
        Err(_) => {
            set_error(format!("{label} is not UTF-8"));
            None
        }
    }
}

/// `rpc_url` if given, else $RPC_URL, as Seashell itself resolves it.
///
/// # Safety
///
/// `(p, len)` must be readable or `(null, 0)`.
unsafe fn rpc_url_from_c(p: *const u8, len: usize) -> Option<Option<String>> {
    if p.is_null() {
        return Some(std::env::var("RPC_URL").ok());
    }
    // SAFETY: forwarded caller contract.
    Some(Some(unsafe { str_from_c(p, len, "rpc url") }?.to_string()))
}

/// Create a Seashell with the builtins, SPL Token, Token-2022, Associated
/// Token Account and the precompiles. Returns null on panic.
#[no_mangle]
pub extern "C" fn seashell_new() -> *mut SeashellHandle {
    quiet_panics();
    guard(ptr::null_mut(), || {
        clear_error();
        let inner = Seashell::new();
        // Seashell installs a global logger echoing every program log at
        // DEBUG; results carry the logs, so keep stderr to errors unless
        // RUST_LOG asks for more.
        solana_logger::setup_with_default("error");
        Box::into_raw(Box::new(SeashellHandle { inner }))
    })
}

/// Free a handle, writing back a loaded scenario if it fetched anything.
/// Null is a no-op.
///
/// # Safety
///
/// `handle` must be null or a pointer from [`seashell_new`], not yet freed.
#[no_mangle]
pub unsafe extern "C" fn seashell_free(handle: *mut SeashellHandle) {
    if handle.is_null() {
        return;
    }
    guard((), || {
        // SAFETY: non-null; the pointer came from `Box::into_raw`.
        drop(unsafe { Box::from_raw(handle) });
    });
}

/// `memoize`: store each successful instruction's accounts back (off by
/// default, so every instruction sees the same starting state).
///
/// # Safety
///
/// See module-level safety model.
#[no_mangle]
pub unsafe extern "C" fn seashell_set_memoize(handle: *mut SeashellHandle, memoize: bool) -> i32 {
    guard(-1, || {
        clear_error();
        // SAFETY: forwarded caller contract.
        let Some(h) = (unsafe { handle_mut(handle) }) else {
            return 1;
        };
        h.inner.config.memoize = memoize;
        0
    })
}

/// Whether an account an instruction references but nobody has may start
/// out empty: `local` for accounts not found at all, `fetched` for ones the
/// RPC reports as missing. Both off by default (a missing account panics,
/// which the instruction call reports as an error).
///
/// # Safety
///
/// See module-level safety model.
#[no_mangle]
pub unsafe extern "C" fn seashell_set_allow_uninitialized(
    handle: *mut SeashellHandle,
    local: bool,
    fetched: bool,
) -> i32 {
    guard(-1, || {
        clear_error();
        // SAFETY: forwarded caller contract.
        let Some(h) = (unsafe { handle_mut(handle) }) else {
            return 1;
        };
        h.inner.config.allow_uninitialized_accounts_local = local;
        h.inner.config.allow_uninitialized_accounts_fetched = fetched;
        0
    })
}

/// Replace SPL Token with P-Token.
///
/// # Safety
///
/// See module-level safety model.
#[no_mangle]
pub unsafe extern "C" fn seashell_use_p_token(handle: *mut SeashellHandle) -> i32 {
    guard(-1, || {
        clear_error();
        // SAFETY: forwarded caller contract.
        let Some(h) = (unsafe { handle_mut(handle) }) else {
            return 1;
        };
        h.inner.use_p_token();
        0
    })
}

/// Add a program from ELF bytes (under BPF Loader 2, as Seashell does).
///
/// # Safety
///
/// `program_id` must point to 32 bytes; `(elf, elf_len)` must be readable.
#[no_mangle]
pub unsafe extern "C" fn seashell_add_program(
    handle: *mut SeashellHandle,
    program_id: *const u8,
    elf: *const u8,
    elf_len: usize,
) -> i32 {
    guard(-1, || {
        clear_error();
        // SAFETY: forwarded caller contract.
        let Some(h) = (unsafe { handle_mut(handle) }) else {
            return 1;
        };
        // SAFETY: forwarded caller contract.
        let Some(id) = (unsafe { pubkey_from_ptr(program_id) }) else {
            return 2;
        };
        // SAFETY: forwarded caller contract.
        let Some(bytes) = (unsafe { slice_from_c(elf, elf_len, "elf") }) else {
            return 3;
        };
        if bytes.is_empty() {
            set_error("empty elf");
            return 3;
        }
        h.inner.load_program_from_bytes(id, bytes);
        0
    })
}

/// Add a program from a `.so` file on disk. `path` is UTF-8 and not
/// NUL-terminated.
///
/// # Safety
///
/// `program_id` must point to 32 bytes; `(path, path_len)` must be readable.
#[no_mangle]
pub unsafe extern "C" fn seashell_add_program_from_file(
    handle: *mut SeashellHandle,
    program_id: *const u8,
    path: *const u8,
    path_len: usize,
) -> i32 {
    guard(-1, || {
        clear_error();
        // SAFETY: forwarded caller contract.
        let Some(h) = (unsafe { handle_mut(handle) }) else {
            return 1;
        };
        // SAFETY: forwarded caller contract.
        let Some(id) = (unsafe { pubkey_from_ptr(program_id) }) else {
            return 2;
        };
        // SAFETY: forwarded caller contract.
        let Some(path) = (unsafe { str_from_c(path, path_len, "path") }) else {
            return 3;
        };
        let elf = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) => {
                set_error(format!("{path}: {e}"));
                return 4;
            }
        };
        h.inner.load_program_from_bytes(id, &elf);
        0
    })
}

/// Load a scenario from `path` (a `.json.gz`; created when missing), fetching
/// accounts nobody has from `rpc_url` and recording them into it. `rpc_url`
/// NULL falls back to $RPC_URL; with neither, the scenario is replayed
/// offline. Fetched accounts are written to `path` when the handle is freed.
///
/// Unlike Seashell's `load_scenario`, the path is given directly rather than
/// resolved under the cargo workspace's `scenarios/`.
///
/// # Safety
///
/// `(path, path_len)` must be readable; `(rpc_url, rpc_url_len)` must be
/// readable or `rpc_url` NULL.
#[no_mangle]
pub unsafe extern "C" fn seashell_load_scenario(
    handle: *mut SeashellHandle,
    path: *const u8,
    path_len: usize,
    rpc_url: *const u8,
    rpc_url_len: usize,
) -> i32 {
    guard(-1, || {
        clear_error();
        // SAFETY: forwarded caller contract.
        let Some(h) = (unsafe { handle_mut(handle) }) else {
            return 1;
        };
        // SAFETY: forwarded caller contract.
        let Some(path) = (unsafe { str_from_c(path, path_len, "path") }) else {
            return 2;
        };
        // SAFETY: forwarded caller contract.
        let Some(rpc) = (unsafe { rpc_url_from_c(rpc_url, rpc_url_len) }) else {
            return 3;
        };
        let path = PathBuf::from(path);
        let fetched = h.inner.config.allow_uninitialized_accounts_fetched;
        h.inner.accounts_db.scenario = match rpc {
            Some(url) => Scenario::from_file_with_rpc(path, url, fetched),
            None => Scenario::from_file(path, fetched),
        };
        0
    })
}

/// Fetch accounts nobody has from `rpc_url` (NULL: $RPC_URL) without
/// recording them anywhere.
///
/// # Safety
///
/// `(rpc_url, rpc_url_len)` must be readable or `rpc_url` NULL.
#[no_mangle]
pub unsafe extern "C" fn seashell_load_temporary_scenario(
    handle: *mut SeashellHandle,
    rpc_url: *const u8,
    rpc_url_len: usize,
) -> i32 {
    guard(-1, || {
        clear_error();
        // SAFETY: forwarded caller contract.
        let Some(h) = (unsafe { handle_mut(handle) }) else {
            return 1;
        };
        // SAFETY: forwarded caller contract.
        let Some(rpc) = (unsafe { rpc_url_from_c(rpc_url, rpc_url_len) }) else {
            return 2;
        };
        let Some(url) = rpc else {
            set_error("no rpc url given and RPC_URL is not set");
            return 3;
        };
        let fetched = h.inner.config.allow_uninitialized_accounts_fetched;
        h.inner.accounts_db.scenario = Scenario::rpc_only(url, fetched);
        0
    })
}

/// Process one instruction. Returns a new result (free with
/// [`seashell_result_free`]) whether it succeeds or fails, or null on bad
/// arguments / panic (e.g. an account no one has, see
/// [`seashell_set_allow_uninitialized`]).
///
/// # Safety
///
/// See module-level safety model and [`SeashellInstruction`].
#[no_mangle]
pub unsafe extern "C" fn seashell_process_instruction(
    handle: *mut SeashellHandle,
    ix: *const SeashellInstruction,
) -> *mut SeashellResult {
    guard(ptr::null_mut(), || {
        clear_error();
        // SAFETY: forwarded caller contract.
        let Some(h) = (unsafe { handle_mut(handle) }) else {
            return ptr::null_mut();
        };
        if ix.is_null() {
            set_error("null instruction");
            return ptr::null_mut();
        }
        // SAFETY: non-null; forwarded caller contract.
        let Some(ix) = (unsafe { instruction_from_c(&*ix) }) else {
            return ptr::null_mut();
        };
        let logger = LogCollector::new_ref();
        h.inner.log_collector = Some(logger.clone());
        let r = catch_unwind(AssertUnwindSafe(|| h.inner.process_instruction(ix)));
        h.inner.log_collector = None;
        let logs = logger.borrow().get_recorded_content().to_vec();
        match r {
            Ok(r) => boxed(SeashellResult::new(r, logs)),
            Err(p) => std::panic::resume_unwind(p),
        }
    })
}

/// The account at `pubkey`: set locally, from the scenario, or fetched over
/// RPC when one is configured. Null if none of those has it.
///
/// # Safety
///
/// See module-level safety model.
#[no_mangle]
pub unsafe extern "C" fn seashell_get_account(
    handle: *const SeashellHandle,
    pubkey: *const u8,
) -> *mut SeashellAccount {
    guard(ptr::null_mut(), || {
        clear_error();
        // SAFETY: forwarded caller contract.
        let Some(h) = (unsafe { handle_ref(handle) }) else {
            return ptr::null_mut();
        };
        // SAFETY: forwarded caller contract.
        let Some(key) = (unsafe { pubkey_from_ptr(pubkey) }) else {
            return ptr::null_mut();
        };
        let db = &h.inner.accounts_db;
        let found = db.account_maybe(&key).or_else(|| {
            if db.scenario.rpc_enabled() {
                db.scenario.try_fetch_from_rpc(&key)
            } else {
                None
            }
        });
        match found {
            Some(a) => account_box(Account::from(a)),
            None => {
                set_error(format!("no account at {key}"));
                ptr::null_mut()
            }
        }
    })
}

/// Create or overwrite an account (a sysvar address sets that sysvar).
///
/// # Safety
///
/// See module-level safety model and [`SeashellKeyedAccount`].
#[no_mangle]
pub unsafe extern "C" fn seashell_set_account(
    handle: *mut SeashellHandle,
    account: *const SeashellKeyedAccount,
) -> i32 {
    guard(-1, || {
        clear_error();
        // SAFETY: forwarded caller contract.
        let Some(h) = (unsafe { handle_mut(handle) }) else {
            return 1;
        };
        if account.is_null() {
            set_error("null account pointer");
            return 2;
        }
        // SAFETY: non-null; forwarded caller contract.
        let Some((key, acct)) = (unsafe { keyed_account_from_c(&*account) }) else {
            return 3;
        };
        h.inner.set_account(key, acct);
        0
    })
}

/// Add lamports to `pubkey`, creating a System-owned account if needed.
///
/// # Safety
///
/// See module-level safety model.
#[no_mangle]
pub unsafe extern "C" fn seashell_airdrop(
    handle: *mut SeashellHandle,
    pubkey: *const u8,
    lamports: u64,
) -> i32 {
    guard(-1, || {
        clear_error();
        // SAFETY: forwarded caller contract.
        let Some(h) = (unsafe { handle_mut(handle) }) else {
            return 1;
        };
        // SAFETY: forwarded caller contract.
        let Some(key) = (unsafe { pubkey_from_ptr(pubkey) }) else {
            return 2;
        };
        h.inner.airdrop(key, lamports);
        0
    })
}

/// Drop every account that isn't executable.
///
/// # Safety
///
/// See module-level safety model.
#[no_mangle]
pub unsafe extern "C" fn seashell_clear_non_program_accounts(handle: *mut SeashellHandle) -> i32 {
    guard(-1, || {
        clear_error();
        // SAFETY: forwarded caller contract.
        let Some(h) = (unsafe { handle_mut(handle) }) else {
            return 1;
        };
        h.inner.clear_non_program_accounts();
        0
    })
}

/// Set Clock's slot and unix timestamp.
///
/// # Safety
///
/// See module-level safety model.
#[no_mangle]
pub unsafe extern "C" fn seashell_warp(
    handle: *mut SeashellHandle,
    slot: u64,
    unix_timestamp: i64,
) -> i32 {
    guard(-1, || {
        clear_error();
        // SAFETY: forwarded caller contract.
        let Some(h) = (unsafe { handle_mut(handle) }) else {
            return 1;
        };
        h.inner.accounts_db.warp(slot, unix_timestamp);
        0
    })
}

#[repr(C)]
pub struct SeashellClock {
    pub slot: u64,
    pub epoch_start_timestamp: i64,
    pub epoch: u64,
    pub leader_schedule_epoch: u64,
    pub unix_timestamp: i64,
}

/// # Safety
///
/// See module-level safety model. `out` must be null or valid for writes.
#[no_mangle]
pub unsafe extern "C" fn seashell_get_clock(
    handle: *const SeashellHandle,
    out: *mut SeashellClock,
) -> i32 {
    guard(-1, || {
        clear_error();
        // SAFETY: forwarded caller contract.
        let Some(h) = (unsafe { handle_ref(handle) }) else {
            return 1;
        };
        if out.is_null() {
            set_error("null clock out pointer");
            return 2;
        }
        let c = h.inner.accounts_db.sysvars.clock();
        let value = SeashellClock {
            slot: c.slot,
            epoch_start_timestamp: c.epoch_start_timestamp,
            epoch: c.epoch,
            leader_schedule_epoch: c.leader_schedule_epoch,
            unix_timestamp: c.unix_timestamp,
        };
        // SAFETY: non-null and valid for writes per caller's contract.
        unsafe { ptr::write_unaligned(out, value) };
        0
    })
}

/// # Safety
///
/// See module-level safety model.
#[no_mangle]
pub unsafe extern "C" fn seashell_set_compute_unit_limit(
    handle: *mut SeashellHandle,
    limit: u64,
) -> i32 {
    guard(-1, || {
        clear_error();
        // SAFETY: forwarded caller contract.
        let Some(h) = (unsafe { handle_mut(handle) }) else {
            return 1;
        };
        h.inner.compute_budget.compute_unit_limit = limit;
        0
    })
}

/// # Safety
///
/// See module-level safety model.
#[no_mangle]
pub unsafe extern "C" fn seashell_get_compute_unit_limit(handle: *const SeashellHandle) -> u64 {
    guard(0, || {
        clear_error();
        // SAFETY: forwarded caller contract.
        unsafe { handle_ref(handle) }.map_or(0, |h| h.inner.compute_budget.compute_unit_limit)
    })
}

/// # Safety
///
/// See module-level safety model.
#[no_mangle]
pub unsafe extern "C" fn seashell_minimum_balance_for_rent_exemption(
    handle: *const SeashellHandle,
    data_len: usize,
) -> u64 {
    guard(0, || {
        clear_error();
        // SAFETY: forwarded caller contract.
        unsafe { handle_ref(handle) }
            .map_or(0, |h| h.inner.accounts_db.sysvars.rent().minimum_balance(data_len))
    })
}

pub struct SeashellAccount {
    inner: Account,
}

fn account_box(a: Account) -> *mut SeashellAccount {
    Box::into_raw(Box::new(SeashellAccount { inner: a }))
}

/// # Safety
///
/// `h` must be either null or a valid pointer to a `SeashellAccount`.
unsafe fn account_ref<'a>(h: *const SeashellAccount) -> Option<&'a SeashellAccount> {
    if h.is_null() {
        set_error("null account handle");
        None
    } else {
        // SAFETY: non-null, valid pointee per caller's contract.
        Some(unsafe { &*h })
    }
}

/// Free an account handle. Null is a no-op.
///
/// # Safety
///
/// `handle` must be null or a pointer from a fn returning `SeashellAccount *`,
/// not yet freed.
#[no_mangle]
pub unsafe extern "C" fn seashell_account_free(handle: *mut SeashellAccount) {
    if handle.is_null() {
        return;
    }
    guard((), || {
        // SAFETY: non-null and the pointer came from `Box::into_raw`.
        drop(unsafe { Box::from_raw(handle) });
    });
}

/// # Safety
///
/// See module-level safety model.
#[no_mangle]
pub unsafe extern "C" fn seashell_account_lamports(handle: *const SeashellAccount) -> u64 {
    guard(0, || {
        clear_error();
        // SAFETY: forwarded caller contract.
        unsafe { account_ref(handle) }.map_or(0, |a| a.inner.lamports)
    })
}

/// 1 if executable, 0 if not, -1 on a bad handle.
///
/// # Safety
///
/// See module-level safety model.
#[no_mangle]
pub unsafe extern "C" fn seashell_account_executable(handle: *const SeashellAccount) -> i32 {
    guard(-1, || {
        clear_error();
        // SAFETY: forwarded caller contract.
        unsafe { account_ref(handle) }.map_or(-1, |a| a.inner.executable as i32)
    })
}

/// # Safety
///
/// See module-level safety model.
#[no_mangle]
pub unsafe extern "C" fn seashell_account_rent_epoch(handle: *const SeashellAccount) -> u64 {
    guard(0, || {
        clear_error();
        // SAFETY: forwarded caller contract.
        unsafe { account_ref(handle) }.map_or(0, |a| a.inner.rent_epoch)
    })
}

/// Writes the owner to `out32`.
///
/// # Safety
///
/// See module-level safety model. `out32` must be 32 writable bytes.
#[no_mangle]
pub unsafe extern "C" fn seashell_account_owner(handle: *const SeashellAccount, out32: *mut u8) -> i32 {
    guard(-1, || {
        clear_error();
        // SAFETY: forwarded caller contract.
        let Some(a) = (unsafe { account_ref(handle) }) else {
            return 1;
        };
        if out32.is_null() {
            set_error("null out pointer");
            return 2;
        }
        // SAFETY: non-null, 32 writable bytes.
        unsafe { write_pubkey(&a.inner.owner, out32) };
        0
    })
}

/// # Safety
///
/// See module-level safety model.
#[no_mangle]
pub unsafe extern "C" fn seashell_account_data_len(handle: *const SeashellAccount) -> usize {
    guard(0, || {
        clear_error();
        // SAFETY: forwarded caller contract.
        unsafe { account_ref(handle) }.map_or(0, |a| a.inner.data.len())
    })
}

/// Account data, probe-then-copy.
///
/// # Safety
///
/// See module-level safety model and [`copy_probe`].
#[no_mangle]
pub unsafe extern "C" fn seashell_account_data_copy(
    handle: *const SeashellAccount,
    buf: *mut u8,
    buf_len: usize,
) -> usize {
    guard(0, || {
        clear_error();
        // SAFETY: forwarded caller contract.
        unsafe { account_ref(handle) }
            // SAFETY: forwarded caller contract.
            .map_or(0, |a| unsafe { copy_probe(&a.inner.data, buf, buf_len) })
    })
}

/// Describe `handle` as a [`SeashellKeyedAccount`] at `pubkey`, to pass back
/// into [`seashell_set_account`]. `out`'s data and owner pointers borrow from
/// `handle` and stay valid until it is freed.
///
/// # Safety
///
/// See module-level safety model. `pubkey` must stay valid as long as `out`
/// is used; `out` must be valid for writes.
#[no_mangle]
pub unsafe extern "C" fn seashell_account_keyed(
    handle: *const SeashellAccount,
    pubkey: *const u8,
    out: *mut SeashellKeyedAccount,
) -> i32 {
    guard(-1, || {
        clear_error();
        // SAFETY: forwarded caller contract.
        let Some(a) = (unsafe { account_ref(handle) }) else {
            return 1;
        };
        if pubkey.is_null() || out.is_null() {
            set_error("null pubkey or out pointer");
            return 2;
        }
        let value = SeashellKeyedAccount {
            pubkey,
            lamports: a.inner.lamports,
            data: a.inner.data.as_ptr(),
            data_len: a.inner.data.len(),
            owner: a.inner.owner.as_ref().as_ptr(),
            executable: a.inner.executable,
            rent_epoch: a.inner.rent_epoch,
        };
        // SAFETY: non-null and valid for writes per caller's contract.
        unsafe { ptr::write_unaligned(out, value) };
        0
    })
}

/// Writes the ed25519 public key for a 32-byte secret seed to `out32`.
///
/// # Safety
///
/// `seed32` must point to 32 readable bytes, `out32` to 32 writable bytes.
#[no_mangle]
pub unsafe extern "C" fn seashell_pubkey_from_seed(seed32: *const u8, out32: *mut u8) -> i32 {
    guard(-1, || {
        clear_error();
        if seed32.is_null() {
            set_error("null seed pointer");
            return 1;
        }
        if out32.is_null() {
            set_error("null out pointer");
            return 2;
        }
        // SAFETY: non-null, 32 readable bytes per caller's contract.
        let seed = unsafe { *(seed32 as *const [u8; 32]) };
        let pk = Keypair::new_from_array(seed).pubkey();
        // SAFETY: non-null, 32 writable bytes.
        unsafe { write_pubkey(&pk, out32) };
        0
    })
}

/// Derives the canonical PDA for `seeds` under `program_id`. Writes the
/// address to `out32` and the bump to `bump_out` (may be null).
///
/// # Safety
///
/// `seeds[i]` must point to `seed_lens[i]` readable bytes for each
/// `i < seeds_count`; `program_id` to 32 readable bytes; `out32` to 32
/// writable bytes.
#[no_mangle]
pub unsafe extern "C" fn seashell_find_program_address(
    seeds: *const *const u8,
    seed_lens: *const usize,
    seeds_count: usize,
    program_id: *const u8,
    out32: *mut u8,
    bump_out: *mut u8,
) -> i32 {
    guard(-1, || {
        clear_error();
        // SAFETY: forwarded caller contract.
        let Some(pid) = (unsafe { pubkey_from_ptr(program_id) }) else {
            return 1;
        };
        if out32.is_null() {
            set_error("null out pointer");
            return 2;
        }
        // SAFETY: forwarded caller contract; helper tolerates (null, 0).
        let (Some(ptrs), Some(lens)) = (unsafe {
            (
                slice_from_c(seeds, seeds_count, "seeds"),
                slice_from_c(seed_lens, seeds_count, "seed lengths"),
            )
        }) else {
            return 3;
        };
        let mut parts: Vec<&[u8]> = Vec::with_capacity(seeds_count);
        for (&p, &n) in ptrs.iter().zip(lens) {
            // SAFETY: each seed is valid for `n` bytes per caller contract.
            let Some(s) = (unsafe { slice_from_c(p, n, "seed") }) else {
                return 3;
            };
            parts.push(s);
        }
        let Some((addr, bump)) = Pubkey::try_find_program_address(&parts, &pid) else {
            set_error("no viable bump for these seeds");
            return 4;
        };
        // SAFETY: `out32` non-null, room for 32 bytes.
        unsafe { write_pubkey(&addr, out32) };
        if !bump_out.is_null() {
            // SAFETY: non-null, valid for a 1-byte write.
            unsafe { ptr::write(bump_out, bump) };
        }
        0
    })
}
