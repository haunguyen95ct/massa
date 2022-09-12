// Copyright (c) 2022 MASSA LABS <info@massa.net>

use std::collections::{BTreeMap, VecDeque};

use bitvec::prelude::*;
use massa_hash::Hash;
use massa_models::{
    address::{Address, AddressDeserializer},
    amount::{Amount, AmountDeserializer, AmountSerializer},
    api::IndexedSlot,
    error::ModelsError,
    prehash::PreHashMap,
    serialization::{BitVecDeserializer, BitVecSerializer},
    slot::{Slot, SlotDeserializer, SlotSerializer},
};
use massa_serialization::{
    DeserializeError, Deserializer, SerializeError, Serializer, U64VarIntDeserializer,
    U64VarIntSerializer,
};
use nom::{
    branch::alt,
    bytes::complete::tag,
    combinator::value,
    error::{context, ContextError, ParseError},
    multi::length_count,
    sequence::tuple,
    IResult, Parser,
};
use num::rational::Ratio;
use std::ops::Bound::{Excluded, Included, Unbounded};

use crate::SelectorController;

/// Selector info about an address
#[derive(Default)]
pub struct SelectorAddressInfo {
    /// Number of active rolls
    pub active_rolls: u64,
    /// Next block draws
    pub next_block_draws: Vec<Slot>,
    /// Next endorsement draws
    pub next_endorsement_draws: Vec<IndexedSlot>,
}

/// Final state of PoS
pub struct PoSFinalState {
    /// contiguous cycle history. Back = newest.
    pub cycle_history: VecDeque<CycleInfo>,
    /// coins to be credited at the end of the slot
    pub deferred_credits: DeferredCredits,
    /// selector controller
    pub selector: Box<dyn SelectorController>,
    /// initial rolls, used for negative cycle lookback
    pub initial_rolls: BTreeMap<Address, u64>,
    /// initial seeds, used for negative cycle lookback (cycles -2, -1 in that order)
    pub initial_seeds: Vec<Hash>,
    /// amount deserializer
    pub amount_deserializer: AmountDeserializer,
    /// slot deserializer
    pub slot_deserializer: SlotDeserializer,
    /// deserializer
    pub deferred_credit_length_deserializer: U64VarIntDeserializer,
    /// address deserializer
    pub address_deserializer: AddressDeserializer,
    /// periods per cycle
    pub periods_per_cycle: u64,
    /// thread count
    pub thread_count: u8,
}

#[derive(Debug, Default, Clone)]
/// Structure containing all the PoS deferred credits information
pub struct DeferredCredits(pub BTreeMap<Slot, PreHashMap<Address, Amount>>);

impl DeferredCredits {
    /// Extends the current DeferredCredits with another but accumulates the addresses and amounts
    pub fn nested_extend(&mut self, other: Self) {
        for (slot, new_credits) in other.0 {
            self.0
                .entry(slot)
                .and_modify(|current_credits| {
                    for (address, new_amount) in new_credits.iter() {
                        current_credits
                            .entry(*address)
                            .and_modify(|current_amount| {
                                *current_amount = current_amount.saturating_add(*new_amount);
                            })
                            .or_insert(*new_amount);
                    }
                })
                .or_insert(new_credits);
        }
    }

    /// Remove zero credits
    pub fn remove_zeros(&mut self) {
        let mut delete_slots = Vec::new();
        for (slot, credits) in &mut self.0 {
            credits.retain(|_addr, amount| !amount.is_zero());
            if credits.is_empty() {
                delete_slots.push(*slot);
            }
        }
        for slot in delete_slots {
            self.0.remove(&slot);
        }
    }
}

impl PoSFinalState {
    fn get_first_cycle_index(&self) -> usize {
        // for bootstrap:
        // if cycle_history is full skip the bootstrap safety cycle
        // if not stream it
        //
        // TODO: use config
        if self.cycle_history.len() >= 6 {
            1
        } else {
            0
        }
    }

