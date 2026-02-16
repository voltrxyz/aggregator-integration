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
async fn test_quote_deposit() {
    let account_map = load_account_map_from_file(FIXTURE_PATHS);
    let (vault_key, vault_account) = load_account_from_file("tests/fixtures/vault.json");
    let vault_state = Vault::load(&vault_account.data).unwrap();
    let asset_mint = vault_state.asset.mint;
    let lp_mint = vault_state.lp.mint;

    let mut voltr_amm = VoltrAmm::new(vault_key, vault_state);
    voltr_amm.update(&account_map).unwrap();

    let amount = 1_000_000;
    let quote_params = QuoteParams {
        input_mint: asset_mint,
        output_mint: lp_mint,
        amount,
        swap_mode: SwapMode::ExactIn,
    };

    let quote_result = voltr_amm.quote(&quote_params).unwrap();
    assert_eq!(amount, quote_result.in_amount);
    assert!(quote_result.out_amount > 0, "Expected non-zero LP output");

    let redeem_params = QuoteParams {
        input_mint: lp_mint,
        output_mint: asset_mint,
        amount: 1_000,
        swap_mode: SwapMode::ExactIn,
    };

    let redeem_result = voltr_amm.quote(&redeem_params);
    if let Err(ref e) = redeem_result {
        let err_str = format!("{}", e);
        assert!(
            !err_str.contains("Invalid Source Mint"),
            "Redeem should not fail with InvalidSourceMint: {}",
            err_str
        );
    }
}
