// Copyright (C) 2019-2021 Parity Technologies (UK) Ltd.
// Copyright (C) 2021 Subspace Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// 	http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Consensus extension module tests for Subspace consensus.

use crate::mock::{
    create_root_block, generate_equivocation_proof, go_to_block, make_pre_digest, new_test_ext,
    progress_to_block, Event, Origin, ReportLongevity, Subspace, System, Test,
    INITIAL_SOLUTION_RANGE, SLOT_PROBABILITY,
};
use crate::{
    compute_randomness, Call, Config, CurrentSlot, EpochConfig, EpochStart, Error, NextEpochConfig,
    NextRandomness, SegmentIndex, UnderConstruction, WeightInfo,
};
use codec::Encode;
use frame_support::weights::Pays;
use frame_support::{
    assert_err, assert_noop, assert_ok, traits::OnFinalize, weights::GetDispatchInfo,
};
use frame_system::{EventRecord, Phase};
use schnorrkel::Keypair;
use sp_consensus_slots::Slot;
use sp_consensus_subspace::digests::NextConfigDescriptor;
use sp_consensus_subspace::{
    digests::Solution, FarmerPublicKey, SubspaceEpochConfiguration, SUBSPACE_ENGINE_ID,
};
use sp_core::Public;
use sp_runtime::traits::Header;
use sp_runtime::transaction_validity::{
    InvalidTransaction, TransactionPriority, TransactionSource, TransactionValidity,
    ValidTransaction,
};
use sp_runtime::{DigestItem, DispatchError};
use subspace_core_primitives::RANDOMNESS_LENGTH;

const EMPTY_RANDOMNESS: [u8; 32] = [
    74, 25, 49, 128, 53, 97, 244, 49, 222, 202, 176, 2, 231, 66, 95, 10, 133, 49, 213, 228, 86,
    161, 164, 127, 217, 153, 138, 37, 48, 192, 248, 0,
];

#[test]
fn empty_randomness_is_correct() {
    let s = compute_randomness([0; RANDOMNESS_LENGTH], 0, std::iter::empty(), None);
    assert_eq!(s, EMPTY_RANDOMNESS);
}

#[test]
fn first_block_epoch_zero_start() {
    let mut ext = new_test_ext();

    ext.execute_with(|| {
        let genesis_slot = Slot::from(100);
        let solution = Solution::genesis_solution();
        let por_randomness = sp_io::hashing::blake2_256(&solution.signature);
        let pre_digest = make_pre_digest(genesis_slot, solution);

        assert_eq!(Subspace::genesis_slot(), Slot::from(0));
        System::initialize(&1, &Default::default(), &pre_digest, Default::default());

        Subspace::do_initialize(1);
        assert_eq!(Subspace::genesis_slot(), genesis_slot);
        assert_eq!(Subspace::current_slot(), genesis_slot);
        assert_eq!(Subspace::epoch_index(), 0);
        assert_eq!(Subspace::author_por_randomness(), Some(por_randomness));

        Subspace::on_finalize(1);
        let header = System::finalize();

        assert_eq!(SegmentIndex::<Test>::get(), 0);
        assert_eq!(UnderConstruction::<Test>::get(0), vec![por_randomness]);
        assert_eq!(Subspace::randomness(), [0; 32]);
        assert_eq!(NextRandomness::<Test>::get(), [0; 32]);

        assert_eq!(header.digest.logs.len(), 4);
        assert_eq!(pre_digest.logs.len(), 1);
        assert_eq!(header.digest.logs[0], pre_digest.logs[0]);

        let consensus_log = sp_consensus_subspace::ConsensusLog::NextEpochData(
            sp_consensus_subspace::digests::NextEpochDescriptor {
                randomness: Subspace::randomness(),
            },
        );
        let consensus_digest = DigestItem::Consensus(SUBSPACE_ENGINE_ID, consensus_log.encode());

        // first epoch descriptor has same info as last.
        assert_eq!(header.digest.logs[1], consensus_digest.clone())
    })
}

