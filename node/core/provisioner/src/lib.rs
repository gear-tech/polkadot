// Copyright 2020 Parity Technologies (UK) Ltd.
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

//! The provisioner is responsible for assembling a relay chain block
//! from a set of available parachain candidates of its choice.

#![deny(missing_docs, unused_crate_dependencies)]

use bitvec::vec::BitVec;
use futures::{
	channel::oneshot, future::BoxFuture, prelude::*, stream::FuturesUnordered, FutureExt,
};
use futures_timer::Delay;

use polkadot_node_primitives::CandidateVotes;
use polkadot_node_subsystem::{
	jaeger,
	messages::{
		CandidateBackingMessage, ChainApiMessage, DisputeCoordinatorMessage, ProvisionableData,
		ProvisionerInherentData, ProvisionerMessage,
	},
	overseer, ActivatedLeaf, ActiveLeavesUpdate, FromOrchestra, LeafStatus, OverseerSignal,
	PerLeafSpan, SpawnedSubsystem, SubsystemError,
};
use polkadot_node_subsystem_util::{
	request_availability_cores, request_persisted_validation_data, TimeoutExt,
};
use polkadot_primitives::v2::{
	BackedCandidate, BlockNumber, CandidateHash, CandidateReceipt, CoreState, DisputeState,
	DisputeStatement, DisputeStatementSet, Hash, MultiDisputeStatementSet, OccupiedCoreAssumption,
	SessionIndex, SignedAvailabilityBitfield, ValidatorIndex,
};
use std::collections::{BTreeMap, HashMap, HashSet};

mod error;
mod metrics;
mod onchain_disputes;

pub use self::metrics::*;
use error::{Error, FatalResult};

#[cfg(test)]
mod tests;

/// How long to wait before proposing.
const PRE_PROPOSE_TIMEOUT: std::time::Duration = core::time::Duration::from_millis(2000);
/// Some timeout to ensure task won't hang around in the background forever on issues.
const SEND_INHERENT_DATA_TIMEOUT: std::time::Duration = core::time::Duration::from_millis(500);

const LOG_TARGET: &str = "parachain::provisioner";

/// The provisioner subsystem.
pub struct ProvisionerSubsystem {
	metrics: Metrics,
}

impl ProvisionerSubsystem {
	/// Create a new instance of the `ProvisionerSubsystem`.
	pub fn new(metrics: Metrics) -> Self {
		Self { metrics }
	}
}

/// A per-relay-parent state for the provisioning subsystem.
pub struct PerRelayParent {
	leaf: ActivatedLeaf,
	backed_candidates: Vec<CandidateReceipt>,
	signed_bitfields: Vec<SignedAvailabilityBitfield>,
	is_inherent_ready: bool,
	awaiting_inherent: Vec<oneshot::Sender<ProvisionerInherentData>>,
	span: PerLeafSpan,
}

impl PerRelayParent {
	fn new(leaf: ActivatedLeaf) -> Self {
		let span = PerLeafSpan::new(leaf.span.clone(), "provisioner");

		Self {
			leaf,
			backed_candidates: Vec::new(),
			signed_bitfields: Vec::new(),
			is_inherent_ready: false,
			awaiting_inherent: Vec::new(),
			span,
		}
	}
}

type InherentDelays = FuturesUnordered<BoxFuture<'static, Hash>>;

#[overseer::subsystem(Provisioner, error=SubsystemError, prefix=self::overseer)]
impl<Context> ProvisionerSubsystem {
	fn start(self, ctx: Context) -> SpawnedSubsystem {
		let future = async move {
			run(ctx, self.metrics)
				.await
				.map_err(|e| SubsystemError::with_origin("provisioner", e))
		}
		.boxed();

		SpawnedSubsystem { name: "provisioner-subsystem", future }
	}
}

#[overseer::contextbounds(Provisioner, prefix = self::overseer)]
async fn run<Context>(mut ctx: Context, metrics: Metrics) -> FatalResult<()> {
	let mut inherent_delays = InherentDelays::new();
	let mut per_relay_parent = HashMap::new();

	loop {
		let result =
			run_iteration(&mut ctx, &mut per_relay_parent, &mut inherent_delays, &metrics).await;

		match result {
			Ok(()) => break,
			err => crate::error::log_error(err)?,
		}
	}

	Ok(())
}

