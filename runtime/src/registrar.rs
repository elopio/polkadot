// Copyright 2017 Parity Technologies (UK) Ltd.
// This file is part of Polkadot.

// Polkadot is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Polkadot is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Polkadot.  If not, see <http://www.gnu.org/licenses/>.

//! Module to handle which parachains/parathreads (collectively referred to as "paras") are
//! registered and which are scheduled. Doesn't manage any of the actual execution/validation logic
//! which is left to `parachains.rs`.

use rstd::{prelude::*, result};
#[cfg(any(feature = "std", test))]
use rstd::marker::PhantomData;
use codec::{Encode, Decode};

use sr_primitives::{
	weights::{SimpleDispatchInfo, DispatchInfo},
	transaction_validity::{TransactionValidityError, ValidTransaction, TransactionValidity},
	traits::{Hash as HashT, SignedExtension}
};

use srml_support::{
	decl_storage, decl_module, decl_event, ensure,
	dispatch::{Result, IsSubType}, traits::{Get, Currency, ReservableCurrency}
};
use system::{self, ensure_root, ensure_signed};
use primitives::parachain::{
	Id as ParaId, CollatorId, Scheduling, LOWEST_USER_ID, SwapAux, Info as ParaInfo, ActiveParas,
	Retriable
};
use crate::parachains;
use sr_primitives::transaction_validity::InvalidTransaction;

/// Parachain registration API.
pub trait Registrar<AccountId> {
	/// Create a new unique parachain identity for later registration.
	fn new_id() -> ParaId;

	/// Register a parachain with given `code` and `initial_head_data`. `id` must not yet be registered or it will
	/// result in a error.
	fn register_para(
		id: ParaId,
		info: ParaInfo,
		code: Vec<u8>,
		initial_head_data: Vec<u8>,
	) -> Result;

	/// Deregister a parachain with given `id`. If `id` is not currently registered, an error is returned.
	fn deregister_para(id: ParaId) -> Result;
}

impl<T: Trait> Registrar<T::AccountId> for Module<T> {
	fn new_id() -> ParaId {
		<NextFreeId>::mutate(|n| { let r = *n; *n = ParaId::from(u32::from(*n) + 1); r })
	}

	fn register_para(
		id: ParaId,
		info: ParaInfo,
		code: Vec<u8>,
		initial_head_data: Vec<u8>,
	) -> Result {
		ensure!(!Paras::exists(id), "Parachain already exists");
		if let Scheduling::Always = info.scheduling {
			Parachains::mutate(|parachains|
				match parachains.binary_search(&id) {
					Ok(_) => Err("Parachain already exists"),
					Err(idx) => {
						parachains.insert(idx, id);
						Ok(())
					}
				}
			)?;
		}
		<parachains::Module<T>>::initialize_para(id, code, initial_head_data);
		Paras::insert(id, info);
		Ok(())
	}

	fn deregister_para(id: ParaId) -> Result {
		let info = Paras::take(id).ok_or("Invalid id")?;
		if let Scheduling::Always = info.scheduling {
			Parachains::mutate(|parachains|
				parachains.binary_search(&id)
					.map(|index| parachains.remove(index))
					.map_err(|_| "Invalid id")
			)?;
		}
		<parachains::Module<T>>::cleanup_para(id);
		Paras::remove(id);
		Ok(())
	}
}

type BalanceOf<T> =
	<<T as Trait>::Currency as Currency<<T as system::Trait>::AccountId>>::Balance;

pub trait Trait: parachains::Trait {
	/// The overarching event type.
	type Event: From<Event> + Into<<Self as system::Trait>::Event>;

	/// The aggregated origin type must support the parachains origin. We require that we can
	/// infallibly convert between this origin and the system origin, but in reality, they're the
	/// same type, we just can't express that to the Rust type system without writing a `where`
	/// clause everywhere.
	type Origin: From<<Self as system::Trait>::Origin>
		+ Into<result::Result<parachains::Origin, <Self as Trait>::Origin>>;

	/// The system's currency for parathread payment.
	type Currency: ReservableCurrency<Self::AccountId>;

	/// The deposit to be paid to run a parathread.
	type ParathreadDeposit: Get<BalanceOf<Self>>;

	/// Handler for when two ParaIds are swapped.
	type SwapAux: SwapAux;

	/// The number of items in the parathread queue, aka the number of blocks in advance to schedule
	/// parachain execution.
	type QueueSize: Get<usize>;

	/// The number of rotations that you will have as grace if you miss a block.
	type MaxRetries: Get<u32>;
}

decl_storage! {
	trait Store for Module<T: Trait> as Registrar {
		// Vector of all parachain IDs, in ascending order.
		Parachains: Vec<ParaId>;

		/// The number of threads to schedule per block.
		ThreadCount: u32;

		/// An array of the queue of set of threads scheduled for the coming blocks; ordered by
		/// ascending para ID. There can be no duplicates of para ID in each list item.
		SelectedThreads: Vec<Vec<(ParaId, CollatorId)>>;

		/// Parathreads/chains scheduled for execution this block. If the collator ID is set, then
		/// a particular collator has already been chosen for the next block, and no other collator
		/// may provide the block. In this case we allow the possibility of the combination being
		/// retried in a later block, expressed by `Retriable`.
		///
		/// Ordered by ParaId.
		Active: Vec<(ParaId, Option<(CollatorId, Retriable)>)>;

		/// The next unused ParaId value. Start this high in order to keep low numbers for
		/// system-level chains.
		NextFreeId: ParaId = LOWEST_USER_ID;

		/// Pending swap operations.
		PendingSwap: map ParaId => Option<ParaId>;

		/// Map of all registered parathreads/chains.
		Paras get(paras): map ParaId => Option<ParaInfo>;

		/// The current queue for parathreads that should be retried.
		RetryQueue get(retry_queue): Vec<Vec<(ParaId, CollatorId)>>;

		/// Users who have paid a parathread's deposit
		Debtors: map ParaId => T::AccountId;
	}
	add_extra_genesis {
		config(parachains): Vec<(ParaId, Vec<u8>, Vec<u8>)>;
		config(_phdata): PhantomData<T>;
		build(build::<T>);
	}
}

