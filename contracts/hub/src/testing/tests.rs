use std::str::FromStr;

use cosmwasm_std::{
    Addr, BankMsg, Coin, CosmosMsg, Decimal, DistributionMsg, Event, Order, OwnedDeps, Reply,
    ReplyOn, StdError, SubMsg, SubMsgResponse, Uint128, WasmMsg,
    testing::{MOCK_CONTRACT_ADDR, MockApi, MockStorage, mock_env, mock_info},
    to_json_binary,
};
use cw20::{Cw20ExecuteMsg, MinterResponse};
use cw20_base::msg::InstantiateMsg as Cw20InstantiateMsg;
use pfc_steak::hub::{
    Batch, CallbackMsg, ConfigResponse, ExecuteMsg, IdleFundsResponse, InstantiateMsg,
    PendingBatch, QueryMsg, ReceiveMsg, StateResponse, UnbondRequest,
    UnbondRequestsByBatchResponseItem, UnbondRequestsByUserResponseItem,
};

use super::{
    custom_querier::CustomQuerier,
    helpers::{mock_dependencies, mock_env_at_timestamp, query_helper},
};
use crate::{
    contract::{
        RECONCILE_GRACE_SECONDS, REDELEGATE_INACTIVE_MAX_LIMIT, REPLY_INSTANTIATE_TOKEN,
        REPLY_REDELEGATE_INACTIVE_BASE, REPLY_REGISTER_RECEIVED_COINS, REPLY_SUBMIT_BATCH, execute,
        instantiate, migrate, reply,
    },
    helpers::{parse_coin, parse_received_fund},
    math::{
        compute_mint_amount, compute_redelegations_for_inactive,
        compute_redelegations_for_rebalancing, compute_redelegations_for_removal,
        compute_undelegations,
    },
    state::State,
    types::{Coins, Delegation, Redelegation, Undelegation},
};

//--------------------------------------------------------------------------------------------------
// Test setup
//--------------------------------------------------------------------------------------------------

fn setup_test() -> OwnedDeps<MockStorage, MockApi, CustomQuerier> {
    let mut deps = mock_dependencies();

    let res = instantiate(
        deps.as_mut(),
        mock_env_at_timestamp(10000),
        mock_info("deployer", &[]),
        InstantiateMsg {
            cw20_code_id: 69420,
            owner: "larry".to_string(),
            name: "Steak Token".to_string(),
            symbol: "STEAK".to_string(),
            denom: "uxyz".to_string(),
            fee_account_type: "Wallet".to_string(),
            fee_account: "the_fee_man".to_string(),
            fee_amount: Decimal::from_ratio(10_u128, 100_u128), //10%
            max_fee_amount: Decimal::from_ratio(20_u128, 100_u128), //20%
            decimals: 6,
            epoch_period: 259200,   // 3 * 24 * 60 * 60 = 3 days
            unbond_period: 1814400, // 21 * 24 * 60 * 60 = 21 days
            validators: vec!["alice".to_string(), "bob".to_string(), "charlie".to_string()],
            label: None,
            marketing: None,
            dust_collector: None,
        },
    )
    .unwrap();

    assert_eq!(res.messages.len(), 1);
    assert_eq!(
        res.messages[0],
        SubMsg::reply_on_success(
            CosmosMsg::Wasm(WasmMsg::Instantiate {
                admin: Some("larry".to_string()),
                code_id: 69420,
                msg: to_json_binary(&Cw20InstantiateMsg {
                    name: "Steak Token".to_string(),
                    symbol: "STEAK".to_string(),
                    decimals: 6,
                    initial_balances: vec![],
                    mint: Some(MinterResponse {
                        minter: MOCK_CONTRACT_ADDR.to_string(),
                        cap: None,
                    }),
                    marketing: None,
                })
                .unwrap(),
                funds: vec![],
                label: "steak_token".to_string(),
            }),
            REPLY_INSTANTIATE_TOKEN,
        )
    );

    let event = Event::new("instantiate")
        .add_attribute("code_id", "69420")
        .add_attribute("_contract_address", "steak_token");

    let res = reply(deps.as_mut(), mock_env_at_timestamp(10000), Reply {
        id: REPLY_INSTANTIATE_TOKEN,
        result: cosmwasm_std::SubMsgResult::Ok(SubMsgResponse {
            events: vec![event],
            data: None,
        }),
    })
    .unwrap();

    assert_eq!(res.messages.len(), 0);

    deps.querier.set_cw20_total_supply("steak_token", 0);
    deps.querier.set_bonded_validators(&["alice", "bob", "charlie"]);
    deps
}

fn callback_submsg(callback: CallbackMsg) -> SubMsg {
    SubMsg::new(CosmosMsg::Wasm(WasmMsg::Execute {
        contract_addr: MOCK_CONTRACT_ADDR.to_string(),
        msg: to_json_binary(&ExecuteMsg::Callback(callback)).unwrap(),
        funds: vec![],
    }))
}

#[test]
fn migration_repairs_active_validator_registry() {
    let mut deps = setup_test();
    let state = State::default();
    state
        .validators
        .update(deps.as_mut().storage, |mut validators| -> Result<_, StdError> {
            validators.retain(|validator| validator != "charlie");
            Ok(validators)
        })
        .unwrap();

    let response = migrate(deps.as_mut(), mock_env(), Default::default()).unwrap();

    // A validator the owner removed must stay removed: the stale active entry is dropped.
    let validators = state.validators.load(deps.as_ref().storage).unwrap();
    assert!(!validators.contains(&"charlie".to_string()));
    let validators_active = state.validators_active.load(deps.as_ref().storage).unwrap();
    assert_eq!(validators_active, vec!["alice".to_string(), "bob".to_string()]);
    assert!(
        response.attributes.iter().any(
            |attribute| attribute.key == "validator_registry_repairs" && attribute.value == "1"
        )
    );
}

#[test]
fn failed_auto_submit_batch_does_not_revert_unbond() {
    let mut deps = setup_test();

    let res = reply(deps.as_mut(), mock_env(), cosmwasm_std::Reply {
        id: REPLY_SUBMIT_BATCH,
        result: cosmwasm_std::SubMsgResult::Err(
            "unbond batch exceeds the derivative token supply".to_string(),
        ),
    })
    .unwrap();

    assert!(res.messages.is_empty());
    assert_eq!(res.events, vec![
        Event::new("steakhub/submit_batch_deferred")
            .add_attribute("reason", "unbond batch exceeds the derivative token supply")
    ]);
}

fn setup_test_fee_split() -> OwnedDeps<MockStorage, MockApi, CustomQuerier> {
    let mut deps = mock_dependencies();

    let res = instantiate(
        deps.as_mut(),
        mock_env_at_timestamp(10000),
        mock_info("deployer", &[]),
        InstantiateMsg {
            cw20_code_id: 69420,
            owner: "larry".to_string(),
            name: "Steak Token".to_string(),
            symbol: "STEAK".to_string(),
            denom: "uxyz".to_string(),
            fee_account_type: "FeeSplit".to_string(),
            fee_account: "fee_split_contract".to_string(),
            fee_amount: Decimal::from_ratio(10_u128, 100_u128), //10%
            max_fee_amount: Decimal::from_ratio(20_u128, 100_u128), //20%
            decimals: 6,
            epoch_period: 259200,   // 3 * 24 * 60 * 60 = 3 days
            unbond_period: 1814400, // 21 * 24 * 60 * 60 = 21 days
            validators: vec!["alice".to_string(), "bob".to_string(), "charlie".to_string()],
            label: None,
            marketing: None,
            dust_collector: None,
        },
    )
    .unwrap();

    assert_eq!(res.messages.len(), 1);
    assert_eq!(
        res.messages[0],
        SubMsg::reply_on_success(
            CosmosMsg::Wasm(WasmMsg::Instantiate {
                admin: Some("larry".to_string()),
                code_id: 69420,
                msg: to_json_binary(&Cw20InstantiateMsg {
                    name: "Steak Token".to_string(),
                    symbol: "STEAK".to_string(),
                    decimals: 6,
                    initial_balances: vec![],
                    mint: Some(MinterResponse {
                        minter: MOCK_CONTRACT_ADDR.to_string(),
                        cap: None,
                    }),
                    marketing: None,
                })
                .unwrap(),
                funds: vec![],
                label: "steak_token".to_string(),
            }),
            REPLY_INSTANTIATE_TOKEN,
        )
    );

    let event = Event::new("instantiate")
        .add_attribute("code_id", "69420")
        .add_attribute("_contract_address", "steak_token");

    let res = reply(deps.as_mut(), mock_env_at_timestamp(10000), Reply {
        id: REPLY_INSTANTIATE_TOKEN,
        result: cosmwasm_std::SubMsgResult::Ok(SubMsgResponse {
            events: vec![event],
            data: None,
        }),
    })
    .unwrap();

    assert_eq!(res.messages.len(), 0);

    deps.querier.set_cw20_total_supply("steak_token", 0);
    deps.querier.set_bonded_validators(&["alice", "bob", "charlie"]);
    deps
}

//--------------------------------------------------------------------------------------------------
// Execution
//--------------------------------------------------------------------------------------------------

#[test]
fn proper_instantiation() {
    let deps = setup_test();

    let res: ConfigResponse = query_helper(deps.as_ref(), QueryMsg::Config {});
    assert_eq!(res, ConfigResponse {
        owner: "larry".to_string(),
        new_owner: None,
        steak_token: "steak_token".to_string(),
        epoch_period: 259200,
        unbond_period: 1814400,
        denom: "uxyz".to_string(),
        fee_type: "Wallet".to_string(),
        fee_account: "the_fee_man".to_string(),
        fee_rate: Decimal::from_ratio(10_u128, 100_u128),
        max_fee_rate: Decimal::from_ratio(20_u128, 100_u128),
        validators: vec!["alice".to_string(), "bob".to_string(), "charlie".to_string(),],
        paused_validators: vec![],
        dust_collector: None,
        token_factory: None
    });

    let res: StateResponse = query_helper(deps.as_ref(), QueryMsg::State {});
    assert_eq!(res, StateResponse {
        total_usteak: Uint128::zero(),
        total_native: Uint128::zero(),
        exchange_rate: Decimal::one(),
        unlocked_coins: vec![],
    },);

    let res: PendingBatch = query_helper(deps.as_ref(), QueryMsg::PendingBatch {});
    assert_eq!(res, PendingBatch {
        id: 1,
        usteak_to_burn: Uint128::zero(),
        est_unbond_start_time: 269200, // 10,000 + 259,200
    },);
    let deps_fee_split = setup_test_fee_split();

    let res_fee_split: ConfigResponse = query_helper(deps_fee_split.as_ref(), QueryMsg::Config {});
    assert_eq!(res_fee_split, ConfigResponse {
        owner: "larry".to_string(),
        new_owner: None,
        steak_token: "steak_token".to_string(),
        epoch_period: 259200,
        unbond_period: 1814400,
        denom: "uxyz".to_string(),
        fee_type: "FeeSplit".to_string(),
        fee_account: "fee_split_contract".to_string(),
        fee_rate: Decimal::from_ratio(10_u128, 100_u128),
        max_fee_rate: Decimal::from_ratio(20_u128, 100_u128),
        validators: vec!["alice".to_string(), "bob".to_string(), "charlie".to_string(),],
        paused_validators: vec![],
        dust_collector: None,
        token_factory: None
    });
}