#[test]
fn author_por_output() {
    let mut ext = new_test_ext();

    ext.execute_with(|| {
        let genesis_slot = Slot::from(10);
        let solution = Solution::genesis_solution();
        let por_randomness = sp_io::hashing::blake2_256(&solution.signature);
        let pre_digest = make_pre_digest(genesis_slot, solution);

        System::initialize(&1, &Default::default(), &pre_digest, Default::default());

        Subspace::do_initialize(1);
        assert_eq!(Subspace::author_por_randomness(), Some(por_randomness));

        Subspace::on_finalize(1);
        System::finalize();
        assert_eq!(Subspace::author_por_randomness(), Some(por_randomness));
    })
}

#[test]
fn can_predict_next_epoch_change() {
    new_test_ext().execute_with(|| {
        let keypair = Keypair::generate();

        assert_eq!(<Test as Config>::EpochDuration::get(), 3);
        // this sets the genesis slot to 6;
        go_to_block(&keypair, 1, 6);
        assert_eq!(*Subspace::genesis_slot(), 6);
        assert_eq!(*Subspace::current_slot(), 6);
        assert_eq!(Subspace::epoch_index(), 0);

        progress_to_block(&keypair, 5);

        assert_eq!(Subspace::epoch_index(), 5 / 3);
        assert_eq!(*Subspace::current_slot(), 10);

        // next epoch change will be at
        assert_eq!(*Subspace::current_epoch_start(), 9); // next change will be 12, 2 slots from now
        assert_eq!(
            Subspace::next_expected_epoch_change(System::block_number()),
            Some(5 + 2)
        );
    })
}

#[test]
fn can_update_solution_range_on_era_change() {
    new_test_ext().execute_with(|| {
        let keypair = Keypair::generate();

        assert_eq!(<Test as Config>::EraDuration::get(), 4);
        assert_eq!(
            <Test as Config>::InitialSolutionRange::get(),
            INITIAL_SOLUTION_RANGE
        );
        // There should be no solution range stored during first era
        assert_eq!(Subspace::solution_range(), INITIAL_SOLUTION_RANGE);

        // We produce blocks on every slot
        progress_to_block(&keypair, 4);
        // Still no solution range update
        assert_eq!(Subspace::solution_range(), INITIAL_SOLUTION_RANGE);
        progress_to_block(&keypair, 5);

        // Second era should have solution range updated
        assert_ne!(Subspace::solution_range(), INITIAL_SOLUTION_RANGE);

        // Because blocks were produced on every slot, apparent pledged space must increase and
        // solution range should decrease
        let last_solution_range = Subspace::solution_range();
        assert!(last_solution_range < INITIAL_SOLUTION_RANGE);

        // Progress almost to era change
        progress_to_block(&keypair, 8);
        // Change era such that it takes more slots than expected
        go_to_block(
            &keypair,
            9,
            u64::from(Subspace::current_slot())
                + (4 * SLOT_PROBABILITY.1 / SLOT_PROBABILITY.0 + 10),
        );
        // This should cause solution range to increase as apparent pledged space decreased
        assert!(Subspace::solution_range() > last_solution_range);
    })
}

#[test]
fn can_update_salt_on_eon_change() {
    new_test_ext().execute_with(|| {
        let keypair = Keypair::generate();

        assert_eq!(<Test as Config>::EonDuration::get(), 5);
        // Initial salt equals to eon
        assert_eq!(Subspace::salt(), 0);

        // We produce blocks on every slot
        progress_to_block(&keypair, 5);
        // Still no salt update
        assert_eq!(Subspace::salt(), 0);
        progress_to_block(&keypair, 6);

        // Second eon should have salt updated
        assert_eq!(Subspace::salt(), 1);

        // We produce blocks on every slot
        progress_to_block(&keypair, 10);
        // Just before eon update, still the same salt as before
        assert_eq!(Subspace::salt(), 1);
        progress_to_block(&keypair, 11);

        // Third eon should have salt updated again
        assert_eq!(Subspace::salt(), 2);
    })
}