#[overseer::contextbounds(Provisioner, prefix = self::overseer)]
async fn run_iteration<Context>(
	ctx: &mut Context,
	per_relay_parent: &mut HashMap<Hash, PerRelayParent>,
	inherent_delays: &mut InherentDelays,
	metrics: &Metrics,
) -> Result<(), Error> {
	loop {
		futures::select! {
			from_overseer = ctx.recv().fuse() => {
				match from_overseer? {
					FromOrchestra::Signal(OverseerSignal::ActiveLeaves(update)) =>
						handle_active_leaves_update(update, per_relay_parent, inherent_delays),
					FromOrchestra::Signal(OverseerSignal::BlockFinalized(..)) => {},
					FromOrchestra::Signal(OverseerSignal::Conclude) => return Ok(()),
					FromOrchestra::Communication { msg } => {
						handle_communication(ctx, per_relay_parent, msg, metrics).await?;
					},
				}
			},
			hash = inherent_delays.select_next_some() => {
				if let Some(state) = per_relay_parent.get_mut(&hash) {
					state.is_inherent_ready = true;

					gum::trace!(
						target: LOG_TARGET,
						relay_parent = ?hash,
						"Inherent Data became ready"
					);

					let return_senders = std::mem::take(&mut state.awaiting_inherent);
					if !return_senders.is_empty() {
						send_inherent_data_bg(ctx, &state, return_senders, metrics.clone()).await?;
					}
				}
			}
		}
	}
}

fn handle_active_leaves_update(
	update: ActiveLeavesUpdate,
	per_relay_parent: &mut HashMap<Hash, PerRelayParent>,
	inherent_delays: &mut InherentDelays,
) {
	for deactivated in &update.deactivated {
		per_relay_parent.remove(deactivated);
	}

	for leaf in update.activated {
		let delay_fut = Delay::new(PRE_PROPOSE_TIMEOUT).map(move |_| leaf.hash).boxed();
		per_relay_parent.insert(leaf.hash, PerRelayParent::new(leaf));
		inherent_delays.push(delay_fut);
	}
}

#[overseer::contextbounds(Provisioner, prefix = self::overseer)]
async fn handle_communication<Context>(
	ctx: &mut Context,
	per_relay_parent: &mut HashMap<Hash, PerRelayParent>,
	message: ProvisionerMessage,
	metrics: &Metrics,
) -> Result<(), Error> {
	match message {
		ProvisionerMessage::RequestInherentData(relay_parent, return_sender) => {
			gum::trace!(target: LOG_TARGET, ?relay_parent, "Inherent data got requested.");

			if let Some(state) = per_relay_parent.get_mut(&relay_parent) {
				if state.is_inherent_ready {
					gum::trace!(target: LOG_TARGET, ?relay_parent, "Calling send_inherent_data.");
					send_inherent_data_bg(ctx, &state, vec![return_sender], metrics.clone())
						.await?;
				} else {
					gum::trace!(
						target: LOG_TARGET,
						?relay_parent,
						"Queuing inherent data request (inherent data not yet ready)."
					);
					state.awaiting_inherent.push(return_sender);
				}
			}
		},
		ProvisionerMessage::ProvisionableData(relay_parent, data) => {
			if let Some(state) = per_relay_parent.get_mut(&relay_parent) {
				let span = state.span.child("provisionable-data");
				let _timer = metrics.time_provisionable_data();

				gum::trace!(target: LOG_TARGET, ?relay_parent, "Received provisionable data.");

				note_provisionable_data(state, &span, data);
			}
		},
	}

	Ok(())
}