#[test]
fn bonding() {
    let mut deps = setup_test();

    // Bond when no delegation has been made
    // In this case, the full deposit simply goes to the first validator
    let res = execute(
        deps.as_mut(),
        mock_env(),
        mock_info("user_1", &[Coin::new(1_000_000, "uxyz")]),
        ExecuteMsg::Bond {
            receiver: None,
            exec_msg: None,
        },
    )
    .unwrap();

    // 3 messages. (switched to 3 so we can 'send' instead of 'transfer' minted tokens, so contract
    // will know about it 1 - delegate
    // 2 - mint token (to ourselves)
    // 3 - send/transfer it
    assert_eq!(res.messages.len(), 3);
    assert_eq!(
        res.messages[0],
        SubMsg::reply_on_success(
            Delegation::new("alice", 1_000_000, "uxyz").to_cosmos_msg(),
            REPLY_REGISTER_RECEIVED_COINS
        )
    );
    assert_eq!(res.messages[1], SubMsg {
        id: 0,
        msg: CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: "steak_token".to_string(),
            msg: to_json_binary(&Cw20ExecuteMsg::Mint {
                recipient: "cosmos2contract".to_string(),
                amount: Uint128::new(1_000_000),
            })
            .unwrap(),
            funds: vec![],
        }),
        gas_limit: None,
        reply_on: ReplyOn::Never,
    });

    assert_eq!(res.messages[2], SubMsg {
        id: 0,
        msg: CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: "steak_token".to_string(),
            msg: to_json_binary(&Cw20ExecuteMsg::Transfer {
                recipient: "user_1".to_string(),
                amount: Uint128::new(1000000),
            })
            .unwrap(),
            funds: vec![],
        }),
        gas_limit: None,
        reply_on: ReplyOn::Never,
    });

    // Bond when there are existing delegations, and Luna:Steak exchange rate is >1
    // Previously user 1 delegated 1,000,000 uluna. We assume we have accumulated 2.5% yield at
    // 1025000 staked
    deps.querier.set_staking_delegations(&[
        Delegation::new("alice", 341667, "uxyz"),
        Delegation::new("bob", 341667, "uxyz"),
        Delegation::new("charlie", 341666, "uxyz"),
    ]);
    deps.querier.set_cw20_total_supply("steak_token", 1000000);

    // Charlie has the smallest amount of delegation, so the full deposit goes to him
    let res = execute(
        deps.as_mut(),
        mock_env(),
        mock_info("user_2", &[Coin::new(12345, "uxyz")]),
        ExecuteMsg::Bond {
            receiver: Some("user_3".to_string()),
            exec_msg: None,
        },
    )
    .unwrap();

    assert_eq!(res.messages.len(), 3);
    assert_eq!(
        res.messages[0],
        SubMsg::reply_on_success(
            Delegation::new("charlie", 12345, "uxyz").to_cosmos_msg(),
            REPLY_REGISTER_RECEIVED_COINS
        )
    );
    assert_eq!(res.messages[1], SubMsg {
        id: 0,
        msg: CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: "steak_token".to_string(),
            msg: to_json_binary(&Cw20ExecuteMsg::Mint {
                recipient: "cosmos2contract".to_string(),
                amount: Uint128::new(12043),
            })
            .unwrap(),
            funds: vec![],
        }),
        gas_limit: None,
        reply_on: ReplyOn::Never,
    });

    assert_eq!(res.messages[2], SubMsg {
        id: 0,
        msg: CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: "steak_token".to_string(),
            msg: to_json_binary(&Cw20ExecuteMsg::Transfer {
                recipient: "user_3".to_string(),
                amount: Uint128::new(12043),
            })
            .unwrap(),
            funds: vec![],
        }),
        gas_limit: None,
        reply_on: ReplyOn::Never,
    });

    // Check the state after bonding
    deps.querier.set_staking_delegations(&[
        Delegation::new("alice", 341667, "uxyz"),
        Delegation::new("bob", 341667, "uxyz"),
        Delegation::new("charlie", 354011, "uxyz"),
    ]);
    deps.querier.set_cw20_total_supply("steak_token", 1012043);

    let res: StateResponse = query_helper(deps.as_ref(), QueryMsg::State {});
    assert_eq!(res, StateResponse {
        total_usteak: Uint128::new(1012043),
        total_native: Uint128::new(1037345),
        exchange_rate: Decimal::from_ratio(1037345u128, 1012043u128),
        unlocked_coins: vec![],
    });
}

#[test]
fn state_uses_every_onchain_delegation() {
    let mut deps = setup_test();
    let state = State::default();

    state.validators.save(deps.as_mut().storage, &vec!["alice".to_string()]).unwrap();
    state.validators_active.save(deps.as_mut().storage, &vec!["alice".to_string()]).unwrap();
    deps.querier.set_cw20_total_supply("steak_token", 100);
    deps.querier.set_staking_delegations(&[
        Delegation::new("alice", 40, "uxyz"),
        Delegation::new("charlie", 60, "uxyz"),
    ]);

    let response: StateResponse = query_helper(deps.as_ref(), QueryMsg::State {});
    assert_eq!(response.total_native, Uint128::new(100));
    assert_eq!(response.exchange_rate, Decimal::one());
}
#[test]
fn bond_ex() {
    let mut deps = setup_test();

    // Bond when no delegation has been made
    // In this case, the full deposit simply goes to the first validator
    let res = execute(
        deps.as_mut(),
        mock_env(),
        mock_info("user_1", &[Coin::new(1_000_000, "uxyz")]),
        ExecuteMsg::BondEx {
            receiver: None,
        },
    )
    .unwrap();

    // 3 messages. (switched to 3 so we can 'send' instead of 'transfer' minted tokens, so contract
    // will know about it 1 - delegate
    // 2 - mint token (to ourselves)
    // 3 - send/transfer it
    assert_eq!(res.messages.len(), 2);
    assert_eq!(
        res.messages[0],
        SubMsg::reply_on_success(
            Delegation::new("alice", 1_000_000, "uxyz").to_cosmos_msg(),
            REPLY_REGISTER_RECEIVED_COINS
        )
    );
    assert_eq!(res.messages[1], SubMsg {
        id: 0,
        msg: CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: "steak_token".to_string(),
            msg: to_json_binary(&Cw20ExecuteMsg::Mint {
                recipient: "user_1".to_string(),
                amount: Uint128::new(1_000_000),
            })
            .unwrap(),
            funds: vec![],
        }),
        gas_limit: None,
        reply_on: ReplyOn::Never,
    });
}

#[test]
fn harvesting() {
    let mut deps = setup_test();

    // Assume users have bonded a total of 1,000,000 uluna and minted the same amount of usteak
    deps.querier.set_staking_delegations(&[
        Delegation::new("alice", 341667, "uxyz"),
        Delegation::new("bob", 341667, "uxyz"),
        Delegation::new("charlie", 341666, "uxyz"),
    ]);
    deps.querier.set_cw20_total_supply("steak_token", 1000000);

    let res = execute(deps.as_mut(), mock_env(), mock_info("worker", &[]), ExecuteMsg::Harvest {})
        .unwrap();

    assert_eq!(res.messages.len(), 4);
    assert_eq!(
        res.messages[0],
        SubMsg::reply_on_success(
            CosmosMsg::Distribution(DistributionMsg::WithdrawDelegatorReward {
                validator: "alice".to_string(),
            }),
            REPLY_REGISTER_RECEIVED_COINS,
        )
    );
    assert_eq!(
        res.messages[1],
        SubMsg::reply_on_success(
            CosmosMsg::Distribution(DistributionMsg::WithdrawDelegatorReward {
                validator: "bob".to_string(),
            }),
            REPLY_REGISTER_RECEIVED_COINS,
        )
    );
    assert_eq!(
        res.messages[2],
        SubMsg::reply_on_success(
            CosmosMsg::Distribution(DistributionMsg::WithdrawDelegatorReward {
                validator: "charlie".to_string(),
            }),
            REPLY_REGISTER_RECEIVED_COINS,
        )
    );
    assert_eq!(res.messages[3], SubMsg {
        id: 0,
        msg: CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: MOCK_CONTRACT_ADDR.to_string(),
            msg: to_json_binary(&ExecuteMsg::Callback(CallbackMsg::Reinvest {})).unwrap(),
            funds: vec![],
        }),
        gas_limit: None,
        reply_on: ReplyOn::Never,
    });
}

#[test]
fn harvesting_and_reinvesting_after_inactive_redelegation() {
    let mut deps = setup_test();
    let state = State::default();

    deps.querier.set_staking_delegations(&[
        Delegation::new("alice", 100, "uxyz"),
        Delegation::new("bob", 200, "uxyz"),
        Delegation::new("charlie", 300, "uxyz"),
    ]);

    let err = execute(
        deps.as_mut(),
        mock_env(),
        mock_info("worker", &[]),
        ExecuteMsg::Callback(CallbackMsg::HarvestAfterRedelegation {}),
    )
    .unwrap_err();
    assert_eq!(err, StdError::generic_err("callbacks can only be invoked by the contract itself"));

    let res = execute(
        deps.as_mut(),
        mock_env(),
        mock_info(MOCK_CONTRACT_ADDR, &[]),
        ExecuteMsg::Callback(CallbackMsg::HarvestAfterRedelegation {}),
    )
    .unwrap();

    assert_eq!(res.messages, vec![
        SubMsg::reply_on_success(
            CosmosMsg::Distribution(DistributionMsg::WithdrawDelegatorReward {
                validator: "alice".to_string(),
            }),
            REPLY_REGISTER_RECEIVED_COINS,
        ),
        SubMsg::reply_on_success(
            CosmosMsg::Distribution(DistributionMsg::WithdrawDelegatorReward {
                validator: "bob".to_string(),
            }),
            REPLY_REGISTER_RECEIVED_COINS,
        ),
        SubMsg::reply_on_success(
            CosmosMsg::Distribution(DistributionMsg::WithdrawDelegatorReward {
                validator: "charlie".to_string(),
            }),
            REPLY_REGISTER_RECEIVED_COINS,
        ),
        callback_submsg(CallbackMsg::ReinvestIfNeeded {}),
    ]);

    deps.querier.set_bank_balances(&[Coin::new(150, "uxyz")]);
    // The redelegations' and the harvest's withdrawals were registered as 50 uxyz of rewards.
    state.unlocked_coins.save(deps.as_mut().storage, &vec![Coin::new(50, "uxyz")]).unwrap();
    deps.querier.set_bonded_validators(&[]);
    let err = execute(
        deps.as_mut(),
        mock_env(),
        mock_info(MOCK_CONTRACT_ADDR, &[]),
        ExecuteMsg::Callback(CallbackMsg::ReinvestIfNeeded {}),
    )
    .unwrap_err();
    assert_eq!(err, StdError::generic_err("cannot reinvest without a bonded active validator"));

    // alice has the smallest delegation but is no longer bonded, so reinvest with bob instead.
    deps.querier.set_bonded_validators(&["bob", "charlie"]);
    let res = execute(
        deps.as_mut(),
        mock_env(),
        mock_info(MOCK_CONTRACT_ADDR, &[]),
        ExecuteMsg::Callback(CallbackMsg::ReinvestIfNeeded {}),
    )
    .unwrap();
    assert_eq!(res.messages[0], SubMsg::new(Delegation::new("bob", 45, "uxyz").to_cosmos_msg()));

    state.unlocked_coins.save(deps.as_mut().storage, &vec![]).unwrap();
    let res = execute(
        deps.as_mut(),
        mock_env(),
        mock_info(MOCK_CONTRACT_ADDR, &[]),
        ExecuteMsg::Callback(CallbackMsg::ReinvestIfNeeded {}),
    )
    .unwrap();
    assert!(res.messages.is_empty());
    assert_eq!(res.attributes, vec![cosmwasm_std::Attribute::new(
        "action",
        "steakhub/reinvest_not_needed"
    )]);
}

#[test]
fn registering_unlocked_coins() {
    let mut deps = setup_test();
    let state = State::default();

    // After withdrawing staking rewards, we parse the `coin_received` event to find the received
    // amounts
    let event = Event::new("coin_received")
        .add_attribute("receiver", MOCK_CONTRACT_ADDR.to_string())
        .add_attribute(
            "amount",
            "123ukrw,234uxyz,345uusd,69420ibc/\
             0471F1C4E7AFD3F07702BEF6DC365268D64570F7C1FDC98EA6098DD6DE59817B",
        );

    reply(deps.as_mut(), mock_env(), Reply {
        id: 2,
        result: cosmwasm_std::SubMsgResult::Ok(SubMsgResponse {
            events: vec![event],
            data: None,
        }),
    })
    .unwrap();

    // Unlocked coins in contract state should have been updated
    let unlocked_coins = state.unlocked_coins.load(deps.as_ref().storage).unwrap();
    assert_eq!(unlocked_coins, vec![
        Coin::new(123, "ukrw"),
        Coin::new(234, "uxyz"),
        Coin::new(345, "uusd"),
        Coin::new(69420, "ibc/0471F1C4E7AFD3F07702BEF6DC365268D64570F7C1FDC98EA6098DD6DE59817B",),
    ]);
}

#[test]
fn reinvesting() {
    let mut deps = setup_test();
    let state = State::default();

    deps.querier.set_staking_delegations(&[
        Delegation::new("alice", 333334, "uxyz"),
        Delegation::new("bob", 333333, "uxyz"),
        Delegation::new("charlie", 333333, "uxyz"),
    ]);
    state.prev_denom.save(deps.as_mut().storage, &Uint128::zero()).unwrap();
    deps.querier.set_bank_balances(&[Coin::new(234u128, "uxyz")]);

    // After the swaps, `unlocked_coins` should contain only uxyz and unknown denoms
    state
        .unlocked_coins
        .save(deps.as_mut().storage, &vec![
            Coin::new(234, "uxyz"),
            Coin::new(
                69420,
                "ibc/0471F1C4E7AFD3F07702BEF6DC365268D64570F7C1FDC98EA6098DD6DE59817B",
            ),
        ])
        .unwrap();

    // Bob has the smallest amount of delegations, so all proceeds go to him
    let res = execute(
        deps.as_mut(),
        mock_env(),
        mock_info(MOCK_CONTRACT_ADDR, &[]),
        ExecuteMsg::Callback(CallbackMsg::Reinvest {}),
    )
    .unwrap();

    assert_eq!(res.messages.len(), 2);
    assert_eq!(res.messages[0], SubMsg {
        id: 0,
        msg: Delegation::new("bob", 234 - 23, "uxyz").to_cosmos_msg(),
        gas_limit: None,
        reply_on: ReplyOn::Never,
    });
    let send_msg = BankMsg::Send {
        to_address: "the_fee_man".into(),
        amount: vec![Coin::new(23u128, "uxyz")],
    };
    assert_eq!(res.messages[1], SubMsg {
        id: 0,
        msg: CosmosMsg::Bank(send_msg),
        gas_limit: None,
        reply_on: ReplyOn::Never,
    });

    // Storage should have been updated
    let unlocked_coins = state.unlocked_coins.load(deps.as_ref().storage).unwrap();
    assert_eq!(unlocked_coins, vec![Coin::new(
        69420,
        "ibc/0471F1C4E7AFD3F07702BEF6DC365268D64570F7C1FDC98EA6098DD6DE59817B",
    )],);
}