#[test]
fn can_enact_next_config() {
    new_test_ext().execute_with(|| {
        let keypair = Keypair::generate();

        assert_eq!(<Test as Config>::EpochDuration::get(), 3);
        // this sets the genesis slot to 6;
        go_to_block(&keypair, 1, 6);
        assert_eq!(*Subspace::genesis_slot(), 6);
        assert_eq!(*Subspace::current_slot(), 6);
        assert_eq!(Subspace::epoch_index(), 0);
        go_to_block(&keypair, 2, 7);

        let current_config = SubspaceEpochConfiguration { c: (0, 4) };

        let next_config = SubspaceEpochConfiguration { c: (1, 4) };

        let next_next_config = SubspaceEpochConfiguration { c: (2, 4) };

        EpochConfig::<Test>::put(current_config);
        NextEpochConfig::<Test>::put(next_config.clone());

        assert_eq!(NextEpochConfig::<Test>::get(), Some(next_config.clone()));

        Subspace::plan_config_change(
            Origin::root(),
            NextConfigDescriptor::V1 {
                c: next_next_config.c,
            },
        )
        .unwrap();

        progress_to_block(&keypair, 4);
        Subspace::on_finalize(9);
        let header = System::finalize();

        assert_eq!(EpochConfig::<Test>::get(), Some(next_config));
        assert_eq!(
            NextEpochConfig::<Test>::get(),
            Some(next_next_config.clone())
        );

        let consensus_log =
            sp_consensus_subspace::ConsensusLog::NextConfigData(NextConfigDescriptor::V1 {
                c: next_next_config.c,
            });
        let consensus_digest = DigestItem::Consensus(SUBSPACE_ENGINE_ID, consensus_log.encode());

        assert_eq!(header.digest.logs[4], consensus_digest.clone())
    });
}

#[test]
fn only_root_can_enact_config_change() {
    new_test_ext().execute_with(|| {
        let next_config = NextConfigDescriptor::V1 { c: (1, 4) };

        let res = Subspace::plan_config_change(Origin::none(), next_config.clone());

        assert_noop!(res, DispatchError::BadOrigin);

        let res = Subspace::plan_config_change(Origin::signed(1), next_config.clone());

        assert_noop!(res, DispatchError::BadOrigin);

        let res = Subspace::plan_config_change(Origin::root(), next_config);

        assert!(res.is_ok());
    });
}

#[test]
fn can_fetch_current_and_next_epoch_data() {
    new_test_ext().execute_with(|| {
        let keypair = Keypair::generate();

        EpochConfig::<Test>::put(SubspaceEpochConfiguration { c: (1, 4) });

        progress_to_block(&keypair, System::block_number() + 4);

        let current_epoch = Subspace::current_epoch();
        assert_eq!(current_epoch.epoch_index, 1);
        assert_eq!(*current_epoch.start_slot, 4);

        let next_epoch = Subspace::next_epoch();
        assert_eq!(next_epoch.epoch_index, 2);
        assert_eq!(*next_epoch.start_slot, 7);

        // the on-chain randomness should always change across epochs
        assert_ne!(current_epoch.randomness, next_epoch.randomness);
    });
}