#[overseer::contextbounds(Provisioner, prefix = self::overseer)]
async fn send_inherent_data_bg<Context>(
	ctx: &mut Context,
	per_relay_parent: &PerRelayParent,
	return_senders: Vec<oneshot::Sender<ProvisionerInherentData>>,
	metrics: Metrics,
) -> Result<(), Error> {
	let leaf = per_relay_parent.leaf.clone();
	let signed_bitfields = per_relay_parent.signed_bitfields.clone();
	let backed_candidates = per_relay_parent.backed_candidates.clone();
	let span = per_relay_parent.span.child("req-inherent-data");

	let mut sender = ctx.sender().clone();

	let bg = async move {
		let _span = span;
		let _timer = metrics.time_request_inherent_data();

		gum::trace!(
			target: LOG_TARGET,
			relay_parent = ?leaf.hash,
			"Sending inherent data in background."
		);

		let send_result = send_inherent_data(
			&leaf,
			&signed_bitfields,
			&backed_candidates,
			return_senders,
			&mut sender,
			&metrics,
		) // Make sure call is not taking forever:
		.timeout(SEND_INHERENT_DATA_TIMEOUT)
		.map(|v| match v {
			Some(r) => r,
			None => Err(Error::SendInherentDataTimeout),
		});

		match send_result.await {
			Err(err) => {
				gum::warn!(target: LOG_TARGET, err = ?err, "failed to assemble or send inherent data");
				metrics.on_inherent_data_request(Err(()));
			},
			Ok(()) => {
				metrics.on_inherent_data_request(Ok(()));
				gum::debug!(
					target: LOG_TARGET,
					signed_bitfield_count = signed_bitfields.len(),
					backed_candidates_count = backed_candidates.len(),
					leaf_hash = ?leaf.hash,
					"inherent data sent successfully"
				);
				metrics.observe_inherent_data_bitfields_count(signed_bitfields.len());
			},
		}
	};

	ctx.spawn("send-inherent-data", bg.boxed())
		.map_err(|_| Error::FailedToSpawnBackgroundTask)?;

	Ok(())
}

fn note_provisionable_data(
	per_relay_parent: &mut PerRelayParent,
	span: &jaeger::Span,
	provisionable_data: ProvisionableData,
) {
	match provisionable_data {
		ProvisionableData::Bitfield(_, signed_bitfield) =>
			per_relay_parent.signed_bitfields.push(signed_bitfield),
		ProvisionableData::BackedCandidate(backed_candidate) => {
			let candidate_hash = backed_candidate.hash();
			gum::trace!(
				target: LOG_TARGET,
				?candidate_hash,
				para = ?backed_candidate.descriptor().para_id,
				"noted backed candidate",
			);
			let _span = span
				.child("provisionable-backed")
				.with_candidate(candidate_hash)
				.with_para_id(backed_candidate.descriptor().para_id);
			per_relay_parent.backed_candidates.push(backed_candidate)
		},
		_ => {},
	}
}

type CoreAvailability = BitVec<u8, bitvec::order::Lsb0>;

