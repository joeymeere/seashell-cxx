/*
 * C ABI for Seashell
 *
 * This header is hand-maintained and must stay in sync with src/lib.rs.
 * Rust panics are caught at the boundary and reported as errors.
 * Handles are not thread-safe; serialize access to each one.
 * All handles are opaque and must be freed with the matching *_free fn.
 * All pubkeys are 32 raw bytes.
 * Status returns: 0 = ok, non-zero = error; call seashell_last_error_copy
 * to read a UTF-8 description of the last error on this thread.
 */

#ifndef SEASHELL_H
#define SEASHELL_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#define SEASHELL_CXX_VERSION "0.1.0"
/* built from this commit since we don't have releases damn you frank */
#define SEASHELL_UPSTREAM_REV "e36678069bfc0a89850ff7c7f686bbf430d97b92"

#ifdef __cplusplus
extern "C" {
#endif

typedef struct SeashellHandle SeashellHandle;
typedef struct SeashellResult SeashellResult;
typedef struct SeashellAccount SeashellAccount;

/* Copy the last thread-local error into `buf`. Returns the full length of
 * the error string. If that exceeds `buf_len`, the buffer is truncated.
 * `buf` may be NULL when `buf_len` is 0 (probe for required size). */
size_t seashell_last_error_copy(uint8_t *buf, size_t buf_len);

/* Static NUL-terminated seashell-cxx version (SEASHELL_CXX_VERSION of the
 * library actually linked). Do not free. */
const uint8_t *seashell_version(void);

/* Pointers in these structs are borrowed for the duration of a call. */
typedef struct {
    const uint8_t *pubkey;
    bool is_signer;
    bool is_writable;
} SeashellAccountMeta;

typedef struct {
    const uint8_t *program_id;
    const SeashellAccountMeta *accounts;
    size_t accounts_len;
    const uint8_t *data;
    size_t data_len;
} SeashellInstruction;

typedef struct {
    const uint8_t *pubkey;
    uint64_t lamports;
    const uint8_t *data; /* may be NULL when data_len is 0 */
    size_t data_len;
    const uint8_t *owner;
    bool executable;
    uint64_t rent_epoch;
} SeashellKeyedAccount;

/* Builtins, SPL Token, Token-2022, Associated Token Account and the
 * precompiles. NULL on panic. */
SeashellHandle *seashell_new(void);
/* Writes back a loaded scenario if anything was fetched into it. */
void seashell_free(SeashellHandle *handle);

/* Store each successful instruction's accounts back. Off by default: every
 * instruction then starts from the same state. */
int32_t seashell_set_memoize(SeashellHandle *handle, bool memoize);
/* Let accounts nobody has start out empty: `local` when not found at all,
 * `fetched` when the RPC reports them missing. Off by default, which makes
 * seashell_process_instruction fail on such an account. */
int32_t seashell_set_allow_uninitialized(SeashellHandle *handle,
                                         bool local, bool fetched);
/* Replace SPL Token with P-Token. */
int32_t seashell_use_p_token(SeashellHandle *handle);

/* Add a program (under BPF Loader 2) from ELF bytes or a .so on disk.
 * `path` is UTF-8, not NUL-terminated. */
int32_t seashell_add_program(SeashellHandle *handle,
                             const uint8_t *program_id,
                             const uint8_t *elf,
                             size_t elf_len);
int32_t seashell_add_program_from_file(SeashellHandle *handle,
                                       const uint8_t *program_id,
                                       const uint8_t *path,
                                       size_t path_len);

/* Load a scenario from `path` (.json.gz, created if missing). Accounts
 * nobody has are fetched from `rpc_url` and recorded into it; `rpc_url`
 * NULL falls back to $RPC_URL, and with neither the scenario is replayed
 * offline. Recorded accounts are written to `path` by seashell_free.
 * Scenario accounts take precedence over ones set locally. */
int32_t seashell_load_scenario(SeashellHandle *handle,
                               const uint8_t *path, size_t path_len,
                               const uint8_t *rpc_url, size_t rpc_url_len);
/* Fetch from `rpc_url` (NULL: $RPC_URL) without recording anything. */
int32_t seashell_load_temporary_scenario(SeashellHandle *handle,
                                         const uint8_t *rpc_url,
                                         size_t rpc_url_len);

/* Returns a new result (free with seashell_result_free) whether the
 * instruction succeeds or fails, or NULL on bad arguments / panic, e.g.
 * an account no one has (see seashell_set_allow_uninitialized). */
SeashellResult *seashell_process_instruction(SeashellHandle *handle,
                                             const SeashellInstruction *ix);

/* Set locally, from the scenario, or fetched over RPC if one is set; NULL
 * if none of those has it. Free with seashell_account_free. */
SeashellAccount *seashell_get_account(const SeashellHandle *handle,
                                      const uint8_t *pubkey);
/* Create or overwrite an account (a sysvar address sets that sysvar). */
int32_t seashell_set_account(SeashellHandle *handle,
                             const SeashellKeyedAccount *account);
/* Add lamports, creating a System-owned account if needed. */
int32_t seashell_airdrop(SeashellHandle *handle, const uint8_t *pubkey,
                         uint64_t lamports);
/* Drop every account that isn't executable. */
int32_t seashell_clear_non_program_accounts(SeashellHandle *handle);

typedef struct {
    uint64_t slot;
    int64_t epoch_start_timestamp;
    uint64_t epoch;
    uint64_t leader_schedule_epoch;
    int64_t unix_timestamp;
} SeashellClock;

int32_t seashell_get_clock(const SeashellHandle *handle, SeashellClock *out);
/* Set Clock's slot and unix timestamp. */
int32_t seashell_warp(SeashellHandle *handle, uint64_t slot,
                      int64_t unix_timestamp);

int32_t seashell_set_compute_unit_limit(SeashellHandle *handle,
                                        uint64_t limit);
uint64_t seashell_get_compute_unit_limit(const SeashellHandle *handle);

uint64_t seashell_minimum_balance_for_rent_exemption(
    const SeashellHandle *handle, size_t data_len);

void seashell_result_free(SeashellResult *handle);

/* 1 = succeeded, 0 = failed, -1 = bad handle. */
int32_t seashell_result_is_ok(const SeashellResult *handle);
uint64_t seashell_result_compute_units(const SeashellResult *handle);

#define SEASHELL_ERR_NONE        0
#define SEASHELL_ERR_INSTRUCTION 1
#define SEASHELL_ERR_TRANSACTION 2 /* never produced: there are no transactions */

/* InstructionError variant indices used most in tests. A program that runs
 * out of compute units fails with PROGRAM_FAILED_TO_COMPLETE. */
#define SEASHELL_IX_ERR_CUSTOM                        25
#define SEASHELL_IX_ERR_COMPUTATIONAL_BUDGET_EXCEEDED 37
#define SEASHELL_IX_ERR_PROGRAM_FAILED_TO_COMPLETE    40

/* Same layout and meaning as litesvm-cxx's LiteSvmTxError and mollusk-cxx's
 * MolluskError. `code` is the InstructionError variant index;
 * `program_error` is the program's u64 error, with Custom(0) as 1 << 32. */
typedef struct {
    int32_t kind;
    uint32_t code;
    uint8_t instruction_index;
    uint64_t program_error;
} SeashellError;

int32_t seashell_result_error_info(const SeashellResult *handle,
                                   SeashellError *out);

/* Probe-then-copy accessors: each returns the full length and copies at
 * most `buf_len` bytes (no NUL). `buf` may be NULL when `buf_len` is 0. */
size_t seashell_result_error_copy(const SeashellResult *handle,
                                  uint8_t *buf, size_t buf_len);
size_t seashell_result_return_data_copy(const SeashellResult *handle,
                                        uint8_t *buf, size_t buf_len);
size_t seashell_result_logs_count(const SeashellResult *handle);
size_t seashell_result_log_copy(const SeashellResult *handle, size_t index,
                                uint8_t *buf, size_t buf_len);

/* Accounts after execution: every account the instruction referenced,
 * none if it failed. Each returned handle is new. */
size_t seashell_result_accounts_count(const SeashellResult *handle);
SeashellAccount *seashell_result_account_at(const SeashellResult *handle,
                                            size_t index,
                                            uint8_t *pubkey_out);
SeashellAccount *seashell_result_get_account(const SeashellResult *handle,
                                             const uint8_t *pubkey);

void seashell_account_free(SeashellAccount *handle);
uint64_t seashell_account_lamports(const SeashellAccount *handle);
int32_t seashell_account_executable(const SeashellAccount *handle);
uint64_t seashell_account_rent_epoch(const SeashellAccount *handle);
int32_t seashell_account_owner(const SeashellAccount *handle, uint8_t *out32);
size_t seashell_account_data_len(const SeashellAccount *handle);
size_t seashell_account_data_copy(const SeashellAccount *handle,
                                  uint8_t *buf, size_t buf_len);
/* Fill `out` to pass this account back in at `pubkey`. Its data and owner
 * pointers borrow from `handle` (valid until it is freed). */
int32_t seashell_account_keyed(const SeashellAccount *handle,
                               const uint8_t *pubkey,
                               SeashellKeyedAccount *out);

/* Public key for a 32-byte ed25519 secret seed. */
int32_t seashell_pubkey_from_seed(const uint8_t *seed32, uint8_t *out32);

/* Canonical program-derived address. `bump_out` may be NULL. */
int32_t seashell_find_program_address(const uint8_t *const *seeds,
                                      const size_t *seed_lens,
                                      size_t seeds_count,
                                      const uint8_t *program_id,
                                      uint8_t *out32,
                                      uint8_t *bump_out);

#ifdef __cplusplus
}
#endif

#endif /* SEASHELL_H */
