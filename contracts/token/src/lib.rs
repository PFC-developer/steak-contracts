use cosmwasm_std::{
    Addr, Binary, Deps, DepsMut, Env, MessageInfo, Response, StdError, StdResult, Storage,
    entry_point,
};
use cw20_base::{
    ContractError,
    contract::{execute as cw20_execute, instantiate as cw20_instantiate, query as cw20_query},
    msg::{ExecuteMsg, InstantiateMsg, QueryMsg},
    state::{MinterData, TOKEN_INFO},
};

#[entry_point]
pub fn instantiate(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    msg: InstantiateMsg,
) -> Result<Response, ContractError> {
    cw20_instantiate(deps, env, info, msg)
}

#[entry_point]
pub fn execute(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    msg: ExecuteMsg,
) -> Result<Response, ContractError> {
    // For `burn`, we assert that the caller is the minter
    // For `burn_from`, we simply disable it
    match msg {
        ExecuteMsg::Burn {
            ..
        } => assert_minter(deps.storage, &info.sender)?,
        ExecuteMsg::BurnFrom {
            ..
        } => return Err(StdError::generic_err("`burn_from` command is disabled").into()),
        _ => (),
    }

    cw20_execute(deps, env, info, msg)
}

fn assert_minter(storage: &dyn Storage, sender: &Addr) -> Result<(), ContractError> {
    let token_info = TOKEN_INFO.load(storage)?;

    if let Some(MinterData {
        minter,
        ..
    }) = &token_info.mint
    {
        if sender != minter {
            return Err(StdError::generic_err("only minter can execute token burn").into());
        }
    }

    Ok(())
}

#[entry_point]
pub fn query(deps: Deps, env: Env, msg: QueryMsg) -> StdResult<Binary> {
    cw20_query(deps, env, msg)
}

#[cfg(test)]
mod tests {
    use cosmwasm_std::{
        OwnedDeps, Uint128,
        testing::{MockApi, MockQuerier, MockStorage, mock_dependencies, mock_env, mock_info},
    };
    use cw20_base::state::{BALANCES, TokenInfo};

    use super::*;

    fn setup_test() -> OwnedDeps<MockStorage, MockApi, MockQuerier> {
        let mut deps = mock_dependencies();

        TOKEN_INFO
            .save(deps.as_mut().storage, &TokenInfo {
                name: "Steak Token".to_string(),
                symbol: "STEAK".to_string(),
                decimals: 6,
                total_supply: Uint128::new(200),
                mint: Some(MinterData {
                    minter: Addr::unchecked("steak_hub"),
                    cap: None,
                }),
            })
            .unwrap();

        BALANCES
            .save(deps.as_mut().storage, &Addr::unchecked("steak_hub"), &Uint128::new(100))
            .unwrap();

        BALANCES
            .save(deps.as_mut().storage, &Addr::unchecked("alice"), &Uint128::new(100))
            .unwrap();

        deps
    }

    #[test]
    fn asserting_minter() {
        let mut deps = setup_test();

        // Alice is not allowed to burn her balance
        let res = execute(deps.as_mut(), mock_env(), mock_info("alice", &[]), ExecuteMsg::Burn {
            amount: Uint128::new(100),
        });
        assert_eq!(res, Err(StdError::generic_err("only minter can execute token burn").into()));

        // Steak Hub can burn
        let res =
            execute(deps.as_mut(), mock_env(), mock_info("steak_hub", &[]), ExecuteMsg::Burn {
                amount: Uint128::new(100),
            });
        assert!(res.is_ok());

        // Steak Hub's token balance should have been reduced
        let balance = BALANCES.load(deps.as_ref().storage, &Addr::unchecked("steak_hub")).unwrap();
        assert_eq!(balance, Uint128::zero());

        // Total supply should have been reduced
        let token_info = TOKEN_INFO.load(deps.as_ref().storage).unwrap();
        assert_eq!(token_info.total_supply, Uint128::new(100));
    }

    #[test]
    fn disabling_burn_from() {
        let mut deps = setup_test();

        // Not even Steak Hub can invoke `burn_from`
        let res =
            execute(deps.as_mut(), mock_env(), mock_info("steak_hub", &[]), ExecuteMsg::BurnFrom {
                owner: "alice".to_string(),
                amount: Uint128::new(100),
            });
        assert_eq!(res, Err(StdError::generic_err("`burn_from` command is disabled").into()));
    }
}