    /// Gets a part of the Proof of Stake cycle_history. Used only in the bootstrap process.
    ///
    /// # Arguments:
    /// `cursor`: indicates the bootstrap state after the previous payload
    ///
    /// # Returns
    /// The PoS part and the updated cursor
    #[allow(clippy::type_complexity)]
    pub fn get_cycle_history_part(
        &self,
        cursor: Option<u64>,
    ) -> Result<(Vec<u8>, Option<u64>, Option<bool>), ModelsError> {
        let cycle_index = if let Some(last_cycle) = cursor {
            if let Some(index) = self
                .cycle_history
                .iter()
                .position(|cycle| cycle.cycle == last_cycle)
            {
                if index == self.cycle_history.len() - 1 {
                    return Ok((Vec::default(), cursor, Some(false)));
                }
                index.saturating_add(1)
            } else {
                // if an outdated cycle is provided start from the beginning
                self.get_first_cycle_index()
            }
        } else {
            self.get_first_cycle_index()
        };
        let mut part = Vec::new();
        let mut last_cycle = None;
        let mut complete_ident = None;
        let u64_ser = U64VarIntSerializer::new();
        let bitvec_ser = BitVecSerializer::new();
        if let Some(CycleInfo {
            cycle,
            complete,
            roll_counts,
            rng_seed,
            production_stats,
        }) = self.cycle_history.get(cycle_index)
        {
            // TODO: limit the whole info with CYCLE_INFO_SIZE_MESSAGE_BYTES
            u64_ser.serialize(cycle, &mut part)?;
            part.push(if *complete { 1 } else { 0 });
            // TODO: limit this with ROLL_COUNTS_PART_SIZE_MESSAGE_BYTES
            u64_ser.serialize(&(roll_counts.len() as u64), &mut part)?;
            for (addr, count) in roll_counts {
                part.extend(addr.to_bytes());
                u64_ser.serialize(count, &mut part)?;
            }
            bitvec_ser.serialize(rng_seed, &mut part)?;
            // TODO: limit this with PRODUCTION_STATS_PART_SIZE_MESSAGE_BYTES
            u64_ser.serialize(&(production_stats.len() as u64), &mut part)?;
            for (addr, stats) in production_stats {
                part.extend(addr.to_bytes());
                u64_ser.serialize(&stats.block_success_count, &mut part)?;
                u64_ser.serialize(&stats.block_failure_count, &mut part)?;
            }
            last_cycle = Some(*cycle);
            complete_ident = Some(*complete);
        }
        Ok((part, last_cycle, complete_ident))
    }

    /// Gets a part of the Proof of Stake deferred_credits. Used only in the bootstrap process.
    ///
    /// # Arguments:
    /// `cursor`: indicates the bootstrap state after the previous payload
    ///
    /// # Returns
    /// The PoS part and the updated cursor
    pub fn get_deferred_credits_part(
        &self,
        cursor: Option<Slot>,
    ) -> Result<(Vec<u8>, Option<Slot>), ModelsError> {
        let last_slot = if let Some(last_slot) = cursor {
            Excluded(last_slot)
        } else {
            Unbounded
        };
        let mut part = Vec::new();
        let mut last_credits_slot = None;
        let slot_ser = SlotSerializer::new();
        let u64_ser = U64VarIntSerializer::new();
        let amount_ser = AmountSerializer::new();
        if self
            .deferred_credits
            .0
            .range((last_slot, Unbounded))
            .last()
            .is_some()
        {
            u64_ser.serialize(&(self.deferred_credits.0.len() as u64), &mut part)?;
        }
        // TODO: iterate in reverse order to avoid steaming credits that will be soon removed
        for (slot, credits) in self.deferred_credits.0.range((last_slot, Unbounded)) {
            // TODO: limit this with DEFERRED_CREDITS_PART_SIZE_MESSAGE_BYTES
            // NOTE: above will prevent the use of lenght_count combinator, many0 did not do the job
            slot_ser.serialize(slot, &mut part)?;
            u64_ser.serialize(&(credits.len() as u64), &mut part)?;
            for (addr, amount) in credits {
                part.extend(addr.to_bytes());
                amount_ser.serialize(amount, &mut part)?;
            }
            last_credits_slot = Some(*slot);
        }
        Ok((part, last_credits_slot))
    }

