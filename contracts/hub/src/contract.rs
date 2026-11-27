use std::convert::TryInto;

use cosmwasm_std::{
    Binary, Decimal, Deps, DepsMut, Env, MessageInfo, Reply, Response, StdError, StdResult,
    entry_point, from_json, to_json_binary,
};
use cw2::{ContractVersion, get_contract_version, set_contract_version};
use cw20::Cw20ReceiveMsg;
use pfc_steak::hub::{
    CallbackMsg, ExecuteMsg, FeeType, InstantiateMsg, MigrateMsg, QueryMsg, ReceiveMsg,
};

use crate::{
    execute,
    helpers::{get_denom_balance, unwrap_reply},
    migrations::ConfigV100,
    queries,
    state::State,
};

/// Contract name that is used for migration.
pub const CONTRACT_NAME: &str = "steak-hub";
/// Contract version that is used for migration.
pub const CONTRACT_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const REPLY_INSTANTIATE_TOKEN: u64 = 1;
pub const REPLY_REGISTER_RECEIVED_COINS: u64 = 2;
pub const REPLY_SUBMIT_BATCH: u64 = 3;
/// `RedelegateInactive` uses one reply id per planned redelegation, starting here, so the reply
/// handler can say which redelegation the staking module refused.
pub const REPLY_REDELEGATE_INACTIVE_BASE: u64 = 1_000;
pub const REDELEGATE_INACTIVE_DEFAULT_LIMIT: u32 = 25;
pub const REDELEGATE_INACTIVE_MAX_LIMIT: u32 = 50;
pub const SPECIAL_SEND_MESSAGE_TO_TRANSFER: &str = "PFC_TRANSFER_NOT_SEND";

#[entry_point]
pub fn instantiate(
    deps: DepsMut,
    env: Env,
    _info: MessageInfo,
    msg: InstantiateMsg,
) -> StdResult<Response> {
    set_contract_version(deps.storage, CONTRACT_NAME, CONTRACT_VERSION)?;
    execute::instantiate(deps, env, msg)
}

#[entry_point]
pub fn execute(deps: DepsMut, env: Env, info: MessageInfo, msg: ExecuteMsg) -> StdResult<Response> {
    let api = deps.api;
    match msg {
        ExecuteMsg::Receive(cw20_msg) => receive(deps, env, info, cw20_msg),
        ExecuteMsg::Bond {
            receiver,
            exec_msg,
        } => execute::bond(
            deps,
            env,
            receiver.map(|s| api.addr_validate(&s)).transpose()?.unwrap_or(info.sender),
            info.funds,
            exec_msg,
            false,
        ),
        ExecuteMsg::BondEx {
            receiver,
        } => execute::bond(
            deps,
            env,
            receiver.map(|s| api.addr_validate(&s)).transpose()?.unwrap_or(info.sender),
            info.funds,
            None,
            true,
        ),
        ExecuteMsg::WithdrawUnbonded {
            receiver,
        } => execute::withdraw_unbonded(
            deps,
            env,
            info.sender.clone(),
            receiver.map(|s| api.addr_validate(&s)).transpose()?.unwrap_or(info.sender),
        ),
        ExecuteMsg::WithdrawUnbondedAdmin {
            address,
        } => execute::withdraw_unbonded_admin(deps, env, info.sender, api.addr_validate(&address)?),
        ExecuteMsg::AddValidator {
            validator,
        } => execute::add_validator(deps, info.sender, validator),
        ExecuteMsg::RemoveValidator {
            validator,
        } => execute::remove_validator(deps, env, info.sender, validator),
        ExecuteMsg::RemoveValidatorEx {
            validator,
        } => execute::remove_validator_ex(deps, env, info.sender, validator),
        ExecuteMsg::Redelegate {
            validator_from,
            validator_to,
        } => execute::redelegate(deps, env, info.sender, validator_from, validator_to),
        ExecuteMsg::RedelegateInactive {
            limit,
            exclude,
        } => {
            execute::redelegate_inactive(deps, env, info.sender, limit, exclude.unwrap_or_default())
        },
        ExecuteMsg::TransferOwnership {
            new_owner,
        } => execute::transfer_ownership(deps, info.sender, new_owner),
        ExecuteMsg::AcceptOwnership {} => execute::accept_ownership(deps, info.sender),
        ExecuteMsg::Harvest {} => execute::harvest(deps, env),
        ExecuteMsg::Rebalance {
            minimum,
        } => execute::rebalance(deps, env, minimum),
        ExecuteMsg::Reconcile {} => execute::reconcile(deps, env),
        ExecuteMsg::SubmitBatch {} => execute::submit_batch(deps, env),
        ExecuteMsg::TransferFeeAccount {
            fee_account_type,
            new_fee_account,
        } => execute::transfer_fee_account(deps, info.sender, fee_account_type, new_fee_account),
        ExecuteMsg::UpdateFee {
            new_fee,
        } => execute::update_fee(deps, info.sender, new_fee),
        ExecuteMsg::Callback(callback_msg) => callback(deps, env, info, callback_msg),
        ExecuteMsg::PauseValidator {
            validator,
        } => execute::pause_validator(deps, env, info.sender, validator),
        ExecuteMsg::UnPauseValidator {
            validator,
        } => execute::unpause_validator(deps, env, info.sender, validator),
        ExecuteMsg::SetUnbondPeriod {
            unbond_period,
        } => execute::set_unbond_period(deps, env, info.sender, unbond_period),
        ExecuteMsg::SetDustCollector {
            dust_collector,
        } => execute::set_dust_collector(deps, env, info.sender, dust_collector),
        ExecuteMsg::CollectDust {
            max_tokens,
        } => {
            let max_tokens_usize_r = max_tokens.try_into();
            if let Ok(max_tokens_usize) = max_tokens_usize_r {
                execute::collect_dust(deps, env, max_tokens_usize)
            } else {
                Err(StdError::generic_err("max_tokens too large"))
            }
        },
        ExecuteMsg::ReturnDenom {} => execute::return_denom(deps, env, info.funds),
        ExecuteMsg::SetBaseDenom {
            new_denom,
        } => execute::set_base_denom(deps, info.sender, new_denom),
    }
}

