use anyhow::Result;
use constants::VOLTR_VAULT_PROGRAM;
use jupiter_amm_interface::{
    try_get_account_data, AccountMap, Amm, AmmContext, KeyedAccount, Quote, QuoteParams, Swap,
    SwapAndAccountMetas, SwapParams,
};
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    program_pack::Pack,
    pubkey::Pubkey,
    system_program::ID as SystemProgramId,
};
use spl_token_2022::{extension::StateWithExtensionsOwned, state::Mint as Mint22};

pub mod constants;
use constants::*;

mod errors;
mod math;
use errors::VoltrAmmError;
use math::*;
use state::Vault;

pub mod state;

fn anchor_discriminator(name: &str) -> [u8; 8] {
    let preimage = format!("global:{}", name);
    let mut sighash = [0u8; 8];
    sighash.copy_from_slice(&solana_sdk::hash::hash(preimage.as_bytes()).to_bytes()[..8]);
    sighash
}

pub fn derive_receipt_pda(vault_key: &Pubkey, user: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[
            REQUEST_WITHDRAW_VAULT_RECEIPT_SEED,
            vault_key.as_ref(),
            user.as_ref(),
        ],
        &VOLTR_VAULT_PROGRAM,
    )
    .0
}

pub fn derive_vault_lp_mint_pda(vault_key: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[VAULT_LP_MINT_SEED, vault_key.as_ref()],
        &VOLTR_VAULT_PROGRAM,
    )
    .0
}

pub struct VoltrAmm {
    pub label: String,
    pub program_id: Pubkey,
    pub vault_key: Pubkey,
    pub vault_state: Vault,
    pub lp_mint_supply: u64,
    pub lp_mint_decimals: u8,
    pub asset_mint_decimals: u8,
    pub asset_token_program: Pubkey,
    pub asset_idle_balance: u64,
}

impl VoltrAmm {
    pub fn new(vault_key: Pubkey, vault_state: Vault) -> Self {
        VoltrAmm {
            label: AMM_LABEL.to_owned(),
            program_id: VOLTR_VAULT_PROGRAM,
            vault_key,
            vault_state,
            lp_mint_supply: 0,
            lp_mint_decimals: 9, // voltr LP is always 9 decimals
            asset_mint_decimals: 0,
            asset_token_program: TOKEN_PROGRAM,
            asset_idle_balance: 0,
        }
    }

    fn quote_redeem(
        &self,
        quote_params: &QuoteParams,
        current_ts: u64,
        _total_asset_value: u64,
        total_lp_supply_after_mgmt_fee: u64,
    ) -> Result<Quote> {
        if self
            .vault_state
            .vault_configuration
            .withdrawal_waiting_period
            != 0
        {
            return Err(VoltrAmmError::WithdrawalWaitingPeriodNotZero.into());
        }

        let amount = quote_params.amount;
        let redemption_fee_bps = self.vault_state.fee_configuration.redemption_fee;

        let total_unlocked_asset = self.vault_state.get_unlocked_asset_value(current_ts)?;

        let asset_to_redeem = calc_withdraw_asset_to_redeem(
            amount,
            total_lp_supply_after_mgmt_fee,
            total_unlocked_asset,
            redemption_fee_bps,
        )?;

        if self.asset_idle_balance < asset_to_redeem {
            return Err(VoltrAmmError::InsufficientIdleBalance.into());
        }

        let (fee_amount, fee_pct) = if redemption_fee_bps > 0 {
            let asset_without_fee = calc_withdraw_asset_to_redeem(
                amount,
                total_lp_supply_after_mgmt_fee,
                total_unlocked_asset,
                0,
            )?;
            let fee_in_asset = asset_without_fee.saturating_sub(asset_to_redeem);
            let pct = rust_decimal::Decimal::new(redemption_fee_bps.into(), 4);
            (fee_in_asset, pct)
        } else {
            (0u64, rust_decimal::Decimal::ZERO)
        };

        Ok(Quote {
            fee_pct,
            in_amount: amount,
            out_amount: asset_to_redeem,
            fee_amount,
            fee_mint: quote_params.output_mint,
            ..Quote::default()
        })
    }