/// The provisioner is the subsystem best suited to choosing which specific
/// backed candidates and availability bitfields should be assembled into the
/// block. To engage this functionality, a
/// `ProvisionerMessage::RequestInherentData` is sent; the response is a set of
/// non-conflicting candidates and the appropriate bitfields. Non-conflicting
/// means that there are never two distinct parachain candidates included for
/// the same parachain and that new parachain candidates cannot be included
/// until the previous one either gets declared available or expired.
///
/// The main complication here is going to be around handling
/// occupied-core-assumptions. We might have candidates that are only
/// includable when some bitfields are included. And we might have candidates
/// that are not includable when certain bitfields are included.
///
/// When we're choosing bitfields to include, the rule should be simple:
/// maximize availability. So basically, include all bitfields. And then
/// choose a coherent set of candidates along with that.
async fn send_inherent_data(
	leaf: &ActivatedLeaf,
	bitfields: &[SignedAvailabilityBitfield],
	candidates: &[CandidateReceipt],
	return_senders: Vec<oneshot::Sender<ProvisionerInherentData>>,
	from_job: &mut impl overseer::ProvisionerSenderTrait,
	metrics: &Metrics,
) -> Result<(), Error> {
	gum::trace!(
		target: LOG_TARGET,
		relay_parent = ?leaf.hash,
		"Requesting availability cores"
	);
	let availability_cores = request_availability_cores(leaf.hash, from_job)
		.await
		.await
		.map_err(|err| Error::CanceledAvailabilityCores(err))??;

	gum::trace!(
		target: LOG_TARGET,
		relay_parent = ?leaf.hash,
		"Selecting disputes"
	);
	let disputes = select_disputes(from_job, metrics, leaf).await?;
	gum::trace!(
		target: LOG_TARGET,
		relay_parent = ?leaf.hash,
		"Selected disputes"
	);

	// Only include bitfields on fresh leaves. On chain reversions, we want to make sure that
	// there will be at least one block, which cannot get disputed, so the chain can make progress.
	let bitfields = match leaf.status {
		LeafStatus::Fresh =>
			select_availability_bitfields(&availability_cores, bitfields, &leaf.hash),
		LeafStatus::Stale => Vec::new(),
	};

	gum::trace!(
		target: LOG_TARGET,
		relay_parent = ?leaf.hash,
		"Selected bitfields"
	);
	let candidates =
		select_candidates(&availability_cores, &bitfields, candidates, leaf.hash, from_job).await?;

	gum::trace!(
		target: LOG_TARGET,
		relay_parent = ?leaf.hash,
		"Selected candidates"
	);

	gum::debug!(
		target: LOG_TARGET,
		availability_cores_len = availability_cores.len(),
		disputes_count = disputes.len(),
		bitfields_count = bitfields.len(),
		candidates_count = candidates.len(),
		leaf_hash = ?leaf.hash,
		"inherent data prepared",
	);

	let inherent_data =
		ProvisionerInherentData { bitfields, backed_candidates: candidates, disputes };

	gum::trace!(
		target: LOG_TARGET,
		relay_parent = ?leaf.hash,
		"Sending back inherent data to requesters."
	);

	for return_sender in return_senders {
		return_sender
			.send(inherent_data.clone())
			.map_err(|_data| Error::InherentDataReturnChannel)?;
	}

	Ok(())
}

/// In general, we want to pick all the bitfields. However, we have the following constraints:
///
/// - not more than one per validator
/// - each 1 bit must correspond to an occupied core
///
/// If we have too many, an arbitrary selection policy is fine. For purposes of maximizing availability,
/// we pick the one with the greatest number of 1 bits.
///
/// Note: This does not enforce any sorting precondition on the output; the ordering there will be unrelated
/// to the sorting of the input.
fn select_availability_bitfields(
	cores: &[CoreState],
	bitfields: &[SignedAvailabilityBitfield],
	leaf_hash: &Hash,
) -> Vec<SignedAvailabilityBitfield> {
	let mut selected: BTreeMap<ValidatorIndex, SignedAvailabilityBitfield> = BTreeMap::new();

	gum::debug!(
		target: LOG_TARGET,
		bitfields_count = bitfields.len(),
		?leaf_hash,
		"bitfields count before selection"
	);

	'a: for bitfield in bitfields.iter().cloned() {
		if bitfield.payload().0.len() != cores.len() {
			gum::debug!(target: LOG_TARGET, ?leaf_hash, "dropping bitfield due to length mismatch");
			continue
		}

		let is_better = selected
			.get(&bitfield.validator_index())
			.map_or(true, |b| b.payload().0.count_ones() < bitfield.payload().0.count_ones());

		if !is_better {
			gum::trace!(
				target: LOG_TARGET,
				val_idx = bitfield.validator_index().0,
				?leaf_hash,
				"dropping bitfield due to duplication - the better one is kept"
			);
			continue
		}

		for (idx, _) in cores.iter().enumerate().filter(|v| !v.1.is_occupied()) {
			// Bit is set for an unoccupied core - invalid
			if *bitfield.payload().0.get(idx).as_deref().unwrap_or(&false) {
				gum::debug!(
					target: LOG_TARGET,
					val_idx = bitfield.validator_index().0,
					?leaf_hash,
					"dropping invalid bitfield - bit is set for an unoccupied core"
				);
				continue 'a
			}
		}

		let _ = selected.insert(bitfield.validator_index(), bitfield);
	}

	gum::debug!(
		target: LOG_TARGET,
		?leaf_hash,
		"selected {} of all {} bitfields (each bitfield is from a unique validator)",
		selected.len(),
		bitfields.len()
	);

	selected.into_iter().map(|(_, b)| b).collect()
}