#[test]
fn reinvesting_fee_split() {
    let mut deps = setup_test_fee_split();
    let state = State::default();

    deps.querier.set_staking_delegations(&[
        Delegation::new("alice", 333334, "uxyz"),
        Delegation::new("bob", 333333, "uxyz"),
        Delegation::new("charlie", 333333, "uxyz"),
    ]);
    state.prev_denom.save(deps.as_mut().storage, &Uint128::zero()).unwrap();
    deps.querier.set_bank_balances(&[Coin::new(234u128, "uxyz")]);

    // After the swaps, `unlocked_coins` should contain only uxyz and unknown denoms
    state
        .unlocked_coins
        .save(deps.as_mut().storage, &vec![
            Coin::new(234, "uxyz"),
            Coin::new(
                69420,
                "ibc/0471F1C4E7AFD3F07702BEF6DC365268D64570F7C1FDC98EA6098DD6DE59817B",
            ),
        ])
        .unwrap();

    // Bob has the smallest amount of delegations, so all proceeds go to him
    let res = execute(
        deps.as_mut(),
        mock_env(),
        mock_info(MOCK_CONTRACT_ADDR, &[]),
        ExecuteMsg::Callback(CallbackMsg::Reinvest {}),
    )
    .unwrap();

    assert_eq!(res.messages.len(), 2);
    assert_eq!(res.messages[0], SubMsg {
        id: 0,
        msg: Delegation::new("bob", 234 - 23, "uxyz").to_cosmos_msg(),
        gas_limit: None,
        reply_on: ReplyOn::Never,
    });
    let send_msg = pfc_fee_split::fee_split_msg::ExecuteMsg::Deposit {
        flush: false,
    };

    assert_eq!(res.messages[1], SubMsg {
        id: 0,
        msg: send_msg
            .into_cosmos_msg("fee_split_contract", vec![Coin::new(23u128, "uxyz")])
            .unwrap(),
        gas_limit: None,
        reply_on: ReplyOn::Never,
    });

    // Storage should have been updated
    let unlocked_coins = state.unlocked_coins.load(deps.as_ref().storage).unwrap();
    assert_eq!(unlocked_coins, vec![Coin::new(
        69420,
        "ibc/0471F1C4E7AFD3F07702BEF6DC365268D64570F7C1FDC98EA6098DD6DE59817B",
    )],);
}

#[test]
fn queuing_unbond() {
    let mut deps = setup_test();
    let state = State::default();

    // Only Steak token is accepted for unbonding requests
    let err = execute(
        deps.as_mut(),
        mock_env(),
        mock_info("random_token", &[]),
        ExecuteMsg::Receive(cw20::Cw20ReceiveMsg {
            sender: "hacker".to_string(),
            amount: Uint128::new(69420),
            msg: to_json_binary(&ReceiveMsg::QueueUnbond {
                receiver: None,
            })
            .unwrap(),
        }),
    )
    .unwrap_err();

    assert_eq!(err, StdError::generic_err("expecting Steak token, received random_token"));

    // User 1 creates an unbonding request before `est_unbond_start_time` is reached. The unbond
    // request is saved, but not the pending batch is not submitted for unbonding
    let res = execute(
        deps.as_mut(),
        mock_env_at_timestamp(12345), // est_unbond_start_time = 269200
        mock_info("steak_token", &[]),
        ExecuteMsg::Receive(cw20::Cw20ReceiveMsg {
            sender: "user_1".to_string(),
            amount: Uint128::new(23456),
            msg: to_json_binary(&ReceiveMsg::QueueUnbond {
                receiver: None,
            })
            .unwrap(),
        }),
    )
    .unwrap();

    assert_eq!(res.messages.len(), 0);

    // User 2 creates an unbonding request after `est_unbond_start_time` is reached. The unbond
    // request is saved, and the pending is automatically submitted for unbonding
    let res = execute(
        deps.as_mut(),
        mock_env_at_timestamp(269201), // est_unbond_start_time = 269200
        mock_info("steak_token", &[]),
        ExecuteMsg::Receive(cw20::Cw20ReceiveMsg {
            sender: "user_2".to_string(),
            amount: Uint128::new(69420),
            msg: to_json_binary(&ReceiveMsg::QueueUnbond {
                receiver: Some("user_3".to_string()),
            })
            .unwrap(),
        }),
    )
    .unwrap();

    assert_eq!(res.messages.len(), 1);
    assert_eq!(res.messages[0], SubMsg {
        id: REPLY_SUBMIT_BATCH,
        msg: CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: MOCK_CONTRACT_ADDR.to_string(),
            msg: to_json_binary(&ExecuteMsg::SubmitBatch {}).unwrap(),
            funds: vec![],
        }),
        gas_limit: None,
        reply_on: ReplyOn::Error,
    });

    // The users' unbonding requests should have been saved
    let ubr1 = state
        .unbond_requests
        .load(deps.as_ref().storage, (1u64, &Addr::unchecked("user_1")))
        .unwrap();
    let ubr2 = state
        .unbond_requests
        .load(deps.as_ref().storage, (1u64, &Addr::unchecked("user_3")))
        .unwrap();

    assert_eq!(ubr1, UnbondRequest {
        id: 1,
        user: Addr::unchecked("user_1"),
        shares: Uint128::new(23456),
    });
    assert_eq!(ubr2, UnbondRequest {
        id: 1,
        user: Addr::unchecked("user_3"),
        shares: Uint128::new(69420),
    });

    // Pending batch should have been updated
    let pending_batch = state.pending_batch.load(deps.as_ref().storage).unwrap();
    assert_eq!(pending_batch, PendingBatch {
        id: 1,
        usteak_to_burn: Uint128::new(92876), // 23,456 + 69,420
        est_unbond_start_time: 269200,
    });
}

#[test]
fn submitting_batch() {
    let mut deps = setup_test();
    let state = State::default();

    // Reproduce the split registry state from the drain incident: Charlie is active but absent
    // from the primary validator registry. His delegation must still be counted exactly once.
    state
        .validators
        .update(deps.as_mut().storage, |mut validators| -> Result<_, StdError> {
            validators.retain(|validator| validator != "charlie");
            Ok(validators)
        })
        .unwrap();

    // uluna bonded: 1,037,345
    // usteak supply: 1,012,043
    // uluna per ustake: 1.025
    deps.querier.set_staking_delegations(&[
        Delegation::new("alice", 345782, "uxyz"),
        Delegation::new("bob", 345782, "uxyz"),
        Delegation::new("charlie", 345781, "uxyz"),
    ]);
    deps.querier.set_cw20_total_supply("steak_token", 1012043);

    // We continue from the contract state at the end of the last test
    let unbond_requests = vec![
        UnbondRequest {
            id: 1,
            user: Addr::unchecked("user_1"),
            shares: Uint128::new(23456),
        },
        UnbondRequest {
            id: 1,
            user: Addr::unchecked("user_3"),
            shares: Uint128::new(69420),
        },
    ];

    for unbond_request in &unbond_requests {
        state
            .unbond_requests
            .save(
                deps.as_mut().storage,
                (unbond_request.id, &Addr::unchecked(unbond_request.user.clone())),
                unbond_request,
            )
            .unwrap();
    }

    state
        .pending_batch
        .save(deps.as_mut().storage, &PendingBatch {
            id: 1,
            usteak_to_burn: Uint128::new(92876), // 23,456 + 69,420
            est_unbond_start_time: 269200,
        })
        .unwrap();

    // Anyone can invoke `submit_batch`. Here we continue from the previous test and assume it is
    // invoked automatically as user 2 submits the unbonding request
    //
    // usteak to burn: 23,456 + 69,420 = 92,876
    // uluna to unbond: 1,037,345 * 92,876 / 1,012,043 = 95,197
    //
    // Target: (1,037,345 - 95,197) / 3 = 314,049
    // Remainer: 1
    // Alice:   345,782 - (314,049 + 1) = 31,732
    // Bob:     345,782 - (314,049 + 0) = 31,733
    // Charlie: 345,781 - (314,049 + 0) = 31,732
    let res = execute(
        deps.as_mut(),
        mock_env_at_timestamp(269201),
        mock_info(MOCK_CONTRACT_ADDR, &[]),
        ExecuteMsg::SubmitBatch {},
    )
    .unwrap();

    assert_eq!(res.messages.len(), 4);
    assert_eq!(
        res.messages[0],
        SubMsg::reply_on_success(
            Undelegation::new("alice", 31732, "uxyz").to_cosmos_msg(),
            REPLY_REGISTER_RECEIVED_COINS
        )
    );
    assert_eq!(
        res.messages[1],
        SubMsg::reply_on_success(
            Undelegation::new("bob", 31733, "uxyz").to_cosmos_msg(),
            REPLY_REGISTER_RECEIVED_COINS
        )
    );
    assert_eq!(
        res.messages[2],
        SubMsg::reply_on_success(
            Undelegation::new("charlie", 31732, "uxyz").to_cosmos_msg(),
            REPLY_REGISTER_RECEIVED_COINS,
        )
    );
    assert_eq!(res.messages[3], SubMsg {
        id: 0,
        msg: CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: "steak_token".to_string(),
            msg: to_json_binary(&Cw20ExecuteMsg::Burn {
                amount: Uint128::new(92876)
            })
            .unwrap(),
            funds: vec![],
        }),
        gas_limit: None,
        reply_on: ReplyOn::Never,
    });

    // A new pending batch should have been created
    let pending_batch = state.pending_batch.load(deps.as_ref().storage).unwrap();
    assert_eq!(pending_batch, PendingBatch {
        id: 2,
        usteak_to_burn: Uint128::zero(),
        est_unbond_start_time: 528401, // 269,201 + 259,200
    });

    // Previous batch should have been updated
    let previous_batch = state.previous_batches.load(deps.as_ref().storage, 1u64).unwrap();
    assert_eq!(previous_batch, Batch {
        id: 1,
        reconciled: false,
        total_shares: Uint128::new(92876),
        amount_unclaimed: Uint128::new(95197),
        est_unbond_end_time: 2083601, // 269,201 + 1,814,400
    });
}

#[test]
fn submitting_empty_batch_rolls_the_epoch_forward() {
    let mut deps = setup_test();
    let state = State::default();

    // Before the epoch ends it is still refused, as for any batch.
    let err = execute(
        deps.as_mut(),
        mock_env_at_timestamp(269_199),
        mock_info("worker", &[]),
        ExecuteMsg::SubmitBatch {},
    )
    .unwrap_err();
    assert_eq!(
        err,
        StdError::generic_err("batch can only be submitted for unbonding after 269200")
    );

    let res = execute(
        deps.as_mut(),
        mock_env_at_timestamp(269_201),
        mock_info("worker", &[]),
        ExecuteMsg::SubmitBatch {},
    )
    .unwrap();
    assert!(res.messages.is_empty());

    // Same batch id, nothing burned, next submission one epoch (259200s) from now.
    let pending = state.pending_batch.load(deps.as_ref().storage).unwrap();
    assert_eq!(pending, PendingBatch {
        id: 1,
        usteak_to_burn: Uint128::zero(),
        est_unbond_start_time: 269_201 + 259_200,
    });
}

