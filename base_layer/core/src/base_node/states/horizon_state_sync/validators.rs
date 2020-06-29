//  Copyright 2020, The Tari Project
//
//  Redistribution and use in source and binary forms, with or without modification, are permitted provided that the
//  following conditions are met:
//
//  1. Redistributions of source code must retain the above copyright notice, this list of conditions and the following
//  disclaimer.
//
//  2. Redistributions in binary form must reproduce the above copyright notice, this list of conditions and the
//  following disclaimer in the documentation and/or other materials provided with the distribution.
//
//  3. Neither the name of the copyright holder nor the names of its contributors may be used to endorse or promote
//  products derived from this software without specific prior written permission.
//
//  THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS" AND ANY EXPRESS OR IMPLIED WARRANTIES,
//  INCLUDING, BUT NOT LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
//  DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL,
//  SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR
//  SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF LIABILITY,
//  WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE
//  USE OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.

use crate::{
    blocks::{BlockHeader, BlockHeaderValidationError},
    chain_storage::{BlockchainBackend, BlockchainDatabase, ChainStorageError},
    consensus::ConsensusManager,
    proof_of_work::{get_median_timestamp, get_target_difficulty, Difficulty, PowError},
    transactions::types::{BlindingFactor, Commitment, CryptoFactories},
    validation::{StatelessValidation, StatelessValidator, ValidationError},
};
use log::*;
use std::{cmp, fmt, sync::Arc};
use tari_crypto::{
    commitment::HomomorphicCommitmentFactory,
    ristretto::RistrettoSecretKey,
    tari_utilities::{epoch_time::EpochTime, hex::Hex, Hashable},
};

const LOG_TARGET: &str = "c::bn::states::horizon_state_sync::validators";

#[derive(Clone)]
pub struct HorizonSyncValidators {
    pub header: Arc<StatelessValidator<BlockHeader>>,
    pub final_state: Arc<StatelessValidator<u64>>,
}

impl HorizonSyncValidators {
    pub fn new<THeader, TFinal>(header: THeader, final_state: TFinal) -> Self
    where
        THeader: StatelessValidation<BlockHeader> + 'static,
        TFinal: StatelessValidation<u64> + 'static,
    {
        Self {
            header: Arc::new(Box::new(header)),
            final_state: Arc::new(Box::new(final_state)),
        }
    }
}

impl fmt::Debug for HorizonSyncValidators {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HorizonHeaderValidators")
            .field("header", &"...")
            .finish()
    }
}

pub struct HorizonHeadersValidator<B> {
    rules: ConsensusManager,
    db: BlockchainDatabase<B>,
}

impl<B: BlockchainBackend> HorizonHeadersValidator<B> {
    pub fn new(db: BlockchainDatabase<B>, rules: ConsensusManager) -> Self {
        Self { db, rules }
    }
}

impl<B: BlockchainBackend> StatelessValidation<BlockHeader> for HorizonHeadersValidator<B> {
    fn validate(&self, header: &BlockHeader) -> Result<(), ValidationError> {
        let header_id = format!("header #{} ({})", header.height, header.hash().to_hex());
        let tip_header = self
            .db
            .fetch_tip_header()
            .map_err(|e| ValidationError::CustomError(e.to_string()))?;
        self.check_median_timestamp(header, &tip_header)?;
        trace!(
            target: LOG_TARGET,
            "BlockHeader validation: Median timestamp is ok for {} ",
            &header_id
        );
        self.check_achieved_and_target_difficulty(header, &tip_header)?;
        trace!(
            target: LOG_TARGET,
            "BlockHeader validation: Achieved difficulty is ok for {} ",
            &header_id
        );
        debug!(
            target: LOG_TARGET,
            "Block header validation: BlockHeader is VALID for {}", &header_id
        );

        Ok(())
    }
}

impl<B: BlockchainBackend> HorizonHeadersValidator<B> {
    pub fn is_genesis(&self, block_header: &BlockHeader) -> bool {
        block_header.height == 0 && self.rules.get_genesis_block_hash() == block_header.hash()
    }