#[cfg(feature = "std")]
fn build<T: Trait>(config: &GenesisConfig<T>) {
	use sr_primitives::traits::Zero;

	let mut p = config.parachains.clone();
	p.sort_unstable_by_key(|&(ref id, _, _)| *id);
	p.dedup_by_key(|&mut (ref id, _, _)| *id);

	let only_ids: Vec<ParaId> = p.iter().map(|&(ref id, _, _)| id).cloned().collect();

	Parachains::put(&only_ids);

	for (id, code, genesis) in p {
		Paras::insert(id, &primitives::parachain::PARACHAIN_INFO);
		// no ingress -- a chain cannot be routed to until it is live.
		<parachains::Code>::insert(&id, &code);
		<parachains::Heads>::insert(&id, &genesis);
		<parachains::Watermarks<T>>::insert(&id, T::BlockNumber::zero());
		// Save initial parachains in registrar
		Paras::insert(id, ParaInfo { scheduling: Scheduling::Always })
	}
}

/// Swap the existence of two items, provided by value, within an ordered list.
///
/// If neither item exists, or if both items exist this will do nothing. If exactly one of the
/// items exists, then it will be removed and the other inserted.
pub fn swap_ordered_existence<T: PartialOrd + Ord + Copy>(ids: &mut [T], one: T, other: T) {
	let maybe_one_pos = ids.binary_search(&one);
	let maybe_other_pos = ids.binary_search(&other);
	match (maybe_one_pos, maybe_other_pos) {
		(Ok(one_pos), Err(_)) => ids[one_pos] = other,
		(Err(_), Ok(other_pos)) => ids[other_pos] = one,
		_ => return,
	};
	ids.sort();
}

decl_module! {
	/// Parachains module.
	pub struct Module<T: Trait> for enum Call where origin: <T as system::Trait>::Origin {
		fn deposit_event() = default;

		/// Register a parachain with given code.
		/// Fails if given ID is already used.
		#[weight = SimpleDispatchInfo::FixedOperational(5_000_000)]
		pub fn register_para(origin,
			#[compact] id: ParaId,
			info: ParaInfo,
			code: Vec<u8>,
			initial_head_data: Vec<u8>,
		) -> Result {
			ensure_root(origin)?;
			<Self as Registrar<T::AccountId>>::
				register_para(id, info, code, initial_head_data)
		}

		/// Deregister a parachain with given id
		#[weight = SimpleDispatchInfo::FixedOperational(10_000)]
		pub fn deregister_para(origin, #[compact] id: ParaId) -> Result {
			ensure_root(origin)?;
			<Self as Registrar<T::AccountId>>::deregister_para(id)
		}

		/// Reset the number of parathreads that can pay to be scheduled in a single block.
		///
		/// - `count`: The number of parathreads.
		///
		/// Must be called from Root origin.
		fn set_thread_count(origin, count: u32) {
			ensure_root(origin)?;
			ThreadCount::put(count);
		}

		/// Register a parathread for immediate use.
		///
		/// Must be sent from a Signed origin that is able to have ParathreadDeposit reserved.
		/// `code` and `initial_head_data` are used to initialize the parathread's state.
		fn register_parathread(origin,
			code: Vec<u8>,
			initial_head_data: Vec<u8>,
		) {
			let who = ensure_signed(origin)?;

			T::Currency::reserve(&who, T::ParathreadDeposit::get())?;

			let info = ParaInfo {
				scheduling: Scheduling::Dynamic,
			};
			let id = <Self as Registrar<T::AccountId>>::new_id();

			let _ = <Self as Registrar<T::AccountId>>::
				register_para(id, info, code, initial_head_data);

			<Debtors<T>>::insert(id, who);

			Self::deposit_event(Event::ParathreadRegistered(id));
		}

		/// Place a bid for a parathread to be progressed in the next block.
		///
		/// This is a kind of special transaction that should by heavily prioritized in the
		/// transaction pool according to the `value`; only `ThreadCount` of them may be presented
		/// in any single block.
		fn select_parathread(origin,
			#[compact] _id: ParaId,
			_collator: CollatorId,
			_head_hash: T::Hash,
		) {
			ensure_signed(origin)?;
			// Everything else is checked for in the transaction `SignedExtension`.
		}

		/// Deregister a parathread and retrieve the deposit.
		///
		/// Must be sent from a `Parachain` origin which is currently a parathread.
		///
		/// Ensure that before calling this that any funds you want emptied from the parathread's
		/// account is moved out; after this it will be impossible to retrieve them (without
		/// governance intervention).
		fn deregister_parathread(origin) {
			let id = parachains::ensure_parachain(<T as Trait>::Origin::from(origin))?;

			let info = Paras::get(id).ok_or("invalid id")?;
			if let Scheduling::Dynamic = info.scheduling {} else { Err("invalid parathread id")? }

			<Self as Registrar<T::AccountId>>::deregister_para(id)?;
			Self::force_unschedule(|i| i == id);

			let debtor = <Debtors<T>>::take(id);
			let _ = T::Currency::unreserve(&debtor, T::ParathreadDeposit::get());

			Self::deposit_event(Event::ParathreadRegistered(id));
		}

		/// Swap a parachain with another parachain or parathread. The origin must be a `Parachain`.
		/// The swap will happen only if there is already an opposite swap pending. If there is not,
		/// the swap will be stored in the pending swaps map, ready for a later confirmatory swap.
		///
		/// The `ParaId`s remain mapped to the same head data and code so external code can rely on
		/// `ParaId` to be a long-term identifier of a notional "parachain". However, their
		/// scheduling info (i.e. whether they're a parathread or parachain), auction information
		/// and the auction deposit are switched.
		fn swap(origin, #[compact] other: ParaId) {
			let id = parachains::ensure_parachain(<T as Trait>::Origin::from(origin))?;

			if PendingSwap::get(other) == Some(id) {
				// actually do the swap.
				T::SwapAux::ensure_can_swap(id, other)?;

				// Remove intention to swap.
				PendingSwap::remove(other);
				Self::force_unschedule(|i| i == id || i == other);
				Parachains::mutate(|ids| swap_ordered_existence(ids, id, other));
				Paras::mutate(id, |i|
					Paras::mutate(other, |j|
						rstd::mem::swap(i, j)
					)
				);

				<Debtors<T>>::mutate(id, |i|
					<Debtors<T>>::mutate(other, |j|
						rstd::mem::swap(i, j)
					)
				);
				let _ = T::SwapAux::on_swap(id, other);
			} else {
				PendingSwap::insert(id, other);
			}
		}

		/// Block initializer. Clears SelectedThreads and constructs/replaces Active.
		fn on_initialize() {
			let next_up = SelectedThreads::mutate(|t| {
				let r = if t.len() >= T::QueueSize::get() {
					// Take the first set of parathreads in queue
					t.remove(0)
				} else {
					vec![]
				};
				while t.len() < T::QueueSize::get() {
					t.push(vec![]);
				}
				r
			});
			// mutable so that we can replace with `None` if parathread appears in new schedule.
			let mut retrying = Self::take_next_retry();
			if let Some(((para, _), _)) = retrying {
				// this isn't really ideal: better would be if there were an earlier pass that set
				// retrying to the first item in the Missed queue that isn't already scheduled, but
				// this is potentially O(m*n) in terms of missed queue size and parathread pool size.
				if next_up.iter().any(|x| x.0 == para) {
					retrying = None
				}
			}

			let mut paras = Parachains::get().into_iter()
				.map(|id| (id, None))
				.chain(next_up.into_iter()
					.map(|(para, collator)|
						(para, Some((collator, Retriable::WithRetries(0))))
					)
				).chain(retrying.into_iter()
					.map(|((para, collator), retries)|
						(para, Some((collator, Retriable::WithRetries(retries + 1))))
					)
				).collect::<Vec<_>>();
			// for Rust's timsort algorithm, sorting a concatenation of two sorted ranges is near
			// O(N).
			paras.sort_by_key(|&(ref id, _)| *id);

			Active::put(paras);
		}

		fn on_finalize() {
			// a block without this will panic, but let's not panic here.
			if let Some(proceeded_vec) = parachains::DidUpdate::get() {
				// Active is sorted and DidUpdate is a sorted subset of its elements.
				//
				// We just go through the contents of active and find any items that don't appear in
				// DidUpdate *and* which are enabled for retry.
				let mut proceeded = proceeded_vec.into_iter();
				let mut i = proceeded.next();
				for sched in Active::get().into_iter() {
					match i {
						// Scheduled parachain proceeded properly. Move onto next item.
						Some(para) if para == sched.0 => i = proceeded.next(),
						// Scheduled `sched` missed their block.
						// Queue for retry if it's allowed.
						_ => if let (i, Some((c, Retriable::WithRetries(n)))) = sched {
							Self::retry_later((i, c), n)
						},
					}
				}
			}
		}
	}
}