/// Determine which cores are free, and then to the degree possible, pick a candidate appropriate to each free core.
async fn select_candidates(
	availability_cores: &[CoreState],
	bitfields: &[SignedAvailabilityBitfield],
	candidates: &[CandidateReceipt],
	relay_parent: Hash,
	sender: &mut impl overseer::ProvisionerSenderTrait,
) -> Result<Vec<BackedCandidate>, Error> {
	let block_number = get_block_number_under_construction(relay_parent, sender).await?;

	let mut selected_candidates =
		Vec::with_capacity(candidates.len().min(availability_cores.len()));

	gum::debug!(
		target: LOG_TARGET,
		leaf_hash=?relay_parent,
		n_candidates = candidates.len(),
		"Candidate receipts (before selection)",
	);

	for (core_idx, core) in availability_cores.iter().enumerate() {
		let (scheduled_core, assumption) = match core {
			CoreState::Scheduled(scheduled_core) => (scheduled_core, OccupiedCoreAssumption::Free),
			CoreState::Occupied(occupied_core) => {
				if bitfields_indicate_availability(core_idx, bitfields, &occupied_core.availability)
				{
					if let Some(ref scheduled_core) = occupied_core.next_up_on_available {
						(scheduled_core, OccupiedCoreAssumption::Included)
					} else {
						continue
					}
				} else {
					if occupied_core.time_out_at != block_number {
						continue
					}
					if let Some(ref scheduled_core) = occupied_core.next_up_on_time_out {
						(scheduled_core, OccupiedCoreAssumption::TimedOut)
					} else {
						continue
					}
				}
			},
			CoreState::Free => continue,
		};

		let validation_data = match request_persisted_validation_data(
			relay_parent,
			scheduled_core.para_id,
			assumption,
			sender,
		)
		.await
		.await
		.map_err(|err| Error::CanceledPersistedValidationData(err))??
		{
			Some(v) => v,
			None => continue,
		};

		let computed_validation_data_hash = validation_data.hash();

		// we arbitrarily pick the first of the backed candidates which match the appropriate selection criteria
		if let Some(candidate) = candidates.iter().find(|backed_candidate| {
			let descriptor = &backed_candidate.descriptor;
			descriptor.para_id == scheduled_core.para_id &&
				descriptor.persisted_validation_data_hash == computed_validation_data_hash
		}) {
			let candidate_hash = candidate.hash();
			gum::trace!(
				target: LOG_TARGET,
				leaf_hash=?relay_parent,
				?candidate_hash,
				para = ?candidate.descriptor.para_id,
				core = core_idx,
				"Selected candidate receipt",
			);

			selected_candidates.push(candidate_hash);
		}
	}

	// now get the backed candidates corresponding to these candidate receipts
	let (tx, rx) = oneshot::channel();
	sender.send_unbounded_message(CandidateBackingMessage::GetBackedCandidates(
		relay_parent,
		selected_candidates.clone(),
		tx,
	));
	let mut candidates = rx.await.map_err(|err| Error::CanceledBackedCandidates(err))?;

	// `selected_candidates` is generated in ascending order by core index, and `GetBackedCandidates`
	// _should_ preserve that property, but let's just make sure.
	//
	// We can't easily map from `BackedCandidate` to `core_idx`, but we know that every selected candidate
	// maps to either 0 or 1 backed candidate, and the hashes correspond. Therefore, by checking them
	// in order, we can ensure that the backed candidates are also in order.
	let mut backed_idx = 0;
	for selected in selected_candidates {
		if selected ==
			candidates.get(backed_idx).ok_or(Error::BackedCandidateOrderingProblem)?.hash()
		{
			backed_idx += 1;
		}
	}
	if candidates.len() != backed_idx {
		Err(Error::BackedCandidateOrderingProblem)?;
	}

	// keep only one candidate with validation code.
	let mut with_validation_code = false;
	candidates.retain(|c| {
		if c.candidate.commitments.new_validation_code.is_some() {
			if with_validation_code {
				return false
			}

			with_validation_code = true;
		}

		true
	});

	gum::debug!(
		target: LOG_TARGET,
		n_candidates = candidates.len(),
		n_cores = availability_cores.len(),
		?relay_parent,
		"Selected backed candidates",
	);

	Ok(candidates)
}