    /// Calculates the achieved and target difficulties at the specified height and compares them.
    pub fn check_achieved_and_target_difficulty(
        &self,
        block_header: &BlockHeader,
        tip_header: &BlockHeader,
    ) -> Result<(), ValidationError>
    {
        let pow_algo = block_header.pow.pow_algo;
        let target = if self.is_genesis(block_header) {
            Difficulty::from(1)
        } else {
            let target_difficulties = self.fetch_target_difficulties(block_header, tip_header)?;

            let constants = self.rules.consensus_constants();
            get_target_difficulty(
                target_difficulties,
                constants.get_difficulty_block_window() as usize,
                constants.get_diff_target_block_interval(),
                constants.min_pow_difficulty(pow_algo),
                constants.get_difficulty_max_block_interval(),
            )
            .or_else(|e| {
                error!(target: LOG_TARGET, "Validation could not get target difficulty");
                Err(e)
            })
            .map_err(|_| {
                ValidationError::BlockHeaderError(BlockHeaderValidationError::ProofOfWorkError(
                    PowError::InvalidProofOfWork,
                ))
            })?
        };

        if block_header.pow.target_difficulty != target {
            warn!(
                target: LOG_TARGET,
                "Recorded header target difficulty was incorrect: (got = {}, expected = {})",
                block_header.pow.target_difficulty,
                target
            );
            return Err(ValidationError::BlockHeaderError(
                BlockHeaderValidationError::ProofOfWorkError(PowError::InvalidTargetDifficulty),
            ));
        }

        let achieved = block_header.achieved_difficulty();
        if achieved < target {
            warn!(
                target: LOG_TARGET,
                "Proof of work for {} was below the target difficulty. Achieved: {}, Target:{}",
                block_header.hash().to_hex(),
                achieved,
                target
            );
            return Err(ValidationError::BlockHeaderError(
                BlockHeaderValidationError::ProofOfWorkError(PowError::AchievedDifficultyTooLow),
            ));
        }

        Ok(())
    }

    /// This function tests that the block timestamp is greater than the median timestamp at the specified height.
    pub fn check_median_timestamp(
        &self,
        block_header: &BlockHeader,
        tip_header: &BlockHeader,
    ) -> Result<(), ValidationError>
    {
        if self.is_genesis(block_header) {
            // Median timestamps check not required for the genesis block header
            return Ok(());
        }

        let start_height = block_header
            .height
            .saturating_sub(self.rules.consensus_constants().get_median_timestamp_count() as u64);

        if start_height == tip_header.height {
            return Ok(());
        }

        let block_nums = (start_height..tip_header.height).collect();
        let mut timestamps = self
            .db
            .fetch_headers(block_nums)
            .map_err(|e| ValidationError::CustomError(e.to_string()))?
            .iter()
            .map(|h| h.timestamp)
            .collect::<Vec<_>>();
        timestamps.push(tip_header.timestamp);

        // TODO: get_median_timestamp incorrectly returns an Option
        let median_timestamp =
            get_median_timestamp(timestamps).expect("get_median_timestamp only returns None if `timestamps` is empty");

        if block_header.timestamp < median_timestamp {
            warn!(
                target: LOG_TARGET,
                "Block header timestamp {} is less than median timestamp: {} for block:{}",
                block_header.timestamp,
                median_timestamp,
                block_header.hash().to_hex()
            );
            return Err(ValidationError::BlockHeaderError(
                BlockHeaderValidationError::InvalidTimestamp,
            ));
        }
        Ok(())
    }

    /// Returns the set of target difficulties for the specified proof of work algorithm.
    fn fetch_target_difficulties(
        &self,
        block_header: &BlockHeader,
        tip_header: &BlockHeader,
    ) -> Result<Vec<(EpochTime, Difficulty)>, ValidationError>
    {
        let block_window = self.rules.consensus_constants().get_difficulty_block_window();
        let start_height = tip_header.height.saturating_sub(block_window);
        if start_height == tip_header.height {
            return Ok(vec![]);
        }

        trace!(
            target: LOG_TARGET,
            "fetch_target_difficulties: tip height = {}, new header height = {}, block window = {}",
            tip_header.height,
            block_header.height,
            block_window
        );

        let block_window = block_window as usize;
        // TODO: create custom iterator for chunks that does not require a large number of u64s to exist in memory
        let heights = (0..=tip_header.height).rev().collect::<Vec<_>>();
        let mut target_difficulties = Vec::with_capacity(block_window);
        for block_nums in heights.chunks(block_window) {
            let headers = self
                .db
                .fetch_headers(block_nums.to_vec())
                .map_err(|err| ValidationError::CustomError(err.to_string()))?;

            let max_remaining = block_window.saturating_sub(target_difficulties.len());
            trace!(
                target: LOG_TARGET,
                "fetch_target_difficulties: max_remaining = {}",
                max_remaining
            );
            target_difficulties.extend(
                headers
                    .into_iter()
                    .filter(|h| h.pow.pow_algo == block_header.pow.pow_algo)
                    .take(max_remaining)
                    .map(|h| (h.timestamp, h.pow.target_difficulty)),
            );

            assert!(
                target_difficulties.len() <= block_window,
                "target_difficulties can never contain more elements than the block window"
            );
            if target_difficulties.len() == block_window {
                break;
            }
        }

        trace!(
            target: LOG_TARGET,
            "fetch_target_difficulties: #returned = {}",
            target_difficulties.len()
        );
        Ok(target_difficulties.into_iter().rev().collect())
    }
}

