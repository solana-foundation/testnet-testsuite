//! Summarise one `getBlock` result (`encoding: json`, `transactionDetails`
//! `full` or `accounts`) into slot totals and the transactions we sent.
//!
//! Pure functions over `serde_json::Value`: raw JSON keeps every
//! transaction version readable (legacy, v0 with loaded addresses, v1 with a
//! `transactionConfig`) without pinning typed RPC structs to one of them.

use std::collections::HashMap;

use serde_json::Value;

pub const VOTE_PROGRAM: &str = "Vote111111111111111111111111111111111111111";
pub const COMPUTE_BUDGET_PROGRAM: &str = "ComputeBudget111111111111111111111111111111";

/// What we know about addresses while reading a block.
#[derive(Default)]
pub struct Known {
    /// Fee payer → participant role (`maker/clob-jup-tight`, `taker/3`, ...).
    pub wallets: HashMap<String, String>,
    /// Program id → short name, for labels. Unknown programs are `other`.
    pub programs: HashMap<String, &'static str>,
}

impl Known {
    pub fn program_name(&self, id: &str) -> &'static str {
        self.programs.get(id).copied().unwrap_or("other")
    }
}

/// Well-known programs every cluster has.
pub fn builtin_programs() -> HashMap<String, &'static str> {
    [
        ("11111111111111111111111111111111", "system"),
        (COMPUTE_BUDGET_PROGRAM, "compute-budget"),
        (VOTE_PROGRAM, "vote"),
        ("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA", "spl-token"),
        (
            "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb",
            "spl-token-2022",
        ),
        ("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL", "ata"),
        ("MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr", "memo"),
        ("metaqbxxUerdq28cj1RbAWkYQm3ybzjb6a8bt518x1s", "metaplex"),
    ]
    .into_iter()
    .map(|(id, name)| (id.to_owned(), name))
    .collect()
}

