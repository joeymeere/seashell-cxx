/*
 * ensure the C ABI works with a simple Caravel program (in this case, the counter example)
 *
 * With SEASHELL_SMOKE_RPC_URL set, also records a scenario from that RPC and
 * replays it offline.
 */
#include "seashell.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static int failures;

#define CHECK(cond)                                                            \
    do {                                                                       \
        if (!(cond)) {                                                         \
            fprintf(stderr, "%s:%d: CHECK(%s) failed\n", __FILE__, __LINE__,   \
                    #cond);                                                    \
            failures++;                                                        \
        }                                                                      \
    } while (0)

static void die_last_error(const char *what) {
    char buf[512] = {0};
    seashell_last_error_copy((uint8_t *)buf, sizeof buf - 1);
    fprintf(stderr, "%s: %s\n", what, buf);
    exit(1);
}

static uint8_t program_id[32], system_program[32], payer[32], counter[32];

static SeashellResult *run(SeashellHandle *s, uint8_t disc,
                           SeashellAccountMeta *metas, size_t n) {
    SeashellInstruction ix = {program_id, metas, n, &disc, 1};
    SeashellResult *r = seashell_process_instruction(s, &ix);
    if (!r) die_last_error("process_instruction");
    return r;
}

static uint64_t read_count(SeashellAccount *a) {
    if (!a) return UINT64_MAX;
    uint8_t data[40] = {0};
    seashell_account_data_copy(a, data, sizeof data);
    seashell_account_free(a);
    uint64_t v;
    memcpy(&v, data, 8);
    return v;
}

static void expect_error(SeashellResult *r, uint32_t code, uint64_t program_error) {
    SeashellError err;
    CHECK(seashell_result_is_ok(r) == 0);
    CHECK(seashell_result_error_info(r, &err) == 0);
    CHECK(err.kind == SEASHELL_ERR_INSTRUCTION);
    CHECK(err.code == code);
    CHECK(err.program_error == program_error);
    CHECK(seashell_result_error_copy(r, NULL, 0) > 0);
    CHECK(seashell_result_accounts_count(r) == 0);
    seashell_result_free(r);
}

/* record an account from RPC into a scenario, then replay it offline */
static void scenario_roundtrip(const char *rpc) {
    /* USDC mint, owned by SPL Token */
    static const uint8_t usdc[32] = {
        198, 250, 122, 243, 190, 219, 173, 58,  61,  101, 243, 106, 171, 201, 116, 49,
        177, 187, 228, 194, 210, 246, 224, 228, 124, 166, 2,   3,   69,  47,  93,  97};
    char path[] = "build/smoke-scenario.json.gz";
    remove(path);

    SeashellHandle *s = seashell_new();
    CHECK(seashell_load_scenario(s, (const uint8_t *)path, strlen(path),
                                 (const uint8_t *)rpc, strlen(rpc)) == 0);
    SeashellAccount *a = seashell_get_account(s, usdc);
    CHECK(a != NULL);
    uint8_t owner[32];
    CHECK(seashell_account_owner(a, owner) == 0);
    CHECK(seashell_account_data_len(a) == 82);
    seashell_account_free(a);
    seashell_free(s); /* writes the scenario */

    FILE *f = fopen(path, "rb");
    CHECK(f != NULL);
    if (f) fclose(f);

    /* offline: no RPC, so it can only come from the file */
    s = seashell_new();
    CHECK(seashell_load_scenario(s, (const uint8_t *)path, strlen(path),
                                 NULL, 0) == 0);
    a = seashell_get_account(s, usdc);
    CHECK(a != NULL && seashell_account_data_len(a) == 82);
    seashell_account_free(a);
    seashell_free(s);
    remove(path);
    printf("scenario round trip: ok\n");
}

int main(int argc, char **argv) {
    if (argc != 2) {
        fprintf(stderr, "usage: %s <counter.so>\n", argv[0]);
        return 2;
    }

    CHECK(strcmp((const char *)seashell_version(), SEASHELL_CXX_VERSION) == 0);

    SeashellHandle *s = seashell_new();
    if (!s) die_last_error("seashell_new");
    memset(program_id, 0xC0, sizeof program_id);
    if (seashell_add_program_from_file(s, program_id, (const uint8_t *)argv[1],
                                       strlen(argv[1])))
        die_last_error("add_program_from_file");

    uint8_t seeds[64];
    memset(seeds, 1, 32);
    memset(seeds + 32, 2, 32);
    CHECK(seashell_pubkey_from_seed(seeds, payer) == 0);
    CHECK(seashell_pubkey_from_seed(seeds + 32, counter) == 0);
    CHECK(seashell_airdrop(s, payer, 10000000000ULL) == 0);

    SeashellAccountMeta init[] = {
        {payer, true, true}, {counter, true, true}, {system_program, false, false}};
    SeashellAccountMeta inc[] = {{counter, false, true}, {payer, true, false}};

    /* the counter account doesn't exist yet: a missing account is an error
     * until uninitialized accounts are allowed */
    uint8_t disc = 0;
    SeashellInstruction init_ix = {program_id, init, 3, &disc, 1};
    CHECK(seashell_process_instruction(s, &init_ix) == NULL);
    CHECK(seashell_last_error_copy(NULL, 0) > 0);
    CHECK(seashell_set_allow_uninitialized(s, true, false) == 0);

    /* without memoize, results aren't stored */
    SeashellResult *r = run(s, 0, init, 3);
    CHECK(seashell_result_is_ok(r) == 1);
    CHECK(seashell_result_compute_units(r) > 0);
    CHECK(seashell_result_logs_count(r) > 0);
    char line[256] = {0};
    CHECK(seashell_result_log_copy(r, 0, (uint8_t *)line, sizeof line - 1) > 0);
    CHECK(strstr(line, "invoke [1]") != NULL);
    SeashellError err;
    CHECK(seashell_result_error_info(r, &err) == 0 && err.kind == SEASHELL_ERR_NONE);
    CHECK(read_count(seashell_result_get_account(r, counter)) == 0);
    seashell_result_free(r);
    CHECK(seashell_get_account(s, counter) == NULL);

    /* with memoize, they are */
    CHECK(seashell_set_memoize(s, true) == 0);
    r = run(s, 0, init, 3);
    CHECK(seashell_result_is_ok(r) == 1);
    seashell_result_free(r);
    CHECK(read_count(seashell_get_account(s, counter)) == 0);

    for (int i = 0; i < 2; i++) {
        r = run(s, 1, inc, 2);
        CHECK(seashell_result_is_ok(r) == 1);
        seashell_result_free(r);
    }
    r = run(s, 2, inc, 2);
    CHECK(seashell_result_is_ok(r) == 1);
    seashell_result_free(r);
    CHECK(read_count(seashell_get_account(s, counter)) == 1);

    /* decrement past zero: InvalidArgument (variant 1, ERROR_INVALID_ARGUMENT) */
    r = run(s, 2, inc, 2);
    CHECK(seashell_result_is_ok(r) == 1);
    seashell_result_free(r);
    expect_error(run(s, 2, inc, 2), 1, 2ULL << 32);

    /* Caravel rejects duplicate writable accounts with ERROR_CUSTOM(7) */
    SeashellAccountMeta dup[] = {{counter, false, true}, {counter, false, true}};
    expect_error(run(s, 1, dup, 2), SEASHELL_IX_ERR_CUSTOM, 0x107);

    /* running out of CUs surfaces as ProgramFailedToComplete */
    CHECK(seashell_set_compute_unit_limit(s, 10) == 0);
    CHECK(seashell_get_compute_unit_limit(s) == 10);
    expect_error(run(s, 1, inc, 2), SEASHELL_IX_ERR_PROGRAM_FAILED_TO_COMPLETE, 0);

    /* accounts and sysvars */
    SeashellAccount *a = seashell_get_account(s, counter);
    SeashellKeyedAccount k;
    uint8_t copy[32];
    memset(copy, 7, 32);
    CHECK(seashell_account_keyed(a, copy, &k) == 0);
    CHECK(seashell_set_account(s, &k) == 0);
    seashell_account_free(a);
    CHECK(read_count(seashell_get_account(s, copy)) == 0);
    CHECK(seashell_clear_non_program_accounts(s) == 0);
    CHECK(seashell_get_account(s, copy) == NULL);

    SeashellClock clock;
    CHECK(seashell_warp(s, 1000, 1234) == 0);
    CHECK(seashell_get_clock(s, &clock) == 0);
    CHECK(clock.slot == 1000 && clock.unix_timestamp == 1234);
    CHECK(seashell_minimum_balance_for_rent_exemption(s, 0) > 0);
    CHECK(seashell_use_p_token(s) == 0);

    /* bad arguments come back as NULL plus an error message */
    SeashellInstruction bad = {NULL, NULL, 0, NULL, 0};
    CHECK(seashell_process_instruction(s, &bad) == NULL);
    CHECK(seashell_last_error_copy(NULL, 0) > 0);

    /* PDA: deterministic */
    const uint8_t *pda_seeds[] = {(const uint8_t *)"counter", payer};
    size_t pda_lens[] = {7, 32};
    uint8_t pda[32], pda2[32], bump = 0, bump2 = 0;
    CHECK(seashell_find_program_address(pda_seeds, pda_lens, 2, program_id, pda,
                                        &bump) == 0);
    CHECK(seashell_find_program_address(pda_seeds, pda_lens, 2, program_id, pda2,
                                        &bump2) == 0);
    CHECK(memcmp(pda, pda2, 32) == 0 && bump == bump2);
    seashell_free(s);

    const char *rpc = getenv("SEASHELL_SMOKE_RPC_URL");
    if (rpc && *rpc) scenario_roundtrip(rpc);

    if (failures) {
        fprintf(stderr, "smoke: %d check(s) failed\n", failures);
        return 1;
    }
    printf("smoke: ok\n");
    return 0;
}
