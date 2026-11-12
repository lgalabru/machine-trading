//! Token programs without the chain: their addresses, associated token
//! accounts under SPL Token or Token-2022, and what a Token-2022 mint's or
//! token account's extensions say. Builds for `wasm32-wasip2`;
//! [`chain::token`](crate::chain::token) builds the instructions on top.
//!
//! A Token-2022 account is the SPL Token layout, then its extensions:
//!
//! ```text
//!  mint     82 bytes, zero padding to 165
//!  account  165 bytes
//!  then     account type (1 mint, 2 account), and TLV entries:
//!           type u16, length u16, value; type 0 ends the list
//! ```
//!
//! A mint or account without extensions is just the base layout.

use solana_pubkey::Pubkey;

use crate::protocols::DecodeError;

/// The SPL Token program.
pub const TOKEN_PROGRAM_ID: Pubkey =
    solana_pubkey::pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");

/// The Token-2022 program (token extensions).
pub const TOKEN_2022_PROGRAM_ID: Pubkey =
    solana_pubkey::pubkey!("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");

/// The Associated Token Account program.
pub const ASSOCIATED_TOKEN_PROGRAM_ID: Pubkey =
    solana_pubkey::pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");

/// The system program.
pub const SYSTEM_PROGRAM_ID: Pubkey = solana_pubkey::pubkey!("11111111111111111111111111111111");

/// A mint's size without extensions.
pub const MINT_LEN: usize = 82;
/// A token account's size without extensions; extended mints are padded to
/// it too, so the account type always sits at this offset.
pub const ACCOUNT_LEN: usize = 165;

/// Whether `program` is one of the two token programs.
pub fn is_token_program(program: &Pubkey) -> bool {
    *program == TOKEN_PROGRAM_ID || *program == TOKEN_2022_PROGRAM_ID
}

/// The associated token account of `owner` for `mint`, under the token
/// program that owns the mint.
pub fn associated_token_address(owner: &Pubkey, mint: &Pubkey, token_program: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[owner.as_ref(), token_program.as_ref(), mint.as_ref()],
        &ASSOCIATED_TOKEN_PROGRAM_ID,
    )
    .0
}

/// A Token-2022 extension, by its type number. The ones the kit reads or
/// checks are named; any other is kept by number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Extension {
    TransferFeeConfig,
    TransferFeeAmount,
    MintCloseAuthority,
    ConfidentialTransferMint,
    DefaultAccountState,
    ImmutableOwner,
    MemoTransfer,
    NonTransferable,
    InterestBearingConfig,
    CpiGuard,
    PermanentDelegate,
    TransferHook,
    MetadataPointer,
    TokenMetadata,
    ScaledUiAmount,
    Pausable,
    Other(u16),
}

impl Extension {
    /// The extension a type number names.
    pub fn from_type(ty: u16) -> Self {
        match ty {
            1 => Self::TransferFeeConfig,
            2 => Self::TransferFeeAmount,
            3 => Self::MintCloseAuthority,
            4 => Self::ConfidentialTransferMint,
            6 => Self::DefaultAccountState,
            7 => Self::ImmutableOwner,
            8 => Self::MemoTransfer,
            9 => Self::NonTransferable,
            10 => Self::InterestBearingConfig,
            11 => Self::CpiGuard,
            12 => Self::PermanentDelegate,
            14 => Self::TransferHook,
            18 => Self::MetadataPointer,
            19 => Self::TokenMetadata,
            25 => Self::ScaledUiAmount,
            26 => Self::Pausable,
            other => Self::Other(other),
        }
    }
}

/// One of a transfer-fee config's two fees: from `epoch` on, `basis_points`
/// of each transfer, at most `maximum_fee` atoms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransferFee {
    pub epoch: u64,
    pub maximum_fee: u64,
    pub basis_points: u16,
}

impl TransferFee {
    /// The fee on a transfer of `amount` atoms, as Token-2022 charges it:
    /// rounded up, capped at the maximum.
    pub fn fee(&self, amount: u64) -> u64 {
        if self.basis_points == 0 || amount == 0 {
            return 0;
        }
        let fee = (u128::from(amount) * u128::from(self.basis_points)).div_ceil(10_000);
        u64::try_from(fee).unwrap_or(u64::MAX).min(self.maximum_fee)
    }
}