    /// Sets a part of the Proof of Stake cycle_history. Used only in the bootstrap process.
    ///
    /// # Arguments
    /// `part`: the raw data received from `get_pos_state_part` and used to update PoS State
    pub fn set_cycle_history_part(&mut self, part: &[u8]) -> Result<Option<u64>, ModelsError> {
        if part.is_empty() {
            return Ok(None);
        }
        let u64_deser = U64VarIntDeserializer::new(Included(u64::MIN), Included(u64::MAX));
        let bitvec_deser = BitVecDeserializer::new();
        let address_deser = AddressDeserializer::new();
        #[allow(clippy::type_complexity)]
        let (rest, cycle): (
            &[u8],
            (
                u64,
                bool,
                Vec<(Address, u64)>,
                bitvec::vec::BitVec<u8>,
                Vec<(Address, u64, u64)>,
            ),
        ) = context(
            "cycle_history",
            tuple((
                context("cycle", |input| {
                    u64_deser.deserialize::<DeserializeError>(input)
                }),
                context(
                    "complete",
                    alt((value(true, tag(&[1])), value(false, tag(&[0])))),
                ),
                context(
                    "roll_counts",
                    length_count(
                        context("roll_counts length", |input| u64_deser.deserialize(input)),
                        tuple((
                            context("address", |input| address_deser.deserialize(input)),
                            context("count", |input| u64_deser.deserialize(input)),
                        )),
                    ),
                ),
                context("rng_seed", |input| bitvec_deser.deserialize(input)),
                context(
                    "production_stats",
                    length_count(
                        context("production_stats length", |input| {
                            u64_deser.deserialize(input)
                        }),
                        tuple((
                            context("address", |input| address_deser.deserialize(input)),
                            context("block_success_count", |input| u64_deser.deserialize(input)),
                            context("block_failure_count", |input| u64_deser.deserialize(input)),
                        )),
                    ),
                ),
            )),
        )
        .parse(part)
        .map_err(|err| ModelsError::DeserializeError(err.to_string()))?;
        let stats_iter =
            cycle
                .4
                .into_iter()
                .map(|(addr, block_success_count, block_failure_count)| {
                    (
                        addr,
                        ProductionStats {
                            block_success_count,
                            block_failure_count,
                        },
                    )
                });
        if rest.is_empty() {
            if let Some(info) = self.cycle_history.back_mut() && info.cycle == cycle.0 {
                info.complete = cycle.1;
                info.roll_counts.extend(cycle.2);
                info.rng_seed.extend(cycle.3);
                info.production_stats.extend(stats_iter);
            } else {
                self.cycle_history.push_back(CycleInfo {
                    cycle: cycle.0,
                    complete: cycle.1,
                    roll_counts: cycle.2.into_iter().collect(),
                    rng_seed: cycle.3,
                    production_stats: stats_iter.collect(),
                })
            }
            Ok(self.cycle_history.back().map(|v| v.cycle))
        } else {
            Err(ModelsError::SerializeError(
                "data is left after set_cycle_history_part PoSFinalState part deserialization"
                    .to_string(),
            ))
        }
    }

    /// Sets a part of the Proof of Stake deferred_credits. Used only in the bootstrap process.
    ///
    /// # Arguments
    /// `part`: the raw data received from `get_pos_state_part` and used to update PoS State
    pub fn set_deferred_credits_part(&mut self, part: &[u8]) -> Result<Option<Slot>, ModelsError> {
        if part.is_empty() {
            return Ok(None);
        }
        let (rest, credits) = context(
            "deferred_credits",
            length_count(
                context("deferred_credits length", |input| {
                    self.deferred_credit_length_deserializer.deserialize(input)
                }),
                tuple((
                    context("slot", |input| {
                        self.slot_deserializer
                            .deserialize::<DeserializeError>(input)
                    }),
                    context(
                        "credits",
                        length_count(
                            context("credits length", |input| {
                                self.deferred_credit_length_deserializer.deserialize(input)
                            }),
                            tuple((
                                context("address", |input| {
                                    self.address_deserializer.deserialize(input)
                                }),
                                context("amount", |input| {
                                    self.amount_deserializer.deserialize(input)
                                }),
                            )),
                        ),
                    ),
                )),
            ),
        )
        .parse(part)
        .map_err(|err| ModelsError::DeserializeError(err.to_string()))?;
        if rest.is_empty() {
            let new_credits = DeferredCredits(
                credits
                    .into_iter()
                    .map(|(slot, credits)| (slot, credits.into_iter().collect()))
                    .collect(),
            );
            self.deferred_credits.nested_extend(new_credits);
            Ok(self.deferred_credits.0.last_key_value().map(|(k, _)| *k))
        } else {
            Err(ModelsError::SerializeError(
                "data is left after set_deferred_credits_part PoSFinalState part deserialization"
                    .to_string(),
            ))
        }
    }
}

