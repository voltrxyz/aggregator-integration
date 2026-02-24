use solana_sdk::pubkey;
use solana_sdk::pubkey::Pubkey;

pub const VOLTR_VAULT_PROGRAM: Pubkey = pubkey!("vVoLTRjQmtFpiYoegx285Ze4gsLJ8ZxgFKVcuvmG1a8");

pub const TOKEN_PROGRAM: Pubkey = pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
pub const TOKEN_22_PROGRAM: Pubkey = pubkey!("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");

pub const PROTOCOL_SEED: &[u8] = b"protocol";
pub const VAULT_LP_MINT_SEED: &[u8] = b"vault_lp_mint";
pub const VAULT_LP_MINT_AUTH_SEED: &[u8] = b"vault_lp_mint_auth";
pub const VAULT_ASSET_IDLE_AUTH_SEED: &[u8] = b"vault_asset_idle_auth";

pub const MAX_FEE_BPS: u16 = 10_000;
pub const ONE_YEAR_U64: u64 = 365 * 24 * 60 * 60;
pub const DEAD_WEIGHT: u64 = 1_000;

pub const AMM_LABEL: &str = "VoltrAmm";