/// A mint's transfer fees: the older applies until the newer's epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransferFeeConfig {
    pub older: TransferFee,
    pub newer: TransferFee,
}

impl TransferFeeConfig {
    /// The fee in force at `epoch`.
    pub fn at(&self, epoch: u64) -> &TransferFee {
        if epoch >= self.newer.epoch {
            &self.newer
        } else {
            &self.older
        }
    }
}

/// A mint's UI amount multiplier (`ScaledUiAmount`): a holder of `x` atoms
/// is shown `x × multiplier`, as a tokenized stock shows splits and
/// dividends. The multiplier becomes `new_multiplier` at
/// `new_multiplier_effective_timestamp` (Unix seconds).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScaledUiAmount {
    pub multiplier: f64,
    pub new_multiplier: f64,
    pub new_multiplier_effective_timestamp: i64,
}

impl ScaledUiAmount {
    /// The multiplier in force at `unix_timestamp`.
    pub fn at(&self, unix_timestamp: i64) -> f64 {
        if unix_timestamp >= self.new_multiplier_effective_timestamp {
            self.new_multiplier
        } else {
            self.multiplier
        }
    }
}

/// A mint, as either token program keeps it.
#[derive(Debug, Clone, PartialEq)]
pub struct Mint {
    /// The program that owns it.
    pub program: Pubkey,
    pub supply: u64,
    pub decimals: u8,
    /// Its Token-2022 extensions, in the account's order.
    pub extensions: Vec<Extension>,
    pub transfer_fee: Option<TransferFeeConfig>,
    /// Whether the mint is paused (the `Pausable` extension).
    pub paused: bool,
    /// Whether new token accounts start frozen (`DefaultAccountState`).
    pub frozen_by_default: bool,
    /// The program its transfers call (`TransferHook`), if the hook is set.
    /// A mint may carry the extension with no program: the hook is off.
    pub transfer_hook: Option<Pubkey>,
    /// Its UI amount multiplier (`ScaledUiAmount`).
    pub scaled_ui_amount: Option<ScaledUiAmount>,
}

impl Mint {
    /// Decodes a mint owned by `program`.
    pub fn decode(program: &Pubkey, data: &[u8]) -> Result<Self, DecodeError> {
        let error = |detail: String| DecodeError::new("mint", detail);
        if !is_token_program(program) {
            return Err(error(format!("owned by {program}, not a token program")));
        }
        if data.len() < MINT_LEN {
            return Err(error(format!("{} bytes", data.len())));
        }
        if data[45] != 1 {
            return Err(error("not initialized".into()));
        }
        let supply = u64::from_le_bytes(data[36..44].try_into().expect("8 bytes"));
        let mut mint = Self {
            program: *program,
            supply,
            decimals: data[44],
            extensions: Vec::new(),
            transfer_fee: None,
            paused: false,
            frozen_by_default: false,
            transfer_hook: None,
            scaled_ui_amount: None,
        };
        let short = |what: &str| error(format!("a short {what}"));
        for (extension, value) in extensions(program, data, MINT_LEN, AccountType::Mint)? {
            match extension {
                Extension::TransferFeeConfig => {
                    // Two optional authorities, the withheld amount, then
                    // the older and the newer fee.
                    let fee = |at: usize| -> Result<TransferFee, DecodeError> {
                        let bytes = value
                            .get(at..at + 18)
                            .ok_or_else(|| error("a short transfer fee config".into()))?;
                        Ok(TransferFee {
                            epoch: u64::from_le_bytes(bytes[0..8].try_into().expect("8")),
                            maximum_fee: u64::from_le_bytes(bytes[8..16].try_into().expect("8")),
                            basis_points: u16::from_le_bytes(bytes[16..18].try_into().expect("2")),
                        })
                    };
                    mint.transfer_fee = Some(TransferFeeConfig {
                        older: fee(72)?,
                        newer: fee(90)?,
                    });
                }
                // The default state: 1 initialized, 2 frozen.
                Extension::DefaultAccountState => {
                    mint.frozen_by_default = value.first() == Some(&2)
                }
                // The authority, then whether it is paused.
                Extension::Pausable => mint.paused = value.get(32) == Some(&1),
                // The authority, then the program; zeros are no program.
                Extension::TransferHook => {
                    let program: [u8; 32] = value
                        .get(32..64)
                        .and_then(|b| b.try_into().ok())
                        .ok_or_else(|| short("transfer hook"))?;
                    mint.transfer_hook =
                        (program != [0; 32]).then(|| Pubkey::new_from_array(program));
                }
                // The authority, the multiplier, when the new one takes
                // effect, and the new one.
                Extension::ScaledUiAmount => {
                    let field = |at: usize| -> Result<[u8; 8], DecodeError> {
                        value
                            .get(at..at + 8)
                            .and_then(|b| b.try_into().ok())
                            .ok_or_else(|| short("scaled UI amount config"))
                    };
                    mint.scaled_ui_amount = Some(ScaledUiAmount {
                        multiplier: f64::from_le_bytes(field(32)?),
                        new_multiplier_effective_timestamp: i64::from_le_bytes(field(40)?),
                        new_multiplier: f64::from_le_bytes(field(48)?),
                    });
                }
                _ => {}
            }
            mint.extensions.push(extension);
        }
        Ok(mint)
    }

