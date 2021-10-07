#![allow(clippy::integer_arithmetic)]
use {
    assert_matches::assert_matches,
    bincode::deserialize,
    solana_banks_client::BanksClient,
    solana_program_test::{processor, ProgramTest, ProgramTestContext, ProgramTestError},
    solana_sdk::{
        account_info::{next_account_info, AccountInfo},
        clock::Clock,
        entrypoint::ProgramResult,
        instruction::{AccountMeta, Instruction, InstructionError},
        program_error::ProgramError,
        pubkey::Pubkey,
        rent::Rent,
        signature::{Keypair, Signer},
        stake::{
            instruction as stake_instruction,
            state::{Authorized, Lockup, StakeState},
        },
        system_instruction, system_program,
        sysvar::{
            clock,
            stake_history::{self, StakeHistory},
            Sysvar,
        },
        transaction::{Transaction, TransactionError},
    },
    solana_vote_program::{
        vote_instruction,
        vote_state::{VoteInit, VoteState},
    },
    std::convert::TryInto,
};

// Use a big number to be sure that we get the right error
const WRONG_SLOT_ERROR: u32 = 123456;

async fn setup_stake(
    context: &mut ProgramTestContext,
    user: &Keypair,
    vote_address: &Pubkey,
    stake_lamports: u64,
) -> Pubkey {
    let stake_keypair = Keypair::new();
    let transaction = Transaction::new_signed_with_payer(
        &stake_instruction::create_account_and_delegate_stake(
            &context.payer.pubkey(),
            &stake_keypair.pubkey(),
            vote_address,
            &Authorized::auto(&user.pubkey()),
            &Lockup::default(),
            stake_lamports,
        ),
        Some(&context.payer.pubkey()),
        &vec![&context.payer, &stake_keypair, user],
        context.last_blockhash,
    );
    context
        .banks_client
        .process_transaction(transaction)
        .await
        .unwrap();
    stake_keypair.pubkey()
}

async fn setup_vote(context: &mut ProgramTestContext) -> Pubkey {
    // warp once to make sure stake config doesn't get rent-collected
    context.warp_to_slot(100).unwrap();
    let mut instructions = vec![];
    let validator_keypair = Keypair::new();
    instructions.push(system_instruction::create_account(
        &context.payer.pubkey(),
        &validator_keypair.pubkey(),
        42,
        0,
        &system_program::id(),
    ));
    let vote_lamports = Rent::default().minimum_balance(VoteState::size_of());
    let vote_keypair = Keypair::new();
    let user_keypair = Keypair::new();
    instructions.append(&mut vote_instruction::create_account(
        &context.payer.pubkey(),
        &vote_keypair.pubkey(),
        &VoteInit {
            node_pubkey: validator_keypair.pubkey(),
            authorized_voter: user_keypair.pubkey(),
            ..VoteInit::default()
        },
        vote_lamports,
    ));

    let transaction = Transaction::new_signed_with_payer(
        &instructions,
        Some(&context.payer.pubkey()),
        &vec![&context.payer, &validator_keypair, &vote_keypair],
        context.last_blockhash,
    );
    context
        .banks_client
        .process_transaction(transaction)
        .await
        .unwrap();

    vote_keypair.pubkey()
}

fn process_instruction(
    _program_id: &Pubkey,
    accounts: &[AccountInfo],
    input: &[u8],
) -> ProgramResult {
    let account_info_iter = &mut accounts.iter();
    let clock_info = next_account_info(account_info_iter)?;
    let clock = &Clock::from_account_info(clock_info)?;
    let expected_slot = u64::from_le_bytes(input.try_into().unwrap());
    if clock.slot == expected_slot {
        Ok(())
    } else {
        Err(ProgramError::Custom(WRONG_SLOT_ERROR))
    }
}