#[test]
fn reconciling() {
    let mut deps = setup_test();
    let state = State::default();

    let previous_batches = vec![
        Batch {
            id: 1,
            reconciled: true,
            total_shares: Uint128::new(92876),
            amount_unclaimed: Uint128::new(95197), // 1.025 Luna per Steak
            est_unbond_end_time: 10000,
        },
        Batch {
            id: 2,
            reconciled: false,
            total_shares: Uint128::new(1345),
            amount_unclaimed: Uint128::new(1385), // 1.030 Luna per Steak
            est_unbond_end_time: 20000,
        },
        Batch {
            id: 3,
            reconciled: false,
            total_shares: Uint128::new(1456),
            amount_unclaimed: Uint128::new(1506), // 1.035 Luna per Steak
            est_unbond_end_time: 30000,
        },
        Batch {
            id: 4,
            reconciled: false,
            total_shares: Uint128::new(1567),
            amount_unclaimed: Uint128::new(1629), // 1.040 Luna per Steak
            est_unbond_end_time: 40000,           // not yet finished unbonding, ignored
        },
    ];

    for previous_batch in &previous_batches {
        state
            .previous_batches
            .save(deps.as_mut().storage, previous_batch.id, previous_batch)
            .unwrap();
    }

    state
        .unlocked_coins
        .save(deps.as_mut().storage, &vec![
            Coin::new(10000, "uxyz"),
            Coin::new(234, "ukrw"),
            Coin::new(345, "uusd"),
            Coin::new(
                69420,
                "ibc/0471F1C4E7AFD3F07702BEF6DC365268D64570F7C1FDC98EA6098DD6DE59817B",
            ),
        ])
        .unwrap();

    deps.querier.set_bank_balances(&[
        Coin::new(12345 + 95197, "uxyz"), // includes batch 1, reconciled but unclaimed
        Coin::new(234, "ukrw"),
        Coin::new(345, "uusd"),
        Coin::new(69420, "ibc/0471F1C4E7AFD3F07702BEF6DC365268D64570F7C1FDC98EA6098DD6DE59817B"),
    ]);

    execute(
        deps.as_mut(),
        mock_env_at_timestamp(35000),
        mock_info("worker", &[]),
        ExecuteMsg::Reconcile {},
    )
    .unwrap();

    // Expected received: batch 2 + batch 3 = 1385 + 1506 = 2891
    // Expected unlocked: 10000
    // Expected: 12891
    // Actual: 12345
    // Shortfall: 12891 - 12345 = 456
    //
    // uluna per batch: 546 / 2 = 273
    // remainder: 0
    // batch 2: 1385 - 273 = 1112
    // batch 3: 1506 - 273 = 1233
    let batch = state.previous_batches.load(deps.as_ref().storage, 2u64).unwrap();
    assert_eq!(batch, Batch {
        id: 2,
        reconciled: true,
        total_shares: Uint128::new(1345),
        amount_unclaimed: Uint128::new(1112), // 1385 - 273
        est_unbond_end_time: 20000,
    });

    let batch = state.previous_batches.load(deps.as_ref().storage, 3u64).unwrap();
    assert_eq!(batch, Batch {
        id: 3,
        reconciled: true,
        total_shares: Uint128::new(1456),
        amount_unclaimed: Uint128::new(1233), // 1506 - 273
        est_unbond_end_time: 30000,
    });

    // Batches 1 and 4 should not have changed
    let batch = state.previous_batches.load(deps.as_ref().storage, 1u64).unwrap();
    assert_eq!(batch, previous_batches[0]);

    let batch = state.previous_batches.load(deps.as_ref().storage, 4u64).unwrap();
    assert_eq!(batch, previous_batches[3]);
}

#[test]
fn withdrawing_unbonded() {
    let mut deps = setup_test();
    let state = State::default();

    // We simulate a most general case:
    // - batches 1 and 2 have finished unbonding
    // - batch 3 have been submitted for unbonding but have not finished
    // - batch 4 is still pending
    let unbond_requests = vec![
        UnbondRequest {
            id: 1,
            user: Addr::unchecked("user_1"),
            shares: Uint128::new(23456),
        },
        UnbondRequest {
            id: 1,
            user: Addr::unchecked("user_3"),
            shares: Uint128::new(69420),
        },
        UnbondRequest {
            id: 2,
            user: Addr::unchecked("user_1"),
            shares: Uint128::new(34567),
        },
        UnbondRequest {
            id: 3,
            user: Addr::unchecked("user_1"),
            shares: Uint128::new(45678),
        },
        UnbondRequest {
            id: 4,
            user: Addr::unchecked("user_1"),
            shares: Uint128::new(56789),
        },
    ];

    for unbond_request in &unbond_requests {
        state
            .unbond_requests
            .save(
                deps.as_mut().storage,
                (unbond_request.id, &Addr::unchecked(unbond_request.user.clone())),
                unbond_request,
            )
            .unwrap();
    }

    let previous_batches = vec![
        Batch {
            id: 1,
            reconciled: true,
            total_shares: Uint128::new(92876),
            amount_unclaimed: Uint128::new(95197), // 1.025 Luna per Steak
            est_unbond_end_time: 10000,
        },
        Batch {
            id: 2,
            reconciled: true,
            total_shares: Uint128::new(34567),
            amount_unclaimed: Uint128::new(35604), // 1.030 Luna per Steak
            est_unbond_end_time: 20000,
        },
        Batch {
            id: 3,
            reconciled: false, // finished unbonding, but not reconciled; ignored
            total_shares: Uint128::new(45678),
            amount_unclaimed: Uint128::new(47276), // 1.035 Luna per Steak
            est_unbond_end_time: 20000,
        },
        Batch {
            id: 4,
            reconciled: true,
            total_shares: Uint128::new(56789),
            amount_unclaimed: Uint128::new(59060), // 1.040 Luna per Steak
            est_unbond_end_time: 30000,            /* reconciled, but not yet finished unbonding;
                                                    * ignored */
        },
    ];

    for previous_batch in &previous_batches {
        state
            .previous_batches
            .save(deps.as_mut().storage, previous_batch.id, previous_batch)
            .unwrap();
    }

    state
        .pending_batch
        .save(deps.as_mut().storage, &PendingBatch {
            id: 4,
            usteak_to_burn: Uint128::new(56789),
            est_unbond_start_time: 100000,
        })
        .unwrap();

    // Attempt to withdraw before any batch has completed unbonding. Should error
    let err = execute(
        deps.as_mut(),
        mock_env_at_timestamp(5000),
        mock_info("user_1", &[]),
        ExecuteMsg::WithdrawUnbonded {
            receiver: None,
        },
    )
    .unwrap_err();

    assert_eq!(err, StdError::generic_err("withdrawable amount is zero"));

    // Attempt to withdraw once batches 1 and 2 have finished unbonding, but 3 has not yet
    //
    // Withdrawable from batch 1: 95,197 * 23,456 / 92,876 = 24,042
    // Withdrawable from batch 2: 35,604
    // Total withdrawable: 24,042 + 35,604 = 59,646
    //
    // Batch 1 should be updated:
    // Total shares: 92,876 - 23,456 = 69,420
    // Unclaimed uluna: 95,197 - 24,042 = 71,155
    //
    // Batch 2 is completely withdrawn, should be purged from storage
    let res = execute(
        deps.as_mut(),
        mock_env_at_timestamp(25000),
        mock_info("user_1", &[]),
        ExecuteMsg::WithdrawUnbonded {
            receiver: None,
        },
    )
    .unwrap();

    assert_eq!(res.messages.len(), 1);
    assert_eq!(res.messages[0], SubMsg {
        id: 0,
        msg: CosmosMsg::Bank(BankMsg::Send {
            to_address: "user_1".to_string(),
            amount: vec![Coin::new(59646, "uxyz")],
        }),
        gas_limit: None,
        reply_on: ReplyOn::Never,
    });

    // Previous batches should have been updated
    let batch = state.previous_batches.load(deps.as_ref().storage, 1u64).unwrap();
    assert_eq!(batch, Batch {
        id: 1,
        reconciled: true,
        total_shares: Uint128::new(69420),
        amount_unclaimed: Uint128::new(71155),
        est_unbond_end_time: 10000,
    });

    let err = state.previous_batches.load(deps.as_ref().storage, 2u64).unwrap_err();
    assert_eq!(err, StdError::NotFound {
        kind: "pfc_steak::hub::Batch".to_string(),
    });

    // User 1's unbond requests in batches 1 and 2 should have been deleted
    let err1 = state
        .unbond_requests
        .load(deps.as_ref().storage, (1u64, &Addr::unchecked("user_1")))
        .unwrap_err();
    let err2 = state
        .unbond_requests
        .load(deps.as_ref().storage, (1u64, &Addr::unchecked("user_1")))
        .unwrap_err();

    assert_eq!(err1, StdError::NotFound {
        kind: "pfc_steak::hub::UnbondRequest".to_string()
    });
    assert_eq!(err2, StdError::NotFound {
        kind: "pfc_steak::hub::UnbondRequest".to_string()
    });

    // User 3 attempt to withdraw; also specifying a receiver
    let res = execute(
        deps.as_mut(),
        mock_env_at_timestamp(25000),
        mock_info("user_3", &[]),
        ExecuteMsg::WithdrawUnbonded {
            receiver: Some("user_2".to_string()),
        },
    )
    .unwrap();

    assert_eq!(res.messages.len(), 1);
    assert_eq!(res.messages[0], SubMsg {
        id: 0,
        msg: CosmosMsg::Bank(BankMsg::Send {
            to_address: "user_2".to_string(),
            amount: vec![Coin::new(71155, "uxyz")],
        }),
        gas_limit: None,
        reply_on: ReplyOn::Never,
    });

    // Batch 1 and user 2's unbonding request should have been purged from storage
    let err = state.previous_batches.load(deps.as_ref().storage, 1u64).unwrap_err();
    assert_eq!(err, StdError::NotFound {
        kind: "pfc_steak::hub::Batch".to_string()
    });

    let err = state
        .unbond_requests
        .load(deps.as_ref().storage, (1u64, &Addr::unchecked("user_3")))
        .unwrap_err();

    assert_eq!(err, StdError::NotFound {
        kind: "pfc_steak::hub::UnbondRequest".to_string()
    });
}

#[test]
fn adding_validator() {
    let mut deps = setup_test();
    let state = State::default();

    let err =
        execute(deps.as_mut(), mock_env(), mock_info("jake", &[]), ExecuteMsg::AddValidator {
            validator: "dave".to_string(),
        })
        .unwrap_err();

    assert_eq!(err, StdError::generic_err("unauthorized: sender is not owner"));

    let err =
        execute(deps.as_mut(), mock_env(), mock_info("larry", &[]), ExecuteMsg::AddValidator {
            validator: "alice".to_string(),
        })
        .unwrap_err();

    assert_eq!(err, StdError::generic_err("validator is already whitelisted"));

    let res =
        execute(deps.as_mut(), mock_env(), mock_info("larry", &[]), ExecuteMsg::AddValidator {
            validator: "dave".to_string(),
        })
        .unwrap();

    assert_eq!(res.messages.len(), 0);

    let validators = state.validators.load(deps.as_ref().storage).unwrap();
    assert_eq!(validators, vec![
        String::from("alice"),
        String::from("bob"),
        String::from("charlie"),
        String::from("dave"),
    ],);
}

#[test]
fn removing_validator() {
    let mut deps = setup_test();
    let state = State::default();

    deps.querier.set_staking_delegations(&[
        Delegation::new("alice", 341667, "uxyz"),
        Delegation::new("bob", 341667, "uxyz"),
        Delegation::new("charlie", 341666, "uxyz"),
    ]);

    let err =
        execute(deps.as_mut(), mock_env(), mock_info("jake", &[]), ExecuteMsg::RemoveValidator {
            validator: "charlie".to_string(),
        })
        .unwrap_err();

    assert_eq!(err, StdError::generic_err("unauthorized: sender is not owner"));

    let err =
        execute(deps.as_mut(), mock_env(), mock_info("larry", &[]), ExecuteMsg::RemoveValidator {
            validator: "dave".to_string(),
        })
        .unwrap_err();

    assert_eq!(err, StdError::generic_err("validator is not already whitelisted"));

    // Target: (341667 + 341667 + 341666) / 2 = 512500
    // Remainder: 0
    // Alice:   512500 + 0 - 341667 = 170833
    // Bob:     512500 + 0 - 341667 = 170833
    let res =
        execute(deps.as_mut(), mock_env(), mock_info("larry", &[]), ExecuteMsg::RemoveValidator {
            validator: "charlie".to_string(),
        })
        .unwrap();

    assert_eq!(res.messages.len(), 2);
    assert_eq!(
        res.messages[0],
        SubMsg::reply_on_success(
            Redelegation::new("charlie", "alice", 170833, "uxyz").to_cosmos_msg(),
            REPLY_REGISTER_RECEIVED_COINS,
        ),
    );
    assert_eq!(
        res.messages[1],
        SubMsg::reply_on_success(
            Redelegation::new("charlie", "bob", 170833, "uxyz").to_cosmos_msg(),
            REPLY_REGISTER_RECEIVED_COINS,
        ),
    );

    let validators = state.validators.load(deps.as_ref().storage).unwrap();
    assert_eq!(validators, vec![String::from("alice"), String::from("bob")],);
    let validators_active = state.validators_active.load(deps.as_ref().storage).unwrap();
    assert!(!validators_active.contains(&"charlie".to_string()));

    let err =
        execute(deps.as_mut(), mock_env(), mock_info("larry", &[]), ExecuteMsg::UnPauseValidator {
            validator: "charlie".to_string(),
        })
        .unwrap_err();
    assert_eq!(err, StdError::generic_err("validator is not in the delegation set"));
}