    /// Atoms received when `amount` atoms are sent at `epoch`: the amount
    /// less the transfer fee, which the destination holds back.
    pub fn received(&self, amount: u64, epoch: u64) -> u64 {
        amount.saturating_sub(self.transfer_fee(amount, epoch))
    }

    /// The transfer fee on `amount` atoms at `epoch`.
    pub fn transfer_fee(&self, amount: u64, epoch: u64) -> u64 {
        self.transfer_fee
            .map_or(0, |config| config.at(epoch).fee(amount))
    }

    /// Why the kit can't move this token, if it can't: a transfer hook
    /// that is set needs accounts the kit doesn't resolve; a
    /// non-transferable or paused mint moves nothing; a mint whose new
    /// accounts start frozen can't fund one.
    pub fn unsupported(&self) -> Option<String> {
        if let Some(program) = self.transfer_hook {
            return Some(format!("its transfers call the hook program {program}"));
        }
        if self.extensions.contains(&Extension::NonTransferable) {
            return Some("it is non-transferable".into());
        }
        if self.paused {
            return Some("it is paused".into());
        }
        if self.frozen_by_default {
            return Some("its new token accounts start frozen".into());
        }
        None
    }
}

/// A token account's base fields, as either token program keeps them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenAccountInfo {
    pub mint: Pubkey,
    pub owner: Pubkey,
    pub amount: u64,
    /// Frozen accounts can't send or receive.
    pub frozen: bool,
    /// Its Token-2022 extensions.
    pub extensions: Vec<Extension>,
    /// Fees withheld in it from transfers it received (`TransferFeeAmount`).
    pub withheld: u64,
}

