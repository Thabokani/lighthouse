use crate::{
    doppelganger_service::DoppelgangerService,
    http_metrics::metrics,
    initialized_validators::InitializedValidators,
    signing_method::{Error as SigningError, SignableMessage, SigningContext, SigningMethod},
    Config,
};
use account_utils::validator_definitions::{PasswordStorage, ValidatorDefinition};
use parking_lot::{Mutex, RwLock};
use slashing_protection::{
    interchange::Interchange, InterchangeError, NotSafe, Safe, SlashingDatabase,
};
use slog::{crit, error, info, warn, Logger};
use slot_clock::SlotClock;
use std::iter::FromIterator;
use std::marker::PhantomData;
use std::path::Path;
use std::sync::Arc;
use task_executor::TaskExecutor;
use types::{
    attestation::Error as AttestationError, graffiti::GraffitiString, AbstractExecPayload, Address,
    AggregateAndProof, Attestation, BeaconBlock, BlindedPayload, ChainSpec, ContributionAndProof,
    Domain, Epoch, EthSpec, Fork, ForkName, Graffiti, Hash256, Keypair, PublicKeyBytes,
    SelectionProof, Signature, SignedAggregateAndProof, SignedBeaconBlock,
    SignedContributionAndProof, SignedRoot, SignedValidatorRegistrationData, SignedVoluntaryExit,
    Slot, SyncAggregatorSelectionData, SyncCommitteeContribution, SyncCommitteeMessage,
    SyncSelectionProof, SyncSubnetId, ValidatorRegistrationData, VoluntaryExit,
};
use validator_dir::ValidatorDir;

pub use crate::doppelganger_service::DoppelgangerStatus;
use crate::preparation_service::ProposalData;

#[derive(Debug, PartialEq)]
pub enum Error {
    DoppelgangerProtected(PublicKeyBytes),
    UnknownToDoppelgangerService(PublicKeyBytes),
    UnknownPubkey(PublicKeyBytes),
    Slashable(NotSafe),
    SameData,
    GreaterThanCurrentSlot { slot: Slot, current_slot: Slot },
    GreaterThanCurrentEpoch { epoch: Epoch, current_epoch: Epoch },
    UnableToSignAttestation(AttestationError),
    UnableToSign(SigningError),
}

impl From<SigningError> for Error {
    fn from(e: SigningError) -> Self {
        Error::UnableToSign(e)
    }
}

/// Number of epochs of slashing protection history to keep.
///
/// This acts as a maximum safe-guard against clock drift.
const SLASHING_PROTECTION_HISTORY_EPOCHS: u64 = 512;

/// Currently used as the default gas limit in execution clients.
///
/// https://github.com/ethereum/builder-specs/issues/17
pub const DEFAULT_GAS_LIMIT: u64 = 30_000_000;

struct LocalValidator {
    validator_dir: ValidatorDir,
    voting_keypair: Keypair,
}

/// We derive our own `PartialEq` to avoid doing equality checks between secret keys.
///
/// It's nice to avoid secret key comparisons from a security perspective, but it's also a little
/// risky when it comes to `HashMap` integrity (that's why we need `PartialEq`).
///
/// Currently, we obtain keypairs from keystores where we derive the `PublicKey` from a `SecretKey`
/// via a hash function. In order to have two equal `PublicKey` with different `SecretKey` we would
/// need to have either:
///
/// - A serious upstream integrity error.
/// - A hash collision.
///
/// It seems reasonable to make these two assumptions in order to avoid the equality checks.
impl PartialEq for LocalValidator {
    fn eq(&self, other: &Self) -> bool {
        self.validator_dir == other.validator_dir
            && self.voting_keypair.pk == other.voting_keypair.pk
    }
}

pub struct ValidatorStore<T, E: EthSpec> {
    validators: Arc<RwLock<InitializedValidators>>,
    slashing_protection: SlashingDatabase,
    slashing_protection_last_prune: Arc<Mutex<Epoch>>,
    genesis_validators_root: Hash256,
    spec: Arc<ChainSpec>,
    log: Logger,
    doppelganger_service: Option<Arc<DoppelgangerService>>,
    slot_clock: T,
    fee_recipient_process: Option<Address>,
    gas_limit: Option<u64>,
    builder_proposals: bool,
    produce_block_v3: bool,
    prefer_builder_proposals: bool,
    builder_boost_factor: Option<u64>,
    task_executor: TaskExecutor,
    _phantom: PhantomData<E>,
}