#[test]
fn tracks_block_numbers_when_current_and_previous_epoch_started() {
    new_test_ext().execute_with(|| {
        let keypair = Keypair::generate();

        // an epoch is 3 slots therefore at block 8 we should be in epoch #3
        // with the previous epochs having the following blocks:
        // epoch 1 - [1, 2, 3]
        // epoch 2 - [4, 5, 6]
        // epoch 3 - [7, 8, 9]
        progress_to_block(&keypair, 8);

        let (last_epoch, current_epoch) = EpochStart::<Test>::get();

        assert_eq!(last_epoch, 4);
        assert_eq!(current_epoch, 7);

        // once we reach block 10 we switch to epoch #4
        progress_to_block(&keypair, 10);

        let (last_epoch, current_epoch) = EpochStart::<Test>::get();

        assert_eq!(last_epoch, 7);
        assert_eq!(current_epoch, 10);
    });
}

#[test]
fn report_equivocation_current_session_works() {
    new_test_ext().execute_with(|| {
        let keypair = Keypair::generate();

        progress_to_block(&keypair, 1);

        let farmer_public_key = FarmerPublicKey::from_slice(&keypair.public.to_bytes());

        // generate an equivocation proof. it creates two headers at the given
        // slot with different block hashes and signed by the given key
        let equivocation_proof = generate_equivocation_proof(&keypair, CurrentSlot::<Test>::get());

        assert_eq!(Subspace::is_in_block_list(&farmer_public_key), false);

        // report the equivocation
        Subspace::report_equivocation(Origin::none(), Box::new(equivocation_proof)).unwrap();

        progress_to_block(&keypair, 2);

        // check that farmer was added to block list
        assert_eq!(Subspace::is_in_block_list(&farmer_public_key), true);
    });
}

#[test]
fn report_equivocation_old_session_works() {
    new_test_ext().execute_with(|| {
        let keypair = Keypair::generate();

        progress_to_block(&keypair, 1);

        let farmer_public_key = FarmerPublicKey::from_slice(&keypair.public.to_bytes());

        // generate an equivocation proof at the current slot
        let equivocation_proof = generate_equivocation_proof(&keypair, CurrentSlot::<Test>::get());

        // create new block and report the equivocation
        // from the previous block
        progress_to_block(&keypair, 2);

        assert_eq!(Subspace::is_in_block_list(&farmer_public_key), false);

        // report the equivocation
        Subspace::report_equivocation(Origin::none(), Box::new(equivocation_proof)).unwrap();

        progress_to_block(&keypair, 3);

        // check that farmer was added to block list
        assert_eq!(Subspace::is_in_block_list(&farmer_public_key), true);
    })
}

#[test]
fn report_equivocation_invalid_equivocation_proof() {
    new_test_ext().execute_with(|| {
        let keypair = Keypair::generate();

        progress_to_block(&keypair, 1);

        let assert_invalid_equivocation = |equivocation_proof| {
            assert_err!(
                Subspace::report_equivocation(Origin::none(), Box::new(equivocation_proof),),
                Error::<Test>::InvalidEquivocationProof,
            )
        };

        // both headers have the same hash, no equivocation.
        let mut equivocation_proof =
            generate_equivocation_proof(&keypair, CurrentSlot::<Test>::get());
        equivocation_proof.second_header = equivocation_proof.first_header.clone();
        assert_invalid_equivocation(equivocation_proof);

        // missing preruntime digest from one header
        let mut equivocation_proof =
            generate_equivocation_proof(&keypair, CurrentSlot::<Test>::get());
        equivocation_proof.first_header.digest_mut().logs.remove(0);
        assert_invalid_equivocation(equivocation_proof);

        // missing seal from one header
        let mut equivocation_proof =
            generate_equivocation_proof(&keypair, CurrentSlot::<Test>::get());
        equivocation_proof.first_header.digest_mut().logs.remove(1);
        assert_invalid_equivocation(equivocation_proof);

        // invalid slot number in proof compared to runtime digest
        let mut equivocation_proof =
            generate_equivocation_proof(&keypair, CurrentSlot::<Test>::get());
        equivocation_proof.slot = Slot::from(0);
        assert_invalid_equivocation(equivocation_proof.clone());

        // different slot numbers in headers
        let h1 = equivocation_proof.first_header;
        let mut equivocation_proof =
            generate_equivocation_proof(&keypair, CurrentSlot::<Test>::get() + 1);

        // use the header from the previous equivocation generated
        // at the previous slot
        equivocation_proof.first_header = h1.clone();

        assert_invalid_equivocation(equivocation_proof.clone());

        // invalid seal signature
        let mut equivocation_proof =
            generate_equivocation_proof(&keypair, CurrentSlot::<Test>::get() + 1);

        // replace the seal digest with the digest from the
        // previous header at the previous slot
        equivocation_proof.first_header.digest_mut().pop();
        equivocation_proof
            .first_header
            .digest_mut()
            .push(h1.digest().logs().last().unwrap().clone());

        assert_invalid_equivocation(equivocation_proof.clone());
    })
}