    fn estimate_management_fee_lp(
        &self,
        current_ts: u64,
        total_asset_value: u64,
        total_lp_supply_incl_fees: u64,
    ) -> Result<u64> {
        let management_fee_bps = self
            .vault_state
            .get_total_fee_configuration_management_fee()?;

        if self.vault_state.fee_update.last_management_fee_update_ts == 0
            || total_asset_value == 0
            || management_fee_bps == 0
        {
            return Ok(0);
        }

        let time_elapsed =
            current_ts.saturating_sub(self.vault_state.fee_update.last_management_fee_update_ts);
        if time_elapsed == 0 {
            return Ok(0);
        }

        let fee_amount_in_asset = calc_management_fee_amount_in_asset(
            time_elapsed,
            total_asset_value,
            management_fee_bps,
        )?;

        if fee_amount_in_asset == 0 || fee_amount_in_asset >= total_asset_value {
            return Ok(0);
        }

        calc_fee_lp_to_mint(
            fee_amount_in_asset,
            total_lp_supply_incl_fees,
            total_asset_value,
        )
    }
}

impl Clone for VoltrAmm {
    fn clone(&self) -> Self {
        VoltrAmm {
            label: self.label.clone(),
            program_id: self.program_id,
            vault_key: self.vault_key,
            vault_state: self.vault_state.clone(),
            lp_mint_supply: self.lp_mint_supply,
            lp_mint_decimals: self.lp_mint_decimals,
            asset_mint_decimals: self.asset_mint_decimals,
            asset_token_program: self.asset_token_program,
            asset_idle_balance: self.asset_idle_balance,
        }
    }
}

#[derive(Copy, Clone, Debug)]
pub struct VoltrSwap {
    pub vault_key: Pubkey,
    pub vault_asset_mint: Pubkey,
    pub vault_asset_idle_ata: Pubkey,
    pub vault_lp_mint: Pubkey,
    pub user_source: Pubkey,
    pub user_destination: Pubkey,
    pub user_transfer_authority: Pubkey,
    pub asset_token_program: Pubkey,
}

impl VoltrSwap {
    pub fn into_instruction(self, deposit_amount: u64) -> Result<Instruction> {
        let metas: Vec<AccountMeta> = self.try_into()?;
        let mut data = Vec::with_capacity(16);
        data.extend_from_slice(&anchor_discriminator("deposit_vault"));
        data.extend_from_slice(&deposit_amount.to_le_bytes());
        Ok(Instruction {
            program_id: VOLTR_VAULT_PROGRAM,
            accounts: metas,
            data,
        })
    }
}

impl TryFrom<VoltrSwap> for Vec<AccountMeta> {
    type Error = anyhow::Error;

    fn try_from(swap: VoltrSwap) -> Result<Self> {
        let (protocol_pda, _) =
            Pubkey::find_program_address(&[PROTOCOL_SEED], &VOLTR_VAULT_PROGRAM);

        let (vault_lp_mint_pda, _) = Pubkey::find_program_address(
            &[VAULT_LP_MINT_SEED, swap.vault_key.as_ref()],
            &VOLTR_VAULT_PROGRAM,
        );

        let (vault_asset_idle_auth_pda, _) = Pubkey::find_program_address(
            &[VAULT_ASSET_IDLE_AUTH_SEED, swap.vault_key.as_ref()],
            &VOLTR_VAULT_PROGRAM,
        );

        let (vault_lp_mint_auth_pda, _) = Pubkey::find_program_address(
            &[VAULT_LP_MINT_AUTH_SEED, swap.vault_key.as_ref()],
            &VOLTR_VAULT_PROGRAM,
        );

        Ok(vec![
            AccountMeta::new_readonly(swap.user_transfer_authority, true),
            AccountMeta::new_readonly(protocol_pda, false),
            AccountMeta::new(swap.vault_key, false),
            AccountMeta::new_readonly(swap.vault_asset_mint, false),
            AccountMeta::new(vault_lp_mint_pda, false),
            AccountMeta::new(swap.user_source, false),
            AccountMeta::new(swap.vault_asset_idle_ata, false),
            AccountMeta::new_readonly(vault_asset_idle_auth_pda, false),
            AccountMeta::new(swap.user_destination, false),
            AccountMeta::new_readonly(vault_lp_mint_auth_pda, false),
            AccountMeta::new_readonly(swap.asset_token_program, false),
            AccountMeta::new_readonly(TOKEN_PROGRAM, false),
            AccountMeta::new_readonly(SystemProgramId, false),
        ])
    }
}