#[tokio::test]
async fn clock_sysvar_updated_from_warp() {
    let program_id = Pubkey::new_unique();
    // Initialize and start the test network
    let program_test = ProgramTest::new(
        "program-test-warp",
        program_id,
        processor!(process_instruction),
    );

    let mut context = program_test.start_with_context().await;
    let expected_slot = 100_000;
    let instruction = Instruction::new_with_bincode(
        program_id,
        &expected_slot,
        vec![AccountMeta::new_readonly(clock::id(), false)],
    );

    // Fail transaction
    let transaction = Transaction::new_signed_with_payer(
        &[instruction.clone()],
        Some(&context.payer.pubkey()),
        &[&context.payer],
        context.last_blockhash,
    );
    assert_eq!(
        context
            .banks_client
            .process_transaction(transaction)
            .await
            .unwrap_err()
            .unwrap(),
        TransactionError::InstructionError(0, InstructionError::Custom(WRONG_SLOT_ERROR))
    );

    // Warp to success!
    context.warp_to_slot(expected_slot).unwrap();
    let instruction = Instruction::new_with_bincode(
        program_id,
        &expected_slot,
        vec![AccountMeta::new_readonly(clock::id(), false)],
    );
    let transaction = Transaction::new_signed_with_payer(
        &[instruction],
        Some(&context.payer.pubkey()),
        &[&context.payer],
        context.last_blockhash,
    );
    context
        .banks_client
        .process_transaction(transaction)
        .await
        .unwrap();

    // Try warping again to the same slot
    assert_eq!(
        context.warp_to_slot(expected_slot).unwrap_err(),
        ProgramTestError::InvalidWarpSlot,
    );
}

#[tokio::test]
async fn rent_collected_from_warp() {
    let program_id = Pubkey::new_unique();
    // Initialize and start the test network
    let program_test = ProgramTest::default();

    let mut context = program_test.start_with_context().await;
    let account_size = 100;
    let keypair = Keypair::new();
    let account_lamports = Rent::default().minimum_balance(account_size) - 100; // not rent exempt
    let instruction = system_instruction::create_account(
        &context.payer.pubkey(),
        &keypair.pubkey(),
        account_lamports,
        account_size as u64,
        &program_id,
    );
    let transaction = Transaction::new_signed_with_payer(
        &[instruction],
        Some(&context.payer.pubkey()),
        &[&context.payer, &keypair],
        context.last_blockhash,
    );
    context
        .banks_client
        .process_transaction(transaction)
        .await
        .unwrap();
    let account = context
        .banks_client
        .get_account(keypair.pubkey())
        .await
        .expect("account exists")
        .unwrap();
    assert_eq!(account.lamports, account_lamports);

    // Warp forward and see that rent has been collected
    // This test was a bit flaky with one warp, but two warps always works
    let slots_per_epoch = context.genesis_config().epoch_schedule.slots_per_epoch;
    context.warp_to_slot(slots_per_epoch).unwrap();
    context.warp_to_slot(slots_per_epoch * 2).unwrap();

    let account = context
        .banks_client
        .get_account(keypair.pubkey())
        .await
        .expect("account exists")
        .unwrap();
    assert!(account.lamports < account_lamports);
}

#[tokio::test]
async fn stake_rewards_from_warp() {
    // Initialize and start the test network
    let program_test = ProgramTest::default();
    let mut context = program_test.start_with_context().await;
    let vote_address = setup_vote(&mut context).await;

    let user_keypair = Keypair::new();
    let stake_lamports = 1_000_000_000_000;
    let stake_address =
        setup_stake(&mut context, &user_keypair, &vote_address, stake_lamports).await;

    let account = context
        .banks_client
        .get_account(stake_address)
        .await
        .expect("account exists")
        .unwrap();
    assert_eq!(account.lamports, stake_lamports);

    // warp one epoch forward for normal inflation, no rewards collected
    let first_normal_slot = context.genesis_config().epoch_schedule.first_normal_slot;
    context.warp_to_slot(first_normal_slot).unwrap();
    let account = context
        .banks_client
        .get_account(stake_address)
        .await
        .expect("account exists")
        .unwrap();
    assert_eq!(account.lamports, stake_lamports);

    context.increment_vote_account_credits(&vote_address, 100);

    // go forward and see that rewards have been distributed
    let slots_per_epoch = context.genesis_config().epoch_schedule.slots_per_epoch;
    context
        .warp_to_slot(first_normal_slot + slots_per_epoch)
        .unwrap();

    let account = context
        .banks_client
        .get_account(stake_address)
        .await
        .expect("account exists")
        .unwrap();
    assert!(account.lamports > stake_lamports);

    // check that stake is fully active
    let stake_history_account = context
        .banks_client
        .get_account(stake_history::id())
        .await
        .expect("account exists")
        .unwrap();

    let clock_account = context
        .banks_client
        .get_account(clock::id())
        .await
        .expect("account exists")
        .unwrap();

    let stake_state: StakeState = deserialize(&account.data).unwrap();
    let stake_history: StakeHistory = deserialize(&stake_history_account.data).unwrap();
    let clock: Clock = deserialize(&clock_account.data).unwrap();
    let stake = stake_state.stake().unwrap();
    assert_matches!(
        stake
            .delegation
            .stake_activating_and_deactivating(clock.epoch, Some(&stake_history)),
        (_, 0, 0)
    );
}