#[test]
fn transferring_ownership() {
    let mut deps = setup_test();
    let state = State::default();

    let err =
        execute(deps.as_mut(), mock_env(), mock_info("jake", &[]), ExecuteMsg::TransferOwnership {
            new_owner: "jake".to_string(),
        })
        .unwrap_err();

    assert_eq!(err, StdError::generic_err("unauthorized: sender is not owner"));

    let res = execute(
        deps.as_mut(),
        mock_env(),
        mock_info("larry", &[]),
        ExecuteMsg::TransferOwnership {
            new_owner: "jake".to_string(),
        },
    )
    .unwrap();

    assert_eq!(res.messages.len(), 0);

    let owner = state.owner.load(deps.as_ref().storage).unwrap();
    assert_eq!(owner, Addr::unchecked("larry"));

    let err = execute(
        deps.as_mut(),
        mock_env(),
        mock_info("pumpkin", &[]),
        ExecuteMsg::AcceptOwnership {},
    )
    .unwrap_err();

    assert_eq!(err, StdError::generic_err("unauthorized: sender is not new owner"));

    let res =
        execute(deps.as_mut(), mock_env(), mock_info("jake", &[]), ExecuteMsg::AcceptOwnership {})
            .unwrap();

    assert_eq!(res.messages.len(), 0);

    let owner = state.owner.load(deps.as_ref().storage).unwrap();
    assert_eq!(owner, Addr::unchecked("jake"));
}

#[test]
fn splitting_fees() {
    let mut deps = setup_test();

    let err = execute(
        deps.as_mut(),
        mock_env(),
        mock_info("jake", &[]),
        ExecuteMsg::TransferFeeAccount {
            fee_account_type: "Wallet".to_string(),
            new_fee_account: "charlie".to_string(),
        },
    )
    .unwrap_err();

    assert_eq!(err, StdError::generic_err("unauthorized: sender is not owner"));

    let err = execute(
        deps.as_mut(),
        mock_env(),
        mock_info("larry", &[]),
        ExecuteMsg::TransferFeeAccount {
            fee_account_type: "xxxx".to_string(),
            new_fee_account: "charlie".to_string(),
        },
    )
    .unwrap_err();

    assert_eq!(err, StdError::generic_err("Invalid Fee type: Wallet or FeeSplit only"));

    execute(deps.as_mut(), mock_env(), mock_info("larry", &[]), ExecuteMsg::TransferFeeAccount {
        fee_account_type: "Wallet".to_string(),
        new_fee_account: "charlie".to_string(),
    })
    .unwrap();
    let res: ConfigResponse = query_helper(deps.as_ref(), QueryMsg::Config {});
    assert_eq!(res, ConfigResponse {
        owner: "larry".to_string(),
        new_owner: None,
        steak_token: "steak_token".to_string(),
        epoch_period: 259200,
        unbond_period: 1814400,
        denom: "uxyz".to_string(),
        fee_type: "Wallet".to_string(),
        fee_account: "charlie".to_string(),
        fee_rate: Decimal::from_ratio(10_u128, 100_u128),
        max_fee_rate: Decimal::from_ratio(20_u128, 100_u128),
        validators: vec!["alice".to_string(), "bob".to_string(), "charlie".to_string(),],
        paused_validators: vec![],
        dust_collector: None,
        token_factory: None
    });

    execute(deps.as_mut(), mock_env(), mock_info("larry", &[]), ExecuteMsg::TransferFeeAccount {
        fee_account_type: "FeeSplit".to_string(),
        new_fee_account: "contract".to_string(),
    })
    .unwrap();
    let res: ConfigResponse = query_helper(deps.as_ref(), QueryMsg::Config {});
    assert_eq!(res, ConfigResponse {
        owner: "larry".to_string(),
        new_owner: None,
        steak_token: "steak_token".to_string(),
        epoch_period: 259200,
        unbond_period: 1814400,
        denom: "uxyz".to_string(),
        fee_type: "FeeSplit".to_string(),
        fee_account: "contract".to_string(),
        fee_rate: Decimal::from_ratio(10_u128, 100_u128),
        max_fee_rate: Decimal::from_ratio(20_u128, 100_u128),
        validators: vec!["alice".to_string(), "bob".to_string(), "charlie".to_string(),],
        paused_validators: vec![],
        dust_collector: None,
        token_factory: None
    });
}
//--------------------------------------------------------------------------------------------------
// Queries
//--------------------------------------------------------------------------------------------------

#[test]
fn querying_previous_batches() {
    let mut deps = mock_dependencies();

    let batches = vec![
        Batch {
            id: 1,
            reconciled: false,
            total_shares: Uint128::new(123),
            amount_unclaimed: Uint128::new(678),
            est_unbond_end_time: 10000,
        },
        Batch {
            id: 2,
            reconciled: true,
            total_shares: Uint128::new(234),
            amount_unclaimed: Uint128::new(789),
            est_unbond_end_time: 15000,
        },
        Batch {
            id: 3,
            reconciled: false,
            total_shares: Uint128::new(345),
            amount_unclaimed: Uint128::new(890),
            est_unbond_end_time: 20000,
        },
        Batch {
            id: 4,
            reconciled: true,
            total_shares: Uint128::new(456),
            amount_unclaimed: Uint128::new(999),
            est_unbond_end_time: 25000,
        },
    ];

    let state = State::default();
    for batch in &batches {
        state.previous_batches.save(deps.as_mut().storage, batch.id, batch).unwrap();
    }

    // Querying a single batch
    let res: Batch = query_helper(deps.as_ref(), QueryMsg::PreviousBatch(1));
    assert_eq!(res, batches[0].clone());

    let res: Batch = query_helper(deps.as_ref(), QueryMsg::PreviousBatch(2));
    assert_eq!(res, batches[1].clone());

    // Query multiple batches
    let res: Vec<Batch> = query_helper(deps.as_ref(), QueryMsg::PreviousBatches {
        start_after: None,
        limit: None,
    });
    assert_eq!(res, batches.clone());

    let res: Vec<Batch> = query_helper(deps.as_ref(), QueryMsg::PreviousBatches {
        start_after: Some(1),
        limit: None,
    });
    assert_eq!(res, vec![batches[1].clone(), batches[2].clone(), batches[3].clone()]);

    let res: Vec<Batch> = query_helper(deps.as_ref(), QueryMsg::PreviousBatches {
        start_after: Some(4),
        limit: None,
    });
    assert_eq!(res, vec![]);

    // Query multiple batches, indexed by whether it has been reconciled
    let res = state
        .previous_batches
        .idx
        .reconciled
        .prefix(true.into())
        .range(deps.as_ref().storage, None, None, Order::Ascending)
        .map(|item| {
            let (_, v) = item.unwrap();
            v
        })
        .collect::<Vec<_>>();

    assert_eq!(res, vec![batches[1].clone(), batches[3].clone()]);

    let res = state
        .previous_batches
        .idx
        .reconciled
        .prefix(false.into())
        .range(deps.as_ref().storage, None, None, Order::Ascending)
        .map(|item| {
            let (_, v) = item.unwrap();
            v
        })
        .collect::<Vec<_>>();

    assert_eq!(res, vec![batches[0].clone(), batches[2].clone()]);
}

#[test]
fn querying_unbond_requests() {
    let mut deps = mock_dependencies();
    let state = State::default();

    let unbond_requests = vec![
        UnbondRequest {
            id: 1,
            user: Addr::unchecked("alice"),
            shares: Uint128::new(123),
        },
        UnbondRequest {
            id: 1,
            user: Addr::unchecked("bob"),
            shares: Uint128::new(234),
        },
        UnbondRequest {
            id: 1,
            user: Addr::unchecked("charlie"),
            shares: Uint128::new(345),
        },
        UnbondRequest {
            id: 2,
            user: Addr::unchecked("alice"),
            shares: Uint128::new(456),
        },
    ];

    for unbond_request in &unbond_requests {
        state
            .unbond_requests
            .save(
                deps.as_mut().storage,
                (unbond_request.id, &Addr::unchecked(unbond_request.user.clone())),
                unbond_request,
            )
            .unwrap();
    }

    let res: Vec<UnbondRequestsByBatchResponseItem> =
        query_helper(deps.as_ref(), QueryMsg::UnbondRequestsByBatch {
            id: 1,
            start_after: None,
            limit: None,
        });
    assert_eq!(res, vec![
        unbond_requests[0].clone().into(),
        unbond_requests[1].clone().into(),
        unbond_requests[2].clone().into(),
    ]);

    let res: Vec<UnbondRequestsByBatchResponseItem> =
        query_helper(deps.as_ref(), QueryMsg::UnbondRequestsByBatch {
            id: 2,
            start_after: None,
            limit: None,
        });
    assert_eq!(res, vec![unbond_requests[3].clone().into()]);

    let res: Vec<UnbondRequestsByUserResponseItem> =
        query_helper(deps.as_ref(), QueryMsg::UnbondRequestsByUser {
            user: "alice".to_string(),
            start_after: None,
            limit: None,
        });
    assert_eq!(res, vec![unbond_requests[0].clone().into(), unbond_requests[3].clone().into(),]);

    let res: Vec<UnbondRequestsByUserResponseItem> =
        query_helper(deps.as_ref(), QueryMsg::UnbondRequestsByUser {
            user: "alice".to_string(),
            start_after: Some(2),
            limit: None,
        });
    assert_eq!(res, vec![unbond_requests[3].clone().into()]);
}

//--------------------------------------------------------------------------------------------------
// Delegations
//--------------------------------------------------------------------------------------------------

#[test]
fn computing_undelegations() {
    let current_delegations = [
        Delegation::new("alice", 400, "uxyz"),
        Delegation::new("bob", 300, "uxyz"),
        Delegation::new("charlie", 200, "uxyz"),
    ];

    // Target: (400 + 300 + 200 - 451) / 3 = 149
    // Remainder: 2
    // Alice:   400 - (149 + 1) = 250
    // Bob:     300 - (149 + 1) = 150
    // Charlie: 200 - (149 + 0) = 51
    let new_undelegations = compute_undelegations(Uint128::new(451), &current_delegations, "uxyz");
    let expected = vec![
        Undelegation::new("alice", 250, "uxyz"),
        Undelegation::new("bob", 150, "uxyz"),
        Undelegation::new("charlie", 51, "uxyz"),
    ];
    assert_eq!(new_undelegations, expected);
}

#[test]
fn computing_redelegations_for_removal() {
    let current_delegations = [
        Delegation::new("alice", 13000, "uxyz"),
        Delegation::new("bob", 12000, "uxyz"),
        Delegation::new("charlie", 11000, "uxyz"),
        Delegation::new("dave", 10000, "uxyz"),
    ];

    // Suppose Dave will be removed
    // uluna_per_validator = (13000 + 12000 + 11000 + 10000) / 3 = 15333
    // remainder = 1
    // to Alice:   15333 + 1 - 13000 = 2334
    // to Bob:     15333 + 0 - 12000 = 3333
    // to Charlie: 15333 + 0 - 11000 = 4333
    let expected = vec![
        Redelegation::new("dave", "alice", 2334, "uxyz"),
        Redelegation::new("dave", "bob", 3333, "uxyz"),
        Redelegation::new("dave", "charlie", 4333, "uxyz"),
    ];

    assert_eq!(
        compute_redelegations_for_removal(
            &current_delegations[3],
            &current_delegations[..3],
            "uxyz",
        ),
        expected,
    );
}