#[derive(Clone, Debug)]
pub struct VoltrRedeemSwap {
    pub vault_key: Pubkey,
    pub vault_asset_mint: Pubkey,
    pub vault_asset_idle_ata: Pubkey,
    pub vault_lp_mint: Pubkey,
    pub user_lp_ata: Pubkey,
    pub user_asset_ata: Pubkey,
    pub user_transfer_authority: Pubkey,
    pub asset_token_program: Pubkey,
    pub placeholder: AccountMeta,
}

impl VoltrRedeemSwap {
    /// Split concatenated account metas into `[request_withdraw_vault, withdraw_vault]` instructions.
    /// The caller must also create the receipt's LP ATA before these instructions
    /// using `derive_receipt_pda` and `derive_vault_lp_mint_pda`.
    pub fn into_instructions(self, lp_amount: u64) -> Result<[Instruction; 2]> {
        let placeholder = self.placeholder.clone();
        let metas: Vec<AccountMeta> = self.try_into()?;

        let split_at = metas
            .iter()
            .position(|meta| *meta == placeholder)
            .ok_or_else(|| anyhow::anyhow!("Placeholder not found in account metas"))?;

        let mut request_data = Vec::with_capacity(18);
        request_data.extend_from_slice(&anchor_discriminator("request_withdraw_vault"));
        request_data.extend_from_slice(&lp_amount.to_le_bytes());
        request_data.push(1); // is_amount_in_lp = true
        request_data.push(0); // is_withdraw_all = false

        let request_withdraw_ix = Instruction {
            program_id: VOLTR_VAULT_PROGRAM,
            accounts: metas[..split_at].to_vec(),
            data: request_data,
        };

        let withdraw_ix = Instruction {
            program_id: VOLTR_VAULT_PROGRAM,
            accounts: metas[split_at + 1..].to_vec(),
            data: anchor_discriminator("withdraw_vault").to_vec(),
        };

        Ok([request_withdraw_ix, withdraw_ix])
    }
}

impl TryFrom<VoltrRedeemSwap> for Vec<AccountMeta> {
    type Error = anyhow::Error;

    fn try_from(swap: VoltrRedeemSwap) -> Result<Self> {
        let (protocol_pda, _) =
            Pubkey::find_program_address(&[PROTOCOL_SEED], &VOLTR_VAULT_PROGRAM);

        let (vault_lp_mint_pda, _) = Pubkey::find_program_address(
            &[VAULT_LP_MINT_SEED, swap.vault_key.as_ref()],
            &VOLTR_VAULT_PROGRAM,
        );

        let (vault_asset_idle_auth_pda, _) = Pubkey::find_program_address(
            &[VAULT_ASSET_IDLE_AUTH_SEED, swap.vault_key.as_ref()],
            &VOLTR_VAULT_PROGRAM,
        );

        let (receipt_pda, _) = Pubkey::find_program_address(
            &[
                REQUEST_WITHDRAW_VAULT_RECEIPT_SEED,
                swap.vault_key.as_ref(),
                swap.user_transfer_authority.as_ref(),
            ],
            &VOLTR_VAULT_PROGRAM,
        );

        let receipt_lp_ata = Pubkey::find_program_address(
            &[
                receipt_pda.as_ref(),
                TOKEN_PROGRAM.as_ref(),
                vault_lp_mint_pda.as_ref(),
            ],
            &ATA_PROGRAM,
        )
        .0;

        let mut metas = Vec::with_capacity(24);

        // request_withdraw_vault accounts
        metas.push(AccountMeta::new(swap.user_transfer_authority, true));
        metas.push(AccountMeta::new_readonly(swap.user_transfer_authority, true));
        metas.push(AccountMeta::new_readonly(protocol_pda, false));
        metas.push(AccountMeta::new_readonly(swap.vault_key, false));
        metas.push(AccountMeta::new_readonly(vault_lp_mint_pda, false));
        metas.push(AccountMeta::new(swap.user_lp_ata, false));
        metas.push(AccountMeta::new(receipt_lp_ata, false));
        metas.push(AccountMeta::new(receipt_pda, false));
        metas.push(AccountMeta::new_readonly(TOKEN_PROGRAM, false));
        metas.push(AccountMeta::new_readonly(SystemProgramId, false));

        // placeholder separator
        metas.push(swap.placeholder);

        // withdraw_vault accounts
        metas.push(AccountMeta::new(swap.user_transfer_authority, true));
        metas.push(AccountMeta::new_readonly(protocol_pda, false));
        metas.push(AccountMeta::new(swap.vault_key, false));
        metas.push(AccountMeta::new_readonly(swap.vault_asset_mint, false));
        metas.push(AccountMeta::new(vault_lp_mint_pda, false));
        metas.push(AccountMeta::new(receipt_lp_ata, false));
        metas.push(AccountMeta::new(swap.vault_asset_idle_ata, false));
        metas.push(AccountMeta::new(vault_asset_idle_auth_pda, false));
        metas.push(AccountMeta::new(swap.user_asset_ata, false));
        metas.push(AccountMeta::new(receipt_pda, false));
        metas.push(AccountMeta::new_readonly(swap.asset_token_program, false));
        metas.push(AccountMeta::new_readonly(TOKEN_PROGRAM, false));
        metas.push(AccountMeta::new_readonly(SystemProgramId, false));

        Ok(metas)
    }
}

