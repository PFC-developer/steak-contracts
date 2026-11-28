use std::str::FromStr;

use cosmwasm_std::{
    Addr, BalanceResponse, BankQuery, Coin, QuerierWrapper, QueryRequest, StdError, StdResult,
    Uint128,
};

use crate::types::Delegation;
/*
/// Unwrap a `Reply` object to extract the response
pub(crate) fn unwrap_reply(reply: Reply) -> StdResult<SubMsgResponse> {
    reply.result.into_result().map_err(StdError::generic_err)
}
*/

/// Query the amounts of Luna a staker is delegating to a specific validator
pub(crate) fn query_delegation(
    querier: &QuerierWrapper,
    validator: &str,
    delegator_addr: &Addr,
    denom: &str,
) -> StdResult<Delegation> {
    Ok(Delegation {
        validator: validator.to_string(),
        amount: querier
            .query_delegation(delegator_addr, validator)?
            .map(|fd| fd.amount.amount.u128())
            .unwrap_or(0),
        denom: denom.into(),
    })
}
pub(crate) fn query_all_delegations(
    querier: &QuerierWrapper,
    delegator_addr: &Addr,
    //  _denom: &str,
) -> StdResult<Vec<Delegation>> {
    Ok(querier
        .query_all_delegations(delegator_addr)?
        .into_iter()
        .map(|std_delegation| Delegation {
            validator: std_delegation.validator.to_string(),
            amount: std_delegation.amount.amount.u128(),
            denom: std_delegation.amount.denom,
        })
        .collect())
}

/// Query the amounts of Luna a staker is delegating to each of the validators specified
pub(crate) fn query_delegations(
    querier: &QuerierWrapper,
    validators: &[String],
    delegator_addr: &Addr,
    denom: &str,
) -> StdResult<Vec<Delegation>> {
    validators
        .iter()
        .map(|validator| query_delegation(querier, validator, delegator_addr, denom))
        .collect()
}

/// `cosmwasm_std::Coin` does not implement `FromStr`, so we have do it ourselves
///
/// Parsing the string with regex doesn't work, because the resulting binary would be too big for
/// including the `regex` library. Example:
/// https://github.com/PFC-Validator/terra-rust/blob/v1.1.8/terra-rust-api/src/client/core_types.rs#L34-L55
///
/// We opt for a dirtier solution. Enumerate characters in the string, and break before the first
/// character that is not a number. Split the string at that index.
///
/// This assumes the denom never starts with a number, which is true on Terra.
pub(crate) fn parse_coin(s: &str) -> StdResult<Coin> {
    for (i, c) in s.char_indices() {
        if c.is_alphabetic() {
            let amount = Uint128::from_str(&s[..i])?;
            let denom = &s[i..];
            return Ok(Coin::new(amount.u128(), denom));
        }
    }

    Err(StdError::generic_err(format!("failed to parse coin: {}", s)))
}

/// Find the amount of a denom sent along a message, assert it is non-zero, and no other denom were
/// sent together
pub(crate) fn parse_received_fund(funds: &[Coin], denom: &str) -> StdResult<Uint128> {
    if funds.len() != 1 {
        return Err(StdError::generic_err(format!(
            "must deposit exactly one coin; received {}",
            funds.len()
        )));
    }

    let fund = &funds[0];
    if fund.denom != denom {
        return Err(StdError::generic_err(format!(
            "expected {} deposit, received {}",
            denom, fund.denom
        )));
    }

    if fund.amount.is_zero() {
        return Err(StdError::generic_err("deposit amount must be non-zero"));
    }

    Ok(fund.amount)
}

pub fn get_denom_balance(
    querier: &QuerierWrapper,
    account_addr: Addr,
    denom: String,
) -> StdResult<Uint128> {
    let balance: BalanceResponse = querier.query(&QueryRequest::Bank(BankQuery::Balance {
        address: account_addr.to_string(),
        denom,
    }))?;
    Ok(balance.amount.amount)
}

/// Parse an RFC 3339 timestamp (`2006-01-02T15:04:05Z`, optionally with fractional seconds or a
/// `±hh:mm` offset), as the staking module writes an unbonding's `completion_time`, into Unix
/// seconds. Fractional seconds round up, so the result is never earlier than the real time.
/// Returns `None` for anything it does not understand.
pub(crate) fn parse_rfc3339_seconds(value: &str) -> Option<u64> {
    let bytes = value.as_bytes();
    let digits = |from: usize, len: usize| -> Option<i64> {
        let part = value.get(from..from + len)?;
        if !part.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        part.parse().ok()
    };
    if bytes.len() < 20
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[13] != b':'
        || bytes[16] != b':'
    {
        return None;
    }
    if bytes[10] != b'T' && bytes[10] != b't' {
        return None;
    }
    let (year, month, day) = (digits(0, 4)?, digits(5, 2)?, digits(8, 2)?);
    let (hour, minute, second) = (digits(11, 2)?, digits(14, 2)?, digits(17, 2)?);
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }

    let mut rest = &value[19..];
    let mut round_up = 0;
    if let Some(fraction) = rest.strip_prefix('.') {
        let len = fraction.bytes().take_while(u8::is_ascii_digit).count();
        if len == 0 {
            return None;
        }
        if fraction[..len].bytes().any(|b| b != b'0') {
            round_up = 1;
        }
        rest = &fraction[len..];
    }
    let offset = match rest {
        "Z" | "z" => 0,
        _ if rest.len() == 6 && rest.as_bytes()[3] == b':' => {
            let sign = match rest.as_bytes()[0] {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            let hours: i64 = rest.get(1..3)?.parse().ok()?;
            let minutes: i64 = rest.get(4..6)?.parse().ok()?;
            sign * (hours * 3600 + minutes * 60)
        },
        _ => return None,
    };

    // Days since 1970-01-01 for a proleptic Gregorian date (H. Hinnant's days_from_civil).
    let y = if month <= 2 {
        year - 1
    } else {
        year
    };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;

    let seconds = days * 86_400 + hour * 3600 + minute * 60 + second - offset + round_up;
    if seconds < 0 {
        None
    } else {
        Some(seconds as u64)
    }
}

/// The latest `completion_time` of the `unbond` events the staking module emitted, if any.
pub(crate) fn latest_unbond_completion(events: &[cosmwasm_std::Event]) -> Option<u64> {
    events
        .iter()
        .filter(|event| event.ty == "unbond")
        .flat_map(|event| event.attributes.iter())
        .filter(|attr| attr.key == "completion_time")
        .filter_map(|attr| parse_rfc3339_seconds(&attr.value))
        .max()
}
