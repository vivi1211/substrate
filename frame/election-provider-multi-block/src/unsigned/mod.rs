// This file is part of Substrate.

// Copyright (C) 2021 Parity Technologies (UK) Ltd.
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

//! The unsigned phase, and its miner.

/// Exports of this pallet
pub use pallet::*;

#[cfg(feature = "runtime-benchmarks")]
mod benchmarking;

/// The miner.
pub mod miner;

#[frame_support::pallet]
mod pallet {
	use crate::{
		types::*,
		unsigned::miner::{self},
		verifier::Verifier,
	};
	use frame_support::pallet_prelude::*;
	use frame_system::pallet_prelude::*;
	use sp_runtime::traits::SaturatedConversion;

	/// convert a DispatchError to a custom InvalidTransaction with the inner code being the error
	/// number.
	fn dispatch_error_to_invalid(error: sp_runtime::DispatchError) -> InvalidTransaction {
		let error_number = match error {
			DispatchError::Module { error, .. } => error,
			_ => 0,
		};
		InvalidTransaction::Custom(error_number)
	}

	pub trait WeightInfo {
		fn submit_unsigned(v: u32, t: u32, a: u32, d: u32) -> Weight;
	}

	impl WeightInfo for () {
		fn submit_unsigned(_v: u32, _t: u32, _a: u32, _d: u32) -> Weight {
			Default::default()
		}
	}

	#[pallet::config]
	#[pallet::disable_frame_system_supertrait_check]
	pub trait Config:
		crate::Config + frame_system::offchain::SendTransactionTypes<Call<Self>>
	{
		/// The repeat threshold of the offchain worker.
		///
		/// For example, if it is 5, that means that at least 5 blocks will elapse between attempts
		/// to submit the worker's solution.
		type OffchainRepeat: Get<Self::BlockNumber>;

		/// The solver used in hte offchain worker miner
		type OffchainSolver: frame_election_provider_support::NposSolver<
			AccountId = Self::AccountId,
		>;

		/// The priority of the unsigned transaction submitted in the unsigned-phase
		type MinerTxPriority: Get<TransactionPriority>;
		/// Maximum number of iteration of balancing that will be executed in the embedded miner of
		/// the pallet.
		type MinerMaxIterations: Get<u32>;
		/// Maximum weight that the miner should consume.
		///
		/// The miner will ensure that the total weight of the unsigned solution will not exceed
		/// this value, based on [`WeightInfo::submit_unsigned`].
		type MinerMaxWeight: Get<Weight>;
		/// Maximum length (bytes) that the mined solution should consume.
		///
		/// The miner will ensure that the total length of the unsigned solution will not exceed
		/// this value.
		type MinerMaxLength: Get<u32>;

		type WeightInfo: super::WeightInfo;
	}

	#[pallet::pallet]
	#[pallet::generate_storage_info]
	pub struct Pallet<T>(PhantomData<T>);

	#[pallet::call]
	impl<T: Config> Pallet<T> {
		#[pallet::weight((0, DispatchClass::Operational))]
		pub fn submit_unsigned(
			origin: OriginFor<T>,
			paged_solution: Box<PagedRawSolution<T>>,
			_witness: SolutionOrSnapshotSize,
		) -> DispatchResultWithPostInfo {
			ensure_none(origin)?;
			let error_message = "Invalid unsigned submission must produce invalid block and \
				 deprive validator from their authoring reward.";

			// phase, round, claimed score, page-count and hash are checked in pre-dispatch. we
			// don't check them here anymore.
			debug_assert!(Self::validate_unsigned_checks(&paged_solution).is_ok());

			// TODO: ensure correct witness

			let only_page = paged_solution
				.solution_pages
				.into_inner()
				.pop()
				.expect("length of `solution_pages` is always `T::Pages`, `T::Pages` is always greater than 1, can be popped; qed.");
			let supports = <T::Verifier as Verifier>::feasibility_check_page(
				only_page,
				crate::Pallet::<T>::msp(),
			)
			.expect(error_message);

			// we know that the claimed score is better then what we currently have because of the
			// pre-dispatch checks, now we only check if the claimed score was *valid*.

			use sp_npos_elections::EvaluateSupport;
			let valid_score = supports.evaluate();
			assert_eq!(valid_score, paged_solution.score, "{}", error_message);

			log!(info, "queued an unsigned solution with score {:?}", valid_score);

			// all good, now we write this to the verifier directly.
			T::Verifier::force_set_single_page_verified_solution(supports, valid_score);

			Ok(None.into())
		}
	}