#[test]
fn computing_redelegations_for_rebalancing() {
    let current_delegations = vec![
        Delegation::new("alice", 69420, "uxyz"),
        Delegation::new("bob", 1234, "uxyz"),
        Delegation::new("charlie", 88888, "uxyz"),
        Delegation::new("dave", 40471, "uxyz"),
        Delegation::new("evan", 2345, "uxyz"),
    ];
    let active_validators: Vec<String> = vec![
        "alice".to_string(),
        "bob".to_string(),
        "charlie".to_string(),
        "dave".to_string(),
        "evan".to_string(),
    ];
    // uluna_per_validator = (69420 + 88888 + 1234 + 40471 + 2345) / 4 = 40471
    // remainer = 3
    // src_delegations:
    //  - alice:   69420 - (40471 + 1) = 28948
    //  - charlie: 88888 - (40471 + 1) = 48416
    // dst_delegations:
    //  - bob:     (40471 + 1) - 1234  = 39238
    //  - evan:    (40471 + 0) - 2345  = 38126
    //
    // Round 1: alice --(28948)--> bob
    // src_delegations:
    //  - charlie: 48416
    // dst_delegations:
    //  - bob:     39238 - 28948 = 10290
    //  - evan:    38126
    //
    // Round 2: charlie --(10290)--> bob
    // src_delegations:
    //  - charlie: 48416 - 10290 = 38126
    // dst_delegations:
    //  - evan:    38126
    //
    // Round 3: charlie --(38126)--> evan
    // Queues are emptied
    let expected = vec![
        Redelegation::new("alice", "bob", 28948, "uxyz"),
        Redelegation::new("charlie", "bob", 10290, "uxyz"),
        Redelegation::new("charlie", "evan", 38126, "uxyz"),
    ];

    assert_eq!(
        compute_redelegations_for_rebalancing(
            active_validators,
            &current_delegations,
            Uint128::from(10u64),
            "uxyz"
        ),
        expected,
    );

    let partially_active =
        vec!["alice".to_string(), "charlie".to_string(), "dave".to_string(), "evan".to_string()];

    // bob is not a target, so his 1234 is drained in full (it used to be left behind because it
    // was below the per-validator target).
    // per target = 202358 / 4 = 50589, remainder 2: alice 50590, charlie 50590, dave/evan 50589
    // sources: bob 1234 (all), alice 18830, charlie 38298; destinations: dave 10118, evan 48244
    let partially_expected = vec![
        Redelegation::new("bob", "dave", 1234, "uxyz"),
        Redelegation::new("alice", "dave", 8884, "uxyz"),
        Redelegation::new("alice", "evan", 9946, "uxyz"),
        Redelegation::new("charlie", "evan", 38298, "uxyz"),
    ];
    assert_eq!(
        compute_redelegations_for_rebalancing(
            partially_active.clone(),
            &current_delegations,
            Uint128::from(10u64),
            "uxyz"
        ),
        partially_expected,
    );

    // dave's 10118 shortfall is under the minimum, so only evan receives; bob is still drained.
    let partially_expected_minimums = vec![
        Redelegation::new("bob", "evan", 1234, "uxyz"),
        Redelegation::new("alice", "evan", 18830, "uxyz"),
        Redelegation::new("charlie", "evan", 28180, "uxyz"),
    ];
    assert_eq!(
        compute_redelegations_for_rebalancing(
            partially_active,
            &current_delegations,
            Uint128::from(15_000u64),
            "uxyz"
        ),
        partially_expected_minimums,
    );

    // A target with no delegation at all is topped up too.
    assert_eq!(
        compute_redelegations_for_rebalancing(
            vec!["alice".to_string(), "frank".to_string()],
            &[Delegation::new("alice", 1000, "uxyz")],
            Uint128::zero(),
            "uxyz"
        ),
        vec![Redelegation::new("alice", "frank", 500, "uxyz")],
    );
}

//--------------------------------------------------------------------------------------------------
// Coins
//--------------------------------------------------------------------------------------------------

#[test]
fn parsing_coin() {
    let coin = parse_coin("12345uatom").unwrap();
    assert_eq!(coin, Coin::new(12345, "uatom"));

    let coin =
        parse_coin("23456ibc/0471F1C4E7AFD3F07702BEF6DC365268D64570F7C1FDC98EA6098DD6DE59817B")
            .unwrap();
    assert_eq!(
        coin,
        Coin::new(23456, "ibc/0471F1C4E7AFD3F07702BEF6DC365268D64570F7C1FDC98EA6098DD6DE59817B",)
    );

    let err = parse_coin("69420").unwrap_err();
    assert_eq!(err, StdError::generic_err("failed to parse coin: 69420"));

    let err = parse_coin("ngmi").unwrap_err();
    assert_eq!(err, StdError::generic_err("Parsing u128: cannot parse integer from empty string"));

    assert!(parse_coin("12💥uatom").is_err());
}

#[test]
fn parsing_coins() {
    let coins = Coins::from_str("").unwrap();
    assert_eq!(coins.0, vec![]);

    let coins = Coins::from_str("12345uatom").unwrap();
    assert_eq!(coins.0, vec![Coin::new(12345, "uatom")]);

    let coins = Coins::from_str("12345uatom,23456uxyz").unwrap();
    assert_eq!(coins.0, vec![Coin::new(12345, "uatom"), Coin::new(23456, "uxyz")]);
}

#[test]
fn adding_coins() {
    let mut coins = Coins(vec![]);

    coins.add(&Coin::new(12345, "uatom")).unwrap();
    assert_eq!(coins.0, vec![Coin::new(12345, "uatom")]);

    coins.add(&Coin::new(23456, "uxyz")).unwrap();
    assert_eq!(coins.0, vec![Coin::new(12345, "uatom"), Coin::new(23456, "uxyz")]);

    coins.add_many(&Coins::from_str("76543uatom,69420uusd").unwrap()).unwrap();
    assert_eq!(coins.0, vec![
        Coin::new(88888, "uatom"),
        Coin::new(23456, "uxyz"),
        Coin::new(69420, "uusd"),
    ]);
}

#[test]
fn receiving_funds() {
    let err = parse_received_fund(&[], "uxyz").unwrap_err();
    assert_eq!(err, StdError::generic_err("must deposit exactly one coin; received 0"));

    let err = parse_received_fund(&[Coin::new(12345, "uatom"), Coin::new(23456, "uxyz")], "uxyz")
        .unwrap_err();
    assert_eq!(err, StdError::generic_err("must deposit exactly one coin; received 2"));

    let err = parse_received_fund(&[Coin::new(12345, "uatom")], "uxyz").unwrap_err();
    assert_eq!(err, StdError::generic_err("expected uxyz deposit, received uatom"));

    let err = parse_received_fund(&[Coin::new(0, "uxyz")], "uxyz").unwrap_err();
    assert_eq!(err, StdError::generic_err("deposit amount must be non-zero"));

    let amount = parse_received_fund(&[Coin::new(69420, "uxyz")], "uxyz").unwrap();
    assert_eq!(amount, Uint128::new(69420));
}

#[test]
fn reconciling_underflow() {
    let mut deps = setup_test();
    let state = State::default();
    let previous_batches = vec![
        Batch {
            id: 1,
            reconciled: true,
            total_shares: Uint128::new(92876),
            amount_unclaimed: Uint128::new(95197), // 1.025 Token per Stake
            est_unbond_end_time: 10000,
        },
        Batch {
            id: 2,
            reconciled: false,
            total_shares: Uint128::new(1345),
            amount_unclaimed: Uint128::new(1385), // 1.030 Token per Stake
            est_unbond_end_time: 20000,
        },
        Batch {
            id: 3,
            reconciled: false,
            total_shares: Uint128::new(1456),
            amount_unclaimed: Uint128::new(1506), // 1.035 Token per Stake
            est_unbond_end_time: 30000,
        },
        Batch {
            id: 4,
            reconciled: false,
            total_shares: Uint128::new(1),
            amount_unclaimed: Uint128::new(1),
            est_unbond_end_time: 30001,
        },
    ];
    for previous_batch in &previous_batches {
        state
            .previous_batches
            .save(deps.as_mut().storage, previous_batch.id, previous_batch)
            .unwrap();
    }
    state
        .unlocked_coins
        .save(deps.as_mut().storage, &vec![
            Coin::new(10000, "uatom"),
            Coin::new(234, "ukrw"),
            Coin::new(345, "uusd"),
            Coin::new(
                69420,
                "ibc/0471F1C4E7AFD3F07702BEF6DC365268D64570F7C1FDC98EA6098DD6DE59817B",
            ),
        ])
        .unwrap();
    deps.querier.set_bank_balances(&[
        Coin::new(12345, "uatom"),
        Coin::new(234, "ukrw"),
        Coin::new(345, "uusd"),
        Coin::new(69420, "ibc/0471F1C4E7AFD3F07702BEF6DC365268D64570F7C1FDC98EA6098DD6DE59817B"),
    ]);
    execute(
        deps.as_mut(),
        mock_env_at_timestamp(35000),
        mock_info("worker", &[]),
        ExecuteMsg::Reconcile {},
    )
    .unwrap();
}

#[test]
fn reconciling_underflow_second() {
    let mut deps = setup_test();
    let state = State::default();
    let previous_batches = vec![
        Batch {
            id: 1,
            reconciled: true,
            total_shares: Uint128::new(92876),
            amount_unclaimed: Uint128::new(95197), // 1.025 Token per Stake
            est_unbond_end_time: 10000,
        },
        Batch {
            id: 2,
            reconciled: false,
            total_shares: Uint128::new(1345),
            amount_unclaimed: Uint128::new(1385), // 1.030 Token per Stake
            est_unbond_end_time: 20000,
        },
        Batch {
            id: 3,
            reconciled: false,
            total_shares: Uint128::new(176),
            amount_unclaimed: Uint128::new(183), // 1.035 Token per Stake
            est_unbond_end_time: 30000,
        },
        Batch {
            id: 4,
            reconciled: false,
            total_shares: Uint128::new(1),
            amount_unclaimed: Uint128::new(1),
            est_unbond_end_time: 30001,
        },
    ];
    for previous_batch in &previous_batches {
        state
            .previous_batches
            .save(deps.as_mut().storage, previous_batch.id, previous_batch)
            .unwrap();
    }
    state
        .unlocked_coins
        .save(deps.as_mut().storage, &vec![
            Coin::new(10000, "uatom"),
            Coin::new(234, "ukrw"),
            Coin::new(345, "uusd"),
            Coin::new(
                69420,
                "ibc/0471F1C4E7AFD3F07702BEF6DC365268D64570F7C1FDC98EA6098DD6DE59817B",
            ),
        ])
        .unwrap();
    deps.querier.set_bank_balances(&[
        Coin::new(12345 - 1323, "uatom"),
        Coin::new(234, "ukrw"),
        Coin::new(345, "uusd"),
        Coin::new(69420, "ibc/0471F1C4E7AFD3F07702BEF6DC365268D64570F7C1FDC98EA6098DD6DE59817B"),
    ]);
    execute(
        deps.as_mut(),
        mock_env_at_timestamp(35000),
        mock_info("worker", &[]),
        ExecuteMsg::Reconcile {},
    )
    .unwrap();
}

#[test]
fn redelegating_inactive_validators() {
    let mut deps = setup_test();
    deps.querier.set_bonded_validators(&["alice", "bob", "charlie"]);

    // charlie is paused (whitelisted, inactive); dave was removed without redelegation and still
    // holds stake. alice and bob are the active set.
    execute(deps.as_mut(), mock_env(), mock_info("larry", &[]), ExecuteMsg::PauseValidator {
        validator: "charlie".to_string(),
    })
    .unwrap();
    deps.querier.set_staking_delegations(&[
        Delegation::new("alice", 100, "uxyz"),
        Delegation::new("bob", 300, "uxyz"),
        Delegation::new("charlie", 200, "uxyz"),
        Delegation::new("dave", 50, "uxyz"),
    ]);

    let err = execute(
        deps.as_mut(),
        mock_env(),
        mock_info("jake", &[]),
        ExecuteMsg::RedelegateInactive {
            limit: None,
            exclude: None,
        },
    )
    .unwrap_err();
    assert_eq!(err, StdError::generic_err("unauthorized: sender is not owner"));

    for limit in [0, REDELEGATE_INACTIVE_MAX_LIMIT + 1] {
        let err = execute(
            deps.as_mut(),
            mock_env(),
            mock_info("larry", &[]),
            ExecuteMsg::RedelegateInactive {
                limit: Some(limit),
                exclude: None,
            },
        )
        .unwrap_err();
        assert_eq!(err, StdError::generic_err("limit must be between 1 and 50"));
    }

    // Full plan: charlie->alice 200 (alice 300, bob 300), then dave's 50 split 25/25.
    // limit 2 cuts the run short; the remainder is picked up by the next call.
    let res = execute(
        deps.as_mut(),
        mock_env(),
        mock_info("larry", &[]),
        ExecuteMsg::RedelegateInactive {
            limit: Some(2),
            exclude: None,
        },
    )
    .unwrap();
    assert_eq!(res.messages, vec![
        SubMsg {
            id: REPLY_REDELEGATE_INACTIVE_BASE,
            msg: Redelegation::new("charlie", "alice", 200, "uxyz").to_cosmos_msg(),
            gas_limit: None,
            reply_on: ReplyOn::Always,
        },
        SubMsg {
            id: REPLY_REDELEGATE_INACTIVE_BASE + 1,
            msg: Redelegation::new("dave", "alice", 25, "uxyz").to_cosmos_msg(),
            gas_limit: None,
            reply_on: ReplyOn::Always,
        },
        callback_submsg(CallbackMsg::HarvestAfterRedelegation {}),
    ]);
    let summary = res.events.iter().find(|e| e.ty == "steakhub/redelegated_inactive").unwrap();
    let attr = |key: &str| {
        summary.attributes.iter().find(|a| a.key == key).map(|a| a.value.clone()).unwrap()
    };
    assert_eq!(attr("amount_planned"), "225");
    assert_eq!(attr("inactive_before"), "250");
    assert_eq!(attr("inactive_unplanned"), "25");

    // Restart: charlie's move went through but dave's was refused (e.g. a transitive
    // redelegation). The next call plans only what is still on inactive validators.
    deps.querier.set_staking_delegations(&[
        Delegation::new("alice", 300, "uxyz"),
        Delegation::new("bob", 300, "uxyz"),
        Delegation::new("dave", 50, "uxyz"),
    ]);
    let res = execute(
        deps.as_mut(),
        mock_env(),
        mock_info("larry", &[]),
        ExecuteMsg::RedelegateInactive {
            limit: None,
            exclude: None,
        },
    )
    .unwrap();
    assert_eq!(res.messages, vec![
        SubMsg {
            id: REPLY_REDELEGATE_INACTIVE_BASE,
            msg: Redelegation::new("dave", "alice", 25, "uxyz").to_cosmos_msg(),
            gas_limit: None,
            reply_on: ReplyOn::Always,
        },
        SubMsg {
            id: REPLY_REDELEGATE_INACTIVE_BASE + 1,
            msg: Redelegation::new("dave", "bob", 25, "uxyz").to_cosmos_msg(),
            gas_limit: None,
            reply_on: ReplyOn::Always,
        },
        callback_submsg(CallbackMsg::HarvestAfterRedelegation {}),
    ]);

    // Nothing left on inactive validators: re-running is a harmless no-op.
    let state = State::default();
    state.prev_denom.save(deps.as_mut().storage, &Uint128::new(123)).unwrap();
    deps.querier.set_staking_delegations(&[
        Delegation::new("alice", 325, "uxyz"),
        Delegation::new("bob", 325, "uxyz"),
    ]);
    let res = execute(
        deps.as_mut(),
        mock_env(),
        mock_info("larry", &[]),
        ExecuteMsg::RedelegateInactive {
            limit: None,
            exclude: None,
        },
    )
    .unwrap();
    assert!(res.messages.is_empty());
    assert_eq!(state.prev_denom.load(deps.as_ref().storage).unwrap(), Uint128::new(123));
}