async fn check_credits_observed(
    banks_client: &mut BanksClient,
    stake_address: Pubkey,
    expected_credits: u64,
) {
    let stake_account = banks_client
        .get_account(stake_address)
        .await
        .unwrap()
        .unwrap();
    let stake_state: StakeState = deserialize(&stake_account.data).unwrap();
    assert_eq!(
        stake_state.stake().unwrap().credits_observed,
        expected_credits
    );
}

#[tokio::test]
async fn stake_merge_immediately_after_activation() {
    let program_test = ProgramTest::default();
    let mut context = program_test.start_with_context().await;
    let vote_address = setup_vote(&mut context).await;
    context.increment_vote_account_credits(&vote_address, 100);

    let first_normal_slot = context.genesis_config().epoch_schedule.first_normal_slot;
    let slots_per_epoch = context.genesis_config().epoch_schedule.slots_per_epoch;
    let mut current_slot = first_normal_slot + slots_per_epoch;
    context.warp_to_slot(current_slot).unwrap();

    // this is annoying, but if no stake has earned rewards, the bank won't
    // iterate through the stakes at all, which means we can only test the
    // behavior of advancing credits observed if another stake is earning rewards

    // make a base stake which receives rewards
    let user_keypair = Keypair::new();
    let stake_lamports = 1_000_000_000_000;
    let base_stake_address =
        setup_stake(&mut context, &user_keypair, &vote_address, stake_lamports).await;
    check_credits_observed(&mut context.banks_client, base_stake_address, 100).await;
    context.increment_vote_account_credits(&vote_address, 100);

    current_slot += slots_per_epoch;
    context.warp_to_slot(current_slot).unwrap();

    // make another stake which will just have its credits observed advanced
    let absorbed_stake_address =
        setup_stake(&mut context, &user_keypair, &vote_address, stake_lamports).await;
    // the new stake is at the right value
    check_credits_observed(&mut context.banks_client, absorbed_stake_address, 200).await;
    // the base stake hasn't been moved forward because no rewards were earned
    check_credits_observed(&mut context.banks_client, base_stake_address, 100).await;

    context.increment_vote_account_credits(&vote_address, 100);
    current_slot += slots_per_epoch;
    context.warp_to_slot(current_slot).unwrap();

    // check that base stake has earned rewards and credits moved forward
    let stake_account = context
        .banks_client
        .get_account(base_stake_address)
        .await
        .unwrap()
        .unwrap();
    let stake_state: StakeState = deserialize(&stake_account.data).unwrap();
    assert_eq!(stake_state.stake().unwrap().credits_observed, 300);
    assert!(stake_account.lamports > stake_lamports);

    // check that new stake hasn't earned rewards, but that credits_observed have been advanced
    let stake_account = context
        .banks_client
        .get_account(absorbed_stake_address)
        .await
        .unwrap()
        .unwrap();
    let stake_state: StakeState = deserialize(&stake_account.data).unwrap();
    assert_eq!(stake_state.stake().unwrap().credits_observed, 300);
    assert_eq!(stake_account.lamports, stake_lamports);

    // sanity-check that the activation epoch was actually last epoch
    let clock_account = context
        .banks_client
        .get_account(clock::id())
        .await
        .unwrap()
        .unwrap();
    let clock: Clock = deserialize(&clock_account.data).unwrap();
    assert_eq!(
        clock.epoch,
        stake_state.delegation().unwrap().activation_epoch + 1
    );

    // sanity-check that it's possible to merge the just-activated stake with the older stake!
    let transaction = Transaction::new_signed_with_payer(
        &stake_instruction::merge(
            &base_stake_address,
            &absorbed_stake_address,
            &user_keypair.pubkey(),
        ),
        Some(&context.payer.pubkey()),
        &vec![&context.payer, &user_keypair],
        context.last_blockhash,
    );
    context
        .banks_client
        .process_transaction(transaction)
        .await
        .unwrap();
}