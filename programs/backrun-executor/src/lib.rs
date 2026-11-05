//! Maintainer executor: invoke a list of swaps, spending each intermediate
//! output in full. Candidate executors may implement their own logic.
use solana_program::{
    account_info::AccountInfo,
    entrypoint::ProgramResult,
    instruction::{AccountMeta, Instruction},
    program::invoke,
    program_error::ProgramError,
    pubkey::Pubkey,
};
#[cfg(not(feature = "no-entrypoint"))]
solana_program::entrypoint!(process_instruction);

fn take<'a>(data: &mut &'a [u8], n: usize) -> Result<&'a [u8], ProgramError> {
    if data.len() < n {
        return Err(ProgramError::InvalidInstructionData);
    }
    let (head, tail) = data.split_at(n);
    *data = tail;
    Ok(head)
}
fn byte(data: &mut &[u8]) -> Result<usize, ProgramError> {
    Ok(take(data, 1)?[0] as usize)
}

pub fn process_instruction(
    _id: &Pubkey,
    accounts: &[AccountInfo],
    mut data: &[u8],
) -> ProgramResult {
    let n = byte(&mut data)?;
    if n > 8 {
        return Err(ProgramError::InvalidInstructionData);
    }
    for _ in 0..n {
        let program = accounts
            .get(byte(&mut data)?)
            .ok_or(ProgramError::NotEnoughAccountKeys)?;
        let dynamic_amount = byte(&mut data)?;
        let input = byte(&mut data)?;
        let len = u16::from_le_bytes(take(&mut data, 2)?.try_into().unwrap()) as usize;
        let count = byte(&mut data)?;
        let mut metas = Vec::with_capacity(count);
        let mut infos = Vec::with_capacity(count + 1);
        for _ in 0..count {
            let info = accounts
                .get(byte(&mut data)?)
                .ok_or(ProgramError::NotEnoughAccountKeys)?;
            let flags = byte(&mut data)?;
            if flags > 3 {
                return Err(ProgramError::InvalidInstructionData);
            }
            metas.push(AccountMeta {
                pubkey: *info.key,
                is_signer: flags & 1 != 0,
                is_writable: flags & 2 != 0,
            });
            infos.push(info.clone());
        }
        let mut ix_data = take(&mut data, len)?.to_vec();
        if dynamic_amount != 0 {
            let balance = accounts
                .get(input)
                .ok_or(ProgramError::NotEnoughAccountKeys)?
                .try_borrow_data()?;
            let amount = balance
                .get(64..72)
                .ok_or(ProgramError::InvalidAccountData)?;
            ix_data
                .get_mut(8..16)
                .ok_or(ProgramError::InvalidInstructionData)?
                .copy_from_slice(amount);
        }
        infos.push(program.clone());
        invoke(
            &Instruction {
                program_id: *program.key,
                accounts: metas,
                data: ix_data,
            },
            &infos,
        )?;
    }
    if !data.is_empty() {
        return Err(ProgramError::InvalidInstructionData);
    }
    Ok(())
}
