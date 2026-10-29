# seashell-cxx

A C ABI for [Seashell](https://github.com/soundsonacid/seashell)

| platform       | link with                                          |
| -------------- | -------------------------------------------------- |
| `darwin-arm64` | `-liconv -lm`                                      |
| `darwin-x64`   | `-liconv -lm`                                      |
| `linux-x64`    | `-lgcc_s -lutil -lrt -lpthread -lm -ldl`           |
| `linux-arm64`  | `-lgcc_s -lutil -lrt -lpthread -lm -ldl`           |

## Example

```c
#include "seashell.h"

uint8_t program_id[32] = {0xC0}, system_program[32] = {0};
uint8_t payer[32] = {/* ... */}, counter[32] = {/* ... */};

SeashellHandle *s = seashell_new();
seashell_add_program_from_file(s, program_id, (const uint8_t *)"program.so", 10);
seashell_set_memoize(s, true);                     /* keep results */
seashell_set_allow_uninitialized(s, true, false); /* new accounts start empty */

/* record anything missing from mainnet into the scenario; with no URL
 * it replays the file offline */
const char *scenario = "scenarios/counter.json.gz";
const char *rpc = "https://api.mainnet-beta.solana.com";
seashell_load_scenario(s, (const uint8_t *)scenario, strlen(scenario),
                       (const uint8_t *)rpc, strlen(rpc));

seashell_airdrop(s, payer, 1000000000);

uint8_t data[] = {0};
SeashellAccountMeta metas[] = {
    {payer, true, true}, {counter, true, true}, {system_program, false, false}};
SeashellInstruction ix = {program_id, metas, 3, data, sizeof data};

SeashellResult *r = seashell_process_instruction(s, &ix);
SeashellError err;
seashell_result_error_info(r, &err);
if (err.kind == SEASHELL_ERR_INSTRUCTION)
    printf("failed: program error %llu\n", (unsigned long long)err.program_error);
seashell_result_free(r);

SeashellAccount *after = seashell_get_account(s, counter);
seashell_account_free(after);
seashell_free(s); /* writes the scenario */
```

```sh
cc -Ipath/to/include test.c path/to/libs/darwin-arm64/libseashell.a -liconv -lm
```

## Notes

- `seashell_load_scenario` takes the scenario path directly, since `load_scenarios` looks for `/scenarios` within a cargo workspace and this isn't present if you're using this
- With an RPC URL set, every address an instruction touches triggers a lookup on the corresponding cluster
- Accounts in a loaded scenario take precedence over ones set locally, so a memoized change to an account that came from RPC isn't visible afterwards
