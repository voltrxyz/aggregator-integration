use amm::{
    constants::VOLTR_VAULT_PROGRAM,
    derive_receipt_pda, derive_vault_lp_mint_pda,
    state::Vault,
    VoltrAmm, VoltrRedeemSwap, VoltrSwap,
};
use jupiter_amm_interface::{Amm, QuoteParams, SwapMode};
use solana_program_test::ProgramTest;
use solana_sdk::{
    instruction::AccountMeta,
    program_pack::Pack,
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    transaction::Transaction,
};
use spl_associated_token_account::{
    get_associated_token_address_with_program_id, instruction::create_associated_token_account,
};
use spl_token::{id as token_program_id, state::Account as TokenAccount};

mod utils;
use utils::*;

const FIXTURE_PATHS: &[&str] = &[
    "tests/fixtures/vault.json",
    "tests/fixtures/protocol.json",
    "tests/fixtures/lp_mint.json",
    "tests/fixtures/asset_mint.json",
    "tests/fixtures/vault_asset_idle_ata.json",
];

#[tokio::test]
async fn test_deposit_and_withdraw() {
    let mut program_test = ProgramTest::default();
    program_test.prefer_bpf(true);

    program_test.add_program("voltr_vault", VOLTR_VAULT_PROGRAM, None);

    let mut account_map = load_account_map_from_file(FIXTURE_PATHS);
    let (vault_key, _) = load_account_from_file("tests/fixtures/vault.json");

    let mint_authority = Keypair::new();
    let (asset_mint_key, _) = load_account_from_file("tests/fixtures/asset_mint.json");
    let asset_mint_account = account_map.get_mut(&asset_mint_key).unwrap();
    asset_mint_account.data[4..36].copy_from_slice(&mint_authority.pubkey().to_bytes());

    // Patch fees to 100 BPS (1%) each
    let fee_config_offset = 8 + 504;
    let issuance_fee_bps: u16 = 100;
    let redemption_fee_bps: u16 = 100;
    {
        let vault_data = &mut account_map.get_mut(&vault_key).unwrap().data;
        vault_data[fee_config_offset + 8..fee_config_offset + 10]
            .copy_from_slice(&redemption_fee_bps.to_le_bytes());
        vault_data[fee_config_offset + 10..fee_config_offset + 12]
            .copy_from_slice(&issuance_fee_bps.to_le_bytes());
    }

    for (address, account) in account_map.iter() {
        program_test.add_account(*address, account.clone());
    }

    let (mut banks_client, payer, recent_blockhash) = program_test.start().await;

    let vault_account = account_map.get(&vault_key).unwrap();
    let vault_state = Vault::load(&vault_account.data).unwrap();
    let asset_mint = vault_state.asset.mint;
    let lp_mint = vault_state.lp.mint;

    let mut voltr_amm = VoltrAmm::new(vault_key, vault_state);
    voltr_amm.update(&account_map).unwrap();

    let user_asset_ata = get_associated_token_address_with_program_id(
        &payer.pubkey(),
        &asset_mint,
        &token_program_id(),
    );
    let user_lp_ata = get_associated_token_address_with_program_id(
        &payer.pubkey(),
        &lp_mint,
        &token_program_id(),
    );

    let deposit_amount = 10_000_000u64;

    let setup_tx = Transaction::new_signed_with_payer(
        &[
            create_associated_token_account(
                &payer.pubkey(),
                &payer.pubkey(),
                &asset_mint,
                &token_program_id(),
            ),
            create_associated_token_account(
                &payer.pubkey(),
                &payer.pubkey(),
                &lp_mint,
                &token_program_id(),
            ),
            spl_token::instruction::mint_to(
                &token_program_id(),
                &asset_mint,
                &user_asset_ata,
                &mint_authority.pubkey(),
                &[],
                deposit_amount,
            )
            .unwrap(),
        ],
        Some(&payer.pubkey()),
        &[&payer, &mint_authority],
        recent_blockhash,
    );

    banks_client
        .process_transaction_with_metadata(setup_tx)
        .await
        .unwrap()
        .result
        .unwrap();

    let deposit_quote = voltr_amm
        .quote(&QuoteParams {
            input_mint: asset_mint,
            output_mint: lp_mint,
            amount: deposit_amount,
            swap_mode: SwapMode::ExactIn,
        })
        .unwrap();

    assert_eq!(deposit_amount, deposit_quote.in_amount);
    assert!(deposit_quote.out_amount > 0);
    assert!(
        deposit_quote.fee_amount > 0,
        "Expected non-zero issuance fee with {} BPS",
        issuance_fee_bps,
    );

    let deposit_ix = VoltrSwap {
        vault_key,
        vault_asset_mint: asset_mint,
        vault_asset_idle_ata: voltr_amm.vault_state.asset.idle_ata,
        vault_lp_mint: lp_mint,
        user_source: user_asset_ata,
        user_destination: user_lp_ata,
        user_transfer_authority: payer.pubkey(),
        asset_token_program: voltr_amm.asset_token_program,
    }
    .into_instruction(deposit_amount)
    .unwrap();

    let deposit_tx = Transaction::new_signed_with_payer(
        &[deposit_ix],
        Some(&payer.pubkey()),
        &[&payer],
        recent_blockhash,
    );

    banks_client
        .process_transaction_with_metadata(deposit_tx)
        .await
        .unwrap()
        .result
        .unwrap();

    let lp_account = banks_client
        .get_account(user_lp_ata)
        .await
        .unwrap()
        .unwrap();
    let lp_data = TokenAccount::unpack(&lp_account.data).unwrap();
    let lp_balance = lp_data.amount;

    let deposit_diff = lp_balance.abs_diff(deposit_quote.out_amount);
    assert!(
        deposit_diff <= 1,
        "Deposit LP mismatch: on-chain={}, quote={}, diff={}",
        lp_balance,
        deposit_quote.out_amount,
        deposit_diff,
    );

    let account_map_addresses: Vec<Pubkey> = account_map.keys().cloned().collect();
    account_map =
        load_account_map_from_bank(&mut banks_client, account_map_addresses.as_slice()).await;
    voltr_amm.update(&account_map).unwrap();

    let withdraw_lp_amount = lp_balance;
    let withdraw_quote = voltr_amm
        .quote(&QuoteParams {
            input_mint: lp_mint,
            output_mint: asset_mint,
            amount: withdraw_lp_amount,
            swap_mode: SwapMode::ExactIn,
        })
        .unwrap();

    assert_eq!(withdraw_lp_amount, withdraw_quote.in_amount);
    assert!(
        withdraw_quote.out_amount > 0,
        "Expected non-zero asset output"
    );
    assert!(
        withdraw_quote.fee_amount > 0,
        "Expected non-zero redemption fee with {} BPS",
        redemption_fee_bps,
    );

    let placeholder = AccountMeta::new_readonly(Pubkey::new_unique(), false);
    let redeem_swap = VoltrRedeemSwap {
        vault_key,
        vault_asset_mint: asset_mint,
        vault_asset_idle_ata: voltr_amm.vault_state.asset.idle_ata,
        vault_lp_mint: lp_mint,
        user_lp_ata,
        user_asset_ata: user_asset_ata,
        user_transfer_authority: payer.pubkey(),
        asset_token_program: voltr_amm.asset_token_program,
        placeholder,
    };

    let [request_withdraw_ix, withdraw_ix] = redeem_swap
        .into_instructions(withdraw_lp_amount)
        .unwrap();

    let receipt_pda = derive_receipt_pda(&vault_key, &payer.pubkey());
    let vault_lp_mint_pda = derive_vault_lp_mint_pda(&vault_key);
    let create_receipt_lp_ata_ix = create_associated_token_account(
        &payer.pubkey(),
        &receipt_pda,
        &vault_lp_mint_pda,
        &token_program_id(),
    );

    let recent_blockhash = banks_client.get_latest_blockhash().await.unwrap();
    let withdraw_tx = Transaction::new_signed_with_payer(
        &[create_receipt_lp_ata_ix, request_withdraw_ix, withdraw_ix],
        Some(&payer.pubkey()),
        &[&payer],
        recent_blockhash,
    );

    banks_client
        .process_transaction_with_metadata(withdraw_tx)
        .await
        .unwrap()
        .result
        .unwrap();

    let asset_account = banks_client
        .get_account(user_asset_ata)
        .await
        .unwrap()
        .unwrap();
    let asset_data = TokenAccount::unpack(&asset_account.data).unwrap();

    let withdraw_diff = asset_data.amount.abs_diff(withdraw_quote.out_amount);
    assert!(
        withdraw_diff <= 1,
        "Withdraw asset mismatch: on-chain={}, quote={}, diff={}",
        asset_data.amount,
        withdraw_quote.out_amount,
        withdraw_diff,
    );

    let lp_account = banks_client
        .get_account(user_lp_ata)
        .await
        .unwrap()
        .unwrap();
    let lp_data = TokenAccount::unpack(&lp_account.data).unwrap();
    assert_eq!(lp_data.amount, 0, "All LP tokens should be burned");

    let account_map_addresses: Vec<Pubkey> = account_map.keys().cloned().collect();
    account_map =
        load_account_map_from_bank(&mut banks_client, account_map_addresses.as_slice()).await;
    voltr_amm.update(&account_map).unwrap();
}