/// State of a cycle for all threads
#[derive(Default, Debug, Clone, PartialEq, Eq)]
pub struct CycleInfo {
    /// cycle number
    pub cycle: u64,
    /// whether the cycle is complete (all slots final)
    pub complete: bool,
    /// number of rolls each staking address has
    pub roll_counts: BTreeMap<Address, u64>,
    /// random seed bits of all slots in the cycle so far
    pub rng_seed: BitVec<u8>,
    /// Per-address production statistics
    pub production_stats: PreHashMap<Address, ProductionStats>,
}

/// Block production statistic
#[derive(Default, Debug, Copy, Clone, PartialEq, Eq)]
pub struct ProductionStats {
    /// Number of successfully created blocks
    pub block_success_count: u64,
    /// Number of blocks missed
    pub block_failure_count: u64,
}

impl ProductionStats {
    /// Check if the production stats are above the required percentage
    pub fn is_satisfying(&self, max_miss_ratio: &Ratio<u64>) -> bool {
        let opportunities_count = self.block_success_count + self.block_failure_count;
        if opportunities_count == 0 {
            return true;
        }
        &Ratio::new(self.block_failure_count, opportunities_count) <= max_miss_ratio
    }

    /// Increment a production stat struct with another
    pub fn extend(&mut self, stats: &ProductionStats) {
        self.block_success_count = self
            .block_success_count
            .saturating_add(stats.block_success_count);
        self.block_failure_count = self
            .block_failure_count
            .saturating_add(stats.block_failure_count);
    }
}

/// Recap of all PoS changes
#[derive(Default, Debug, Clone)]
pub struct PoSChanges {
    /// extra block seed bits added
    pub seed_bits: BitVec<u8>,

    /// new roll counts for addresses (can be 0 to remove the address from the registry)
    pub roll_changes: PreHashMap<Address, u64>,

    /// updated production statistics
    pub production_stats: PreHashMap<Address, ProductionStats>,

    /// set deferred credits indexed by target slot (can be set to 0 to cancel some, in case of slash)
    /// ordered structure to ensure slot iteration order is deterministic
    pub deferred_credits: DeferredCredits,
}

impl PoSChanges {
    /// Check if changes are empty
    pub fn is_empty(&self) -> bool {
        self.seed_bits.is_empty()
            && self.roll_changes.is_empty()
            && self.production_stats.is_empty()
            && self.deferred_credits.0.is_empty()
    }
    /// Extends the current `PosChanges` with another one
    pub fn extend(&mut self, other: PoSChanges) {
        // extend seed bits
        self.seed_bits.extend(other.seed_bits);

        // extend roll changes
        self.roll_changes.extend(other.roll_changes);

        // extend production stats
        for (other_addr, other_stats) in other.production_stats {
            self.production_stats
                .entry(other_addr)
                .or_insert_with(ProductionStats::default)
                .extend(&other_stats);
        }

        // extend deferred credits
        self.deferred_credits.nested_extend(other.deferred_credits);
    }
}

/// `PoSChanges` Serializer
pub struct PoSChangesSerializer {
    bit_vec_serializer: BitVecSerializer,
    u64_serializer: U64VarIntSerializer,
    slot_serializer: SlotSerializer,
    amount_serializer: AmountSerializer,
}

impl Default for PoSChangesSerializer {
    fn default() -> Self {
        Self::new()
    }
}

impl PoSChangesSerializer {
    /// Create a new `PoSChanges` Serializer
    pub fn new() -> PoSChangesSerializer {
        PoSChangesSerializer {
            bit_vec_serializer: BitVecSerializer::new(),
            u64_serializer: U64VarIntSerializer::new(),
            slot_serializer: SlotSerializer::new(),
            amount_serializer: AmountSerializer::new(),
        }
    }
}