/// Produces a block number 1 higher than that of the relay parent
/// in the event of an invalid `relay_parent`, returns `Ok(0)`
async fn get_block_number_under_construction(
	relay_parent: Hash,
	sender: &mut impl overseer::ProvisionerSenderTrait,
) -> Result<BlockNumber, Error> {
	let (tx, rx) = oneshot::channel();
	sender.send_message(ChainApiMessage::BlockNumber(relay_parent, tx)).await;

	match rx.await.map_err(|err| Error::CanceledBlockNumber(err))? {
		Ok(Some(n)) => Ok(n + 1),
		Ok(None) => Ok(0),
		Err(err) => Err(err.into()),
	}
}

/// The availability bitfield for a given core is the transpose
/// of a set of signed availability bitfields. It goes like this:
///
/// - construct a transverse slice along `core_idx`
/// - bitwise-or it with the availability slice
/// - count the 1 bits, compare to the total length; true on 2/3+
fn bitfields_indicate_availability(
	core_idx: usize,
	bitfields: &[SignedAvailabilityBitfield],
	availability: &CoreAvailability,
) -> bool {
	let mut availability = availability.clone();
	let availability_len = availability.len();

	for bitfield in bitfields {
		let validator_idx = bitfield.validator_index().0 as usize;
		match availability.get_mut(validator_idx) {
			None => {
				// in principle, this function might return a `Result<bool, Error>` so that we can more clearly express this error condition
				// however, in practice, that would just push off an error-handling routine which would look a whole lot like this one.
				// simpler to just handle the error internally here.
				gum::warn!(
					target: LOG_TARGET,
					validator_idx = %validator_idx,
					availability_len = %availability_len,
					"attempted to set a transverse bit at idx {} which is greater than bitfield size {}",
					validator_idx,
					availability_len,
				);

				return false
			},
			Some(mut bit_mut) => *bit_mut |= bitfield.payload().0[core_idx],
		}
	}

	3 * availability.count_ones() >= 2 * availability.len()
}

#[derive(Debug)]
enum RequestType {
	/// Query recent disputes, could be an excessive amount.
	Recent,
	/// Query the currently active and very recently concluded disputes.
	Active,
}

/// Request open disputes identified by `CandidateHash` and the `SessionIndex`.
async fn request_disputes(
	sender: &mut impl overseer::ProvisionerSenderTrait,
	active_or_recent: RequestType,
) -> Vec<(SessionIndex, CandidateHash)> {
	let (tx, rx) = oneshot::channel();
	let msg = match active_or_recent {
		RequestType::Recent => DisputeCoordinatorMessage::RecentDisputes(tx),
		RequestType::Active => DisputeCoordinatorMessage::ActiveDisputes(tx),
	};
	// Bounded by block production - `ProvisionerMessage::RequestInherentData`.
	sender.send_unbounded_message(msg);

	let recent_disputes = match rx.await {
		Ok(r) => r,
		Err(oneshot::Canceled) => {
			gum::warn!(target: LOG_TARGET, "Unable to gather {:?} disputes", active_or_recent);
			Vec::new()
		},
	};
	recent_disputes
}

/// Request the relevant dispute statements for a set of disputes identified by `CandidateHash` and the `SessionIndex`.
async fn request_votes(
	sender: &mut impl overseer::ProvisionerSenderTrait,
	disputes_to_query: Vec<(SessionIndex, CandidateHash)>,
) -> Vec<(SessionIndex, CandidateHash, CandidateVotes)> {
	let (tx, rx) = oneshot::channel();
	// Bounded by block production - `ProvisionerMessage::RequestInherentData`.
	sender.send_unbounded_message(DisputeCoordinatorMessage::QueryCandidateVotes(
		disputes_to_query,
		tx,
	));

	match rx.await {
		Ok(v) => v,
		Err(oneshot::Canceled) => {
			gum::warn!(target: LOG_TARGET, "Unable to query candidate votes");
			Vec::new()
		},
	}
}