	#[pallet::validate_unsigned]
	impl<T: Config> ValidateUnsigned for Pallet<T> {
		type Call = Call<T>;
		fn validate_unsigned(source: TransactionSource, call: &Self::Call) -> TransactionValidity {
			if let Call::submit_unsigned { paged_solution, .. } = call {
				match source {
					TransactionSource::Local | TransactionSource::InBlock => { /* allowed */ },
					_ => return InvalidTransaction::Call.into(),
				}

				let _ = Self::validate_unsigned_checks(paged_solution.as_ref())
					.map_err(|err| {
						log!(debug, "unsigned transaction validation failed due to {:?}", err);
						err
					})
					.map_err(dispatch_error_to_invalid)?;

				ValidTransaction::with_tag_prefix("OffchainElection")
					// The higher the score[0], the better a paged_solution is.
					.priority(
						T::MinerTxPriority::get()
							.saturating_add(paged_solution.score[0].saturated_into()),
					)
					// Used to deduplicate unsigned solutions: each validator should produce one
					// paged_solution per round at most, and solutions are not propagate.
					.and_provides(paged_solution.round)
					// Transaction should stay in the pool for the duration of the unsigned phase.
					.longevity(T::UnsignedPhase::get().saturated_into::<u64>())
					// We don't propagate this. This can never be validated at a remote node.
					.propagate(false)
					.build()
			} else {
				InvalidTransaction::Call.into()
			}
		}

		fn pre_dispatch(call: &Self::Call) -> Result<(), TransactionValidityError> {
			if let Call::submit_unsigned { paged_solution, .. } = call {
				Self::validate_unsigned_checks(paged_solution.as_ref())
					.map_err(dispatch_error_to_invalid)
					.map_err(Into::into)
			} else {
				Err(InvalidTransaction::Call.into())
			}
		}
	}

	#[pallet::hooks]
	impl<T: Config> Hooks<BlockNumberFor<T>> for Pallet<T> {
		fn offchain_worker(now: T::BlockNumber) {
			use sp_runtime::offchain::storage_lock::{BlockAndTime, StorageLock};

			// Create a lock with the maximum deadline of number of blocks in the unsigned phase.
			// This should only come useful in an **abrupt** termination of execution, otherwise the
			// guard will be dropped upon successful execution.
			let mut lock =
				StorageLock::<BlockAndTime<frame_system::Pallet<T>>>::with_block_deadline(
					miner::OffchainWorkerMiner::<T>::OFFCHAIN_LOCK,
					T::UnsignedPhase::get().saturated_into(),
				);

			match lock.try_lock() {
				Ok(_guard) => {
					Self::do_synchronized_offchain_worker(now);
				},
				Err(deadline) => {
					log!(debug, "offchain worker lock not released, deadline is {:?}", deadline);
				},
			};
		}
	}

	impl<T: Config> Pallet<T> {
		/// Internal logic of the offchain worker, to be executed only when the offchain lock is
		/// acquired with success.
		fn do_synchronized_offchain_worker(now: T::BlockNumber) {
			use miner::OffchainWorkerMiner;

			let current_phase = crate::Pallet::<T>::current_phase();
			log!(trace, "lock for offchain worker acquired. Phase = {:?}", current_phase);
			match current_phase {
				Phase::Unsigned((true, opened)) if opened == now => {
					// Mine a new solution, cache it, and attempt to submit it
					let initial_output =
						OffchainWorkerMiner::<T>::ensure_offchain_repeat_frequency(now)
							.and_then(|_| OffchainWorkerMiner::<T>::mine_check_save_submit());
					log!(debug, "initial offchain worker output: {:?}", initial_output);
				},
				Phase::Unsigned((true, opened)) if opened < now => {
					// Try and resubmit the cached solution, and recompute ONLY if it is not
					// feasible.
					let resubmit_output =
						OffchainWorkerMiner::<T>::ensure_offchain_repeat_frequency(now).and_then(
							|_| OffchainWorkerMiner::<T>::restore_or_compute_then_maybe_submit(),
						);
					log!(debug, "resubmit offchain worker output: {:?}", resubmit_output);
				},
				_ => {},
			}

			// TODO: we don't clear the cache here. I don't think it has any implications. It will
			// be overwritten sometime in the future. In either case, once we add the snapshot hash
			// check, an outdated cache is never a problem. backport this as well.
		}

		/// The checks that should happen in the `ValidateUnsigned`'s `pre_dispatch` and
		/// `validate_unsigned` functions.
		///
		/// These check both for snapshot independent checks, and some checks that are specific to
		/// the unsigned phase.
		pub(crate) fn validate_unsigned_checks(
			paged_solution: &PagedRawSolution<T>,
		) -> DispatchResult {
			Self::unsigned_specific_checks(paged_solution)
				.and(crate::Pallet::<T>::snapshot_independent_checks(paged_solution))
				.map_err(Into::into)
		}

