//! Engine payload validator implementation for Taiko execution payloads.
use alethia_reth_block::config::TaikoEvmConfig;
use alethia_reth_chainspec::{hardfork::TaikoHardforks, spec::TaikoChainSpec};
use alethia_reth_primitives::{
    engine::{TaikoEngineTypes, types::TaikoExecutionData},
    payload::attributes::TaikoPayloadAttributes,
    transaction::is_allowed_tx_type,
};
use alloy_consensus::{BlockHeader, proofs};
use alloy_eips::eip7685::EMPTY_REQUESTS_HASH;
use alloy_primitives::B256;
use alloy_rpc_types_engine::{ExecutionPayloadV1, PayloadError};
use alloy_rpc_types_eth::Withdrawals;
use reth::{chainspec::EthChainSpec, primitives::RecoveredBlock};
use reth_engine_primitives::EngineApiValidator;
use reth_engine_tree::tree::{TreeConfig, payload_validator::BasicEngineValidator};
use reth_ethereum::{Block, EthPrimitives};
use reth_evm::ConfigureEngineEvm;
use reth_node_api::{
    AddOnsContext, FullNodeComponents, NewPayloadError, NodeTypes, PayloadTypes, PayloadValidator,
};
use reth_node_builder::{
    invalid_block_hook::InvalidBlockHookExt,
    rpc::{ChangesetCache, EngineValidatorBuilder, PayloadValidatorBuilder},
};
use reth_payload_primitives::{
    EngineApiMessageVersion, EngineObjectValidationError, InvalidPayloadAttributesError,
    MessageValidationKind, PayloadAttributes, PayloadOrAttributes, validate_withdrawals_presence,
};
use reth_primitives_traits::{Block as BlockTrait, SealedBlock};
use std::sync::Arc;

/// Taiko-specific payload validation errors that do not map to an upstream Ethereum fork rule.
#[derive(Debug, thiserror::Error)]
enum TaikoPayloadValidationError {
    /// The payload contains blob transactions, which Taiko network never accepts.
    #[error("blob transactions are unsupported")]
    BlobTransactionsUnsupported,
    /// Unzen payloads must carry the original header difficulty through the Taiko sidecar.
    #[error("missing header difficulty for Unzen payload")]
    MissingUnzenHeaderDifficulty,
    /// Complete Taiko payloads must carry the Shanghai withdrawals list, including when empty.
    #[error("missing withdrawals for complete Taiko payload")]
    MissingWithdrawals,
    /// Legacy hash-only payloads must carry the committed withdrawals root in the sidecar.
    #[error("missing withdrawals hash for legacy Taiko payload")]
    MissingLegacyWithdrawalsHash,
}

/// Builder for [`TaikoEngineValidator`].
#[derive(Debug, Default, Clone)]
pub struct TaikoEngineValidatorBuilder;

impl<N> PayloadValidatorBuilder<N> for TaikoEngineValidatorBuilder
where
    N: FullNodeComponents<Evm = TaikoEvmConfig>,
    N::Types: NodeTypes<
            Primitives = EthPrimitives,
            ChainSpec = TaikoChainSpec,
            Payload = TaikoEngineTypes,
        >,
{
    /// The consensus implementation to build.
    type Validator = TaikoEngineValidator;

    /// Creates the engine validator.
    async fn build(self, ctx: &AddOnsContext<'_, N>) -> eyre::Result<Self::Validator> {
        Ok(TaikoEngineValidator::new(ctx.config.chain.clone()))
    }
}