#[test]
fn report_equivocation_validate_unsigned_prevents_duplicates() {
    new_test_ext().execute_with(|| {
        let keypair = Keypair::generate();

        progress_to_block(&keypair, 1);

        let farmer_public_key = FarmerPublicKey::from_slice(&keypair.public.to_bytes());

        let equivocation_proof = generate_equivocation_proof(&keypair, CurrentSlot::<Test>::get());

        let inner = Call::report_equivocation {
            equivocation_proof: Box::new(equivocation_proof.clone()),
        };

        // Only local/in block reports are allowed
        assert_eq!(
            <Subspace as sp_runtime::traits::ValidateUnsigned>::validate_unsigned(
                TransactionSource::External,
                &inner,
            ),
            InvalidTransaction::Call.into(),
        );

        // The transaction is valid when passed as local
        let tx_tag = (farmer_public_key, CurrentSlot::<Test>::get());
        assert_eq!(
            <Subspace as sp_runtime::traits::ValidateUnsigned>::validate_unsigned(
                TransactionSource::Local,
                &inner,
            ),
            TransactionValidity::Ok(ValidTransaction {
                priority: TransactionPriority::MAX - 1,
                requires: vec![],
                provides: vec![("SubspaceEquivocation", tx_tag).encode()],
                longevity: ReportLongevity::get(),
                propagate: false,
            })
        );

        // The pre dispatch checks should also pass
        assert_ok!(<Subspace as sp_runtime::traits::ValidateUnsigned>::pre_dispatch(&inner));

        // Submit the report
        Subspace::report_equivocation(Origin::none(), Box::new(equivocation_proof)).unwrap();

        // The report should now be considered stale and the transaction is invalid.
        // The check for staleness should be done on both `validate_unsigned` and on `pre_dispatch`
        assert_err!(
            <Subspace as sp_runtime::traits::ValidateUnsigned>::validate_unsigned(
                TransactionSource::Local,
                &inner,
            ),
            InvalidTransaction::Stale,
        );

        assert_err!(
            <Subspace as sp_runtime::traits::ValidateUnsigned>::pre_dispatch(&inner),
            InvalidTransaction::Stale,
        );
    });
}

#[test]
fn report_equivocation_has_valid_weight() {
    // the weight is always the same.
    assert!((1..=1000)
        .map(|_| { <Test as Config>::WeightInfo::report_equivocation() })
        .all(|w| w == 10_000));
}