impl Amm for VoltrAmm {
    fn from_keyed_account(keyed_account: &KeyedAccount, _amm_context: &AmmContext) -> Result<Self> {
        let vault_state = Vault::load(&keyed_account.account.data)?;
        Ok(VoltrAmm::new(keyed_account.key, vault_state))
    }

    fn label(&self) -> String {
        self.label.clone()
    }

    fn program_id(&self) -> Pubkey {
        self.program_id
    }

    fn key(&self) -> Pubkey {
        self.vault_key
    }

    fn get_reserve_mints(&self) -> Vec<Pubkey> {
        vec![self.vault_state.asset.mint, self.vault_state.lp.mint]
    }

    fn get_accounts_to_update(&self) -> Vec<Pubkey> {
        vec![
            self.vault_key,
            self.vault_state.lp.mint,
            self.vault_state.asset.mint,
            self.vault_state.asset.idle_ata,
        ]
    }

    fn update(&mut self, account_map: &AccountMap) -> Result<()> {
        let vault_data = try_get_account_data(account_map, &self.vault_key)?;
        self.vault_state = Vault::load(vault_data)?;

        let lp_mint_data = try_get_account_data(account_map, &self.vault_state.lp.mint)?;
        let lp_mint = spl_token::state::Mint::unpack(lp_mint_data)?;
        self.lp_mint_supply = lp_mint.supply;
        self.lp_mint_decimals = lp_mint.decimals;

        let asset_mint_key = self.vault_state.asset.mint;
        let asset_account = account_map
            .get(&asset_mint_key)
            .ok_or_else(|| anyhow::anyhow!("Asset mint account not found"))?;

        self.asset_token_program = asset_account.owner;

        if asset_account.owner == TOKEN_PROGRAM {
            let mint = spl_token::state::Mint::unpack(&asset_account.data)?;
            self.asset_mint_decimals = mint.decimals;
        } else {
            let mint = StateWithExtensionsOwned::<Mint22>::unpack(asset_account.data.to_vec())?;
            self.asset_mint_decimals = mint.base.decimals;
        }

        let idle_ata_data = try_get_account_data(account_map, &self.vault_state.asset.idle_ata)?;
        if self.asset_token_program == TOKEN_PROGRAM {
            let idle_account = spl_token::state::Account::unpack(idle_ata_data)?;
            self.asset_idle_balance = idle_account.amount;
        } else {
            let idle_account = StateWithExtensionsOwned::<spl_token_2022::state::Account>::unpack(
                idle_ata_data.to_vec(),
            )?;
            self.asset_idle_balance = idle_account.base.amount;
        }

        Ok(())
    }