		/// The checks that are specific to the (this) unsigned pallet.
		///
		/// ensure solution has the correct phase, and it has only 1 page.
		pub fn unsigned_specific_checks(
			paged_solution: &PagedRawSolution<T>,
		) -> Result<(), crate::Error<T>> {
			ensure!(
				crate::Pallet::<T>::current_phase().is_unsigned_open(),
				crate::Error::<T>::EarlySubmission
			);

			ensure!(paged_solution.solution_pages.len() == 1, crate::Error::<T>::WrongPageCount);

			Ok(())
		}

		#[cfg(test)]
		pub(crate) fn sanity_check() -> Result<(), &'static str> {
			Ok(())
		}
	}
}

#[cfg(test)]
mod validate_unsigned {
	use frame_election_provider_support::Support;
	use frame_support::{
		pallet_prelude::InvalidTransaction,
		unsigned::{TransactionSource, TransactionValidityError, ValidateUnsigned},
	};

	use crate::{mock::*, types::*, verifier::Verifier};

	#[test]
	fn retracts_weak_score_accepts_threshold_better() {
		ExtBuilder::default()
			.solution_improvement_threshold(sp_runtime::Perbill::from_percent(10))
			.build_and_execute(|| {
				roll_to_snapshot_created();

				let solution = mine_full_solution().unwrap();
				load_solution_for_verification(solution.clone());
				roll_to_full_verification();

				// Some good solution is queued now.
				assert_eq!(<VerifierPallet as Verifier>::queued_solution(), Some([55, 130, 8650]));

				roll_to_unsigned_open();

				// this is just worse
				let attempt = fake_unsigned_solution([20, 0, 0]);
				let call = super::Call::submit_unsigned {
					paged_solution: Box::new(attempt),
					witness: witness(),
				};
				assert_eq!(
					UnsignedPallet::validate_unsigned(TransactionSource::Local, &call).unwrap_err(),
					TransactionValidityError::Invalid(InvalidTransaction::Custom(2)),
				);

				// this is better, but not enough better.
				let insufficient_improvement = 55 * 105 / 100;
				let attempt = fake_unsigned_solution([insufficient_improvement, 0, 0]);
				let call = super::Call::submit_unsigned {
					paged_solution: Box::new(attempt),
					witness: witness(),
				};
				assert_eq!(
					UnsignedPallet::validate_unsigned(TransactionSource::Local, &call).unwrap_err(),
					TransactionValidityError::Invalid(InvalidTransaction::Custom(2)),
				);

				// note that we now have to use a solution with 2 winners, just to pass all of the
				// snapshot independent checks.
				let mut paged = raw_paged_from_supports(
					vec![vec![
						(40, Support { total: 10, voters: vec![(3, 5)] }),
						(30, Support { total: 10, voters: vec![(3, 5)] }),
					]],
					0,
				);
				let sufficient_improvement = 55 * 115 / 100;
				paged.score = [sufficient_improvement, 0, 0];
				let call = super::Call::submit_unsigned {
					paged_solution: Box::new(paged),
					witness: witness(),
				};
				assert!(UnsignedPallet::validate_unsigned(TransactionSource::Local, &call).is_ok());
			})
	}

	#[test]
	fn retracts_wrong_round() {
		ExtBuilder::default().build_and_execute(|| {
			roll_to_unsigned_open();

			let mut attempt = fake_unsigned_solution([5, 0, 0]);
			attempt.round += 1;
			let call = super::Call::submit_unsigned {
				paged_solution: Box::new(attempt),
				witness: witness(),
			};

			assert_eq!(
				UnsignedPallet::validate_unsigned(TransactionSource::Local, &call).unwrap_err(),
				// WrongRound is index 1
				TransactionValidityError::Invalid(InvalidTransaction::Custom(1)),
			);
		})
	}

	#[test]
	fn retracts_too_many_pages_unsigned() {
		ExtBuilder::default().pages(3).build_and_execute(|| {
			// NOTE: unsigned solutions should have just 1 page, regardless of the configured
			// page count.
			roll_to_unsigned_open();
			let attempt = mine_full_solution().unwrap();
			let call = super::Call::submit_unsigned {
				paged_solution: Box::new(attempt),
				witness: witness(),
			};

			assert_eq!(
				UnsignedPallet::validate_unsigned(TransactionSource::Local, &call).unwrap_err(),
				// WrongPageCount is index 3
				TransactionValidityError::Invalid(InvalidTransaction::Custom(3)),
			);

			let attempt = mine_solution(2).unwrap();
			let call = super::Call::submit_unsigned {
				paged_solution: Box::new(attempt),
				witness: witness(),
			};

			assert_eq!(
				UnsignedPallet::validate_unsigned(TransactionSource::Local, &call).unwrap_err(),
				TransactionValidityError::Invalid(InvalidTransaction::Custom(3)),
			);

			let attempt = mine_solution(1).unwrap();
			let call = super::Call::submit_unsigned {
				paged_solution: Box::new(attempt),
				witness: witness(),
			};

			assert!(UnsignedPallet::validate_unsigned(TransactionSource::Local, &call).is_ok(),);
		})
	}