#[derive(Debug, Default, PartialEq)]
pub struct BlockSummary {
    pub block_time: Option<i64>,
    pub block_height: Option<u64>,
    pub tx_total: u32,
    pub tx_vote: u32,
    pub tx_failed: u32,
    /// `None` when the details carry no CU (accounts mode).
    pub cu_consumed: Option<u64>,
    pub cost_units: Option<u64>,
    pub fees_lamports: u64,
    /// Priority price (micro-lamports per CU) of every non-vote transaction.
    pub prio_prices: Vec<u64>,
    pub ours: Vec<OurTx>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OurTx {
    pub signature: String,
    pub payer: String,
    pub role: String,
    pub ok: bool,
    /// Error kind in mtm-chain's format (`InstructionError:Custom(6001)`).
    pub error: Option<String>,
    /// Name of the top-level program whose instruction failed.
    pub error_program: Option<&'static str>,
    pub cu_consumed: Option<u64>,
    pub cu_limit: Option<u64>,
    pub prio_price: Option<u64>,
    pub fee_lamports: u64,
    /// Top-level programs invoked, compute budget excluded, in order.
    pub programs: Vec<&'static str>,
}

pub fn summarize(block: &Value, known: &Known) -> BlockSummary {
    let mut summary = BlockSummary {
        block_time: block["blockTime"].as_i64(),
        block_height: block["blockHeight"].as_u64(),
        ..BlockSummary::default()
    };
    let (mut cu, mut cost, mut has_cu) = (0_u64, 0_u64, false);
    for tx in block["transactions"].as_array().into_iter().flatten() {
        let meta = &tx["meta"];
        let keys = account_keys(tx);
        let instructions = tx["transaction"]["message"]["instructions"].as_array();
        let is_vote = match instructions {
            Some(ixs) => ixs
                .iter()
                .any(|ix| program_of(ix, &keys) == Some(VOTE_PROGRAM)),
            None => keys.iter().any(|key| key == VOTE_PROGRAM),
        };
        let ok = meta["err"].is_null();
        let fee = meta["fee"].as_u64().unwrap_or_default();
        summary.tx_total += 1;
        summary.tx_vote += u32::from(is_vote);
        summary.tx_failed += u32::from(!ok);
        summary.fees_lamports += fee;
        if let Some(units) = meta["computeUnitsConsumed"].as_u64() {
            has_cu = true;
            cu += units;
            cost += meta["costUnits"].as_u64().unwrap_or_default();
        }
        let budget = instructions.map(|_| budget(tx, &keys));
        if !is_vote && let Some(budget) = budget {
            summary.prio_prices.push(budget.price.unwrap_or_default());
        }
        let Some(payer) = keys.first() else { continue };
        let Some(role) = known.wallets.get(payer.as_str()) else {
            continue;
        };
        let error = (!ok).then(|| error_kind(&meta["err"]));
        let error_program = (!ok)
            .then(|| failing_instruction(&meta["err"]))
            .flatten()
            .and_then(|index| instructions?.get(index))
            .and_then(|ix| program_of(ix, &keys))
            .map(|id| known.program_name(id));
        let mut programs: Vec<&'static str> = Vec::new();
        for ix in instructions.into_iter().flatten() {
            if let Some(id) = program_of(ix, &keys)
                && id != COMPUTE_BUDGET_PROGRAM
            {
                let name = known.program_name(id);
                if !programs.contains(&name) {
                    programs.push(name);
                }
            }
        }
        summary.ours.push(OurTx {
            signature: tx["transaction"]["signatures"][0]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
            payer: payer.clone(),
            role: role.clone(),
            ok,
            error,
            error_program,
            cu_consumed: meta["computeUnitsConsumed"].as_u64(),
            cu_limit: budget.and_then(|b| b.limit),
            prio_price: budget.and_then(|b| b.price),
            fee_lamports: fee,
            programs,
        });
    }
    if has_cu {
        summary.cu_consumed = Some(cu);
        summary.cost_units = Some(cost);
    }
    summary
}

/// Every account key in index order: static keys, then the loaded writable
/// and readonly addresses (v0). Accounts mode lists objects with `pubkey`.
fn account_keys(tx: &Value) -> Vec<String> {
    let transaction = &tx["transaction"];
    let listed = transaction["message"]["accountKeys"]
        .as_array()
        .or_else(|| transaction["accountKeys"].as_array());
    let mut keys: Vec<String> = listed
        .into_iter()
        .flatten()
        .filter_map(|key| key.as_str().or_else(|| key["pubkey"].as_str()))
        .map(str::to_owned)
        .collect();
    // Accounts mode already includes loaded addresses (with a `source`).
    if transaction["message"]["accountKeys"].is_array() {
        let loaded = &tx["meta"]["loadedAddresses"];
        for list in ["writable", "readonly"] {
            keys.extend(
                loaded[list]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(str::to_owned),
            );
        }
    }
    keys
}

fn program_of<'a>(ix: &Value, keys: &'a [String]) -> Option<&'a str> {
    let index = usize::try_from(ix["programIdIndex"].as_u64()?).ok()?;
    keys.get(index).map(String::as_str)
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Budget {
    limit: Option<u64>,
    /// micro-lamports per CU
    price: Option<u64>,
}

/// The compute budget: v1 carries it in `transactionConfig` (priority fee in
/// total lamports); legacy and v0 in ComputeBudget instructions.
fn budget(tx: &Value, keys: &[String]) -> Budget {
    let config = &tx["transaction"]["message"]["transactionConfig"];
    if config.is_object() {
        let limit = config["computeUnitLimit"].as_u64();
        let price = match (config["priorityFee"].as_u64(), limit) {
            (Some(fee), Some(limit)) if limit > 0 => Some(
                u64::try_from(u128::from(fee) * 1_000_000 / u128::from(limit)).unwrap_or(u64::MAX),
            ),
            (Some(_), _) => Some(0),
            _ => None,
        };
        return Budget { limit, price };
    }
    let mut budget = Budget::default();
    for ix in tx["transaction"]["message"]["instructions"]
        .as_array()
        .into_iter()
        .flatten()
    {
        if program_of(ix, keys) != Some(COMPUTE_BUDGET_PROGRAM) {
            continue;
        }
        let Some(data) = ix["data"]
            .as_str()
            .and_then(|data| bs58::decode(data).into_vec().ok())
        else {
            continue;
        };
        match data.split_first() {
            Some((2, rest)) if rest.len() >= 4 => {
                budget.limit = rest[..4]
                    .try_into()
                    .ok()
                    .map(|b| u64::from(u32::from_le_bytes(b)));
            }
            Some((3, rest)) if rest.len() >= 8 => {
                budget.price = rest[..8].try_into().ok().map(u64::from_le_bytes);
            }
            _ => {}
        }
    }
    budget
}