#[test]
fn refused_inactive_redelegation_is_skipped() {
    let mut deps = setup_test();

    let res = reply(deps.as_mut(), mock_env(), Reply {
        id: REPLY_REDELEGATE_INACTIVE_BASE + 1,
        result: cosmwasm_std::SubMsgResult::Err(
            "redelegation to this validator already in progress; first redelegation to this \
             validator must complete before next redelegation"
                .to_string(),
        ),
    })
    .unwrap();
    assert!(res.messages.is_empty());
    assert_eq!(res.events, vec![
        Event::new("steakhub/redelegation_skipped").add_attribute("index", "1").add_attribute(
            "reason",
            "redelegation to this validator already in progress; first redelegation to this \
             validator must complete before next redelegation",
        )
    ]);

    // Success registers withdrawn rewards like other redelegations; with no coin_received events
    // there is nothing to register.
    let res = reply(deps.as_mut(), mock_env(), Reply {
        id: REPLY_REDELEGATE_INACTIVE_BASE,
        result: cosmwasm_std::SubMsgResult::Ok(SubMsgResponse {
            events: vec![],
            data: None,
        }),
    })
    .unwrap();
    assert!(res.events.is_empty());
}

#[test]
fn computing_redelegations_for_inactive() {
    let active = vec!["alice".to_string(), "bob".to_string()];
    let delegations = [
        Delegation::new("alice", 100, "uxyz"),
        Delegation::new("bob", 300, "uxyz"),
        Delegation::new("charlie", 200, "uxyz"),
        Delegation::new("dave", 50, "uxyz"),
    ];

    assert_eq!(
        compute_redelegations_for_inactive(&active, &active, &delegations, 10, "uxyz"),
        vec![
            Redelegation::new("charlie", "alice", 200, "uxyz"),
            Redelegation::new("dave", "alice", 25, "uxyz"),
            Redelegation::new("dave", "bob", 25, "uxyz"),
        ]
    );
    assert_eq!(
        compute_redelegations_for_inactive(&active, &active, &delegations, 1, "uxyz"),
        vec![Redelegation::new("charlie", "alice", 200, "uxyz"),]
    );
    // An active validator with no delegation yet is still a target.
    let active = vec!["alice".to_string(), "erin".to_string()];
    let delegations = [Delegation::new("alice", 100, "uxyz"), Delegation::new("dave", 100, "uxyz")];
    assert_eq!(
        compute_redelegations_for_inactive(&active, &active, &delegations, 10, "uxyz"),
        vec![Redelegation::new("dave", "erin", 100, "uxyz"),]
    );
    assert!(compute_redelegations_for_inactive(&[], &[], &delegations, 10, "uxyz").is_empty());
}

#[test]
fn redelegating_inactive_respects_exclude_and_bonded_set() {
    let mut deps = setup_test();
    execute(deps.as_mut(), mock_env(), mock_info("larry", &[]), ExecuteMsg::PauseValidator {
        validator: "charlie".to_string(),
    })
    .unwrap();
    deps.querier.set_staking_delegations(&[
        Delegation::new("alice", 100, "uxyz"),
        Delegation::new("bob", 300, "uxyz"),
        Delegation::new("charlie", 200, "uxyz"),
        Delegation::new("dave", 50, "uxyz"),
    ]);

    // Nothing in the active set is bonded on chain: refuse rather than move stake nowhere useful.
    deps.querier.set_bonded_validators(&[]);
    let err = execute(
        deps.as_mut(),
        mock_env(),
        mock_info("larry", &[]),
        ExecuteMsg::RedelegateInactive {
            limit: None,
            exclude: None,
        },
    )
    .unwrap_err();
    assert_eq!(
        err,
        StdError::generic_err("no active validator is currently bonded on chain to redelegate to")
    );

    // bob is active but not bonded (e.g. jailed): he receives nothing. charlie is excluded as a
    // source (e.g. blocked by an incoming redelegation), so only dave's stake moves, all to alice.
    deps.querier.set_bonded_validators(&["alice", "charlie"]);
    let res = execute(
        deps.as_mut(),
        mock_env(),
        mock_info("larry", &[]),
        ExecuteMsg::RedelegateInactive {
            limit: None,
            exclude: Some(vec!["charlie".to_string()]),
        },
    )
    .unwrap();
    assert_eq!(res.messages, vec![
        SubMsg {
            id: REPLY_REDELEGATE_INACTIVE_BASE,
            msg: Redelegation::new("dave", "alice", 50, "uxyz").to_cosmos_msg(),
            gas_limit: None,
            reply_on: ReplyOn::Always,
        },
        callback_submsg(CallbackMsg::HarvestAfterRedelegation {}),
    ]);
    let summary = res.events.iter().find(|e| e.ty == "steakhub/redelegated_inactive").unwrap();
    let attr = |key: &str| {
        summary.attributes.iter().find(|a| a.key == key).map(|a| a.value.clone()).unwrap()
    };
    assert_eq!(attr("inactive_before"), "250");
    assert_eq!(attr("inactive_excluded"), "200");
    assert_eq!(attr("inactive_unplanned"), "200");
}

#[test]
fn successful_inactive_redelegation_registers_withdrawn_rewards() {
    let mut deps = setup_test();
    let state = State::default();
    state.unlocked_coins.save(deps.as_mut().storage, &vec![]).unwrap();

    reply(deps.as_mut(), mock_env(), Reply {
        id: REPLY_REDELEGATE_INACTIVE_BASE,
        result: cosmwasm_std::SubMsgResult::Ok(SubMsgResponse {
            events: vec![
                Event::new("coin_received")
                    .add_attribute("receiver", MOCK_CONTRACT_ADDR)
                    .add_attribute("amount", "123uxyz"),
                Event::new("coin_received")
                    .add_attribute("receiver", "someone_else")
                    .add_attribute("amount", "999uxyz"),
            ],
            data: None,
        }),
    })
    .unwrap();

    let unlocked = state.unlocked_coins.load(deps.as_ref().storage).unwrap();
    assert_eq!(unlocked, vec![Coin::new(123, "uxyz")]);
}

#[test]
fn reconcile_waits_out_the_grace_period() {
    let mut deps = setup_test();
    let state = State::default();
    // Batch 1 is due at 20000 but its funds are paid out at the end of the block, so they are not
    // in the balance yet when a Reconcile lands in the first block after.
    let batch = Batch {
        id: 1,
        reconciled: false,
        total_shares: Uint128::new(1000),
        amount_unclaimed: Uint128::new(1000),
        est_unbond_end_time: 20000,
    };
    state.previous_batches.save(deps.as_mut().storage, batch.id, &batch).unwrap();
    deps.querier.set_bank_balances(&[]);

    execute(
        deps.as_mut(),
        mock_env_at_timestamp(20000 + RECONCILE_GRACE_SECONDS),
        mock_info("anyone", &[]),
        ExecuteMsg::Reconcile {},
    )
    .unwrap();
    assert_eq!(
        state.previous_batches.load(deps.as_ref().storage, 1u64).unwrap(),
        batch,
        "a reconcile inside the grace period must not write down"
    );

    // Past the grace period, funds that are still missing are treated as slashed.
    execute(
        deps.as_mut(),
        mock_env_at_timestamp(20000 + RECONCILE_GRACE_SECONDS + 1),
        mock_info("anyone", &[]),
        ExecuteMsg::Reconcile {},
    )
    .unwrap();
    let reconciled = state.previous_batches.load(deps.as_ref().storage, 1u64).unwrap();
    assert!(reconciled.reconciled);
    assert_eq!(reconciled.amount_unclaimed, Uint128::zero());
}

#[test]
fn reconcile_does_not_count_funds_owed_to_reconciled_batches() {
    let mut deps = setup_test();
    let state = State::default();
    // Batch 1 is reconciled and still owed 500. Batch 2 expected 1000 but only 800 arrived.
    let batch1 = Batch {
        id: 1,
        reconciled: true,
        total_shares: Uint128::new(500),
        amount_unclaimed: Uint128::new(500),
        est_unbond_end_time: 10000,
    };
    let batch2 = Batch {
        id: 2,
        reconciled: false,
        total_shares: Uint128::new(1000),
        amount_unclaimed: Uint128::new(1000),
        est_unbond_end_time: 20000,
    };
    state.previous_batches.save(deps.as_mut().storage, batch1.id, &batch1).unwrap();
    state.previous_batches.save(deps.as_mut().storage, batch2.id, &batch2).unwrap();
    deps.querier.set_bank_balances(&[Coin::new(500 + 800, "uxyz")]);

    execute(
        deps.as_mut(),
        mock_env_at_timestamp(30000),
        mock_info("anyone", &[]),
        ExecuteMsg::Reconcile {},
    )
    .unwrap();

    // Without the reserve, batch 1's 500 would cover the 200 shortfall and batch 2 would keep 1000.
    assert_eq!(
        state.previous_batches.load(deps.as_ref().storage, 2u64).unwrap().amount_unclaimed,
        Uint128::new(800)
    );
    assert_eq!(
        state.previous_batches.load(deps.as_ref().storage, 1u64).unwrap().amount_unclaimed,
        Uint128::new(500)
    );
}

#[test]
fn reinvest_restakes_recorded_side_effect_rewards() {
    let mut deps = setup_test();
    let state = State::default();
    deps.querier.set_staking_delegations(&[
        Delegation::new("alice", 333334, "uxyz"),
        Delegation::new("bob", 333333, "uxyz"),
        Delegation::new("charlie", 333333, "uxyz"),
    ]);
    // 300 uxyz of rewards were withdrawn automatically by earlier bonds / unbonds / redelegations
    // and recorded; the old balance-delta baseline would have seen no new rewards at all.
    deps.querier.set_bank_balances(&[Coin::new(300, "uxyz")]);
    state.prev_denom.save(deps.as_mut().storage, &Uint128::new(300)).unwrap();
    state.unlocked_coins.save(deps.as_mut().storage, &vec![Coin::new(300, "uxyz")]).unwrap();

    let res = execute(
        deps.as_mut(),
        mock_env(),
        mock_info(MOCK_CONTRACT_ADDR, &[]),
        ExecuteMsg::Callback(CallbackMsg::Reinvest {}),
    )
    .unwrap();
    assert_eq!(res.messages[0], SubMsg::new(Delegation::new("bob", 270, "uxyz").to_cosmos_msg()));
    assert_eq!(
        res.messages[1],
        SubMsg::new(CosmosMsg::Bank(BankMsg::Send {
            to_address: "the_fee_man".into(),
            amount: vec![Coin::new(30, "uxyz")],
        }))
    );
    assert!(state.unlocked_coins.load(deps.as_ref().storage).unwrap().is_empty());
}