decl_event!{
	pub enum Event {
		/// A parathread was registered; its new ID is supplied.
		ParathreadRegistered(ParaId),

		/// The parathread of the supplied ID was de-registered.
		ParathreadDeregistered(ParaId),
	}
}

impl<T: Trait> Module<T> {
        /// Ensures that the given `ParaId` corresponds to a registered parathread, and returns a descriptor if so.
	pub fn ensure_thread_id(id: ParaId) -> Option<ParaInfo> {
		Paras::get(id).and_then(|info| if let Scheduling::Dynamic = info.scheduling {
			Some(info)
		} else {
			None
		})
	}

	fn retry_later(sched: (ParaId, CollatorId), retries: u32) {
		if retries < T::MaxRetries::get() {
			RetryQueue::mutate(|q| {
				q.resize(T::MaxRetries::get() as usize, vec![]);
				q[retries as usize].push(sched);
			});
		}
	}

	fn take_next_retry() -> Option<((ParaId, CollatorId), u32)> {
		RetryQueue::mutate(|q| {
			for (i, q) in q.iter_mut().enumerate() {
				if !q.is_empty() {
					return Some((q.remove(0), i as u32));
				}
			}
			None
		})
	}

	/// Forcibly remove the threads matching `m` from all current and future scheduling.
	fn force_unschedule(m: impl Fn(ParaId) -> bool) {
		RetryQueue::mutate(|qs| for q in qs.iter_mut() {
			q.retain(|i| !m(i.0))
		});
		SelectedThreads::mutate(|qs| for q in qs.iter_mut() {
			q.retain(|i| !m(i.0))
		});
		Active::mutate(|a| for i in a.iter_mut() {
			if m(i.0) {
				if let Some((_, ref mut r)) = i.1 {
					*r = Retriable::Never;
				}
			}
		});
	}
}

impl<T: Trait> ActiveParas for Module<T> {
	fn active_paras() -> Vec<(ParaId, Option<(CollatorId, Retriable)>)> {
		Active::get()
	}
}

/// Ensure that parathread selections happen prioritized by fees.
#[derive(Encode, Decode, Clone, Eq, PartialEq)]
pub struct LimitParathreadCommits<T: Trait + Send + Sync>(rstd::marker::PhantomData<T>) where
	<T as system::Trait>::Call: IsSubType<Module<T>, T>;

impl<T: Trait + Send + Sync> rstd::fmt::Debug for LimitParathreadCommits<T> where
	<T as system::Trait>::Call: IsSubType<Module<T>, T>
{
	fn fmt(&self, f: &mut rstd::fmt::Formatter) -> rstd::fmt::Result {
		write!(f, "LimitParathreadCommits<T>")
	}
}

/// Custom validity errors used in Polkadot while validating transactions.
#[repr(u8)]
pub enum Error {
	/// Parathread ID has already been submitted for this block.
	Duplicate = 0,
	/// Parathread ID does not identify a parathread.
	InvalidId = 1,
}