fn receive(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    cw20_msg: Cw20ReceiveMsg,
) -> StdResult<Response> {
    let api = deps.api;
    match from_json(&cw20_msg.msg)? {
        ReceiveMsg::QueueUnbond {
            receiver,
        } => {
            let state = State::default();

            let steak_token = state.steak_token.load(deps.storage)?;
            if info.sender != steak_token {
                return Err(StdError::generic_err(format!(
                    "expecting Steak token, received {}",
                    info.sender
                )));
            }

            execute::queue_unbond(
                deps,
                env,
                api.addr_validate(&receiver.unwrap_or(cw20_msg.sender))?,
                cw20_msg.amount,
            )
        },
    }
}

fn callback(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    callback_msg: CallbackMsg,
) -> StdResult<Response> {
    if env.contract.address != info.sender {
        return Err(StdError::generic_err("callbacks can only be invoked by the contract itself"));
    }

    match callback_msg {
        CallbackMsg::Reinvest {} => execute::reinvest(deps, env),
        CallbackMsg::HarvestAfterRedelegation {
            exclude,
        } => execute::harvest_after_redelegation(deps, env, exclude),
        CallbackMsg::ReinvestIfNeeded {
            exclude,
        } => execute::reinvest_if_needed(deps, env, exclude),
    }
}

fn is_redelegate_inactive_reply(id: u64) -> bool {
    (REPLY_REDELEGATE_INACTIVE_BASE
        ..REPLY_REDELEGATE_INACTIVE_BASE + u64::from(REDELEGATE_INACTIVE_MAX_LIMIT))
        .contains(&id)
}

#[entry_point]
pub fn reply(deps: DepsMut, env: Env, reply: Reply) -> StdResult<Response> {
    match reply.id {
        1 => execute::register_steak_token(deps, unwrap_reply(reply)?),
        REPLY_REGISTER_RECEIVED_COINS => {
            execute::register_received_coins(deps, env, unwrap_reply(reply)?.events)
        },
        REPLY_SUBMIT_BATCH => Ok(execute::submit_batch_deferred(reply)),
        id if is_redelegate_inactive_reply(id) => {
            execute::redelegate_inactive_reply(deps, env, reply)
        },
        id => Err(StdError::generic_err(format!(
            "invalid reply id: {}; must be 1-3 or a redelegate-inactive id",
            id
        ))),
    }
}

#[entry_point]
pub fn query(deps: Deps, env: Env, msg: QueryMsg) -> StdResult<Binary> {
    match msg {
        QueryMsg::Config {} => to_json_binary(&queries::config(deps)?),
        QueryMsg::State {} => to_json_binary(&queries::state(deps, env)?),
        QueryMsg::PendingBatch {} => to_json_binary(&queries::pending_batch(deps)?),
        QueryMsg::PreviousBatch(id) => to_json_binary(&queries::previous_batch(deps, id)?),
        QueryMsg::PreviousBatches {
            start_after,
            limit,
        } => to_json_binary(&queries::previous_batches(deps, start_after, limit)?),
        QueryMsg::UnbondRequestsByBatch {
            id,
            start_after,
            limit,
        } => to_json_binary(&queries::unbond_requests_by_batch(deps, id, start_after, limit)?),
        QueryMsg::UnbondRequestsByUser {
            user,
            start_after,
            limit,
        } => to_json_binary(&queries::unbond_requests_by_user(deps, user, start_after, limit)?),
        QueryMsg::Unreconciled {} => to_json_binary(&queries::previous_batches_unreconciled(deps)?),
    }
}