impl<N> EngineValidatorBuilder<N> for TaikoEngineValidatorBuilder
where
    N: FullNodeComponents<Evm = TaikoEvmConfig>,
    N::Types: NodeTypes<
            Primitives = EthPrimitives,
            ChainSpec = TaikoChainSpec,
            Payload = TaikoEngineTypes,
        >,
    N::Evm: ConfigureEngineEvm<TaikoExecutionData>,
{
    /// The tree validator type that will be used by the consensus engine.
    type EngineValidator = BasicEngineValidator<N::Provider, N::Evm, TaikoEngineValidator>;

    /// Builds the tree validator for the consensus engine.
    async fn build_tree_validator(
        self,
        ctx: &AddOnsContext<'_, N>,
        tree_config: TreeConfig,
        changeset_cache: ChangesetCache,
    ) -> eyre::Result<Self::EngineValidator> {
        let validator = <Self as PayloadValidatorBuilder<N>>::build(self, ctx).await?;
        let data_dir = ctx.config.datadir.clone().resolve_datadir(ctx.config.chain.chain());
        let invalid_block_hook = ctx.create_invalid_block_hook(&data_dir).await?;
        Ok(BasicEngineValidator::new(
            ctx.node.provider().clone(),
            Arc::new(ctx.node.consensus().clone()),
            ctx.node.evm_config().clone(),
            validator,
            tree_config,
            invalid_block_hook,
            changeset_cache,
            ctx.node.task_executor().clone(),
        ))
    }
}

/// Validator for the Taiko engine API.
#[derive(Debug, Clone)]
pub struct TaikoEngineValidator {
    /// Chain spec used for payload and attribute validation rules.
    pub chain_spec: Arc<TaikoChainSpec>,
}

impl TaikoEngineValidator {
    /// Instantiates a new validator.
    pub const fn new(chain_spec: Arc<TaikoChainSpec>) -> Self {
        Self { chain_spec }
    }
}

impl<Types> PayloadValidator<Types> for TaikoEngineValidator
where
    Types: PayloadTypes<ExecutionData = TaikoExecutionData>,
{
    /// The block type used by the engine.
    type Block = Block;

    /// Converts the given payload into a sealed block without recovering signatures.
    fn convert_payload_to_block(
        &self,
        payload: Types::ExecutionData,
    ) -> Result<SealedBlock<Self::Block>, NewPayloadError> {
        let TaikoExecutionData { execution_payload, withdrawals, taiko_sidecar } = payload;

        let expected_hash = execution_payload.block_hash;
        let is_unzen_active = self.chain_spec.is_unzen_active(execution_payload.timestamp);
        let transactions_present = execution_payload.transactions.is_some();
        let legacy_hash_only = !transactions_present && withdrawals.is_none();

        if is_unzen_active && taiko_sidecar.header_difficulty.is_none() {
            return Err(NewPayloadError::other(
                TaikoPayloadValidationError::MissingUnzenHeaderDifficulty,
            ));
        }
        if transactions_present && withdrawals.is_none() {
            return Err(NewPayloadError::other(TaikoPayloadValidationError::MissingWithdrawals));
        }

        // First parse the block.
        let mut block = Into::<ExecutionPayloadV1>::into(execution_payload).try_into_block()?;
        if let Some(header_difficulty) = taiko_sidecar.header_difficulty {
            block.header.difficulty = header_difficulty;
        }
        block.header.parent_beacon_block_root = is_unzen_active.then_some(B256::ZERO);
        block.header.blob_gas_used = is_unzen_active.then_some(0);
        block.header.excess_blob_gas = is_unzen_active.then_some(0);
        block.header.requests_hash = is_unzen_active.then_some(EMPTY_REQUESTS_HASH);
        if legacy_hash_only {
            block.header.transactions_root = taiko_sidecar.tx_hash;
            let withdrawals_hash =
                taiko_sidecar.withdrawals_hash.filter(|hash| !hash.is_zero()).ok_or_else(|| {
                    NewPayloadError::other(
                        TaikoPayloadValidationError::MissingLegacyWithdrawalsHash,
                    )
                })?;
            block.header.withdrawals_root = Some(withdrawals_hash);
            block.body.withdrawals = None;
        } else if let Some(withdrawals) = withdrawals {
            let withdrawals = Withdrawals::new(withdrawals);
            block.header.withdrawals_root = Some(proofs::calculate_withdrawals_root(&withdrawals));
            block.body.withdrawals = Some(withdrawals);
        }
        let sealed_block = block.seal_slow();

        // Ensure the hash included in the payload matches the block hash
        if expected_hash != sealed_block.hash() {
            return Err(PayloadError::BlockHash {
                execution: sealed_block.hash(),
                consensus: expected_hash,
            })
            .map_err(|e| NewPayloadError::Other(e.into()));
        }

        Ok(sealed_block)
    }

    /// Ensures that the given payload does not violate any consensus rules that concern the block's
    /// layout.
    ///
    /// This function must convert the payload into the executable block and pre-validate its
    /// fields.
    fn ensure_well_formed_payload(
        &self,
        payload: Types::ExecutionData,
    ) -> Result<RecoveredBlock<Self::Block>, NewPayloadError> {
        let sealed_block =
            <Self as PayloadValidator<Types>>::convert_payload_to_block(self, payload)?;

        if sealed_block.body().transactions().into_iter().any(|tx| !is_allowed_tx_type(tx)) {
            return Err(NewPayloadError::other(
                TaikoPayloadValidationError::BlobTransactionsUnsupported,
            ));
        }

        sealed_block.try_recover().map_err(|e| NewPayloadError::Other(e.into()))
    }

    /// Validates the payload attributes with respect to the header.
    fn validate_payload_attributes_against_header(
        &self,
        attr: &Types::PayloadAttributes,
        header: &<Self::Block as BlockTrait>::Header,
    ) -> Result<(), InvalidPayloadAttributesError> {
        // We allow the payload attributes to have a timestamp that is equal to the parent header's
        // timestamp in Taiko network.
        if attr.timestamp() < header.timestamp() {
            return Err(InvalidPayloadAttributesError::InvalidTimestamp);
        }
        Ok(())
    }
}