/// `{"InstructionError":[2,{"Custom":6001}]}` → `InstructionError:Custom(6001)`;
/// `{"InstructionError":[0,"ComputationalBudgetExceeded"]}` →
/// `InstructionError:ComputationalBudgetExceeded`; `"BlockhashNotFound"`
/// stays; `{"InsufficientFundsForRent":{...}}` → `InsufficientFundsForRent`.
/// Matches `mtm_chain::tag::error_kind` on the client side.
pub fn error_kind(err: &Value) -> String {
    if let Some(name) = err.as_str() {
        return name.to_owned();
    }
    let Some((name, inner)) = err.as_object().and_then(|o| o.iter().next()) else {
        return "unknown".to_owned();
    };
    if name == "InstructionError" {
        let error = &inner[1];
        let detail = match error {
            Value::String(s) => s.clone(),
            Value::Object(o) => match o.iter().next() {
                Some((variant, Value::Number(n))) => format!("{variant}({n})"),
                Some((variant, Value::String(s))) => format!("{variant}({s:?})"),
                Some((variant, _)) => variant.clone(),
                None => "unknown".to_owned(),
            },
            _ => "unknown".to_owned(),
        };
        return format!("InstructionError:{detail}");
    }
    name.clone()
}

fn failing_instruction(err: &Value) -> Option<usize> {
    usize::try_from(err["InstructionError"][0].as_u64()?).ok()
}