#[test]
fn valid_equivocation_reports_dont_pay_fees() {
    new_test_ext().execute_with(|| {
        let keypair = Keypair::generate();

        progress_to_block(&keypair, 1);

        // generate an equivocation proof.
        let equivocation_proof = generate_equivocation_proof(&keypair, CurrentSlot::<Test>::get());

        // check the dispatch info for the call.
        let info = Call::<Test>::report_equivocation {
            equivocation_proof: Box::new(equivocation_proof.clone()),
        }
        .get_dispatch_info();

        // it should have non-zero weight and the fee has to be paid.
        assert!(info.weight > 0);
        assert_eq!(info.pays_fee, Pays::Yes);

        // report the equivocation.
        let post_info =
            Subspace::report_equivocation(Origin::none(), Box::new(equivocation_proof.clone()))
                .unwrap();

        // the original weight should be kept, but given that the report
        // is valid the fee is waived.
        assert!(post_info.actual_weight.is_none());
        assert_eq!(post_info.pays_fee, Pays::No);

        // report the equivocation again which is invalid now since it is
        // duplicate.
        let post_info = Subspace::report_equivocation(Origin::none(), Box::new(equivocation_proof))
            .err()
            .unwrap()
            .post_info;

        // the fee is not waived and the original weight is kept.
        assert!(post_info.actual_weight.is_none());
        assert_eq!(post_info.pays_fee, Pays::Yes);
    })
}

#[test]
fn store_root_block_works() {
    new_test_ext().execute_with(|| {
        let keypair = Keypair::generate();

        progress_to_block(&keypair, 1);

        let root_block = create_root_block(0);

        let call = Call::<Test>::store_root_blocks {
            root_blocks: vec![root_block],
        };
        // Root blocks don't require fee
        assert_eq!(call.get_dispatch_info().pays_fee, Pays::No);

        Subspace::store_root_blocks(Origin::none(), vec![root_block]).unwrap();
        assert_eq!(
            System::events(),
            vec![EventRecord {
                phase: Phase::Initialization,
                event: Event::Subspace(crate::Event::RootBlockStored(root_block)),
                topics: vec![],
            }]
        );
    });
}

#[test]
fn store_root_block_validate_unsigned_prevents_duplicates() {
    new_test_ext().execute_with(|| {
        let keypair = Keypair::generate();

        progress_to_block(&keypair, 1);

        let root_block = create_root_block(0);

        let inner = Call::store_root_blocks {
            root_blocks: vec![root_block],
        };

        // Only local/in block reports are allowed
        assert_eq!(
            <Subspace as sp_runtime::traits::ValidateUnsigned>::validate_unsigned(
                TransactionSource::External,
                &inner,
            ),
            InvalidTransaction::Call.into(),
        );

        // The transaction is valid when passed as local
        assert_eq!(
            <Subspace as sp_runtime::traits::ValidateUnsigned>::validate_unsigned(
                TransactionSource::Local,
                &inner,
            ),
            TransactionValidity::Ok(ValidTransaction {
                priority: TransactionPriority::MAX,
                requires: vec![],
                provides: vec![],
                longevity: 0,
                propagate: false,
            })
        );

        // The pre dispatch checks should also pass
        assert_ok!(<Subspace as sp_runtime::traits::ValidateUnsigned>::pre_dispatch(&inner));

        // Submit the report
        Subspace::store_root_blocks(Origin::none(), vec![root_block]).unwrap();

        // The report should now be considered stale and the transaction is invalid.
        // The check for staleness should be done on both `validate_unsigned` and on `pre_dispatch`
        assert_err!(
            <Subspace as sp_runtime::traits::ValidateUnsigned>::validate_unsigned(
                TransactionSource::Local,
                &inner,
            ),
            InvalidTransaction::BadMandatory,
        );
        assert_err!(
            <Subspace as sp_runtime::traits::ValidateUnsigned>::pre_dispatch(&inner),
            InvalidTransaction::BadMandatory,
        );

        let inner2 = Call::store_root_blocks {
            root_blocks: vec![create_root_block(1), create_root_block(1)],
        };

        // Same root block can't be included twice even in the same extrinsic
        assert_err!(
            <Subspace as sp_runtime::traits::ValidateUnsigned>::validate_unsigned(
                TransactionSource::Local,
                &inner2,
            ),
            InvalidTransaction::BadMandatory,
        );
        assert_err!(
            <Subspace as sp_runtime::traits::ValidateUnsigned>::pre_dispatch(&inner2),
            InvalidTransaction::BadMandatory,
        );
    });
}