/// Extend `acc` by `n` random, picks of not-yet-present in `acc` items of `recent` without repetition and additions of recent.
fn extend_by_random_subset_without_repetition(
	acc: &mut Vec<(SessionIndex, CandidateHash)>,
	extension: Vec<(SessionIndex, CandidateHash)>,
	n: usize,
) {
	use rand::Rng;

	let lut = acc.iter().cloned().collect::<HashSet<(SessionIndex, CandidateHash)>>();

	let mut unique_new =
		extension.into_iter().filter(|recent| !lut.contains(recent)).collect::<Vec<_>>();

	// we can simply add all
	if unique_new.len() <= n {
		acc.extend(unique_new)
	} else {
		acc.reserve(n);
		let mut rng = rand::thread_rng();
		for _ in 0..n {
			let idx = rng.gen_range(0..unique_new.len());
			acc.push(unique_new.swap_remove(idx));
		}
	}
	// assure sorting stays candid according to session index
	acc.sort_unstable_by(|a, b| a.0.cmp(&b.0));
}

/// The maximum number of disputes Provisioner will include in the inherent data.
/// Serves as a protection not to flood the Runtime with excessive data.
const MAX_DISPUTES_FORWARDED_TO_RUNTIME: usize = 1_000;

async fn select_disputes(
	sender: &mut impl overseer::ProvisionerSenderTrait,
	metrics: &metrics::Metrics,
	_leaf: &ActivatedLeaf,
) -> Result<MultiDisputeStatementSet, Error> {
	// Helper lambda
	// Gets the active disputes as input and partitions it in seen and unseen disputes by the Runtime
	// Returns as much unseen disputes as possible and optionally some seen disputes up to `MAX_DISPUTES_FORWARDED_TO_RUNTIME` limit.
	let generate_unseen_active_subset =
		|active: Vec<(SessionIndex, CandidateHash)>,
		 onchain: HashMap<(SessionIndex, CandidateHash), DisputeState>|
		 -> Vec<(SessionIndex, CandidateHash)> {
			let (seen_onchain, mut unseen_onchain): (
				Vec<(SessionIndex, CandidateHash)>,
				Vec<(SessionIndex, CandidateHash)>,
			) = active.into_iter().partition(|d| onchain.contains_key(d));

			if unseen_onchain.len() > MAX_DISPUTES_FORWARDED_TO_RUNTIME {
				// Even unseen on-chain don't fit within the limit. Add as many as possible.
				let mut unseen_subset = Vec::with_capacity(MAX_DISPUTES_FORWARDED_TO_RUNTIME);
				extend_by_random_subset_without_repetition(
					&mut unseen_subset,
					unseen_onchain,
					MAX_DISPUTES_FORWARDED_TO_RUNTIME,
				);
				unseen_subset
			} else {
				// Add all unseen onchain disputes and as much of the seen ones as there is space.
				let n_unseen_onchain = unseen_onchain.len();
				extend_by_random_subset_without_repetition(
					&mut unseen_onchain,
					seen_onchain,
					MAX_DISPUTES_FORWARDED_TO_RUNTIME.saturating_sub(n_unseen_onchain),
				);
				unseen_onchain
			}
		};

	// Helper lambda
	// Extends the active disputes with recent ones up to `MAX_DISPUTES_FORWARDED_TO_RUNTIME` limit. Unseen recent disputes are prioritised.
	let generate_active_and_unseen_recent_subset =
		|recent: Vec<(SessionIndex, CandidateHash)>,
		 mut active: Vec<(SessionIndex, CandidateHash)>,
		 onchain: HashMap<(SessionIndex, CandidateHash), DisputeState>|
		 -> Vec<(SessionIndex, CandidateHash)> {
			let mut n_active = active.len();
			// All active disputes can be sent. Fill the rest of the space with recent ones.
			// We assume there is not enough space for all recent disputes. So we prioritise the unseen ones.
			let (seen_onchain, unseen_onchain): (
				Vec<(SessionIndex, CandidateHash)>,
				Vec<(SessionIndex, CandidateHash)>,
			) = recent.into_iter().partition(|d| onchain.contains_key(d));

			extend_by_random_subset_without_repetition(
				&mut active,
				unseen_onchain,
				MAX_DISPUTES_FORWARDED_TO_RUNTIME.saturating_sub(n_active),
			);
			n_active = active.len();

			if n_active < MAX_DISPUTES_FORWARDED_TO_RUNTIME {
				// Looks like we can add some of the seen disputes too
				extend_by_random_subset_without_repetition(
					&mut active,
					seen_onchain,
					MAX_DISPUTES_FORWARDED_TO_RUNTIME.saturating_sub(n_active),
				);
			}
			active
		};

	gum::trace!(
		target: LOG_TARGET,
		relay_parent = ?_leaf.hash,
		"Request recent disputes"
	);

	// We use `RecentDisputes` instead of `ActiveDisputes` because redundancy is fine.
	// It's heavier than `ActiveDisputes` but ensures that everything from the dispute
	// window gets on-chain, unlike `ActiveDisputes`.
	// In case of an overload condition, we limit ourselves to active disputes, and fill up to the
	// upper bound of disputes to pass to wasm `fn create_inherent_data`.
	// If the active ones are already exceeding the bounds, randomly select a subset.
	let recent = request_disputes(sender, RequestType::Recent).await;

	gum::trace!(
		target: LOG_TARGET,
		relay_paent = ?_leaf.hash,
		"Received recent disputes"
	);

	gum::trace!(
		target: LOG_TARGET,
		relay_paent = ?_leaf.hash,
		"Request on chain disputes"
	);

	// On chain disputes are fetched from the runtime. We want to prioritise the inclusion of unknown
	// disputes in the inherent data. The call relies on staging Runtime API. If the staging API is not
	// enabled in the binary an empty set is generated which doesn't affect the rest of the logic.
	let onchain = match onchain_disputes::get_onchain_disputes(sender, _leaf.hash.clone()).await {
		Ok(r) => r,
		Err(e) => {
			gum::debug!(
				target: LOG_TARGET,
				?e,
				"Can't fetch onchain disputes. Will continue with empty onchain disputes set.",
			);
			HashMap::new()
		},
	};

	gum::trace!(
		target: LOG_TARGET,
		relay_paent = ?_leaf.hash,
		"Received on chain disputes"
	);

	gum::trace!(
		target: LOG_TARGET,
		relay_paent = ?_leaf.hash,
		"Filtering disputes"
	);

	let disputes = if recent.len() > MAX_DISPUTES_FORWARDED_TO_RUNTIME {
		gum::warn!(
			target: LOG_TARGET,
			"Recent disputes are excessive ({} > {}), reduce to active ones, and selected",
			recent.len(),
			MAX_DISPUTES_FORWARDED_TO_RUNTIME
		);
		let active = request_disputes(sender, RequestType::Active).await;
		if active.len() > MAX_DISPUTES_FORWARDED_TO_RUNTIME {
			generate_unseen_active_subset(active, onchain)
		} else {
			generate_active_and_unseen_recent_subset(recent, active, onchain)
		}
	} else {
		recent
	};

	gum::trace!(
		target: LOG_TARGET,
		relay_paent = ?_leaf.hash,
		"Calling `request_votes`"
	);

	// Load all votes for all disputes from the coordinator.
	let dispute_candidate_votes = request_votes(sender, disputes).await;

	gum::trace!(
		target: LOG_TARGET,
		relay_paent = ?_leaf.hash,
		"Finished `request_votes`"
	);

	// Transform all `CandidateVotes` into `MultiDisputeStatementSet`.
	Ok(dispute_candidate_votes
		.into_iter()
		.map(|(session_index, candidate_hash, votes)| {
			let valid_statements = votes
				.valid
				.into_iter()
				.map(|(i, (s, sig))| (DisputeStatement::Valid(s), i, sig));

			let invalid_statements = votes
				.invalid
				.into_iter()
				.map(|(i, (s, sig))| (DisputeStatement::Invalid(s), i, sig));

			metrics.inc_valid_statements_by(valid_statements.len());
			metrics.inc_invalid_statements_by(invalid_statements.len());
			metrics.inc_dispute_statement_sets_by(1);

			DisputeStatementSet {
				candidate_hash,
				session: session_index,
				statements: valid_statements.chain(invalid_statements).collect(),
			}
		})
		.collect())
}