/// Nearest-rank percentile of an unsorted sample (`q` in 0..=100).
pub fn percentile(values: &mut [u64], q: u32) -> Option<u64> {
    if values.is_empty() {
        return None;
    }
    values.sort_unstable();
    let rank = (values.len() * q as usize).div_ceil(100).max(1);
    values.get(rank - 1).copied()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const PAYER: &str = "E2ka2rrMB2zxyvZwACFTSNdA2n3gqiz38jsEXvwWTG4i";
    const ORCA: &str = "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc";

    fn known() -> Known {
        let mut programs = builtin_programs();
        programs.insert(ORCA.to_owned(), "orca-whirlpool");
        Known {
            wallets: [(PAYER.to_owned(), "taker/0".to_owned())].into(),
            programs,
        }
    }

    fn block() -> Value {
        // set_compute_unit_limit(200_000), set_compute_unit_price(1_000)
        let limit = bs58::encode([2, 0x40, 0x0d, 0x03, 0x00]).into_string();
        let price = bs58::encode([3, 0xe8, 0x03, 0, 0, 0, 0, 0, 0]).into_string();
        json!({
            "blockTime": 1792165356, "blockHeight": 16987,
            "transactions": [
                { // vote
                    "meta": { "computeUnitsConsumed": 2100, "costUnits": 4185, "err": null, "fee": 10000,
                              "loadedAddresses": { "readonly": [], "writable": [] } },
                    "transaction": { "message": {
                        "accountKeys": ["CeG7oLVDs3oeCzQHoaorCFz4ugUMQ9FRiPFVEjjETKoQ", "UYKDYzFBDRM6a9t7TcUJYY4douPy8hXERho5H8r2b7T", VOTE_PROGRAM],
                        "instructions": [{ "accounts": [1, 1], "data": "", "programIdIndex": 2 }] },
                        "signatures": ["vote-sig"] },
                    "version": "legacy"
                },
                { // ours, legacy, failed in the Orca instruction
                    "meta": { "computeUnitsConsumed": 30000, "costUnits": 40000,
                              "err": { "InstructionError": [2, { "Custom": 6001 }] }, "fee": 205000,
                              "loadedAddresses": { "readonly": [], "writable": [] } },
                    "transaction": { "message": {
                        "accountKeys": [PAYER, COMPUTE_BUDGET_PROGRAM, ORCA],
                        "instructions": [
                            { "accounts": [], "data": limit, "programIdIndex": 1 },
                            { "accounts": [], "data": price, "programIdIndex": 1 },
                            { "accounts": [0], "data": "", "programIdIndex": 2 }] },
                        "signatures": ["ours-sig"] },
                    "version": "legacy"
                },
                { // someone else's v1; program index points into loaded addresses
                    "meta": { "computeUnitsConsumed": 17950, "costUnits": 20611, "err": null, "fee": 5090,
                              "loadedAddresses": { "readonly": [ORCA], "writable": [] } },
                    "transaction": { "message": {
                        "accountKeys": ["2jkKsa9fU5A3AvSjiuYWFx7RfqWd2s4pNENxcgmsB7bR"],
                        "instructions": [{ "accounts": [0], "data": "", "programIdIndex": 1 }],
                        "transactionConfig": { "computeUnitLimit": 90000, "priorityFee": 90 } },
                        "signatures": ["v1-sig"] },
                    "version": 1
                }
            ]
        })
    }

    #[test]
    fn summarizes_counts_cu_fees_and_prices() {
        let summary = summarize(&block(), &known());
        assert_eq!(
            (summary.tx_total, summary.tx_vote, summary.tx_failed),
            (3, 1, 1)
        );
        assert_eq!(summary.cu_consumed, Some(2100 + 30000 + 17950));
        assert_eq!(summary.cost_units, Some(4185 + 40000 + 20611));
        assert_eq!(summary.fees_lamports, 10000 + 205000 + 5090);
        // 1_000 set explicitly; 90 lamports over 90_000 CU = 1_000 micro-lamports/CU
        assert_eq!(summary.prio_prices, vec![1_000, 1_000]);
    }

    #[test]
    fn records_our_transactions_with_the_failing_program() {
        let summary = summarize(&block(), &known());
        assert_eq!(summary.ours.len(), 1);
        let ours = &summary.ours[0];
        assert_eq!(ours.signature, "ours-sig");
        assert_eq!(ours.role, "taker/0");
        assert!(!ours.ok);
        assert_eq!(ours.error.as_deref(), Some("InstructionError:Custom(6001)"));
        assert_eq!(ours.error_program, Some("orca-whirlpool"));
        assert_eq!(ours.cu_limit, Some(200_000));
        assert_eq!(ours.prio_price, Some(1_000));
        assert_eq!(ours.programs, vec!["orca-whirlpool"]);
    }

    #[test]
    fn accounts_mode_has_counts_but_no_cu() {
        let block = json!({ "blockTime": 1, "transactions": [
            { "meta": { "err": null, "fee": 5000 },
              "transaction": { "accountKeys": [{ "pubkey": PAYER, "signer": true }, { "pubkey": VOTE_PROGRAM }],
                               "signatures": ["a"] } },
            { "meta": { "err": "BlockhashNotFound", "fee": 5000 },
              "transaction": { "accountKeys": [{ "pubkey": PAYER, "signer": true }], "signatures": ["b"] } }
        ]});
        let summary = summarize(&block, &known());
        assert_eq!(
            (summary.tx_total, summary.tx_vote, summary.tx_failed),
            (2, 1, 1)
        );
        assert_eq!(summary.cu_consumed, None);
        assert!(summary.prio_prices.is_empty());
        assert_eq!(summary.ours[1].error.as_deref(), Some("BlockhashNotFound"));
    }

    #[test]
    fn error_kinds_match_the_client_format() {
        assert_eq!(
            error_kind(&json!({ "InstructionError": [0, "ComputationalBudgetExceeded"] })),
            "InstructionError:ComputationalBudgetExceeded"
        );
        assert_eq!(
            error_kind(&json!({ "InsufficientFundsForRent": { "account_index": 3 } })),
            "InsufficientFundsForRent"
        );
        assert_eq!(
            mtm_chain::tag::error_kind("InstructionError(2, Custom(6001))"),
            error_kind(&json!({ "InstructionError": [2, { "Custom": 6001 }] }))
        );
    }

    #[test]
    fn percentiles_use_nearest_rank() {
        let mut values = vec![5, 1, 4, 2, 3];
        assert_eq!(percentile(&mut values, 50), Some(3));
        assert_eq!(percentile(&mut values, 90), Some(5));
        assert_eq!(percentile(&mut values, 0), Some(1));
        assert_eq!(percentile(&mut [], 50), None);
    }
}