	#[test]
	fn retracts_wrong_winner_count() {
		ExtBuilder::default().desired_targets(2).build_and_execute(|| {
			roll_to_unsigned_open();

			let paged = raw_paged_from_supports(
				vec![vec![(40, Support { total: 10, voters: vec![(3, 10)] })]],
				0,
			);

			let call = super::Call::submit_unsigned {
				paged_solution: Box::new(paged),
				witness: witness(),
			};

			assert_eq!(
				UnsignedPallet::validate_unsigned(TransactionSource::Local, &call).unwrap_err(),
				// WrongWinnerCount is index 4
				TransactionValidityError::Invalid(InvalidTransaction::Custom(4)),
			);
		});
	}

	#[test]
	fn retracts_wrong_phase() {
		ExtBuilder::default().build_and_execute(|| {
			let solution = raw_paged_solution_low_score();

			let call = super::Call::submit_unsigned {
				paged_solution: Box::new(solution.clone()),
				witness: witness(),
			};

			// initial
			assert_eq!(MultiBlock::current_phase(), Phase::Off);
			assert!(matches!(
				<UnsignedPallet as ValidateUnsigned>::validate_unsigned(
					TransactionSource::Local,
					&call
				)
				.unwrap_err(),
				// because EarlySubmission is index 0.
				TransactionValidityError::Invalid(InvalidTransaction::Custom(0))
			));
			assert!(matches!(
				<UnsignedPallet as ValidateUnsigned>::pre_dispatch(&call).unwrap_err(),
				TransactionValidityError::Invalid(InvalidTransaction::Custom(0))
			));

			// signed
			roll_to(15);
			assert_eq!(MultiBlock::current_phase(), Phase::Signed);
			assert!(matches!(
				<UnsignedPallet as ValidateUnsigned>::validate_unsigned(
					TransactionSource::Local,
					&call
				)
				.unwrap_err(),
				TransactionValidityError::Invalid(InvalidTransaction::Custom(0))
			));
			assert!(matches!(
				<UnsignedPallet as ValidateUnsigned>::pre_dispatch(&call).unwrap_err(),
				TransactionValidityError::Invalid(InvalidTransaction::Custom(0))
			));

			// unsigned
			roll_to(25);
			assert!(MultiBlock::current_phase().is_unsigned());

			assert_ok!(<UnsignedPallet as ValidateUnsigned>::validate_unsigned(
				TransactionSource::Local,
				&call
			));
			assert_ok!(<UnsignedPallet as ValidateUnsigned>::pre_dispatch(&call));

			// unsigned -- but not enabled.
			<crate::CurrentPhase<Runtime>>::put(Phase::Unsigned((false, 25)));
			assert!(MultiBlock::current_phase().is_unsigned());
			assert!(matches!(
				<UnsignedPallet as ValidateUnsigned>::validate_unsigned(
					TransactionSource::Local,
					&call
				)
				.unwrap_err(),
				TransactionValidityError::Invalid(InvalidTransaction::Custom(0))
			));
			assert!(matches!(
				<UnsignedPallet as ValidateUnsigned>::pre_dispatch(&call).unwrap_err(),
				TransactionValidityError::Invalid(InvalidTransaction::Custom(0))
			));
		})
	}

	#[test]
	fn priority_is_set() {
		ExtBuilder::default()
			.miner_tx_priority(20)
			.desired_targets(0)
			.build_and_execute(|| {
				roll_to(25);
				assert!(MultiBlock::current_phase().is_unsigned());

				let solution = fake_unsigned_solution([5, 0, 0]);
				let call = super::Call::submit_unsigned {
					paged_solution: Box::new(solution.clone()),
					witness: witness(),
				};

				assert_eq!(
					<UnsignedPallet as ValidateUnsigned>::validate_unsigned(
						TransactionSource::Local,
						&call
					)
					.unwrap()
					.priority,
					25
				);
			})
	}
}

#[cfg(test)]
mod call {
	use crate::{mock::*, AssignmentOf};

	type Assignment = AssignmentOf<Runtime>;

	#[test]
	fn unfeasible_solution_panics() {
		todo!("basic test to show that the unsigned call panics.");
	}

	#[test]
	fn wrong_witness_panics() {
		todo!("similarly, passing in bad witness must also be checked.")
	}
}