#[entry_point]
pub fn migrate(mut deps: DepsMut, env: Env, _msg: MigrateMsg) -> StdResult<Response> {
    let contract_version = match get_contract_version(deps.storage) {
        Ok(version) => version,
        Err(_) => ContractVersion {
            contract: "steak-hub".to_string(),
            version: "0".to_string(),
        },
    };
    match contract_version.contract.as_ref() {
        #[allow(clippy::single_match)]
        "steak-hub" => match contract_version.version.as_ref() {
            #[allow(clippy::single_match)]
            "0" => {
                let state = State::default();
                let owner = state.owner.load(deps.storage)?;
                state.denom.save(deps.storage, &"uluna".to_string())?;
                state.fee_account.save(deps.storage, &owner)?;
                state.max_fee_rate.save(deps.storage, &Decimal::zero())?;
                state.fee_rate.save(deps.storage, &Decimal::zero())?;
                state.fee_account_type.save(deps.storage, &FeeType::Wallet)?;
                ConfigV100::upgrade_stores(
                    deps.storage,
                    &deps.querier,
                    env.contract.address.clone(),
                )?;
                state.dust_collector.save(deps.storage, &None)?;
            },
            "2.1.4" => {
                let state = State::default();
                ConfigV100::upgrade_stores(
                    deps.storage,
                    &deps.querier,
                    env.contract.address.clone(),
                )?;
                state.fee_account_type.save(deps.storage, &FeeType::Wallet)?;
                state.dust_collector.save(deps.storage, &None)?;
            },
            "2.1.5" => {
                ConfigV100::upgrade_stores(
                    deps.storage,
                    &deps.querier,
                    env.contract.address.clone(),
                )?;
                let state = State::default();
                state.fee_account_type.save(deps.storage, &FeeType::Wallet)?;
                state.dust_collector.save(deps.storage, &None)?;
            },
            "2.1.6" | "2.1.7" => {
                let state = State::default();
                // note: this is also done in ConfigV100::upgrade
                let denom = state.denom.load(deps.storage)?;
                state.prev_denom.save(
                    deps.storage,
                    &get_denom_balance(&deps.querier, env.contract.address.clone(), denom)?,
                )?;

                state.fee_account_type.save(deps.storage, &FeeType::Wallet)?;
                state.dust_collector.save(deps.storage, &None)?;
            },
            "2.1.8" | "2.1.16" => {
                let state = State::default();
                state.fee_account_type.save(deps.storage, &FeeType::Wallet)?;
                state.dust_collector.save(deps.storage, &None)?;
            },
            "3.0.1" => {
                let state = State::default();
                state.dust_collector.save(deps.storage, &None)?;
            },
            _ => {},
        },
        _ => {
            return Err(StdError::generic_err("contract name is not the same. aborting {}"));
        },
    }
    /*
    let state = State::default();

    state.max_fee_rate.save(deps.storage,&Decimal::from_ratio(10u32,100u32))?;
    state.fee_rate.save(deps.storage,&Decimal::from_ratio(10u32,100u32))?;

     */
    // The pre-fix `remove_validator` dropped a validator from the whitelist but then added it to
    // the active set. The whitelist reflects the owner's removal decision, so the repair drops
    // active entries that are no longer whitelisted rather than re-whitelisting them.
    let state = State::default();
    let mut validator_registry_repairs = 0u64;
    if let Some(mut active_validators) = state.validators_active.may_load(deps.storage)? {
        let validators = state.validators.may_load(deps.storage)?.unwrap_or_default();
        let before = active_validators.len();
        active_validators.retain(|v| validators.contains(v));
        validator_registry_repairs = (before - active_validators.len()) as u64;
        state.validators_active.save(deps.storage, &active_validators)?;
    }

    let reserve_initialization =
        execute::prepare_native_reserved_initialization(deps.branch(), &env)?;
    let (native_reserved, legacy_written_down) = match &reserve_initialization {
        execute::ReserveInitStep::Done {
            reserved,
            written_down,
        } => (reserved.to_string(), written_down.to_string()),
        _ => ("pending".to_string(), "pending".to_string()),
    };

    set_contract_version(deps.storage, CONTRACT_NAME, CONTRACT_VERSION)?;

    Ok(Response::new()
        .add_event(execute::reserve_initialization_event(&reserve_initialization))
        .add_attribute("previous_contract_name", &contract_version.contract)
        .add_attribute("previous_contract_version", &contract_version.version)
        .add_attribute("new_contract_name", CONTRACT_NAME)
        .add_attribute("new_contract_version", CONTRACT_VERSION)
        .add_attribute("validator_registry_repairs", validator_registry_repairs.to_string())
        .add_attribute("native_reserved", native_reserved)
        .add_attribute("legacy_reconciled_written_down", legacy_written_down))
}