impl TokenAccountInfo {
    /// Decodes a token account owned by `program`.
    pub fn decode(program: &Pubkey, data: &[u8]) -> Result<Self, DecodeError> {
        let error = |detail: String| DecodeError::new("token account", detail);
        if !is_token_program(program) {
            return Err(error(format!("owned by {program}, not a token program")));
        }
        if data.len() < ACCOUNT_LEN {
            return Err(error(format!("{} bytes", data.len())));
        }
        let key = |at: usize| Pubkey::new_from_array(data[at..at + 32].try_into().expect("32"));
        let state = data[108];
        if state == 0 {
            return Err(error("not initialized".into()));
        }
        let mut info = Self {
            mint: key(0),
            owner: key(32),
            amount: u64::from_le_bytes(data[64..72].try_into().expect("8")),
            frozen: state == 2,
            extensions: Vec::new(),
            withheld: 0,
        };
        for (extension, value) in extensions(program, data, ACCOUNT_LEN, AccountType::Account)? {
            if extension == Extension::TransferFeeAmount {
                info.withheld = value
                    .get(..8)
                    .map_or(0, |b| u64::from_le_bytes(b.try_into().expect("8")));
            }
            info.extensions.push(extension);
        }
        Ok(info)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AccountType {
    Mint = 1,
    Account = 2,
}

/// The TLV entries after a Token-2022 account's base, with their values.
fn extensions<'a>(
    program: &Pubkey,
    data: &'a [u8],
    base: usize,
    expected: AccountType,
) -> Result<Vec<(Extension, &'a [u8])>, DecodeError> {
    let what = match expected {
        AccountType::Mint => "mint",
        AccountType::Account => "token account",
    };
    if *program != TOKEN_2022_PROGRAM_ID || data.len() == base {
        return Ok(Vec::new());
    }
    if data.len() <= ACCOUNT_LEN || data[base..ACCOUNT_LEN].iter().any(|b| *b != 0) {
        return Err(DecodeError::new(
            what,
            format!("{} bytes, with no account type", data.len()),
        ));
    }
    if data[ACCOUNT_LEN] != expected as u8 {
        return Err(DecodeError::new(
            what,
            format!("account type {}", data[ACCOUNT_LEN]),
        ));
    }
    let mut found = Vec::new();
    let mut at = ACCOUNT_LEN + 1;
    while let Some(header) = data.get(at..at + 4) {
        let ty = u16::from_le_bytes([header[0], header[1]]);
        let len = usize::from(u16::from_le_bytes([header[2], header[3]]));
        if ty == 0 {
            break;
        }
        let value = data.get(at + 4..at + 4 + len).ok_or_else(|| {
            DecodeError::new(what, format!("extension {ty} runs past the account"))
        })?;
        found.push((Extension::from_type(ty), value));
        at += 4 + len;
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use spl_token_2022_interface::{
        extension::{
            BaseStateWithExtensionsMut, ExtensionType, StateWithExtensionsMut,
            default_account_state::DefaultAccountState,
            transfer_fee::{
                TransferFee as SplFee, TransferFeeAmount, TransferFeeConfig as SplConfig,
            },
        },
        state::{Account, AccountState, Mint as SplMint},
    };

    use super::*;

    fn fee(epoch: u64, maximum_fee: u64, basis_points: u16) -> SplFee {
        SplFee {
            epoch: epoch.into(),
            maximum_fee: maximum_fee.into(),
            transfer_fee_basis_points: basis_points.into(),
        }
    }

    /// A Token-2022 mint with a transfer fee and frozen default accounts,
    /// written by the program's own crate.
    fn extended_mint() -> Vec<u8> {
        let types = [
            ExtensionType::TransferFeeConfig,
            ExtensionType::DefaultAccountState,
        ];
        let len = ExtensionType::try_calculate_account_len::<SplMint>(&types).unwrap();
        let mut data = vec![0; len];
        let mut state = StateWithExtensionsMut::<SplMint>::unpack_uninitialized(&mut data).unwrap();
        let config = state.init_extension::<SplConfig>(true).unwrap();
        config.older_transfer_fee = fee(0, 1_000, 50);
        config.newer_transfer_fee = fee(10, 5_000, 100);
        state
            .init_extension::<DefaultAccountState>(true)
            .unwrap()
            .state = AccountState::Frozen as u8;
        state.base = SplMint {
            supply: 42,
            decimals: 6,
            is_initialized: true,
            ..SplMint::default()
        };
        state.pack_base();
        state.init_account_type().unwrap();
        data
    }

    #[test]
    fn reads_a_token_2022_mints_fees_and_default_state() {
        let mint = Mint::decode(&TOKEN_2022_PROGRAM_ID, &extended_mint()).unwrap();
        assert_eq!((mint.supply, mint.decimals), (42, 6));
        assert_eq!(
            mint.extensions,
            vec![Extension::TransferFeeConfig, Extension::DefaultAccountState]
        );
        assert!(mint.frozen_by_default);
        assert!(mint.unsupported().unwrap().contains("frozen"));
        // 0.5% before epoch 10, capped at 1,000 atoms; 1% after, at 5,000.
        assert_eq!(mint.transfer_fee(10_000, 0), 50);
        assert_eq!(mint.transfer_fee(10_001, 0), 51);
        assert_eq!(mint.transfer_fee(1_000_000, 0), 1_000);
        assert_eq!(mint.transfer_fee(10_000, 10), 100);
        assert_eq!(mint.received(1_000_000, 12), 995_000);
        // The program's crate charges the same.
        let spl = fee(0, 1_000, 50);
        for amount in [0, 1, 199, 200, 10_001, 1_000_000] {
            assert_eq!(
                mint.transfer_fee(amount, 0),
                spl.calculate_fee(amount).unwrap()
            );
        }
    }

    /// A mint laid out as the tokenized stocks and stablecoins on mainnet
    /// are (SPCX, SPCXx, PYUSD, PAXG): a permanent delegate, accounts
    /// initialized by default, pausable, confidential transfers, a transfer
    /// hook with no program, a UI multiplier and metadata.
    fn stock_like_mint(hook: Option<Pubkey>) -> Vec<u8> {
        use spl_token_2022_interface::extension::{
            confidential_transfer::ConfidentialTransferMint, metadata_pointer::MetadataPointer,
            pausable::PausableConfig, permanent_delegate::PermanentDelegate,
            scaled_ui_amount::ScaledUiAmountConfig, transfer_hook::TransferHook,
        };
        use spl_token_metadata_interface::state::TokenMetadata;
        let authority = Pubkey::new_unique();
        let types = [
            ExtensionType::PermanentDelegate,
            ExtensionType::DefaultAccountState,
            ExtensionType::Pausable,
            ExtensionType::ConfidentialTransferMint,
            ExtensionType::TransferHook,
            ExtensionType::ScaledUiAmount,
            ExtensionType::MetadataPointer,
        ];
        let metadata = TokenMetadata {
            name: "SpaceX".into(),
            symbol: "SPCX".into(),
            uri: "https://example.invalid/spcx.json".into(),
            ..TokenMetadata::default()
        };
        let len = ExtensionType::try_calculate_account_len::<SplMint>(&types).unwrap()
            + 4
            + metadata.tlv_size_of().unwrap();
        let mut data = vec![0; len];
        let mut state = StateWithExtensionsMut::<SplMint>::unpack_uninitialized(&mut data).unwrap();
        state
            .init_extension::<PermanentDelegate>(true)
            .unwrap()
            .delegate = Some(authority).try_into().unwrap();
        state
            .init_extension::<DefaultAccountState>(true)
            .unwrap()
            .state = AccountState::Initialized as u8;
        state
            .init_extension::<PausableConfig>(true)
            .unwrap()
            .authority = Some(authority).try_into().unwrap();
        state
            .init_extension::<ConfidentialTransferMint>(true)
            .unwrap()
            .authority = Some(authority).try_into().unwrap();
        let transfer_hook = state.init_extension::<TransferHook>(true).unwrap();
        transfer_hook.authority = Some(authority).try_into().unwrap();
        transfer_hook.program_id = hook.try_into().unwrap();
        let scaled = state.init_extension::<ScaledUiAmountConfig>(true).unwrap();
        scaled.authority = Some(authority).try_into().unwrap();
        scaled.multiplier = 1.5.into();
        scaled.new_multiplier_effective_timestamp = 1_800_000_000.into();
        scaled.new_multiplier = 3.0.into();
        state
            .init_extension::<MetadataPointer>(true)
            .unwrap()
            .authority = Some(authority).try_into().unwrap();
        state.base = SplMint {
            supply: 96_000_000_000,
            decimals: 6,
            is_initialized: true,
            ..SplMint::default()
        };
        state.pack_base();
        state.init_account_type().unwrap();
        state.init_variable_len_extension(&metadata, false).unwrap();
        data
    }

    #[test]
    fn reads_a_tokenized_stocks_mint() {
        let mint = Mint::decode(&TOKEN_2022_PROGRAM_ID, &stock_like_mint(None)).unwrap();
        assert_eq!((mint.supply, mint.decimals), (96_000_000_000, 6));
        assert_eq!(
            mint.extensions,
            vec![
                Extension::PermanentDelegate,
                Extension::DefaultAccountState,
                Extension::Pausable,
                Extension::ConfidentialTransferMint,
                Extension::TransferHook,
                Extension::ScaledUiAmount,
                Extension::MetadataPointer,
                Extension::TokenMetadata,
            ]
        );
        assert!(!mint.paused && !mint.frozen_by_default);
        // The hook is there with no program: transfers call nothing.
        assert_eq!(mint.transfer_hook, None);
        assert_eq!(mint.unsupported(), None);
        let scaled = mint.scaled_ui_amount.unwrap();
        assert_eq!(scaled.at(1_799_999_999), 1.5);
        assert_eq!(scaled.at(1_800_000_000), 3.0);

        // With a hook program set, the kit refuses it.
        let program = Pubkey::new_unique();
        let hooked = Mint::decode(&TOKEN_2022_PROGRAM_ID, &stock_like_mint(Some(program))).unwrap();
        assert_eq!(hooked.transfer_hook, Some(program));
        assert!(hooked.unsupported().unwrap().contains("hook"));
    }

    #[test]
    fn reads_plain_mints_and_accounts_under_either_program() {
        let mut data = vec![0; MINT_LEN];
        data[44] = 9;
        data[45] = 1;
        for program in [TOKEN_PROGRAM_ID, TOKEN_2022_PROGRAM_ID] {
            let mint = Mint::decode(&program, &data).unwrap();
            assert_eq!(mint.decimals, 9);
            assert!(mint.extensions.is_empty() && mint.unsupported().is_none());
        }
        assert!(Mint::decode(&SYSTEM_PROGRAM_ID, &data).is_err());
        // Extensions only exist under Token-2022.
        assert!(Mint::decode(&TOKEN_2022_PROGRAM_ID, &extended_mint()[..100]).is_err());
    }

    #[test]
    fn reads_an_extended_token_account() {
        let owner = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let types = [
            ExtensionType::TransferFeeAmount,
            ExtensionType::ImmutableOwner,
        ];
        let len = ExtensionType::try_calculate_account_len::<Account>(&types).unwrap();
        let mut data = vec![0; len];
        let mut state = StateWithExtensionsMut::<Account>::unpack_uninitialized(&mut data).unwrap();
        state
            .init_extension::<TransferFeeAmount>(true)
            .unwrap()
            .withheld_amount = 7.into();
        state
            .init_extension::<spl_token_2022_interface::extension::immutable_owner::ImmutableOwner>(
                true,
            )
            .unwrap();
        state.base = Account {
            mint,
            owner,
            amount: 993,
            state: AccountState::Initialized,
            ..Account::default()
        };
        state.pack_base();
        state.init_account_type().unwrap();
        let info = TokenAccountInfo::decode(&TOKEN_2022_PROGRAM_ID, &data).unwrap();
        assert_eq!((info.mint, info.owner, info.amount), (mint, owner, 993));
        assert_eq!(info.withheld, 7);
        assert!(!info.frozen);
        assert_eq!(
            info.extensions,
            vec![Extension::TransferFeeAmount, Extension::ImmutableOwner]
        );
        // The same bytes under SPL Token are not an account it wrote.
        assert!(TokenAccountInfo::decode(&TOKEN_PROGRAM_ID, &data[..ACCOUNT_LEN]).is_ok());
    }

    #[test]
    fn associated_accounts_differ_by_program() {
        let owner = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let classic = associated_token_address(&owner, &mint, &TOKEN_PROGRAM_ID);
        let extended = associated_token_address(&owner, &mint, &TOKEN_2022_PROGRAM_ID);
        assert_ne!(classic, extended);
        assert_eq!(
            classic,
            spl_associated_token_account_interface::address::get_associated_token_address(
                &owner, &mint
            )
        );
        assert_eq!(
            extended,
            spl_associated_token_account_interface::address::get_associated_token_address_with_program_id(
                &owner,
                &mint,
                &TOKEN_2022_PROGRAM_ID
            )
        );
    }
}