impl Serializer<PoSChanges> for PoSChangesSerializer {
    fn serialize(&self, value: &PoSChanges, buffer: &mut Vec<u8>) -> Result<(), SerializeError> {
        // seed_bits
        self.bit_vec_serializer
            .serialize(&value.seed_bits, buffer)?;

        // roll_changes
        let entry_count: u64 = value.roll_changes.len().try_into().map_err(|err| {
            SerializeError::GeneralError(format!("too many entries in roll_changes: {}", err))
        })?;
        self.u64_serializer.serialize(&entry_count, buffer)?;
        for (addr, roll) in value.roll_changes.iter() {
            buffer.extend(addr.to_bytes());
            self.u64_serializer.serialize(roll, buffer)?;
        }

        // production_stats
        let entry_count: u64 = value.production_stats.len().try_into().map_err(|err| {
            SerializeError::GeneralError(format!("too many entries in production_stats: {}", err))
        })?;
        self.u64_serializer.serialize(&entry_count, buffer)?;
        for (
            addr,
            ProductionStats {
                block_success_count,
                block_failure_count,
            },
        ) in value.production_stats.iter()
        {
            buffer.extend(addr.to_bytes());
            self.u64_serializer.serialize(block_success_count, buffer)?;
            self.u64_serializer.serialize(block_failure_count, buffer)?;
        }

        // deferred_credits
        let entry_count: u64 = value.deferred_credits.0.len().try_into().map_err(|err| {
            SerializeError::GeneralError(format!("too many entries in deferred_credits: {}", err))
        })?;
        self.u64_serializer.serialize(&entry_count, buffer)?;
        for (slot, credits) in value.deferred_credits.0.iter() {
            self.slot_serializer.serialize(slot, buffer)?;
            let credits_entry_count: u64 = credits.len().try_into().map_err(|err| {
                SerializeError::GeneralError(format!("too many entries in credits: {}", err))
            })?;
            self.u64_serializer
                .serialize(&credits_entry_count, buffer)?;
            for (addr, amount) in credits {
                buffer.extend(addr.to_bytes());
                self.amount_serializer.serialize(amount, buffer)?;
            }
        }
        Ok(())
    }
}

/// `PoSChanges` Deserializer
pub struct PoSChangesDeserializer {
    bit_vec_deserializer: BitVecDeserializer,
    roll_changes_deserializer: RollChangesDeserializer,
    production_stats_deserializer: ProductionStatsDeserializer,
    deferred_credits_deserializer: DeferredCreditsDeserializer,
}

impl PoSChangesDeserializer {
    /// Create a new `PoSChanges` Deserializer
    pub fn new(thread_count: u8) -> PoSChangesDeserializer {
        PoSChangesDeserializer {
            bit_vec_deserializer: BitVecDeserializer::new(),
            roll_changes_deserializer: RollChangesDeserializer::new(),
            production_stats_deserializer: ProductionStatsDeserializer::new(),
            deferred_credits_deserializer: DeferredCreditsDeserializer::new(thread_count),
        }
    }
}

impl Deserializer<PoSChanges> for PoSChangesDeserializer {
    fn deserialize<'a, E: ParseError<&'a [u8]> + ContextError<&'a [u8]>>(
        &self,
        buffer: &'a [u8],
    ) -> IResult<&'a [u8], PoSChanges, E> {
        context(
            "Failed PoSChanges deserialization",
            tuple((
                |input| self.bit_vec_deserializer.deserialize(input),
                |input| self.roll_changes_deserializer.deserialize(input),
                |input| self.production_stats_deserializer.deserialize(input),
                |input| self.deferred_credits_deserializer.deserialize(input),
            )),
        )
        .map(
            |(seed_bits, roll_changes, production_stats, deferred_credits)| PoSChanges {
                seed_bits,
                roll_changes,
                production_stats,
                deferred_credits,
            },
        )
        .parse(buffer)
    }
}

struct RollChangesDeserializer {
    address_deserializer: AddressDeserializer,
    u64_deserializer: U64VarIntDeserializer,
}

impl RollChangesDeserializer {
    fn new() -> RollChangesDeserializer {
        RollChangesDeserializer {
            address_deserializer: AddressDeserializer::new(),
            u64_deserializer: U64VarIntDeserializer::new(Included(u64::MIN), Included(u64::MAX)),
        }
    }
}

impl Deserializer<PreHashMap<Address, u64>> for RollChangesDeserializer {
    fn deserialize<'a, E: ParseError<&'a [u8]> + ContextError<&'a [u8]>>(
        &self,
        buffer: &'a [u8],
    ) -> IResult<&'a [u8], PreHashMap<Address, u64>, E> {
        context(
            "Failed RollChanges deserialization",
            length_count(
                context("Failed length deserialization", |input| {
                    self.u64_deserializer.deserialize(input)
                }),
                tuple((
                    |input| self.address_deserializer.deserialize(input),
                    |input| self.u64_deserializer.deserialize(input),
                )),
            ),
        )
        .map(|elements| elements.into_iter().collect())
        .parse(buffer)
    }
}