    fn quote(&self, quote_params: &QuoteParams) -> Result<Quote> {
        let asset_mint = self.vault_state.asset.mint;
        let lp_mint = self.vault_state.lp.mint;

        let is_issue = quote_params.input_mint == asset_mint && quote_params.output_mint == lp_mint;
        let is_redeem =
            quote_params.input_mint == lp_mint && quote_params.output_mint == asset_mint;

        if !is_issue && !is_redeem {
            return Err(VoltrAmmError::InvalidSourceMint.into());
        }

        let total_asset_value = self.vault_state.get_total_asset_value();
        let total_lp_supply_incl_fees = self
            .vault_state
            .get_total_lp_supply_incl_fees(self.lp_mint_supply)?;

        let current_ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(self.vault_state.last_updated_ts);

        let mgmt_fee_lp = self.estimate_management_fee_lp(
            current_ts,
            total_asset_value,
            total_lp_supply_incl_fees,
        )?;

        let total_lp_supply_after_mgmt_fee = total_lp_supply_incl_fees
            .checked_add(mgmt_fee_lp)
            .ok_or(VoltrAmmError::MathOverflow)?;

        if is_redeem {
            return self.quote_redeem(
                quote_params,
                current_ts,
                total_asset_value,
                total_lp_supply_after_mgmt_fee,
            );
        }

        let amount = quote_params.amount;
        let issuance_fee_bps = self.vault_state.fee_configuration.issuance_fee;

        let lp_to_mint_before_dead_weight = if total_lp_supply_incl_fees == 0 {
            calc_init_lp_to_mint(amount, self.asset_mint_decimals, self.lp_mint_decimals)?
        } else {
            calc_deposit_lp_to_mint(
                amount,
                total_lp_supply_after_mgmt_fee,
                total_asset_value,
                issuance_fee_bps,
            )?
        };

        let lp_to_mint = if self.vault_state.dead_weight == 0 {
            if lp_to_mint_before_dead_weight < DEAD_WEIGHT {
                return Err(VoltrAmmError::InvalidAmount.into());
            }
            lp_to_mint_before_dead_weight.saturating_sub(DEAD_WEIGHT)
        } else {
            lp_to_mint_before_dead_weight
        };

        let (fee_amount, fee_pct) = if issuance_fee_bps > 0 {
            let lp_without_fee = if total_lp_supply_incl_fees == 0 {
                lp_to_mint
            } else {
                calc_deposit_lp_to_mint(
                    amount,
                    total_lp_supply_after_mgmt_fee,
                    total_asset_value,
                    0,
                )?
            };
            let fee_in_lp = lp_without_fee.saturating_sub(lp_to_mint);
            let pct = rust_decimal::Decimal::new(issuance_fee_bps.into(), 4);
            (fee_in_lp, pct)
        } else {
            (0u64, rust_decimal::Decimal::ZERO)
        };

        Ok(Quote {
            fee_pct,
            in_amount: amount,
            out_amount: lp_to_mint,
            fee_amount,
            fee_mint: quote_params.input_mint,
            ..Quote::default()
        })
    }

    fn get_swap_and_account_metas(&self, swap_params: &SwapParams) -> Result<SwapAndAccountMetas> {
        let SwapParams {
            source_mint,
            source_token_account,
            destination_mint,
            destination_token_account,
            token_transfer_authority,
            ..
        } = swap_params;

        let is_issue = *source_mint == self.vault_state.asset.mint
            && *destination_mint == self.vault_state.lp.mint;
        let is_redeem = *source_mint == self.vault_state.lp.mint
            && *destination_mint == self.vault_state.asset.mint;

        if !is_issue && !is_redeem {
            return Err(VoltrAmmError::InvalidSourceMint.into());
        }

        if is_redeem {
            let mut account_metas: Vec<AccountMeta> = VoltrRedeemSwap {
                vault_key: self.vault_key,
                vault_asset_mint: self.vault_state.asset.mint,
                vault_asset_idle_ata: self.vault_state.asset.idle_ata,
                vault_lp_mint: self.vault_state.lp.mint,
                user_lp_ata: *source_token_account,
                user_asset_ata: *destination_token_account,
                user_transfer_authority: *token_transfer_authority,
                asset_token_program: self.asset_token_program,
                placeholder: swap_params.placeholder_account_meta(),
            }
            .try_into()?;
            account_metas.push(swap_params.placeholder_account_meta());
            return Ok(SwapAndAccountMetas {
                swap: Swap::TokenSwap,
                account_metas,
            });
        }

        let mut account_metas: Vec<AccountMeta> = VoltrSwap {
            vault_key: self.vault_key,
            vault_asset_mint: self.vault_state.asset.mint,
            vault_asset_idle_ata: self.vault_state.asset.idle_ata,
            vault_lp_mint: self.vault_state.lp.mint,
            user_source: *source_token_account,
            user_destination: *destination_token_account,
            user_transfer_authority: *token_transfer_authority,
            asset_token_program: self.asset_token_program,
        }
        .try_into()?;
        account_metas.push(swap_params.placeholder_account_meta());
        Ok(SwapAndAccountMetas {
            swap: Swap::TokenSwap,
            account_metas,
        })
    }

    fn clone_amm(&self) -> Box<dyn Amm + Send + Sync> {
        Box::new(self.clone())
    }

    fn supports_exact_out(&self) -> bool {
        false
    }
}
