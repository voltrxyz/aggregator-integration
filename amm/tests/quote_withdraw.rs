use amm::{state::Vault, VoltrAmm};
use jupiter_amm_interface::{Amm, QuoteParams, SwapMode};

mod utils;
use utils::*;

const FIXTURE_PATHS: &[&str] = &[
    "tests/fixtures/vault.json",
    "tests/fixtures/lp_mint.json",
    "tests/fixtures/asset_mint.json",
    "tests/fixtures/vault_asset_idle_ata.json",
];

#[tokio::test]
async fn test_quote_withdraw() {
    let account_map = load_account_map_from_file(FIXTURE_PATHS);
    let (vault_key, _) = load_account_from_file("tests/fixtures/vault.json");
    let vault_account = account_map.get(&vault_key).unwrap();
    let vault_state = Vault::load(&vault_account.data).unwrap();
    let asset_mint = vault_state.asset.mint;
    let lp_mint = vault_state.lp.mint;

    let mut voltr_amm = VoltrAmm::new(vault_key, vault_state);
    voltr_amm.update(&account_map).unwrap();

    let lp_amount = 50_000_000;
    let quote_params = QuoteParams {
        input_mint: lp_mint,
        output_mint: asset_mint,
        amount: lp_amount,
        swap_mode: SwapMode::ExactIn,
    };

    let quote_result = voltr_amm.quote(&quote_params).unwrap();
    assert_eq!(lp_amount, quote_result.in_amount);
    assert!(
        quote_result.out_amount > 0,
        "Expected non-zero asset output"
    );

    let redemption_fee_bps = voltr_amm.vault_state.fee_configuration.redemption_fee;
    if redemption_fee_bps > 0 {
        assert!(
            quote_result.fee_amount > 0,
            "Expected non-zero fee with redemption_fee_bps={}",
            redemption_fee_bps
        );
    }

    let deposit_params = QuoteParams {
        input_mint: asset_mint,
        output_mint: lp_mint,
        amount: 1_000_000,
        swap_mode: SwapMode::ExactIn,
    };
    let deposit_result = voltr_amm.quote(&deposit_params).unwrap();
    assert!(
        deposit_result.out_amount > 0,
        "Deposit quote should still work"
    );
}