#[test]
fn reinvest_never_bonds_funds_owed_to_batches() {
    let mut deps = setup_test();
    let state = State::default();
    deps.querier.set_staking_delegations(&[
        Delegation::new("alice", 333334, "uxyz"),
        Delegation::new("bob", 333333, "uxyz"),
        Delegation::new("charlie", 333333, "uxyz"),
    ]);
    // A matured batch is owed 400 of the 500 balance, so only 100 of the 300 recorded can be
    // bonded.
    let batch = Batch {
        id: 1,
        reconciled: false,
        total_shares: Uint128::new(400),
        amount_unclaimed: Uint128::new(400),
        est_unbond_end_time: 100,
    };
    state.previous_batches.save(deps.as_mut().storage, batch.id, &batch).unwrap();
    deps.querier.set_bank_balances(&[Coin::new(500, "uxyz")]);
    state.unlocked_coins.save(deps.as_mut().storage, &vec![Coin::new(300, "uxyz")]).unwrap();

    let res = execute(
        deps.as_mut(),
        mock_env(),
        mock_info(MOCK_CONTRACT_ADDR, &[]),
        ExecuteMsg::Callback(CallbackMsg::Reinvest {}),
    )
    .unwrap();
    assert_eq!(res.messages[0], SubMsg::new(Delegation::new("bob", 90, "uxyz").to_cosmos_msg()));
    // The rest stays recorded for a later reinvest.
    assert_eq!(state.unlocked_coins.load(deps.as_ref().storage).unwrap(), vec![Coin::new(
        200, "uxyz"
    )]);
}

#[test]
fn querying_and_reinvesting_idle_funds() {
    let mut deps = setup_test();
    let state = State::default();
    deps.querier.set_staking_delegations(&[
        Delegation::new("alice", 333334, "uxyz"),
        Delegation::new("bob", 333333, "uxyz"),
        Delegation::new("charlie", 333333, "uxyz"),
    ]);
    // Balance 1000: 100 recorded rewards, 300 owed to a batch still unbonding (its funds are not
    // here yet, but they are reserved anyway), so 600 is idle.
    let batch = Batch {
        id: 1,
        reconciled: false,
        total_shares: Uint128::new(300),
        amount_unclaimed: Uint128::new(300),
        est_unbond_end_time: u64::MAX,
    };
    state.previous_batches.save(deps.as_mut().storage, batch.id, &batch).unwrap();
    deps.querier.set_bank_balances(&[Coin::new(1000, "uxyz")]);
    state.unlocked_coins.save(deps.as_mut().storage, &vec![Coin::new(100, "uxyz")]).unwrap();

    let idle: IdleFundsResponse = query_helper(deps.as_ref(), QueryMsg::IdleFunds {});
    assert_eq!(idle, IdleFundsResponse {
        denom: "uxyz".to_string(),
        balance: Uint128::new(1000),
        unlocked: Uint128::new(100),
        owed_to_batches: Uint128::new(300),
        idle: Uint128::new(600),
    });

    let err =
        execute(deps.as_mut(), mock_env(), mock_info("jake", &[]), ExecuteMsg::ReinvestIdle {
            amount: None,
        })
        .unwrap_err();
    assert_eq!(err, StdError::generic_err("unauthorized: sender is not owner"));

    let err =
        execute(deps.as_mut(), mock_env(), mock_info("larry", &[]), ExecuteMsg::ReinvestIdle {
            amount: Some(Uint128::new(601)),
        })
        .unwrap_err();
    assert_eq!(err, StdError::generic_err("amount exceeds idle funds: 601 > 600"));

    let res =
        execute(deps.as_mut(), mock_env(), mock_info("larry", &[]), ExecuteMsg::ReinvestIdle {
            amount: None,
        })
        .unwrap();
    assert_eq!(res.messages, vec![SubMsg::reply_on_success(
        Delegation::new("bob", 600, "uxyz").to_cosmos_msg(),
        REPLY_REGISTER_RECEIVED_COINS,
    )]);
}

#[test]
fn bonding_prices_in_rewards_not_yet_restaked() {
    let mut deps = setup_test();
    let state = State::default();
    deps.querier.set_staking_delegations(&[
        Delegation::new("alice", 400, "uxyz"),
        Delegation::new("bob", 300, "uxyz"),
        Delegation::new("charlie", 300, "uxyz"),
    ]);
    deps.querier.set_cw20_total_supply("steak_token", 1000);
    // 100 of recorded rewards belong to existing holders: 90 after the 10% fee. Backing is
    // 1000 + 90 = 1090 per 1000 shares, so a 1090 deposit buys 1000 shares, not 1090.
    state.unlocked_coins.save(deps.as_mut().storage, &vec![Coin::new(100, "uxyz")]).unwrap();
    deps.querier.set_bank_balances(&[Coin::new(100 + 1090, "uxyz")]);

    let res = execute(
        deps.as_mut(),
        mock_env(),
        mock_info("user_1", &[Coin::new(1090, "uxyz")]),
        ExecuteMsg::Bond {
            receiver: None,
            exec_msg: None,
        },
    )
    .unwrap();
    let bonded = res.events.iter().find(|e| e.ty == "steakhub/bonded").unwrap();
    let minted = bonded.attributes.iter().find(|a| a.key == "usteak_minted").unwrap();
    assert_eq!(minted.value, "1000");
}

#[test]
fn bonding_and_reinvesting_skip_validators_that_are_not_bonded() {
    let mut deps = setup_test();
    let state = State::default();
    deps.querier.set_staking_delegations(&[
        Delegation::new("alice", 300, "uxyz"),
        Delegation::new("bob", 100, "uxyz"),
        Delegation::new("charlie", 200, "uxyz"),
    ]);
    deps.querier.set_cw20_total_supply("steak_token", 600);
    // bob has the smallest delegation but is jailed (not in the chain's bonded set).
    deps.querier.set_bonded_validators(&["alice", "charlie"]);
    deps.querier.set_bank_balances(&[Coin::new(1000, "uxyz")]);

    let res = execute(
        deps.as_mut(),
        mock_env(),
        mock_info("user_1", &[Coin::new(1000, "uxyz")]),
        ExecuteMsg::Bond {
            receiver: None,
            exec_msg: None,
        },
    )
    .unwrap();
    assert_eq!(res.messages[0].msg, Delegation::new("charlie", 1000, "uxyz").to_cosmos_msg());

    state.unlocked_coins.save(deps.as_mut().storage, &vec![Coin::new(100, "uxyz")]).unwrap();
    let res = execute(
        deps.as_mut(),
        mock_env(),
        mock_info(MOCK_CONTRACT_ADDR, &[]),
        ExecuteMsg::Callback(CallbackMsg::Reinvest {}),
    )
    .unwrap();
    assert_eq!(res.messages[0].msg, Delegation::new("charlie", 90, "uxyz").to_cosmos_msg());
}

#[test]
fn rebalancing_is_owner_only_drains_unbonded_and_isolates_moves() {
    let mut deps = setup_test();
    deps.querier.set_staking_delegations(&[
        Delegation::new("alice", 100, "uxyz"),
        Delegation::new("bob", 300, "uxyz"),
        Delegation::new("charlie", 200, "uxyz"),
    ]);
    deps.querier.set_bonded_validators(&["alice", "bob"]);

    let err = execute(deps.as_mut(), mock_env(), mock_info("jake", &[]), ExecuteMsg::Rebalance {
        minimum: Uint128::zero(),
    })
    .unwrap_err();
    assert_eq!(err, StdError::generic_err("unauthorized: sender is not owner"));

    // charlie is active but not bonded: drained. Targets alice and bob get 300 each.
    let res = execute(deps.as_mut(), mock_env(), mock_info("larry", &[]), ExecuteMsg::Rebalance {
        minimum: Uint128::zero(),
    })
    .unwrap();
    assert_eq!(res.messages, vec![SubMsg {
        id: REPLY_REDELEGATE_INACTIVE_BASE,
        msg: Redelegation::new("charlie", "alice", 200, "uxyz").to_cosmos_msg(),
        gas_limit: None,
        reply_on: ReplyOn::Always,
    }]);
    assert!(res.events.iter().any(|e| e.ty == "steakhub/redelegation_planned"));
}

#[test]
fn removing_validator_redelegates_only_to_bonded_active_validators() {
    let mut deps = setup_test();
    deps.querier.set_bonded_validators(&["alice", "bob", "charlie"]);
    execute(deps.as_mut(), mock_env(), mock_info("larry", &[]), ExecuteMsg::PauseValidator {
        validator: "bob".to_string(),
    })
    .unwrap();
    deps.querier.set_staking_delegations(&[
        Delegation::new("alice", 100, "uxyz"),
        Delegation::new("bob", 100, "uxyz"),
        Delegation::new("charlie", 300, "uxyz"),
    ]);

    // bob is paused, so all of charlie's stake goes to alice.
    let res =
        execute(deps.as_mut(), mock_env(), mock_info("larry", &[]), ExecuteMsg::RemoveValidator {
            validator: "charlie".to_string(),
        })
        .unwrap();
    assert_eq!(res.messages.len(), 1);
    assert_eq!(
        res.messages[0].msg,
        Redelegation::new("charlie", "alice", 300, "uxyz").to_cosmos_msg()
    );
}

#[test]
fn submit_batch_defers_while_unbonding_entries_are_full() {
    let mut deps = setup_test();
    let state = State::default();
    let now = 10_000_000u64;
    deps.querier.set_staking_delegations(&[
        Delegation::new("alice", 400, "uxyz"),
        Delegation::new("bob", 300, "uxyz"),
        Delegation::new("charlie", 300, "uxyz"),
    ]);
    deps.querier.set_cw20_total_supply("steak_token", 1000);
    state
        .pending_batch
        .save(deps.as_mut().storage, &PendingBatch {
            id: 8,
            usteak_to_burn: Uint128::new(100),
            est_unbond_start_time: 0,
        })
        .unwrap();
    // Seven earlier batches are still unbonding: the 8th undelegation per validator would be
    // refused by the staking module and fail the whole batch.
    for id in 1..=7u64 {
        let batch = Batch {
            id,
            reconciled: false,
            total_shares: Uint128::new(10),
            amount_unclaimed: Uint128::new(10),
            est_unbond_end_time: now + id,
        };
        state.previous_batches.save(deps.as_mut().storage, id, &batch).unwrap();
    }

    let err = execute(
        deps.as_mut(),
        mock_env_at_timestamp(now),
        mock_info("worker", &[]),
        ExecuteMsg::SubmitBatch {},
    )
    .unwrap_err();
    assert_eq!(
        err,
        StdError::generic_err(format!(
            "7 unbonding batches are still in flight; batch can be submitted after {}",
            now + 1 + RECONCILE_GRACE_SECONDS + 1
        ))
    );

    // Once the oldest has matured (plus the grace), the batch goes through.
    execute(
        deps.as_mut(),
        mock_env_at_timestamp(now + 1 + RECONCILE_GRACE_SECONDS + 1),
        mock_info("worker", &[]),
        ExecuteMsg::SubmitBatch {},
    )
    .unwrap();
}

#[test]
fn minting_with_zero_supply_or_dust_amounts() {
    // Everyone unbonded but rounding dust is still delegated: mint 1:1 instead of 0.
    assert_eq!(
        compute_mint_amount(
            Uint128::zero(),
            Uint128::new(1000),
            &[Delegation::new("alice", 5, "uxyz")],
            Uint128::zero()
        )
        .unwrap(),
        Uint128::new(1000)
    );
    // A deposit worth less than one token is refused rather than swallowed.
    assert_eq!(
        compute_mint_amount(
            Uint128::new(1000),
            Uint128::new(1),
            &[Delegation::new("alice", 2000, "uxyz")],
            Uint128::zero()
        )
        .unwrap_err(),
        StdError::generic_err("bond amount is too small to mint any derivative token")
    );
}

#[test]
fn burning_stray_steak() {
    let mut deps = setup_test();
    let state = State::default();
    state
        .pending_batch
        .save(deps.as_mut().storage, &PendingBatch {
            id: 1,
            usteak_to_burn: Uint128::new(100),
            est_unbond_start_time: 0,
        })
        .unwrap();
    // The hub holds 150: 100 queued for unbonding, 50 transferred to it directly.
    deps.querier.set_cw20_balance("steak_token", MOCK_CONTRACT_ADDR, 150);

    let err =
        execute(deps.as_mut(), mock_env(), mock_info("jake", &[]), ExecuteMsg::BurnStraySteak {})
            .unwrap_err();
    assert_eq!(err, StdError::generic_err("unauthorized: sender is not owner"));

    let res =
        execute(deps.as_mut(), mock_env(), mock_info("larry", &[]), ExecuteMsg::BurnStraySteak {})
            .unwrap();
    assert_eq!(res.messages, vec![SubMsg::new(CosmosMsg::Wasm(WasmMsg::Execute {
        contract_addr: "steak_token".to_string(),
        msg: to_json_binary(&Cw20ExecuteMsg::Burn {
            amount: Uint128::new(50),
        })
        .unwrap(),
        funds: vec![],
    }))]);
}