// EngineApiValidator implementation for TaikoEngineValidator
impl<Types> EngineApiValidator<Types> for TaikoEngineValidator
where
    Types: PayloadTypes<PayloadAttributes = TaikoPayloadAttributes, ExecutionData = TaikoExecutionData>,
{
    /// Validates the presence or exclusion of fork-specific fields based on the payload attributes
    /// and the message version.
    fn validate_version_specific_fields(
        &self,
        version: EngineApiMessageVersion,
        payload_or_attrs: PayloadOrAttributes<'_, Types::ExecutionData, Types::PayloadAttributes>,
    ) -> Result<(), EngineObjectValidationError> {
        let is_legacy_hash_only = version == EngineApiMessageVersion::V2 &&
            matches!(
                &payload_or_attrs,
                PayloadOrAttributes::ExecutionPayload(payload)
                    if payload.execution_payload.transactions.is_none() &&
                        payload.withdrawals.is_none() &&
                        payload
                            .taiko_sidecar
                            .withdrawals_hash
                            .is_some_and(|hash| !hash.is_zero())
            );
        if is_legacy_hash_only {
            return Ok(())
        }

        validate_withdrawals_presence(
            self.chain_spec.as_ref(),
            version,
            payload_or_attrs.message_validation_kind(),
            payload_or_attrs.timestamp(),
            payload_or_attrs.withdrawals().is_some(),
        )
    }

    /// Ensures that the payload attributes are valid for the given [`EngineApiMessageVersion`].
    fn ensure_well_formed_attributes(
        &self,
        version: EngineApiMessageVersion,
        attributes: &Types::PayloadAttributes,
    ) -> Result<(), EngineObjectValidationError> {
        validate_withdrawals_presence(
            self.chain_spec.as_ref(),
            version,
            MessageValidationKind::PayloadAttributes,
            attributes.timestamp(),
            attributes.withdrawals().is_some(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alethia_reth_chainspec::{TAIKO_DEVNET, hardfork::TaikoHardfork};
    use alethia_reth_primitives::{
        engine::{
            TaikoEngineTypes,
            types::{TaikoExecutionData, TaikoExecutionDataSidecar},
        },
        payload::attributes::{RpcL1Origin, TaikoBlockMetadata, TaikoPayloadAttributes},
    };
    use alloy_consensus::{BlockBody, Header, constants::EMPTY_WITHDRAWALS};
    use alloy_eips::merge::BEACON_NONCE;
    use alloy_hardforks::ForkCondition;
    use alloy_primitives::{Address, B256, Bytes, U256};
    use alloy_rpc_types_engine::{ExecutionPayloadV1, PayloadAttributes as EthPayloadAttributes};
    use alloy_rpc_types_eth::{Withdrawal, Withdrawals};
    use reth_primitives_traits::BlockBody as _;

    #[test]
    fn formats_blob_transactions_unsupported_error() {
        assert_eq!(
            TaikoPayloadValidationError::BlobTransactionsUnsupported.to_string(),
            "blob transactions are unsupported"
        );
    }

    #[test]
    fn rejects_unzen_payload_without_header_difficulty() {
        let validator = TaikoEngineValidator::new(Arc::new(unzen_chain_spec()));
        let payload = sample_unzen_execution_data(U256::from(7_u64), None, None);

        let err =
            <TaikoEngineValidator as PayloadValidator<TaikoEngineTypes>>::convert_payload_to_block(
                &validator, payload,
            )
            .expect_err("Unzen payloads must supply header difficulty explicitly");

        assert_eq!(err.to_string(), "missing header difficulty for Unzen payload");
    }

    #[test]
    fn accepts_unzen_payload_when_sidecar_supplies_header_difficulty() {
        let validator = TaikoEngineValidator::new(Arc::new(unzen_chain_spec()));
        let payload = sample_unzen_execution_data(
            U256::from(7_u64),
            Some(U256::from(7_u64)),
            Some(B256::ZERO),
        );

        let sealed =
            <TaikoEngineValidator as PayloadValidator<TaikoEngineTypes>>::convert_payload_to_block(
                &validator,
                payload.clone(),
            )
            .expect("explicit header difficulty should restore the original hash");

        assert_eq!(sealed.hash(), payload.execution_payload.block_hash);
        assert_eq!(sealed.header().difficulty, U256::from(7_u64));
    }

    #[test]
    fn accepts_unzen_payload_when_validator_infers_parent_beacon_block_root() {
        let validator = TaikoEngineValidator::new(Arc::new(unzen_chain_spec()));
        let payload = sample_unzen_execution_data(
            U256::from(7_u64),
            Some(U256::from(7_u64)),
            Some(B256::ZERO),
        );

        let sealed =
            <TaikoEngineValidator as PayloadValidator<TaikoEngineTypes>>::convert_payload_to_block(
                &validator,
                payload.clone(),
            )
            .expect("validator should infer parent beacon block root from Unzen activation");

        assert_eq!(sealed.hash(), payload.execution_payload.block_hash);
        assert_eq!(sealed.header().parent_beacon_block_root, Some(B256::ZERO));
        assert_eq!(sealed.header().blob_gas_used, Some(0));
        assert_eq!(sealed.header().excess_blob_gas, Some(0));
        assert_eq!(sealed.header().requests_hash, Some(EMPTY_REQUESTS_HASH));
    }

    #[test]
    fn complete_payload_uses_full_withdrawals_and_ignores_sidecar_hash() {
        let validator = TaikoEngineValidator::new(Arc::new(unzen_chain_spec()));
        let withdrawals = sample_state_neutral_withdrawals();
        let payload = sample_unzen_execution_data_with_withdrawals(
            U256::from(7_u64),
            Some(U256::from(7_u64)),
            Some(B256::ZERO),
            Some(withdrawals.clone()),
            Some(B256::repeat_byte(0xaa)),
            true,
        );

        let sealed =
            <TaikoEngineValidator as PayloadValidator<TaikoEngineTypes>>::convert_payload_to_block(
                &validator,
                payload.clone(),
            )
            .expect("full withdrawals must take precedence over the sidecar hash");

        assert_eq!(sealed.hash(), payload.execution_payload.block_hash);
        assert_eq!(
            sealed.header().withdrawals_root,
            Some(proofs::calculate_withdrawals_root(&withdrawals))
        );
        assert_eq!(sealed.body().withdrawals.as_ref(), Some(&withdrawals));
    }

    #[test]
    fn complete_payload_rejects_missing_withdrawals() {
        let validator = TaikoEngineValidator::new(Arc::new(unzen_chain_spec()));
        let payload = sample_unzen_execution_data_with_withdrawals(
            U256::from(7_u64),
            Some(U256::from(7_u64)),
            Some(B256::ZERO),
            None,
            Some(EMPTY_WITHDRAWALS),
            true,
        );

        let err =
            <TaikoEngineValidator as PayloadValidator<TaikoEngineTypes>>::convert_payload_to_block(
                &validator, payload,
            )
            .expect_err("complete transaction payloads must include withdrawals");

        assert_eq!(err.to_string(), "missing withdrawals for complete Taiko payload");
    }

    #[test]
    fn legacy_hash_only_payload_preserves_committed_roots_without_fake_body() {
        let validator = TaikoEngineValidator::new(Arc::new(unzen_chain_spec()));
        let payload = sample_unzen_execution_data_with_withdrawals(
            U256::from(7_u64),
            Some(U256::from(7_u64)),
            Some(B256::ZERO),
            None,
            Some(EMPTY_WITHDRAWALS),
            false,
        );

        let sealed =
            <TaikoEngineValidator as PayloadValidator<TaikoEngineTypes>>::convert_payload_to_block(
                &validator,
                payload.clone(),
            )
            .expect("legacy payload should restore roots from the Taiko sidecar");

        assert_eq!(sealed.hash(), payload.execution_payload.block_hash);
        assert_eq!(sealed.header().withdrawals_root, Some(EMPTY_WITHDRAWALS));
        assert!(sealed.body().withdrawals.is_none());
        assert!(payload.execution_payload.transactions.is_none());
    }

    #[test]
    fn legacy_hash_only_payload_rejects_missing_withdrawals_hash() {
        let validator = TaikoEngineValidator::new(Arc::new(unzen_chain_spec()));
        let payload = sample_unzen_execution_data_with_withdrawals(
            U256::from(7_u64),
            Some(U256::from(7_u64)),
            Some(B256::ZERO),
            None,
            None,
            false,
        );

        let err =
            <TaikoEngineValidator as PayloadValidator<TaikoEngineTypes>>::convert_payload_to_block(
                &validator, payload,
            )
            .expect_err("legacy payload must include a nonzero withdrawals hash");

        assert_eq!(err.to_string(), "missing withdrawals hash for legacy Taiko payload");
    }

    #[test]
    fn state_neutral_withdrawals_metadata_mutations_change_block_hash() {
        let validator = TaikoEngineValidator::new(Arc::new(unzen_chain_spec()));
        let original = sample_state_neutral_withdrawals();
        let payload = sample_unzen_execution_data_with_withdrawals(
            U256::from(7_u64),
            Some(U256::from(7_u64)),
            Some(B256::ZERO),
            Some(original.clone()),
            Some(EMPTY_WITHDRAWALS),
            true,
        );
        let mut mutations = Vec::new();

        let mut changed_index = original.clone().into_inner();
        changed_index[0].index += 1;
        mutations.push(changed_index);

        let mut changed_validator = original.clone().into_inner();
        changed_validator[0].validator_index += 1;
        mutations.push(changed_validator);

        let mut changed_order = original.into_inner();
        changed_order.swap(0, 1);
        mutations.push(changed_order);

        for mutated in mutations {
            let mut candidate = payload.clone();
            candidate.withdrawals = Some(mutated);
            let err = <TaikoEngineValidator as PayloadValidator<TaikoEngineTypes>>::convert_payload_to_block(
                &validator,
                candidate,
            )
            .expect_err("withdrawals metadata mutations must invalidate the committed block hash");
            assert!(err.to_string().contains("block hash"), "unexpected error: {err}");
        }
    }

    #[test]
    fn engine_v2_allows_only_the_legacy_hash_only_withdrawals_exception() {
        let validator = TaikoEngineValidator::new(Arc::new(unzen_chain_spec()));
        let legacy = sample_unzen_execution_data_with_withdrawals(
            U256::from(7_u64),
            Some(U256::from(7_u64)),
            Some(B256::ZERO),
            None,
            Some(EMPTY_WITHDRAWALS),
            false,
        );
        <TaikoEngineValidator as EngineApiValidator<TaikoEngineTypes>>::validate_version_specific_fields(
            &validator,
            EngineApiMessageVersion::V2,
            PayloadOrAttributes::ExecutionPayload(&legacy),
        )
        .expect("V2 must preserve Taiko-Geth's legacy hash-only exception");

        let complete_missing_withdrawals = sample_unzen_execution_data_with_withdrawals(
            U256::from(7_u64),
            Some(U256::from(7_u64)),
            Some(B256::ZERO),
            None,
            Some(EMPTY_WITHDRAWALS),
            true,
        );
        let err = <TaikoEngineValidator as EngineApiValidator<TaikoEngineTypes>>::validate_version_specific_fields(
            &validator,
            EngineApiMessageVersion::V2,
            PayloadOrAttributes::ExecutionPayload(&complete_missing_withdrawals),
        )
        .expect_err("complete V2 payloads must not use the hash-only exception");

        assert!(err.to_string().contains("no withdrawals post-Shanghai"));
    }

    #[test]
    fn forkchoice_v2_rejects_missing_withdrawals_and_accepts_empty_list() {
        let validator = TaikoEngineValidator::new(Arc::new(unzen_chain_spec()));
        let missing = sample_payload_attributes(None);
        let err =
            <TaikoEngineValidator as EngineApiValidator<TaikoEngineTypes>>::ensure_well_formed_attributes(
                &validator,
                EngineApiMessageVersion::V2,
                &missing,
            )
            .expect_err("post-Shanghai payload attributes must include withdrawals");
        assert!(err.to_string().contains("no withdrawals post-Shanghai"));

        let empty = sample_payload_attributes(Some(Vec::new()));
        <TaikoEngineValidator as EngineApiValidator<TaikoEngineTypes>>::ensure_well_formed_attributes(
            &validator,
            EngineApiMessageVersion::V2,
            &empty,
        )
        .expect("an explicit empty withdrawals list is valid");
    }

    fn unzen_chain_spec() -> TaikoChainSpec {
        let mut chain_spec = (*TAIKO_DEVNET).as_ref().clone();
        chain_spec.inner.hardforks.insert(TaikoHardfork::Unzen, ForkCondition::Timestamp(0));
        chain_spec
    }

    fn sample_unzen_execution_data(
        difficulty: U256,
        header_difficulty: Option<U256>,
        parent_beacon_block_root: Option<B256>,
    ) -> TaikoExecutionData {
        sample_unzen_execution_data_with_withdrawals(
            difficulty,
            header_difficulty,
            parent_beacon_block_root,
            Some(Withdrawals::default()),
            Some(EMPTY_WITHDRAWALS),
            true,
        )
    }

    fn sample_state_neutral_withdrawals() -> Withdrawals {
        Withdrawals::new(vec![
            Withdrawal {
                index: 1,
                validator_index: 2,
                address: Address::with_last_byte(0x61),
                amount: 0,
            },
            Withdrawal {
                index: 3,
                validator_index: 4,
                address: Address::with_last_byte(0x62),
                amount: 0,
            },
        ])
    }

    fn sample_payload_attributes(withdrawals: Option<Vec<Withdrawal>>) -> TaikoPayloadAttributes {
        TaikoPayloadAttributes {
            payload_attributes: EthPayloadAttributes {
                timestamp: 1,
                prev_randao: B256::ZERO,
                suggested_fee_recipient: Address::ZERO,
                withdrawals,
                parent_beacon_block_root: Some(B256::ZERO),
                slot_number: None,
            },
            base_fee_per_gas: U256::from(1_u64),
            block_metadata: TaikoBlockMetadata {
                beneficiary: Address::ZERO,
                gas_limit: 30_000_000,
                timestamp: U256::from(1_u64),
                mix_hash: B256::ZERO,
                tx_list: Some(Bytes::new()),
                extra_data: Bytes::new(),
            },
            l1_origin: RpcL1Origin {
                block_id: U256::ZERO,
                l2_block_hash: B256::ZERO,
                l1_block_height: None,
                l1_block_hash: None,
                build_payload_args_id: [0; 8],
                is_forced_inclusion: false,
                signature: [0; 65],
            },
            anchor_transaction: None,
        }
    }

    fn sample_unzen_execution_data_with_withdrawals(
        difficulty: U256,
        header_difficulty: Option<U256>,
        parent_beacon_block_root: Option<B256>,
        withdrawals: Option<Withdrawals>,
        withdrawals_hash: Option<B256>,
        transactions_present: bool,
    ) -> TaikoExecutionData {
        let withdrawals_root = withdrawals
            .as_ref()
            .map(|withdrawals| proofs::calculate_withdrawals_root(withdrawals))
            .or(withdrawals_hash);
        let block = reth_ethereum::Block {
            header: Header {
                parent_hash: B256::with_last_byte(0x11),
                beneficiary: Address::with_last_byte(0x22),
                state_root: B256::with_last_byte(0x33),
                transactions_root: alloy_consensus::proofs::calculate_transaction_root(&Vec::<
                    reth_ethereum::TransactionSigned,
                >::new(
                )),
                receipts_root: B256::with_last_byte(0x44),
                withdrawals_root,
                logs_bloom: Default::default(),
                number: 1,
                gas_limit: 30_000_000,
                gas_used: 0,
                timestamp: 1,
                mix_hash: B256::with_last_byte(0x55),
                nonce: BEACON_NONCE.into(),
                base_fee_per_gas: Some(1),
                extra_data: Bytes::default(),
                difficulty,
                parent_beacon_block_root,
                blob_gas_used: Some(0),
                excess_blob_gas: Some(0),
                requests_hash: Some(EMPTY_REQUESTS_HASH),
                ..Default::default()
            },
            body: BlockBody {
                transactions: vec![],
                ommers: vec![],
                withdrawals: withdrawals.clone(),
            },
        };
        let block_hash = block.header.hash_slow();
        let execution_payload = ExecutionPayloadV1::from_block_unchecked(block_hash, &block);
        let withdrawals = withdrawals.map(Withdrawals::into_inner);
        let execution_payload =
            alethia_reth_primitives::engine::types::TaikoExecutionPayloadV1::from(
                execution_payload,
            );
        let execution_payload = if transactions_present {
            execution_payload
        } else {
            alethia_reth_primitives::engine::types::TaikoExecutionPayloadV1 {
                transactions: None,
                ..execution_payload
            }
        };

        TaikoExecutionData {
            execution_payload,
            withdrawals,
            taiko_sidecar: TaikoExecutionDataSidecar {
                tx_hash: block.body.calculate_tx_root(),
                withdrawals_hash,
                header_difficulty,
                taiko_block: Some(true),
            },
        }
    }
}