pub struct HorizonFinalStateValidator<B> {
    rules: ConsensusManager,
    db: BlockchainDatabase<B>,
    factories: CryptoFactories,
}

impl<B: BlockchainBackend> HorizonFinalStateValidator<B> {
    pub fn new(db: BlockchainDatabase<B>, rules: ConsensusManager, factories: CryptoFactories) -> Self {
        Self { db, rules, factories }
    }
}

impl<B: BlockchainBackend> StatelessValidation<u64> for HorizonFinalStateValidator<B> {
    fn validate(&self, horizon_height: &u64) -> Result<(), ValidationError> {
        let total_offset = self.fetch_total_offset_commitment(*horizon_height)?;
        let utxo_commitment = self.fetch_aggregate_utxo_commitment()?;
        let emission_h = self.get_emission_commitment_at(*horizon_height);
        let kernel_excess = self.fetch_aggregate_kernel_excess()?;

        // Validate: ∑UTXO_i ?= Emission.H + ∑KERNEL_EXCESS_i.G + ∑OFFSET_i
        if utxo_commitment != &(&emission_h + &kernel_excess) + &total_offset {
            return Err(ValidationError::custom_error(format!(
                "Final state validation failed: The UTXO commitment did not equal the expected emission at height {}",
                horizon_height
            )));
        }

        Ok(())
    }
}

impl<B: BlockchainBackend> HorizonFinalStateValidator<B> {
    fn fetch_total_offset_commitment(&self, height: u64) -> Result<Commitment, ValidationError> {
        let header_iter = HeaderIter::new(&self.db, height, 100);
        let mut total_offset = BlindingFactor::default();
        let mut total = 0;
        for header in header_iter {
            total += 1;
            let header = header.map_err(ValidationError::custom_error)?;
            total_offset = total_offset + header.total_kernel_offset;
        }
        trace!(target: LOG_TARGET, "Fetched {} headers", total);
        let offset_commitment = self.factories.commitment.commit(&total_offset, &0u64.into());
        Ok(offset_commitment)
    }

    fn fetch_aggregate_utxo_commitment(&self) -> Result<Commitment, ValidationError> {
        let utxos = self.db.fetch_all_utxos().map_err(ValidationError::custom_error)?;
        trace!(target: LOG_TARGET, "Fetched {} UTXOs", utxos.len());
        Ok(utxos.into_iter().map(|utxo| utxo.commitment).sum())
    }

    fn get_emission_commitment_at(&self, height: u64) -> Commitment {
        let emission = self.rules.emission_schedule().supply_at_block(height);
        trace!(
            target: LOG_TARGET,
            "Expected emission at height {} is {}",
            height,
            emission
        );
        self.factories
            .commitment
            .commit_value(&RistrettoSecretKey::default(), emission.into())
    }

    fn fetch_aggregate_kernel_excess(&self) -> Result<Commitment, ValidationError> {
        let kernels = self.db.fetch_all_kernels().map_err(ValidationError::custom_error)?;
        trace!(target: LOG_TARGET, "Fetched {} kernels", kernels.len());
        Ok(kernels.into_iter().map(|k| k.excess).sum())
    }
}

// TODO: This is probably generally useful - figure out where to put this
/// Iterator for BlockHeaders. This iterator loads headers in chunks of size `chunk_size` for a low memory footprint and
/// emits them back one at a time
pub(super) struct HeaderIter<'a, B> {
    chunk: Vec<BlockHeader>,
    chunk_size: usize,
    cursor: usize,
    is_error: bool,
    max_height: u64,
    db: &'a BlockchainDatabase<B>,
}

impl<'a, B> HeaderIter<'a, B> {
    pub fn new(db: &'a BlockchainDatabase<B>, max_height: u64, chunk_size: usize) -> Self {
        Self {
            db,
            chunk_size,
            cursor: 0,
            is_error: false,
            max_height,
            chunk: Vec::with_capacity(chunk_size),
        }
    }

    fn next_chunk(&self) -> Vec<u64> {
        let upper_bound = cmp::min(self.cursor + self.chunk_size, self.max_height as usize);
        (self.cursor..=upper_bound).map(|n| n as u64).collect()
    }
}

impl<B: BlockchainBackend> Iterator for HeaderIter<'_, B> {
    type Item = Result<BlockHeader, ChainStorageError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.is_error {
            return None;
        }

        if self.chunk.is_empty() {
            let block_nums = self.next_chunk();
            // We're done: No more block headers to fetch
            if block_nums.is_empty() {
                return None;
            }

            match self.db.fetch_headers(block_nums) {
                Ok(headers) => {
                    self.cursor += headers.len();
                    self.chunk.extend(headers);
                },
                Err(err) => {
                    // On the next call, the iterator will end
                    self.is_error = true;
                    return Some(Err(err));
                },
            }
        }

        Some(Ok(self.chunk.remove(0)))
    }
}