impl<T: SlotClock + 'static, E: EthSpec> ValidatorStore<T, E> {
    // All arguments are different types. Making the fields `pub` is undesired. A builder seems
    // unnecessary.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        validators: InitializedValidators,
        slashing_protection: SlashingDatabase,
        genesis_validators_root: Hash256,
        spec: ChainSpec,
        doppelganger_service: Option<Arc<DoppelgangerService>>,
        slot_clock: T,
        config: &Config,
        task_executor: TaskExecutor,
        log: Logger,
    ) -> Self {
        Self {
            validators: Arc::new(RwLock::new(validators)),
            slashing_protection,
            slashing_protection_last_prune: Arc::new(Mutex::new(Epoch::new(0))),
            genesis_validators_root,
            spec: Arc::new(spec),
            log,
            doppelganger_service,
            slot_clock,
            fee_recipient_process: config.fee_recipient,
            gas_limit: config.gas_limit,
            builder_proposals: config.builder_proposals,
            produce_block_v3: config.produce_block_v3,
            prefer_builder_proposals: config.prefer_builder_proposals,
            builder_boost_factor: config.builder_boost_factor,
            task_executor,
            _phantom: PhantomData,
        }
    }

    /// Register all local validators in doppelganger protection to try and prevent instances of
    /// duplicate validators operating on the network at the same time.
    ///
    /// This function has no effect if doppelganger protection is disabled.
    pub fn register_all_in_doppelganger_protection_if_enabled(&self) -> Result<(), String> {
        if let Some(doppelganger_service) = &self.doppelganger_service {
            for pubkey in self.validators.read().iter_voting_pubkeys() {
                doppelganger_service.register_new_validator::<E, _>(*pubkey, &self.slot_clock)?
            }
        }

        Ok(())
    }

    /// Returns `true` if doppelganger protection is enabled, or else `false`.
    pub fn doppelganger_protection_enabled(&self) -> bool {
        self.doppelganger_service.is_some()
    }

    pub fn initialized_validators(&self) -> Arc<RwLock<InitializedValidators>> {
        self.validators.clone()
    }

    /// Indicates if the `voting_public_key` exists in self and is enabled.
    pub fn has_validator(&self, voting_public_key: &PublicKeyBytes) -> bool {
        self.validators
            .read()
            .validator(voting_public_key)
            .is_some()
    }

    /// Insert a new validator to `self`, where the validator is represented by an EIP-2335
    /// keystore on the filesystem.
    #[allow(clippy::too_many_arguments)]
    pub async fn add_validator_keystore<P: AsRef<Path>>(
        &self,
        voting_keystore_path: P,
        password_storage: PasswordStorage,
        enable: bool,
        graffiti: Option<GraffitiString>,
        suggested_fee_recipient: Option<Address>,
        gas_limit: Option<u64>,
        builder_proposals: Option<bool>,
        builder_boost_factor: Option<u64>,
        prefer_builder_proposals: Option<bool>,
    ) -> Result<ValidatorDefinition, String> {
        let mut validator_def = ValidatorDefinition::new_keystore_with_password(
            voting_keystore_path,
            password_storage,
            graffiti.map(Into::into),
            suggested_fee_recipient,
            gas_limit,
            builder_proposals,
            builder_boost_factor,
            prefer_builder_proposals,
        )
        .map_err(|e| format!("failed to create validator definitions: {:?}", e))?;

        validator_def.enabled = enable;

        self.add_validator(validator_def).await
    }

    /// Insert a new validator to `self`.
    ///
    /// This function includes:
    ///
    /// - Adding the validator definition to the YAML file, saving it to the filesystem.
    /// - Enabling the validator with the slashing protection database.
    /// - If `enable == true`, starting to perform duties for the validator.
    // FIXME: ignore this clippy lint until the validator store is refactored to use async locks
    #[allow(clippy::await_holding_lock)]
    pub async fn add_validator(
        &self,
        validator_def: ValidatorDefinition,
    ) -> Result<ValidatorDefinition, String> {
        let validator_pubkey = validator_def.voting_public_key.compress();

        self.slashing_protection
            .register_validator(validator_pubkey)
            .map_err(|e| format!("failed to register validator: {:?}", e))?;

        if let Some(doppelganger_service) = &self.doppelganger_service {
            doppelganger_service
                .register_new_validator::<E, _>(validator_pubkey, &self.slot_clock)?;
        }

        self.validators
            .write()
            .add_definition_replace_disabled(validator_def.clone())
            .await
            .map_err(|e| format!("Unable to add definition: {:?}", e))?;

        Ok(validator_def)
    }

    /// Returns `ProposalData` for the provided `pubkey` if it exists in `InitializedValidators`.
    /// `ProposalData` fields include defaulting logic described in `get_fee_recipient_defaulting`,
    /// `get_gas_limit_defaulting`, and `get_builder_proposals_defaulting`.
    pub fn proposal_data(&self, pubkey: &PublicKeyBytes) -> Option<ProposalData> {
        self.validators
            .read()
            .validator(pubkey)
            .map(|validator| ProposalData {
                validator_index: validator.get_index(),
                fee_recipient: self
                    .get_fee_recipient_defaulting(validator.get_suggested_fee_recipient()),
                gas_limit: self.get_gas_limit_defaulting(validator.get_gas_limit()),
                builder_proposals: self
                    .get_builder_proposals_defaulting(validator.get_builder_proposals()),
            })
    }

    /// Attempts to resolve the pubkey to a validator index.
    ///
    /// It may return `None` if the `pubkey` is:
    ///
    /// - Unknown.
    /// - Known, but with an unknown index.
    pub fn validator_index(&self, pubkey: &PublicKeyBytes) -> Option<u64> {
        self.validators.read().get_index(pubkey)
    }

    /// Returns all voting pubkeys for all enabled validators.
    ///
    /// The `filter_func` allows for filtering pubkeys based upon their `DoppelgangerStatus`. There
    /// are two primary functions used here:
    ///
    /// - `DoppelgangerStatus::only_safe`: only returns pubkeys which have passed doppelganger
    ///     protection and are safe-enough to sign messages.
    /// - `DoppelgangerStatus::ignored`: returns all the pubkeys from `only_safe` *plus* those still
    ///     undergoing protection. This is useful for collecting duties or other non-signing tasks.
    #[allow(clippy::needless_collect)] // Collect is required to avoid holding a lock.
    pub fn voting_pubkeys<I, F>(&self, filter_func: F) -> I
    where
        I: FromIterator<PublicKeyBytes>,
        F: Fn(DoppelgangerStatus) -> Option<PublicKeyBytes>,
    {
        // Collect all the pubkeys first to avoid interleaving locks on `self.validators` and
        // `self.doppelganger_service()`.
        let pubkeys = self
            .validators
            .read()
            .iter_voting_pubkeys()
            .cloned()
            .collect::<Vec<_>>();

        pubkeys
            .into_iter()
            .map(|pubkey| {
                self.doppelganger_service
                    .as_ref()
                    .map(|doppelganger_service| doppelganger_service.validator_status(pubkey))
                    // Allow signing on all pubkeys if doppelganger protection is disabled.
                    .unwrap_or_else(|| DoppelgangerStatus::SigningEnabled(pubkey))
            })
            .filter_map(filter_func)
            .collect()
    }

    /// Returns doppelganger statuses for all enabled validators.
    #[allow(clippy::needless_collect)] // Collect is required to avoid holding a lock.
    pub fn doppelganger_statuses(&self) -> Vec<DoppelgangerStatus> {
        // Collect all the pubkeys first to avoid interleaving locks on `self.validators` and
        // `self.doppelganger_service`.
        let pubkeys = self
            .validators
            .read()
            .iter_voting_pubkeys()
            .cloned()
            .collect::<Vec<_>>();

        pubkeys
            .into_iter()
            .map(|pubkey| {
                self.doppelganger_service
                    .as_ref()
                    .map(|doppelganger_service| doppelganger_service.validator_status(pubkey))
                    // Allow signing on all pubkeys if doppelganger protection is disabled.
                    .unwrap_or_else(|| DoppelgangerStatus::SigningEnabled(pubkey))
            })
            .collect()
    }

    /// Check if the `validator_pubkey` is permitted by the doppleganger protection to sign
    /// messages.
    pub fn doppelganger_protection_allows_signing(&self, validator_pubkey: PublicKeyBytes) -> bool {
        self.doppelganger_service
            .as_ref()
            // If there's no doppelganger service then we assume it is purposefully disabled and
            // declare that all keys are safe with regard to it.
            .map_or(true, |doppelganger_service| {
                doppelganger_service
                    .validator_status(validator_pubkey)
                    .only_safe()
                    .is_some()
            })
    }

    pub fn num_voting_validators(&self) -> usize {
        self.validators.read().num_enabled()
    }

    fn fork(&self, epoch: Epoch) -> Fork {
        self.spec.fork_at_epoch(epoch)
    }

    pub fn produce_block_v3(&self) -> bool {
        self.produce_block_v3
    }

    /// Returns a `SigningMethod` for `validator_pubkey` *only if* that validator is considered safe
    /// by doppelganger protection.
    fn doppelganger_checked_signing_method(
        &self,
        validator_pubkey: PublicKeyBytes,
    ) -> Result<Arc<SigningMethod>, Error> {
        if self.doppelganger_protection_allows_signing(validator_pubkey) {
            self.validators
                .read()
                .signing_method(&validator_pubkey)
                .ok_or(Error::UnknownPubkey(validator_pubkey))
        } else {
            Err(Error::DoppelgangerProtected(validator_pubkey))
        }
    }

    /// Returns a `SigningMethod` for `validator_pubkey` regardless of that validators doppelganger
    /// protection status.
    ///
    /// ## Warning
    ///
    /// This method should only be used for signing non-slashable messages.
    fn doppelganger_bypassed_signing_method(
        &self,
        validator_pubkey: PublicKeyBytes,
    ) -> Result<Arc<SigningMethod>, Error> {
        self.validators
            .read()
            .signing_method(&validator_pubkey)
            .ok_or(Error::UnknownPubkey(validator_pubkey))
    }

    fn signing_context(&self, domain: Domain, signing_epoch: Epoch) -> SigningContext {
        if domain == Domain::VoluntaryExit {
            match self.spec.fork_name_at_epoch(signing_epoch) {
                ForkName::Base | ForkName::Altair | ForkName::Merge | ForkName::Capella => {
                    SigningContext {
                        domain,
                        epoch: signing_epoch,
                        fork: self.fork(signing_epoch),
                        genesis_validators_root: self.genesis_validators_root,
                    }
                }
                // EIP-7044
                ForkName::Deneb => SigningContext {
                    domain,
                    epoch: signing_epoch,
                    fork: Fork {
                        previous_version: self.spec.capella_fork_version,
                        current_version: self.spec.capella_fork_version,
                        epoch: signing_epoch,
                    },
                    genesis_validators_root: self.genesis_validators_root,
                },
            }
        } else {
            SigningContext {
                domain,
                epoch: signing_epoch,
                fork: self.fork(signing_epoch),
                genesis_validators_root: self.genesis_validators_root,
            }
        }
    }

    pub async fn randao_reveal(
        &self,
        validator_pubkey: PublicKeyBytes,
        signing_epoch: Epoch,
    ) -> Result<Signature, Error> {
        let signing_method = self.doppelganger_checked_signing_method(validator_pubkey)?;
        let signing_context = self.signing_context(Domain::Randao, signing_epoch);

        let signature = signing_method
            .get_signature::<E, BlindedPayload<E>>(
                SignableMessage::RandaoReveal(signing_epoch),
                signing_context,
                &self.spec,
                &self.task_executor,
            )
            .await?;

        Ok(signature)
    }

    pub fn graffiti(&self, validator_pubkey: &PublicKeyBytes) -> Option<Graffiti> {
        self.validators.read().graffiti(validator_pubkey)
    }

    /// Returns the fee recipient for the given public key. The priority order for fetching
    /// the fee recipient is:
    /// 1. validator_definitions.yml
    /// 2. process level fee recipient
    pub fn get_fee_recipient(&self, validator_pubkey: &PublicKeyBytes) -> Option<Address> {
        // If there is a `suggested_fee_recipient` in the validator definitions yaml
        // file, use that value.
        self.get_fee_recipient_defaulting(self.suggested_fee_recipient(validator_pubkey))
    }

    pub fn get_fee_recipient_defaulting(&self, fee_recipient: Option<Address>) -> Option<Address> {
        // If there's nothing in the file, try the process-level default value.
        fee_recipient.or(self.fee_recipient_process)
    }

    /// Returns the suggested_fee_recipient from `validator_definitions.yml` if any.
    /// This has been pulled into a private function so the read lock is dropped easily
    fn suggested_fee_recipient(&self, validator_pubkey: &PublicKeyBytes) -> Option<Address> {
        self.validators
            .read()
            .suggested_fee_recipient(validator_pubkey)
    }

    /// Returns the gas limit for the given public key. The priority order for fetching
    /// the gas limit is:
    ///
    /// 1. validator_definitions.yml
    /// 2. process level gas limit
    /// 3. `DEFAULT_GAS_LIMIT`
    pub fn get_gas_limit(&self, validator_pubkey: &PublicKeyBytes) -> u64 {
        self.get_gas_limit_defaulting(self.validators.read().gas_limit(validator_pubkey))
    }

    fn get_gas_limit_defaulting(&self, gas_limit: Option<u64>) -> u64 {
        // If there is a `gas_limit` in the validator definitions yaml
        // file, use that value.
        gas_limit
            // If there's nothing in the file, try the process-level default value.
            .or(self.gas_limit)
            // If there's no process-level default, use the `DEFAULT_GAS_LIMIT`.
            .unwrap_or(DEFAULT_GAS_LIMIT)
    }

    /// Returns a `bool` for the given public key that denotes whether this validator should use the
    /// builder API. The priority order for fetching this value is:
    ///
    /// 1. validator_definitions.yml
    /// 2. process level flag
    pub fn get_builder_proposals(&self, validator_pubkey: &PublicKeyBytes) -> bool {
        // If there is a `suggested_fee_recipient` in the validator definitions yaml
        // file, use that value.
        self.get_builder_proposals_defaulting(
            self.validators.read().builder_proposals(validator_pubkey),
        )
    }

    /// Returns a `u64` for the given public key that denotes the builder boost factor. The priority order for fetching this value is:
    ///
    /// 1. validator_definitions.yml
    /// 2. process level flag
    pub fn get_builder_boost_factor(&self, validator_pubkey: &PublicKeyBytes) -> Option<u64> {
        self.validators
            .read()
            .builder_boost_factor(validator_pubkey)
            .or(self.builder_boost_factor)
    }

    /// Returns a `bool` for the given public key that denotes whether this validator should prefer a
    /// builder payload. The priority order for fetching this value is:
    ///
    /// 1. validator_definitions.yml
    /// 2. process level flag
    pub fn get_prefer_builder_proposals(&self, validator_pubkey: &PublicKeyBytes) -> bool {
        self.validators
            .read()
            .prefer_builder_proposals(validator_pubkey)
            .unwrap_or(self.prefer_builder_proposals)
    }

    fn get_builder_proposals_defaulting(&self, builder_proposals: Option<bool>) -> bool {
        builder_proposals
            // If there's nothing in the file, try the process-level default value.
            .unwrap_or(self.builder_proposals)
    }

    /// Translate the per validator `builder_proposals`, `builder_boost_factor` and
    /// `prefer_builder_proposals` to a boost factor, if available.
    /// - If `prefer_builder_proposals` is true, set boost factor to `u64::MAX` to indicate a
    /// preference for builder payloads.
    /// - If `builder_boost_factor` is a value other than None, return its value as the boost factor.
    /// - If `builder_proposals` is set to false, set boost factor to 0 to indicate a preference for
    ///   local payloads.
    /// - Else return `None` to indicate no preference between builder and local payloads.
    pub fn determine_validator_builder_boost_factor(
        &self,
        validator_pubkey: &PublicKeyBytes,
    ) -> Option<u64> {
        let validator_prefer_builder_proposals = self
            .validators
            .read()
            .prefer_builder_proposals(validator_pubkey);

        if matches!(validator_prefer_builder_proposals, Some(true)) {
            return Some(u64::MAX);
        }

        self.validators
            .read()
            .builder_boost_factor(validator_pubkey)
            .or_else(|| {
                if matches!(
                    self.validators.read().builder_proposals(validator_pubkey),
                    Some(false)
                ) {
                    return Some(0);
                }
                None
            })
    }

    /// Translate the process-wide `builder_proposals`, `builder_boost_factor` and
    /// `prefer_builder_proposals` configurations to a boost factor.
    /// - If `prefer_builder_proposals` is true, set boost factor to `u64::MAX` to indicate a
    ///   preference for builder payloads.
    /// - If `builder_boost_factor` is a value other than None, return its value as the boost factor.
    /// - If `builder_proposals` is set to false, set boost factor to 0 to indicate a preference for
    ///   local payloads.
    /// - Else return `None` to indicate no preference between builder and local payloads.
    pub fn determine_default_builder_boost_factor(&self) -> Option<u64> {
        if self.prefer_builder_proposals {
            return Some(u64::MAX);
        }
        self.builder_boost_factor.or({
            if self.builder_proposals {
                Some(0)
            } else {
                None
            }
        })
    }

    pub async fn sign_block<Payload: AbstractExecPayload<E>>(
        &self,
        validator_pubkey: PublicKeyBytes,
        block: BeaconBlock<E, Payload>,
        current_slot: Slot,
    ) -> Result<SignedBeaconBlock<E, Payload>, Error> {
        // Make sure the block slot is not higher than the current slot to avoid potential attacks.
        if block.slot() > current_slot {
            warn!(
                self.log,
                "Not signing block with slot greater than current slot";
                "block_slot" => block.slot().as_u64(),
                "current_slot" => current_slot.as_u64()
            );
            return Err(Error::GreaterThanCurrentSlot {
                slot: block.slot(),
                current_slot,
            });
        }

        let signing_epoch = block.epoch();
        let signing_context = self.signing_context(Domain::BeaconProposer, signing_epoch);
        let domain_hash = signing_context.domain_hash(&self.spec);

        // Check for slashing conditions.
        let slashing_status = self.slashing_protection.check_and_insert_block_proposal(
            &validator_pubkey,
            &block.block_header(),
            domain_hash,
        );

        match slashing_status {
            // We can safely sign this block without slashing.
            Ok(Safe::Valid) => {
                metrics::inc_counter_vec(&metrics::SIGNED_BLOCKS_TOTAL, &[metrics::SUCCESS]);

                let signing_method = self.doppelganger_checked_signing_method(validator_pubkey)?;
                let signature = signing_method
                    .get_signature::<E, Payload>(
                        SignableMessage::BeaconBlock(&block),
                        signing_context,
                        &self.spec,
                        &self.task_executor,
                    )
                    .await?;
                Ok(SignedBeaconBlock::from_block(block, signature))
            }
            Ok(Safe::SameData) => {
                warn!(
                    self.log,
                    "Skipping signing of previously signed block";
                );
                metrics::inc_counter_vec(&metrics::SIGNED_BLOCKS_TOTAL, &[metrics::SAME_DATA]);
                Err(Error::SameData)
            }
            Err(NotSafe::UnregisteredValidator(pk)) => {
                warn!(
                    self.log,
                    "Not signing block for unregistered validator";
                    "msg" => "Carefully consider running with --init-slashing-protection (see --help)",
                    "public_key" => format!("{:?}", pk)
                );
                metrics::inc_counter_vec(&metrics::SIGNED_BLOCKS_TOTAL, &[metrics::UNREGISTERED]);
                Err(Error::Slashable(NotSafe::UnregisteredValidator(pk)))
            }
            Err(e) => {
                crit!(
                    self.log,
                    "Not signing slashable block";
                    "error" => format!("{:?}", e)
                );
                metrics::inc_counter_vec(&metrics::SIGNED_BLOCKS_TOTAL, &[metrics::SLASHABLE]);
                Err(Error::Slashable(e))
            }
        }
    }

    pub async fn sign_attestation(
        &self,
        validator_pubkey: PublicKeyBytes,
        validator_committee_position: usize,
        attestation: &mut Attestation<E>,
        current_epoch: Epoch,
    ) -> Result<(), Error> {
        // Make sure the target epoch is not higher than the current epoch to avoid potential attacks.
        if attestation.data.target.epoch > current_epoch {
            return Err(Error::GreaterThanCurrentEpoch {
                epoch: attestation.data.target.epoch,
                current_epoch,
            });
        }

        // Checking for slashing conditions.
        let signing_epoch = attestation.data.target.epoch;
        let signing_context = self.signing_context(Domain::BeaconAttester, signing_epoch);
        let domain_hash = signing_context.domain_hash(&self.spec);
        let slashing_status = self.slashing_protection.check_and_insert_attestation(
            &validator_pubkey,
            &attestation.data,
            domain_hash,
        );

        match slashing_status {
            // We can safely sign this attestation.
            Ok(Safe::Valid) => {
                let signing_method = self.doppelganger_checked_signing_method(validator_pubkey)?;
                let signature = signing_method
                    .get_signature::<E, BlindedPayload<E>>(
                        SignableMessage::AttestationData(&attestation.data),
                        signing_context,
                        &self.spec,
                        &self.task_executor,
                    )
                    .await?;
                attestation
                    .add_signature(&signature, validator_committee_position)
                    .map_err(Error::UnableToSignAttestation)?;

                metrics::inc_counter_vec(&metrics::SIGNED_ATTESTATIONS_TOTAL, &[metrics::SUCCESS]);

                Ok(())
            }
            Ok(Safe::SameData) => {
                warn!(
                    self.log,
                    "Skipping signing of previously signed attestation"
                );
                metrics::inc_counter_vec(
                    &metrics::SIGNED_ATTESTATIONS_TOTAL,
                    &[metrics::SAME_DATA],
                );
                Err(Error::SameData)
            }
            Err(NotSafe::UnregisteredValidator(pk)) => {
                warn!(
                    self.log,
                    "Not signing attestation for unregistered validator";
                    "msg" => "Carefully consider running with --init-slashing-protection (see --help)",
                    "public_key" => format!("{:?}", pk)
                );
                metrics::inc_counter_vec(
                    &metrics::SIGNED_ATTESTATIONS_TOTAL,
                    &[metrics::UNREGISTERED],
                );
                Err(Error::Slashable(NotSafe::UnregisteredValidator(pk)))
            }
            Err(e) => {
                crit!(
                    self.log,
                    "Not signing slashable attestation";
                    "attestation" => format!("{:?}", attestation.data),
                    "error" => format!("{:?}", e)
                );
                metrics::inc_counter_vec(
                    &metrics::SIGNED_ATTESTATIONS_TOTAL,
                    &[metrics::SLASHABLE],
                );
                Err(Error::Slashable(e))
            }
        }
    }

    pub async fn sign_voluntary_exit(
        &self,
        validator_pubkey: PublicKeyBytes,
        voluntary_exit: VoluntaryExit,
    ) -> Result<SignedVoluntaryExit, Error> {
        let signing_epoch = voluntary_exit.epoch;
        let signing_context = self.signing_context(Domain::VoluntaryExit, signing_epoch);
        let signing_method = self.doppelganger_bypassed_signing_method(validator_pubkey)?;

        let signature = signing_method
            .get_signature::<E, BlindedPayload<E>>(
                SignableMessage::VoluntaryExit(&voluntary_exit),
                signing_context,
                &self.spec,
                &self.task_executor,
            )
            .await?;

        metrics::inc_counter_vec(&metrics::SIGNED_VOLUNTARY_EXITS_TOTAL, &[metrics::SUCCESS]);

        Ok(SignedVoluntaryExit {
            message: voluntary_exit,
            signature,
        })
    }

    pub async fn sign_validator_registration_data(
        &self,
        validator_registration_data: ValidatorRegistrationData,
    ) -> Result<SignedValidatorRegistrationData, Error> {
        let domain_hash = self.spec.get_builder_domain();
        let signing_root = validator_registration_data.signing_root(domain_hash);

        let signing_method =
            self.doppelganger_bypassed_signing_method(validator_registration_data.pubkey)?;
        let signature = signing_method
            .get_signature_from_root::<E, BlindedPayload<E>>(
                SignableMessage::ValidatorRegistration(&validator_registration_data),
                signing_root,
                &self.task_executor,
                None,
            )
            .await?;

        metrics::inc_counter_vec(
            &metrics::SIGNED_VALIDATOR_REGISTRATIONS_TOTAL,
            &[metrics::SUCCESS],
        );

        Ok(SignedValidatorRegistrationData {
            message: validator_registration_data,
            signature,
        })
    }

    /// Signs an `AggregateAndProof` for a given validator.
    ///
    /// The resulting `SignedAggregateAndProof` is sent on the aggregation channel and cannot be
    /// modified by actors other than the signing validator.
    pub async fn produce_signed_aggregate_and_proof(
        &self,
        validator_pubkey: PublicKeyBytes,
        aggregator_index: u64,
        aggregate: Attestation<E>,
        selection_proof: SelectionProof,
    ) -> Result<SignedAggregateAndProof<E>, Error> {
        let signing_epoch = aggregate.data.target.epoch;
        let signing_context = self.signing_context(Domain::AggregateAndProof, signing_epoch);

        let message = AggregateAndProof {
            aggregator_index,
            aggregate,
            selection_proof: selection_proof.into(),
        };

        let signing_method = self.doppelganger_checked_signing_method(validator_pubkey)?;
        let signature = signing_method
            .get_signature::<E, BlindedPayload<E>>(
                SignableMessage::SignedAggregateAndProof(&message),
                signing_context,
                &self.spec,
                &self.task_executor,
            )
            .await?;

        metrics::inc_counter_vec(&metrics::SIGNED_AGGREGATES_TOTAL, &[metrics::SUCCESS]);

        Ok(SignedAggregateAndProof { message, signature })
    }

    /// Produces a `SelectionProof` for the `slot`, signed by with corresponding secret key to
    /// `validator_pubkey`.
    pub async fn produce_selection_proof(
        &self,
        validator_pubkey: PublicKeyBytes,
        slot: Slot,
    ) -> Result<SelectionProof, Error> {
        let signing_epoch = slot.epoch(E::slots_per_epoch());
        let signing_context = self.signing_context(Domain::SelectionProof, signing_epoch);

        // Bypass the `with_validator_signing_method` function.
        //
        // This is because we don't care about doppelganger protection when it comes to selection
        // proofs. They are not slashable and we need them to subscribe to subnets on the BN.
        //
        // As long as we disallow `SignedAggregateAndProof` then these selection proofs will never
        // be published on the network.
        let signing_method = self.doppelganger_bypassed_signing_method(validator_pubkey)?;

        let signature = signing_method
            .get_signature::<E, BlindedPayload<E>>(
                SignableMessage::SelectionProof(slot),
                signing_context,
                &self.spec,
                &self.task_executor,
            )
            .await
            .map_err(Error::UnableToSign)?;

        metrics::inc_counter_vec(&metrics::SIGNED_SELECTION_PROOFS_TOTAL, &[metrics::SUCCESS]);

        Ok(signature.into())
    }

    /// Produce a `SyncSelectionProof` for `slot` signed by the secret key of `validator_pubkey`.
    pub async fn produce_sync_selection_proof(
        &self,
        validator_pubkey: &PublicKeyBytes,
        slot: Slot,
        subnet_id: SyncSubnetId,
    ) -> Result<SyncSelectionProof, Error> {
        let signing_epoch = slot.epoch(E::slots_per_epoch());
        let signing_context =
            self.signing_context(Domain::SyncCommitteeSelectionProof, signing_epoch);

        // Bypass `with_validator_signing_method`: sync committee messages are not slashable.
        let signing_method = self.doppelganger_bypassed_signing_method(*validator_pubkey)?;

        metrics::inc_counter_vec(
            &metrics::SIGNED_SYNC_SELECTION_PROOFS_TOTAL,
            &[metrics::SUCCESS],
        );

        let message = SyncAggregatorSelectionData {
            slot,
            subcommittee_index: subnet_id.into(),
        };

        let signature = signing_method
            .get_signature::<E, BlindedPayload<E>>(
                SignableMessage::SyncSelectionProof(&message),
                signing_context,
                &self.spec,
                &self.task_executor,
            )
            .await
            .map_err(Error::UnableToSign)?;

        Ok(signature.into())
    }

    pub async fn produce_sync_committee_signature(
        &self,
        slot: Slot,
        beacon_block_root: Hash256,
        validator_index: u64,
        validator_pubkey: &PublicKeyBytes,
    ) -> Result<SyncCommitteeMessage, Error> {
        let signing_epoch = slot.epoch(E::slots_per_epoch());
        let signing_context = self.signing_context(Domain::SyncCommittee, signing_epoch);

        // Bypass `with_validator_signing_method`: sync committee messages are not slashable.
        let signing_method = self.doppelganger_bypassed_signing_method(*validator_pubkey)?;

        let signature = signing_method
            .get_signature::<E, BlindedPayload<E>>(
                SignableMessage::SyncCommitteeSignature {
                    beacon_block_root,
                    slot,
                },
                signing_context,
                &self.spec,
                &self.task_executor,
            )
            .await
            .map_err(Error::UnableToSign)?;

        metrics::inc_counter_vec(
            &metrics::SIGNED_SYNC_COMMITTEE_MESSAGES_TOTAL,
            &[metrics::SUCCESS],
        );

        Ok(SyncCommitteeMessage {
            slot,
            beacon_block_root,
            validator_index,
            signature,
        })
    }

    pub async fn produce_signed_contribution_and_proof(
        &self,
        aggregator_index: u64,
        aggregator_pubkey: PublicKeyBytes,
        contribution: SyncCommitteeContribution<E>,
        selection_proof: SyncSelectionProof,
    ) -> Result<SignedContributionAndProof<E>, Error> {
        let signing_epoch = contribution.slot.epoch(E::slots_per_epoch());
        let signing_context = self.signing_context(Domain::ContributionAndProof, signing_epoch);

        // Bypass `with_validator_signing_method`: sync committee messages are not slashable.
        let signing_method = self.doppelganger_bypassed_signing_method(aggregator_pubkey)?;

        let message = ContributionAndProof {
            aggregator_index,
            contribution,
            selection_proof: selection_proof.into(),
        };

        let signature = signing_method
            .get_signature::<E, BlindedPayload<E>>(
                SignableMessage::SignedContributionAndProof(&message),
                signing_context,
                &self.spec,
                &self.task_executor,
            )
            .await
            .map_err(Error::UnableToSign)?;

        metrics::inc_counter_vec(
            &metrics::SIGNED_SYNC_COMMITTEE_CONTRIBUTIONS_TOTAL,
            &[metrics::SUCCESS],
        );

        Ok(SignedContributionAndProof { message, signature })
    }

    pub fn import_slashing_protection(
        &self,
        interchange: Interchange,
    ) -> Result<(), InterchangeError> {
        self.slashing_protection
            .import_interchange_info(interchange, self.genesis_validators_root)?;
        Ok(())
    }

    /// Export slashing protection data while also disabling the given keys in the database.
    ///
    /// If any key is unknown to the slashing protection database it will be silently omitted
    /// from the result. It is the caller's responsibility to check whether all keys provided
    /// had data returned for them.
    pub fn export_slashing_protection_for_keys(
        &self,
        pubkeys: &[PublicKeyBytes],
    ) -> Result<Interchange, InterchangeError> {
        self.slashing_protection.with_transaction(|txn| {
            let known_pubkeys = pubkeys
                .iter()
                .filter_map(|pubkey| {
                    let validator_id = self
                        .slashing_protection
                        .get_validator_id_ignoring_status(txn, pubkey)
                        .ok()?;

                    Some(
                        self.slashing_protection
                            .update_validator_status(txn, validator_id, false)
                            .map(|()| *pubkey),
                    )
                })
                .collect::<Result<Vec<PublicKeyBytes>, _>>()?;
            self.slashing_protection.export_interchange_info_in_txn(
                self.genesis_validators_root,
                Some(&known_pubkeys),
                txn,
            )
        })
    }

    /// Prune the slashing protection database so that it remains performant.
    ///
    /// This function will only do actual pruning periodically, so it should usually be
    /// cheap to call. The `first_run` flag can be used to print a more verbose message when pruning
    /// runs.
    pub fn prune_slashing_protection_db(&self, current_epoch: Epoch, first_run: bool) {
        // Attempt to prune every SLASHING_PROTECTION_HISTORY_EPOCHs, with a tolerance for
        // missing the epoch that aligns exactly.
        let mut last_prune = self.slashing_protection_last_prune.lock();
        if current_epoch / SLASHING_PROTECTION_HISTORY_EPOCHS
            <= *last_prune / SLASHING_PROTECTION_HISTORY_EPOCHS
        {
            return;
        }

        if first_run {
            info!(
                self.log,
                "Pruning slashing protection DB";
                "epoch" => current_epoch,
                "msg" => "pruning may take several minutes the first time it runs"
            );
        } else {
            info!(self.log, "Pruning slashing protection DB"; "epoch" => current_epoch);
        }

        let _timer = metrics::start_timer(&metrics::SLASHING_PROTECTION_PRUNE_TIMES);

        let new_min_target_epoch = current_epoch.saturating_sub(SLASHING_PROTECTION_HISTORY_EPOCHS);
        let new_min_slot = new_min_target_epoch.start_slot(E::slots_per_epoch());

        let all_pubkeys: Vec<_> = self.voting_pubkeys(DoppelgangerStatus::ignored);

        if let Err(e) = self
            .slashing_protection
            .prune_all_signed_attestations(all_pubkeys.iter(), new_min_target_epoch)
        {
            error!(
                self.log,
                "Error during pruning of signed attestations";
                "error" => ?e,
            );
            return;
        }

        if let Err(e) = self
            .slashing_protection
            .prune_all_signed_blocks(all_pubkeys.iter(), new_min_slot)
        {
            error!(
                self.log,
                "Error during pruning of signed blocks";
                "error" => ?e,
            );
            return;
        }

        *last_prune = current_epoch;

        info!(self.log, "Completed pruning of slashing protection DB");
    }
}