impl<T: Trait + Send + Sync> SignedExtension for LimitParathreadCommits<T> where
	<T as system::Trait>::Call: IsSubType<Module<T>, T>
{
	type AccountId = T::AccountId;
	type Call = <T as system::Trait>::Call;
	type AdditionalSigned = ();
	type Pre = ();

	fn additional_signed(&self)
		-> rstd::result::Result<Self::AdditionalSigned, TransactionValidityError>
	{
		Ok(())
	}

	fn validate(
		&self,
		_who: &Self::AccountId,
		call: &Self::Call,
		_info: DispatchInfo,
		_len: usize,
	) -> TransactionValidity {
		let mut r = ValidTransaction::default();
		if let Some(local_call) = call.is_sub_type() {
			if let Call::select_parathread(id, collator, hash) = local_call {
				// ensure that the para ID is actually a parathread.
				let e = TransactionValidityError::from(InvalidTransaction::Custom(Error::InvalidId as u8));
				<Module<T>>::ensure_thread_id(*id).ok_or(e)?;

				// ensure that we haven't already had a full complement of selected parathreads.
				let mut upcoming_selected_threads = SelectedThreads::get();
				if upcoming_selected_threads.is_empty() {
					upcoming_selected_threads.push(vec![]);
				}
				let i = upcoming_selected_threads.len() - 1;
				let selected_threads = &mut upcoming_selected_threads[i];
				let thread_count = ThreadCount::get() as usize;
				ensure!(
					selected_threads.len() < thread_count,
					InvalidTransaction::ExhaustsResources.into()
				);

				// ensure that this is not selecting a duplicate parathread ID
				let e = TransactionValidityError::from(InvalidTransaction::Custom(Error::Duplicate as u8));
				let pos = selected_threads
					.binary_search_by(|&(ref other_id, _)| other_id.cmp(id))
					.err()
					.ok_or(e)?;

				// ensure that this is a live bid (i.e. that the thread's chain head matches)
				let e = TransactionValidityError::from(InvalidTransaction::Custom(Error::InvalidId as u8));
				let head = <parachains::Module<T>>::parachain_head(id).ok_or(e)?;
				let actual = T::Hashing::hash(&head);
				ensure!(&actual == hash, InvalidTransaction::Stale.into());

				// updated the selected threads.
				selected_threads.insert(pos, (*id, collator.clone()));
				rstd::mem::drop(selected_threads);
				SelectedThreads::put(upcoming_selected_threads);

				// provides the state-transition for this head-data-hash; this should cue the pool
				// to throw out competing transactions with lesser fees.
				r.provides = vec![hash.encode()];
			}
		}
		Ok(r)
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use bitvec::vec::BitVec;
	use sr_io::TestExternalities;
	use substrate_primitives::{H256, Pair};
	use sr_primitives::{
		traits::{
			BlakeTwo256, IdentityLookup, OnInitialize, OnFinalize, Dispatchable,
			AccountIdConversion,
		}, testing::{UintAuthorityId, Header}, Perbill
	};
	use primitives::{
		parachain::{
			ValidatorId, Info as ParaInfo, Scheduling, LOWEST_USER_ID, AttestedCandidate,
			CandidateReceipt, HeadData, ValidityAttestation, Statement, Chain, CollatorPair,
		},
		Balance, BlockNumber,
	};
	use srml_support::{
		impl_outer_origin, impl_outer_dispatch, assert_ok, parameter_types, assert_noop,
	};
	use keyring::Sr25519Keyring;

	use crate::parachains;
	use crate::slots;
	use crate::attestations;

	impl_outer_origin! {
		pub enum Origin for Test {
			parachains,
		}
	}

	impl_outer_dispatch! {
		pub enum Call for Test where origin: Origin {
			parachains::Parachains,
			registrar::Registrar,
		}
	}

	#[derive(Clone, Eq, PartialEq)]
	pub struct Test;
	parameter_types! {
		pub const BlockHashCount: u32 = 250;
		pub const MaximumBlockWeight: u32 = 4 * 1024 * 1024;
		pub const MaximumBlockLength: u32 = 4 * 1024 * 1024;
		pub const AvailableBlockRatio: Perbill = Perbill::from_percent(75);
	}
	impl system::Trait for Test {
		type Origin = Origin;
		type Call = Call;
		type Index = u64;
		type BlockNumber = u64;
		type Hash = H256;
		type Hashing = BlakeTwo256;
		type AccountId = u64;
		type Lookup = IdentityLookup<u64>;
		type Header = Header;
		type Event = ();
		type BlockHashCount = BlockHashCount;
		type MaximumBlockWeight = MaximumBlockWeight;
		type MaximumBlockLength = MaximumBlockLength;
		type AvailableBlockRatio = AvailableBlockRatio;
		type Version = ();
	}

	parameter_types! {
		pub const ExistentialDeposit: Balance = 0;
		pub const TransferFee: Balance = 0;
		pub const CreationFee: Balance = 0;
	}

	impl balances::Trait for Test {
		type Balance = Balance;
		type OnFreeBalanceZero = ();
		type OnNewAccount = ();
		type Event = ();
		type DustRemoval = ();
		type TransferPayment = ();
		type ExistentialDeposit = ExistentialDeposit;
		type TransferFee = TransferFee;
		type CreationFee = CreationFee;
	}

	parameter_types!{
		pub const LeasePeriod: u64 = 10;
		pub const EndingPeriod: u64 = 3;
	}

	impl slots::Trait for Test {
		type Event = ();
		type Currency = balances::Module<Test>;
		type Parachains = Registrar;
		type EndingPeriod = EndingPeriod;
		type LeasePeriod = LeasePeriod;
		type Randomness = RandomnessCollectiveFlip;
	}

	parameter_types!{
		pub const AttestationPeriod: BlockNumber = 100;
	}

	impl attestations::Trait for Test {
		type AttestationPeriod = AttestationPeriod;
		type ValidatorIdentities = parachains::ValidatorIdentities<Test>;
		type RewardAttestation = ();
	}

	parameter_types! {
		pub const Period: BlockNumber = 1;
		pub const Offset: BlockNumber = 0;
		pub const DisabledValidatorsThreshold: Perbill = Perbill::from_percent(17);
	}

	impl session::Trait for Test {
		type OnSessionEnding = ();
		type Keys = UintAuthorityId;
		type ShouldEndSession = session::PeriodicSessions<Period, Offset>;
		type SessionHandler = session::TestSessionHandler;
		type Event = ();
		type SelectInitialValidators = ();
		type ValidatorId = u64;
		type ValidatorIdOf = ();
		type DisabledValidatorsThreshold = DisabledValidatorsThreshold;
	}

	impl parachains::Trait for Test {
		type Origin = Origin;
		type Call = Call;
		type ParachainCurrency = balances::Module<Test>;
		type ActiveParachains = Registrar;
		type Registrar = Registrar;
		type Randomness = RandomnessCollectiveFlip;
	}

	parameter_types! {
		pub const ParathreadDeposit: Balance = 10;
		pub const QueueSize: usize = 2;
		pub const MaxRetries: u32 = 3;
	}

	impl Trait for Test {
		type Event = ();
		type Origin = Origin;
		type Currency = balances::Module<Test>;
		type ParathreadDeposit = ParathreadDeposit;
		type SwapAux = slots::Module<Test>;
		type QueueSize = QueueSize;
		type MaxRetries = MaxRetries;
	}

	type Balances = balances::Module<Test>;
	type Parachains = parachains::Module<Test>;
	type System = system::Module<Test>;
	type Slots = slots::Module<Test>;
	type Registrar = Module<Test>;
	type RandomnessCollectiveFlip = randomness_collective_flip::Module<Test>;

	const AUTHORITY_KEYS: [Sr25519Keyring; 8] = [
		Sr25519Keyring::Alice,
		Sr25519Keyring::Bob,
		Sr25519Keyring::Charlie,
		Sr25519Keyring::Dave,
		Sr25519Keyring::Eve,
		Sr25519Keyring::Ferdie,
		Sr25519Keyring::One,
		Sr25519Keyring::Two,
	];

	fn new_test_ext(parachains: Vec<(ParaId, Vec<u8>, Vec<u8>)>) -> TestExternalities {
		let mut t = system::GenesisConfig::default().build_storage::<Test>().unwrap();

		let authority_keys = [
			Sr25519Keyring::Alice,
			Sr25519Keyring::Bob,
			Sr25519Keyring::Charlie,
			Sr25519Keyring::Dave,
			Sr25519Keyring::Eve,
			Sr25519Keyring::Ferdie,
			Sr25519Keyring::One,
			Sr25519Keyring::Two,
		];

		// stashes are the index.
		let session_keys: Vec<_> = authority_keys.iter().enumerate()
			.map(|(i, _k)| (i as u64, UintAuthorityId(i as u64)))
			.collect();

		let authorities: Vec<_> = authority_keys.iter().map(|k| ValidatorId::from(k.public())).collect();

		let balances: Vec<_> = (0..authority_keys.len()).map(|i| (i as u64, 10_000_000)).collect();

		parachains::GenesisConfig {
			authorities: authorities.clone(),
		}.assimilate_storage::<Test>(&mut t).unwrap();

		GenesisConfig::<Test> {
			parachains,
			_phdata: Default::default(),
		}.assimilate_storage(&mut t).unwrap();

		session::GenesisConfig::<Test> {
			keys: session_keys,
		}.assimilate_storage(&mut t).unwrap();

		balances::GenesisConfig::<Test> {
			balances,
			vesting: vec![],
		}.assimilate_storage(&mut t).unwrap();

		t.into()
	}

	fn init_block() {
		println!("Initializing {}", System::block_number());
		System::on_initialize(System::block_number());
		Registrar::on_initialize(System::block_number());
		Parachains::on_initialize(System::block_number());
		Slots::on_initialize(System::block_number());
	}

	fn run_to_block(n: u64) {
		println!("Running until block {}", n);
		while System::block_number() < n {
			if System::block_number() > 1 {
				println!("Finalizing {}", System::block_number());
				if !parachains::DidUpdate::exists() {
					println!("Null heads update");
					assert_ok!(Parachains::set_heads(system::RawOrigin::None.into(), vec![]));
				}
				Slots::on_finalize(System::block_number());
				Parachains::on_finalize(System::block_number());
				Registrar::on_finalize(System::block_number());
				System::on_finalize(System::block_number());
			}
			System::set_block_number(System::block_number() + 1);
			init_block();
		}
	}

	fn schedule_thread(id: ParaId, head_data: &[u8], col: &CollatorId) {
		let tx: LimitParathreadCommits<Test> = LimitParathreadCommits(Default::default());
		let hdh = BlakeTwo256::hash(head_data);
		let inner_call = super::Call::select_parathread(id, col.clone(), hdh);
		let call = Call::Registrar(inner_call);
		let origin = 4u64;
		assert!(tx.validate(&origin, &call, Default::default(), 0).is_ok());
		assert_ok!(call.dispatch(Origin::signed(origin)));
	}

	fn user_id(i: u32) -> ParaId {
		LOWEST_USER_ID + i
	}

	fn attest(id: ParaId, collator: &CollatorPair, head_data: &[u8], block_data: &[u8]) -> AttestedCandidate {
		let block_data_hash = BlakeTwo256::hash(block_data);
		let candidate = CandidateReceipt {
			parachain_index: id,
			collator: collator.public(),
			signature: block_data_hash.using_encoded(|d| collator.sign(d)),
			head_data: HeadData(head_data.to_vec()),
			egress_queue_roots: vec![],
			fees: 0,
			block_data_hash,
			upward_messages: vec![],
		};
		let payload = (Statement::Valid(candidate.hash()), System::parent_hash()).encode();
		let roster = Parachains::calculate_duty_roster().0.validator_duty;
		AttestedCandidate {
			candidate,
			validity_votes: AUTHORITY_KEYS.iter()
				.enumerate()
				.filter(|(i, _)| roster[*i] == Chain::Parachain(id))
				.map(|(_, k)| k.sign(&payload).into())
				.map(ValidityAttestation::Explicit)
				.collect(),
			validator_indices: roster.iter()
				.map(|i| i == &Chain::Parachain(id))
				.collect::<BitVec>(),
		}
	}

	#[test]
	fn basic_setup_works() {
		new_test_ext(vec![]).execute_with(|| {
			assert_eq!(super::Parachains::get(), vec![]);
			assert_eq!(ThreadCount::get(), 0);
			assert_eq!(Active::get(), vec![]);
			assert_eq!(NextFreeId::get(), LOWEST_USER_ID);
			assert_eq!(PendingSwap::get(&ParaId::from(0u32)), None);
			assert_eq!(Paras::get(&ParaId::from(0u32)), None);
		});
	}

	#[test]
	fn genesis_registration_works() {
		let parachains = vec![
			(5u32.into(), vec![1,2,3], vec![1]),
			(100u32.into(), vec![4,5,6], vec![2,]),
		];

		new_test_ext(parachains).execute_with(|| {
			// Need to trigger on_initialize
			run_to_block(2);
			// Genesis registration works
			assert_eq!(Registrar::active_paras(), vec![(5u32.into(), None), (100u32.into(), None)]);
			assert_eq!(
				Registrar::paras(&ParaId::from(5u32)),
				Some(ParaInfo { scheduling: Scheduling::Always }),
			);
			assert_eq!(
				Registrar::paras(&ParaId::from(100u32)),
				Some(ParaInfo { scheduling: Scheduling::Always }),
			);
			assert_eq!(Parachains::parachain_code(&ParaId::from(5u32)), Some(vec![1, 2, 3]));
			assert_eq!(Parachains::parachain_code(&ParaId::from(100u32)), Some(vec![4, 5, 6]));
		});
	}

	#[test]
	fn swap_chain_and_thread_works() {
		new_test_ext(vec![]).execute_with(|| {
			assert_ok!(Registrar::set_thread_count(Origin::ROOT, 1));

			// Need to trigger on_initialize
			run_to_block(2);

			// Register a new parathread
			assert_ok!(Registrar::register_parathread(
				Origin::signed(1u64),
				vec![1; 3],
				vec![1; 3],
			));

			// Lease out a new parachain
			assert_ok!(Slots::new_auction(Origin::ROOT, 5, 1));
			assert_ok!(Slots::bid(Origin::signed(1), 0, 1, 1, 4, 1));

			run_to_block(9);
			// Ensure that the thread is scheduled around the swap time.
			let col = Sr25519Keyring::One.public().into();
			schedule_thread(user_id(0), &[1; 3], &col);

			run_to_block(10);
			let h = BlakeTwo256::hash(&[2u8; 3]);
			assert_ok!(Slots::fix_deploy_data(Origin::signed(1), 0, user_id(1), h, vec![2; 3]));
			assert_ok!(Slots::elaborate_deploy_data(Origin::signed(0), user_id(1), vec![2; 3]));
			assert_ok!(Slots::set_offboarding(Origin::signed(user_id(1).into_account()), 1));

			run_to_block(11);
			// should be one active parachain and one active parathread.
			assert_eq!(Registrar::active_paras(), vec![
				(user_id(0), Some((col.clone(), Retriable::WithRetries(0)))),
				(user_id(1), None),
			]);

			// One half of the swap call does not actually trigger the swap.
			assert_ok!(Registrar::swap(parachains::Origin::Parachain(user_id(0)).into(), user_id(1)));

			// Nothing changes from what was originally registered
			assert_eq!(Registrar::paras(&user_id(0)), Some(ParaInfo { scheduling: Scheduling::Dynamic }));
			assert_eq!(Registrar::paras(&user_id(1)), Some(ParaInfo { scheduling: Scheduling::Always }));
			assert_eq!(super::Parachains::get(), vec![user_id(1)]);
			assert_eq!(Slots::managed_ids(), vec![user_id(1)]);
			assert_eq!(Slots::deposits(user_id(1)), vec![1; 3]);
			assert_eq!(Slots::offboarding(user_id(1)), 1);
			assert_eq!(Parachains::parachain_code(&user_id(0)), Some(vec![1u8; 3]));
			assert_eq!(Parachains::parachain_head(&user_id(0)), Some(vec![1u8; 3]));
			assert_eq!(Parachains::parachain_code(&user_id(1)), Some(vec![2u8; 3]));
			assert_eq!(Parachains::parachain_head(&user_id(1)), Some(vec![2u8; 3]));
			// Intention to swap is added
			assert_eq!(PendingSwap::get(user_id(0)), Some(user_id(1)));

			// Intention to swap is reciprocated, swap actually happens
			assert_ok!(Registrar::swap(parachains::Origin::Parachain(user_id(1)).into(), user_id(0)));

			assert_eq!(Registrar::paras(&user_id(0)), Some(ParaInfo { scheduling: Scheduling::Always }));
			assert_eq!(Registrar::paras(&user_id(1)), Some(ParaInfo { scheduling: Scheduling::Dynamic }));
			assert_eq!(super::Parachains::get(), vec![user_id(0)]);
			assert_eq!(Slots::managed_ids(), vec![user_id(0)]);
			assert_eq!(Slots::deposits(user_id(0)), vec![1; 3]);
			assert_eq!(Slots::offboarding(user_id(0)), 1);
			assert_eq!(Parachains::parachain_code(&user_id(0)), Some(vec![1u8; 3]));
			assert_eq!(Parachains::parachain_head(&user_id(0)), Some(vec![1u8; 3]));
			assert_eq!(Parachains::parachain_code(&user_id(1)), Some(vec![2u8; 3]));
			assert_eq!(Parachains::parachain_head(&user_id(1)), Some(vec![2u8; 3]));

			// Intention to swap is no longer present
			assert_eq!(PendingSwap::get(user_id(0)), None);
			assert_eq!(PendingSwap::get(user_id(1)), None);

			run_to_block(12);
			// thread should not be queued or scheduled any more, even though it would otherwise be
			// being retried..
			assert_eq!(Registrar::active_paras(), vec![(user_id(0), None)]);
		});
	}

	#[test]
	fn swap_handles_funds_correctly() {
		new_test_ext(vec![]).execute_with(|| {
			assert_ok!(Registrar::set_thread_count(Origin::ROOT, 1));

			// Need to trigger on_initialize
			run_to_block(2);

			let initial_1_balance = Balances::free_balance(1);
			let initial_2_balance = Balances::free_balance(2);

			// User 1 register a new parathread
			assert_ok!(Registrar::register_parathread(
				Origin::signed(1),
				vec![1; 3],
				vec![1; 3],
			));

			// User 2 leases out a new parachain
			assert_ok!(Slots::new_auction(Origin::ROOT, 5, 1));
			assert_ok!(Slots::bid(Origin::signed(2), 0, 1, 1, 4, 1));

			run_to_block(9);

			// Swap the parachain and parathread
			assert_ok!(Registrar::swap(parachains::Origin::Parachain(user_id(0)).into(), user_id(1)));
			assert_ok!(Registrar::swap(parachains::Origin::Parachain(user_id(1)).into(), user_id(0)));

			// Deregister the parathread that was originally a parachain
			assert_ok!(Registrar::deregister_parathread(parachains::Origin::Parachain(user_id(1)).into()));

			// Go past when a parachain loses its slot
			run_to_block(50);

			// Funds are correctly returned
			assert_eq!(Balances::free_balance(1), initial_1_balance);
			assert_eq!(Balances::free_balance(2), initial_2_balance);
		});
	}

	#[test]
	fn register_deregister_chains_works() {
		let parachains = vec![
			(1u32.into(), vec![1; 3], vec![1; 3]),
		];

		new_test_ext(parachains).execute_with(|| {
			// Need to trigger on_initialize
			run_to_block(2);

			// Genesis registration works
			assert_eq!(Registrar::active_paras(), vec![(1u32.into(), None)]);
			assert_eq!(
				Registrar::paras(&ParaId::from(1u32)),
				Some(ParaInfo { scheduling: Scheduling::Always })
			);
			assert_eq!(Parachains::parachain_code(&ParaId::from(1u32)), Some(vec![1; 3]));

			// Register a new parachain
			assert_ok!(Registrar::register_para(
				Origin::ROOT,
				2u32.into(),
				ParaInfo { scheduling: Scheduling::Always },
				vec![2; 3],
				vec![2; 3],
			));

			let orig_bal = Balances::free_balance(&3u64);
			// Register a new parathread
			assert_ok!(Registrar::register_parathread(
				Origin::signed(3u64),
				vec![3; 3],
				vec![3; 3],
			));
			// deposit should be taken (reserved)
			assert_eq!(Balances::free_balance(&3u64) + ParathreadDeposit::get(), orig_bal);
			assert_eq!(Balances::reserved_balance(&3u64), ParathreadDeposit::get());

			run_to_block(3);

			// New paras are registered
			assert_eq!(Registrar::active_paras(), vec![(1u32.into(), None), (2u32.into(), None)]);
			assert_eq!(
				Registrar::paras(&ParaId::from(2u32)),
				Some(ParaInfo { scheduling: Scheduling::Always })
			);
			assert_eq!(
				Registrar::paras(&user_id(0)),
				Some(ParaInfo { scheduling: Scheduling::Dynamic })
			);
			assert_eq!(Parachains::parachain_code(&ParaId::from(2u32)), Some(vec![2; 3]));
			assert_eq!(Parachains::parachain_code(&user_id(0)), Some(vec![3; 3]));

			assert_ok!(Registrar::deregister_para(Origin::ROOT, 2u32.into()));
			assert_ok!(Registrar::deregister_parathread(
				parachains::Origin::Parachain(user_id(0)).into()
			));
			// reserved balance should be returned.
			assert_eq!(Balances::free_balance(&3u64), orig_bal);
			assert_eq!(Balances::reserved_balance(&3u64), 0);

			run_to_block(4);

			assert_eq!(Registrar::active_paras(), vec![(1u32.into(), None)]);
			assert_eq!(Registrar::paras(&ParaId::from(2u32)), None);
			assert_eq!(Parachains::parachain_code(&ParaId::from(2u32)), None);
			assert_eq!(Registrar::paras(&user_id(0)), None);
			assert_eq!(Parachains::parachain_code(&user_id(0)), None);
		});
	}

	#[test]
	fn parathread_scheduling_works() {
		new_test_ext(vec![]).execute_with(|| {
			assert_ok!(Registrar::set_thread_count(Origin::ROOT, 1));

			run_to_block(2);

			// Register a new parathread
			assert_ok!(Registrar::register_parathread(
				Origin::signed(3u64),
				vec![3; 3],
				vec![3; 3],
			));

			run_to_block(3);

			// transaction submitted to get parathread progressed.
			let col = Sr25519Keyring::One.public().into();
			schedule_thread(user_id(0), &[3; 3], &col);

			run_to_block(5);
			assert_eq!(Registrar::active_paras(), vec![
				(user_id(0), Some((col.clone(), Retriable::WithRetries(0))))
			]);
			assert_ok!(Parachains::set_heads(Origin::NONE, vec![
				attest(user_id(0), &Sr25519Keyring::One.pair().into(), &[3; 3], &[0; 0])
			]));

			run_to_block(6);
			// at next block, it shouldn't be retried.
			assert_eq!(Registrar::active_paras(), vec![]);
		});
	}

	#[test]
	fn removing_scheduled_parathread_works() {
		new_test_ext(vec![]).execute_with(|| {
			assert_ok!(Registrar::set_thread_count(Origin::ROOT, 1));

			run_to_block(2);

			// Register some parathreads.
			assert_ok!(Registrar::register_parathread(Origin::signed(3), vec![3; 3], vec![3; 3]));

			run_to_block(3);
			// transaction submitted to get parathread progressed.
			let col = Sr25519Keyring::One.public().into();
			schedule_thread(user_id(0), &[3; 3], &col);

			// now we remove the parathread
			assert_ok!(Registrar::deregister_parathread(
				parachains::Origin::Parachain(user_id(0)).into()
			));

			run_to_block(5);
			assert_eq!(Registrar::active_paras(), vec![]);  // should not be scheduled.

			assert_ok!(Registrar::register_parathread(Origin::signed(3), vec![4; 3], vec![4; 3]));

			run_to_block(6);
			// transaction submitted to get parathread progressed.
			schedule_thread(user_id(1), &[4; 3], &col);

			run_to_block(9);
			// thread's slot was missed and is now being re-scheduled.

			assert_ok!(Registrar::deregister_parathread(
				parachains::Origin::Parachain(user_id(1)).into()
			));

			run_to_block(10);
			// thread's rescheduled slot was missed, but should not be reschedule since it was
			// removed.
			assert_eq!(Registrar::active_paras(), vec![]);  // should not be scheduled.
		});
	}

	#[test]
	fn parathread_rescheduling_works() {
		new_test_ext(vec![]).execute_with(|| {
			assert_ok!(Registrar::set_thread_count(Origin::ROOT, 1));

			run_to_block(2);

			// Register some parathreads.
			assert_ok!(Registrar::register_parathread(Origin::signed(3), vec![3; 3], vec![3; 3]));
			assert_ok!(Registrar::register_parathread(Origin::signed(4), vec![4; 3], vec![4; 3]));
			assert_ok!(Registrar::register_parathread(Origin::signed(5), vec![5; 3], vec![5; 3]));

			run_to_block(3);

			// transaction submitted to get parathread progressed.
			let col = Sr25519Keyring::One.public().into();
			schedule_thread(user_id(0), &[3; 3], &col);

			// 4x: the initial time it was scheduled, plus 3 retries.
			for n in 5..9 {
				run_to_block(n);
				assert_eq!(Registrar::active_paras(), vec![
					(user_id(0), Some((col.clone(), Retriable::WithRetries((n - 5) as u32))))
				]);
			}

			// missed too many times. dropped.
			run_to_block(9);
			assert_eq!(Registrar::active_paras(), vec![]);

			// schedule and miss all 3 and check that they go through the queueing system ok.
			assert_ok!(Registrar::set_thread_count(Origin::ROOT, 2));
			schedule_thread(user_id(0), &[3; 3], &col);
			schedule_thread(user_id(1), &[4; 3], &col);

			run_to_block(10);
			schedule_thread(user_id(2), &[5; 3], &col);

			// 0 and 1 scheduled as normal.
			run_to_block(11);
			assert_eq!(Registrar::active_paras(), vec![
				(user_id(0), Some((col.clone(), Retriable::WithRetries(0)))),
				(user_id(1), Some((col.clone(), Retriable::WithRetries(0))))
			]);

			// 2 scheduled, 0 retried
			run_to_block(12);
			assert_eq!(Registrar::active_paras(), vec![
				(user_id(0), Some((col.clone(), Retriable::WithRetries(1)))),
				(user_id(2), Some((col.clone(), Retriable::WithRetries(0)))),
			]);

			// 1 retried
			run_to_block(13);
			assert_eq!(Registrar::active_paras(), vec![
				(user_id(1), Some((col.clone(), Retriable::WithRetries(1))))
			]);

			// 2 retried
			run_to_block(14);
			assert_eq!(Registrar::active_paras(), vec![
				(user_id(2), Some((col.clone(), Retriable::WithRetries(1))))
			]);

			run_to_block(15);
			assert_eq!(Registrar::active_paras(), vec![
				(user_id(0), Some((col.clone(), Retriable::WithRetries(2))))
			]);

			run_to_block(16);
			assert_eq!(Registrar::active_paras(), vec![
				(user_id(1), Some((col.clone(), Retriable::WithRetries(2))))
			]);

			run_to_block(17);
			assert_eq!(Registrar::active_paras(), vec![
				(user_id(2), Some((col.clone(), Retriable::WithRetries(2))))
			]);
		});
	}

	#[test]
	fn parathread_auction_handles_basic_errors() {
		new_test_ext(vec![]).execute_with(|| {
			run_to_block(2);
			let o = Origin::signed(0);
			assert_ok!(Registrar::register_parathread(o, vec![7, 8, 9], vec![1, 1, 1]));

			run_to_block(3);
			assert_eq!(
				Registrar::paras(&user_id(0)),
				Some(ParaInfo { scheduling: Scheduling::Dynamic })
			);

			let good_para_id = user_id(0);
			let bad_para_id = user_id(1);
			let bad_head_hash = <Test as system::Trait>::Hashing::hash(&vec![1, 2, 1]);
			let good_head_hash = <Test as system::Trait>::Hashing::hash(&vec![1, 1, 1]);
			let info = DispatchInfo::default();

			// Allow for threads
			assert_ok!(Registrar::set_thread_count(Origin::ROOT, 10));

			// Bad parathread id
			let col = CollatorId::default();
			let inner = super::Call::select_parathread(bad_para_id, col.clone(), good_head_hash);
			let call = Call::Registrar(inner);
			assert!(
				LimitParathreadCommits::<Test>(std::marker::PhantomData)
					.validate(&0, &call, info, 0).is_err()
			);

			// Bad head data
			let inner = super::Call::select_parathread(good_para_id, col.clone(), bad_head_hash);
			let call = Call::Registrar(inner);
			assert!(
				LimitParathreadCommits::<Test>(std::marker::PhantomData)
					.validate(&0, &call, info, 0).is_err()
			);

			// No duplicates
			let inner = super::Call::select_parathread(good_para_id, col.clone(), good_head_hash);
			let call = Call::Registrar(inner);
			assert!(
				LimitParathreadCommits::<Test>(std::marker::PhantomData)
					.validate(&0, &call, info, 0).is_ok()
			);
			assert!(
				LimitParathreadCommits::<Test>(std::marker::PhantomData)
					.validate(&0, &call, info, 0).is_err()
			);
		});
	}

	#[test]
	fn parathread_auction_works() {
		new_test_ext(vec![]).execute_with(|| {
			run_to_block(2);
			// Register 5 parathreads
			for x in 0..5 {
				let o = Origin::signed(x as u64);
				assert_ok!(Registrar::register_parathread(o, vec![x; 3], vec![x; 3]));
			}

			run_to_block(3);

			for x in 0..5 {
				assert_eq!(
					Registrar::paras(&user_id(x)),
					Some(ParaInfo { scheduling: Scheduling::Dynamic })
				);
			}

			// Only 3 slots available... who will win??
			assert_ok!(Registrar::set_thread_count(Origin::ROOT, 3));

			// Everyone wants a thread
			for x in 0..5 {
				let para_id = user_id(x as u32);
				let collator_id = CollatorId::default();
				let head_hash = <Test as system::Trait>::Hashing::hash(&vec![x; 3]);
				let inner = super::Call::select_parathread(para_id, collator_id, head_hash);
				let call = Call::Registrar(inner);
				let info = DispatchInfo::default();

				// First 3 transactions win a slot
				if x < 3 {
					assert!(
						LimitParathreadCommits::<Test>(std::marker::PhantomData)
							.validate(&0, &call, info, 0)
							.is_ok()
					);
				} else {
					// All others lose
					assert_noop!(
						LimitParathreadCommits::<Test>(std::marker::PhantomData)
							.validate(&0, &call, info, 0),
						InvalidTransaction::ExhaustsResources.into()
					);
				}
			}

			// 3 Threads are selected
			assert_eq!(
				SelectedThreads::get()[1],
				vec![
					(user_id(0), CollatorId::default()),
					(user_id(1), CollatorId::default()),
					(user_id(2), CollatorId::default()),
				]
			);

			// Assuming Queue Size is 2
			assert_eq!(<Test as self::Trait>::QueueSize::get(), 2);

			// 2 blocks later
			run_to_block(5);
			// Threads left queue
			assert_eq!(SelectedThreads::get()[0], vec![]);
			// Threads are active
			assert_eq!(
				Registrar::active_paras(),
				vec![
					(user_id(0), Some((CollatorId::default(), Retriable::WithRetries(0)))),
					(user_id(1), Some((CollatorId::default(), Retriable::WithRetries(0)))),
					(user_id(2), Some((CollatorId::default(), Retriable::WithRetries(0)))),
				]
			);
		});
	}
}