struct ProductionStatsDeserializer {
    address_deserializer: AddressDeserializer,
    u64_deserializer: U64VarIntDeserializer,
}

impl ProductionStatsDeserializer {
    fn new() -> ProductionStatsDeserializer {
        ProductionStatsDeserializer {
            address_deserializer: AddressDeserializer::new(),
            u64_deserializer: U64VarIntDeserializer::new(Included(u64::MIN), Included(u64::MAX)),
        }
    }
}

impl Deserializer<PreHashMap<Address, ProductionStats>> for ProductionStatsDeserializer {
    fn deserialize<'a, E: ParseError<&'a [u8]> + ContextError<&'a [u8]>>(
        &self,
        buffer: &'a [u8],
    ) -> IResult<&'a [u8], PreHashMap<Address, ProductionStats>, E> {
        context(
            "Failed ProductionStats deserialization",
            length_count(
                context("Failed length deserialization", |input| {
                    self.u64_deserializer.deserialize(input)
                }),
                tuple((
                    |input| self.address_deserializer.deserialize(input),
                    |input| self.u64_deserializer.deserialize(input),
                    |input| self.u64_deserializer.deserialize(input),
                )),
            ),
        )
        .map(|elements| {
            elements
                .into_iter()
                .map(|(addr, block_success_count, block_failure_count)| {
                    (
                        addr,
                        ProductionStats {
                            block_success_count,
                            block_failure_count,
                        },
                    )
                })
                .collect()
        })
        .parse(buffer)
    }
}

struct DeferredCreditsDeserializer {
    u64_deserializer: U64VarIntDeserializer,
    slot_deserializer: SlotDeserializer,
    credit_deserializer: CreditDeserializer,
}

impl DeferredCreditsDeserializer {
    fn new(thread_count: u8) -> DeferredCreditsDeserializer {
        DeferredCreditsDeserializer {
            u64_deserializer: U64VarIntDeserializer::new(Included(u64::MIN), Included(u64::MAX)),
            slot_deserializer: SlotDeserializer::new(
                (Included(0), Included(u64::MAX)),
                (Included(0), Excluded(thread_count)),
            ),
            credit_deserializer: CreditDeserializer::new(),
        }
    }
}

impl Deserializer<DeferredCredits> for DeferredCreditsDeserializer {
    fn deserialize<'a, E: ParseError<&'a [u8]> + ContextError<&'a [u8]>>(
        &self,
        buffer: &'a [u8],
    ) -> IResult<&'a [u8], DeferredCredits, E> {
        context(
            "Failed DeferredCredits deserialization",
            length_count(
                context("Failed length deserialization", |input| {
                    self.u64_deserializer.deserialize(input)
                }),
                tuple((
                    |input| self.slot_deserializer.deserialize(input),
                    |input| self.credit_deserializer.deserialize(input),
                )),
            ),
        )
        .map(|elements| DeferredCredits(elements.into_iter().collect()))
        .parse(buffer)
    }
}

struct CreditDeserializer {
    u64_deserializer: U64VarIntDeserializer,
    address_deserializer: AddressDeserializer,
    amount_deserializer: AmountDeserializer,
}

impl CreditDeserializer {
    fn new() -> CreditDeserializer {
        CreditDeserializer {
            u64_deserializer: U64VarIntDeserializer::new(Included(u64::MIN), Included(u64::MAX)),
            address_deserializer: AddressDeserializer::new(),
            amount_deserializer: AmountDeserializer::new(
                Included(Amount::MIN),
                Included(Amount::MAX),
            ),
        }
    }
}

impl Deserializer<PreHashMap<Address, Amount>> for CreditDeserializer {
    fn deserialize<'a, E: ParseError<&'a [u8]> + ContextError<&'a [u8]>>(
        &self,
        buffer: &'a [u8],
    ) -> IResult<&'a [u8], PreHashMap<Address, Amount>, E> {
        context(
            "Failed Credit deserialization",
            length_count(
                context("Failed length deserialization", |input| {
                    self.u64_deserializer.deserialize(input)
                }),
                tuple((
                    |input| self.address_deserializer.deserialize(input),
                    |input| self.amount_deserializer.deserialize(input),
                )),
            ),
        )
        .map(|elements| elements.into_iter().collect())
        .parse(buffer)
    }
}

/// Selections of endorsements and producer
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    /// Choosen endorsements
    pub endorsements: Vec<Address>,
    /// Choosen block producer
    pub producer: Address,
}