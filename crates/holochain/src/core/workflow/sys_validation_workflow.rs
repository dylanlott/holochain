//! The workflow and queue consumer for sys validation

use super::*;
use crate::{
    conductor::api::CellConductorApiT,
    core::{
        queue_consumer::{OneshotWriter, TriggerSender, WorkComplete},
        state::{
            cascade::Cascade,
            dht_op_integration::{IntegratedDhtOpsStore, IntegrationLimboStore},
            element_buf::ElementBuf,
            metadata::MetadataBuf,
            validation_db::{ValidationLimboStatus, ValidationLimboStore, ValidationLimboValue},
            workspace::{Workspace, WorkspaceResult},
        },
        sys_validate::*,
    },
};
use error::WorkflowResult;
use fallible_iterator::FallibleIterator;
use holo_hash::DhtOpHash;
use holochain_keystore::Signature;
use holochain_p2p::HolochainP2pCell;
use holochain_state::{
    buffer::{BufferedStore, KvBuf},
    db::{INTEGRATED_DHT_OPS, INTEGRATION_LIMBO},
    prelude::{GetDb, Reader, Writer},
};
use holochain_types::{dht_op::DhtOp, header::NewEntryHeaderRef, Entry, Timestamp};
use holochain_zome_types::{
    header::{ElementDelete, EntryType, EntryUpdate, LinkAdd, LinkRemove},
    Header,
};
use std::convert::TryInto;
use tracing::*;

#[instrument(skip(workspace, writer, trigger_app_validation, network, conductor_api))]
pub async fn sys_validation_workflow(
    mut workspace: SysValidationWorkspace<'_>,
    writer: OneshotWriter,
    trigger_app_validation: &mut TriggerSender,
    network: HolochainP2pCell,
    conductor_api: impl CellConductorApiT,
) -> WorkflowResult<WorkComplete> {
    let complete = sys_validation_workflow_inner(&mut workspace, network, conductor_api).await?;

    // --- END OF WORKFLOW, BEGIN FINISHER BOILERPLATE ---

    // commit the workspace
    writer
        .with_writer(|writer| Ok(workspace.flush_to_txn(writer)?))
        .await?;

    // trigger other workflows
    trigger_app_validation.trigger();

    Ok(complete)
}

async fn sys_validation_workflow_inner(
    workspace: &mut SysValidationWorkspace<'_>,
    network: HolochainP2pCell,
    conductor_api: impl CellConductorApiT,
) -> WorkflowResult<WorkComplete> {
    // Drain all the ops
    let mut ops: Vec<ValidationLimboValue> = workspace
        .validation_limbo
        .drain_iter()?
        .filter(|vlv| {
            match vlv.status {
                // We only want pending or awaiting sys dependency ops
                ValidationLimboStatus::Pending | ValidationLimboStatus::AwaitingSysDeps => Ok(true),
                ValidationLimboStatus::SysValidated | ValidationLimboStatus::AwaitingAppDeps => {
                    Ok(false)
                }
            }
        })
        .collect()?;

    // Sort the ops
    ops.sort_unstable_by_key(|v| DhtOpOrder::from(&v.op));

    for vlv in ops {
        let ValidationLimboValue {
            op,
            basis,
            time_added,
            num_tries,
            ..
        } = vlv;
        let (status, op) = validate_op(op, workspace, network.clone(), &conductor_api).await?;
        match &status {
            ValidationLimboStatus::Pending
            | ValidationLimboStatus::AwaitingSysDeps
            | ValidationLimboStatus::SysValidated => {
                // TODO: Some of the ops go straight to integration and
                // skip app validation so we need to write those to the
                // integration limbo and not the validation limbo
                let hash = DhtOpHash::with_data(&op).await;
                let vlv = ValidationLimboValue {
                    status,
                    op,
                    basis,
                    time_added,
                    last_try: Some(Timestamp::now()),
                    num_tries: num_tries + 1,
                };
                workspace.validation_limbo.put(hash, vlv)?;
            }
            ValidationLimboStatus::AwaitingAppDeps => {
                unreachable!("We should not be returning this status from system validation")
            }
        }
    }
    Ok(WorkComplete::Complete)
}

async fn validate_op(
    op: DhtOp,
    workspace: &mut SysValidationWorkspace<'_>,
    network: HolochainP2pCell,
    conductor_api: &impl CellConductorApiT,
) -> WorkflowResult<(ValidationLimboStatus, DhtOp)> {
    match validate_op_inner(op, workspace, network, conductor_api).await {
        Ok(op) => Ok((ValidationLimboStatus::SysValidated, op)),
        // TODO: Handle the errors that result in pending or awaiting deps
        Err(_) => todo!(),
    }
}

async fn validate_op_inner(
    op: DhtOp,
    workspace: &mut SysValidationWorkspace<'_>,
    network: HolochainP2pCell,
    conductor_api: &impl CellConductorApiT,
) -> SysValidationResult<DhtOp> {
    match op {
        DhtOp::StoreElement(signature, header, maybe_entry) => {
            store_header(&header, workspace.cascade(network)).await?;

            all_op_check(&signature, &header).await?;
            Ok(DhtOp::StoreElement(signature, header, maybe_entry))
        }
        DhtOp::StoreEntry(signature, header, entry) => {
            store_entry(
                (&header).into(),
                entry.as_ref(),
                conductor_api,
                workspace.cascade(network),
            )
            .await?;

            let header = header.into();
            all_op_check(&signature, &header).await?;
            Ok(DhtOp::StoreEntry(
                signature,
                header.try_into().expect("type hasn't changed"),
                entry,
            ))
        }
        DhtOp::RegisterAgentActivity(signature, header) => {
            register_agent_activity(&header, &workspace).await?;

            all_op_check(&signature, &header).await?;
            Ok(DhtOp::RegisterAgentActivity(signature, header))
        }
        DhtOp::RegisterUpdatedBy(signature, header) => {
            register_updated_by(&header, &workspace.element_vault).await?;

            let header = header.into();
            all_op_check(&signature, &header).await?;
            Ok(DhtOp::RegisterUpdatedBy(
                signature,
                header.try_into().expect("type hasn't changed"),
            ))
        }
        DhtOp::RegisterDeletedBy(signature, header)
        | DhtOp::RegisterDeletedEntryHeader(signature, header) => {
            register_deleted(&header, &workspace.element_vault).await?;

            let header = header.into();
            all_op_check(&signature, &header).await?;
            Ok(DhtOp::RegisterDeletedBy(
                signature,
                header.try_into().expect("type hasn't changed"),
            ))
        }
        DhtOp::RegisterAddLink(signature, header) => {
            register_add_link(&header, workspace, network).await?;

            let header = header.into();
            all_op_check(&signature, &header).await?;
            Ok(DhtOp::RegisterAddLink(
                signature,
                header.try_into().expect("type hasn't changed"),
            ))
        }
        DhtOp::RegisterRemoveLink(signature, header) => {
            register_remove_link(&header, workspace).await?;

            let header = header.into();
            all_op_check(&signature, &header).await?;
            Ok(DhtOp::RegisterRemoveLink(
                signature,
                header.try_into().expect("type hasn't changed"),
            ))
        }
    }
}

async fn all_op_check(signature: &Signature, header: &Header) -> SysValidationResult<()> {
    verify_header_signature(&signature, &header).await?;
    author_key_is_valid(header.author()).await?;
    Ok(())
}

async fn register_agent_activity(
    header: &Header,
    workspace: &SysValidationWorkspace<'_>,
) -> SysValidationResult<()> {
    // Get data ready to validate
    let author = header.author();
    let prev_header_hash = header.prev_header();

    // Checks
    check_prev_header(&header)?;
    check_valid_if_dna(&header, &workspace.meta_vault)?;
    if let Some(prev_header_hash) = prev_header_hash {
        check_holding_prev_header(
            author.clone(),
            prev_header_hash,
            &workspace.meta_vault,
            &workspace.element_vault,
        )
        .await?;
    }
    check_chain_rollback(&header, &workspace.meta_vault, &workspace.element_vault).await?;
    Ok(())
}

async fn store_header(header: &Header, cascade: Cascade<'_, '_>) -> SysValidationResult<()> {
    // Get data ready to validate
    let prev_header_hash = header.prev_header();

    // Checks
    check_prev_header(header)?;
    if let Some(prev_header_hash) = prev_header_hash {
        let prev_header = check_header_exists(prev_header_hash.clone(), cascade).await?;
        check_prev_timestamp(&header, prev_header.header())?;
        check_prev_seq(&header, prev_header.header())?;
    }
    Ok(())
}

async fn store_entry(
    header: NewEntryHeaderRef<'_>,
    entry: &Entry,
    conductor_api: &impl CellConductorApiT,
    cascade: Cascade<'_, '_>,
) -> SysValidationResult<()> {
    // Get data ready to validate
    let entry_type = header.entry_type();
    let entry_hash = header.entry_hash();

    // Checks
    check_entry_type(entry_type, entry)?;
    if let EntryType::App(app_entry_type) = entry_type {
        let entry_def = check_app_entry_type(app_entry_type, conductor_api).await?;
        check_not_private(&entry_def)?;
    }
    check_entry_hash(entry_hash, entry).await?;
    check_entry_size(entry)?;

    // Additional checks if this is an EntryUpdate
    if let NewEntryHeaderRef::Update(entry_update) = header {
        let original_header =
            check_header_exists(entry_update.original_header_address.clone(), cascade).await?;
        update_check(entry_update, original_header.header())?;
    }
    Ok(())
}

async fn register_updated_by(
    entry_update: &EntryUpdate,
    element_vault: &ElementBuf<'_>,
) -> SysValidationResult<()> {
    // Get data ready to validate
    let original_header_address = &entry_update.original_header_address;

    // Checks
    let original_element = check_holding_element(original_header_address, element_vault).await?;
    update_check(entry_update, original_element.header())?;
    Ok(())
}

async fn register_deleted(
    element_delete: &ElementDelete,
    element_vault: &ElementBuf<'_>,
) -> SysValidationResult<()> {
    // Get data ready to validate
    let removed_header_address = &element_delete.removes_address;

    // Checks
    check_holding_header(removed_header_address, element_vault).await?;
    Ok(())
}

async fn register_add_link(
    link_add: &LinkAdd,
    workspace: &mut SysValidationWorkspace<'_>,
    network: HolochainP2pCell,
) -> SysValidationResult<()> {
    // Get data ready to validate
    let base_entry_address = &link_add.base_address;

    // Checks
    check_holding_entry(base_entry_address, &workspace.element_vault).await?;
    check_entry_exists(base_entry_address.clone(), workspace.cascade(network)).await?;
    check_tag_size(&link_add.tag)?;
    Ok(())
}

async fn register_remove_link(
    link_remove: &LinkRemove,
    workspace: &SysValidationWorkspace<'_>,
) -> SysValidationResult<()> {
    // Get data ready to validate
    let link_add_address = &link_remove.link_add_address;

    // Checks
    let link_add = check_holding_header(link_add_address, &workspace.element_vault).await?;
    let (link_add, link_add_hash) = link_add.into_header_and_signature().0.into_inner();
    check_link_in_metadata(link_add, &link_add_hash, &workspace.meta_vault)?;
    Ok(())
}

fn update_check(entry_update: &EntryUpdate, original_header: &Header) -> SysValidationResult<()> {
    check_new_entry_header(original_header)?;
    let original_header: NewEntryHeaderRef = original_header
        .try_into()
        .expect("This can't fail due to the above check_new_entry_header");
    check_update_reference(entry_update, &original_header)?;
    Ok(())
}

/// Type for deriving ordering of DhtOps
/// Don't change the order of this enum unless
/// you mean to change the order we process ops
#[allow(missing_docs)]
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum DhtOpOrder {
    RegisterAgentActivity,
    StoreEntry,
    StoreElement,
    RegisterUpdatedBy,
    RegisterDeletedBy,
    RegisterDeletedEntryHeader,
    RegisterAddLink,
    RegisterRemoveLink,
}

impl From<&DhtOp> for DhtOpOrder {
    fn from(op: &DhtOp) -> Self {
        use DhtOpOrder::*;
        match op {
            DhtOp::StoreElement(_, _, _) => StoreElement,
            DhtOp::StoreEntry(_, _, _) => StoreEntry,
            DhtOp::RegisterAgentActivity(_, _) => RegisterAgentActivity,
            DhtOp::RegisterUpdatedBy(_, _) => RegisterUpdatedBy,
            DhtOp::RegisterDeletedBy(_, _) => RegisterDeletedBy,
            DhtOp::RegisterDeletedEntryHeader(_, _) => RegisterDeletedEntryHeader,
            DhtOp::RegisterAddLink(_, _) => RegisterAddLink,
            DhtOp::RegisterRemoveLink(_, _) => RegisterRemoveLink,
        }
    }
}

pub struct SysValidationWorkspace<'env> {
    pub integration_limbo: IntegrationLimboStore<'env>,
    pub integrated_dht_ops: IntegratedDhtOpsStore<'env>,
    pub validation_limbo: ValidationLimboStore<'env>,
    pub element_vault: ElementBuf<'env>,
    pub meta_vault: MetadataBuf<'env>,
    pub element_cache: ElementBuf<'env>,
    pub meta_cache: MetadataBuf<'env>,
}

impl<'env: 'a, 'a> SysValidationWorkspace<'env> {
    pub fn cascade(&'a mut self, network: HolochainP2pCell) -> Cascade<'env, 'a> {
        Cascade::new(
            &self.element_vault,
            &self.meta_vault,
            &mut self.element_cache,
            &mut self.meta_cache,
            network,
        )
    }
}

impl<'env> Workspace<'env> for SysValidationWorkspace<'env> {
    fn new(reader: &'env Reader<'env>, dbs: &impl GetDb) -> WorkspaceResult<Self> {
        let db = dbs.get_db(&*INTEGRATED_DHT_OPS)?;
        let integrated_dht_ops = KvBuf::new(reader, db)?;

        let db = dbs.get_db(&*INTEGRATION_LIMBO)?;
        let integration_limbo = KvBuf::new(reader, db)?;

        let validation_limbo = ValidationLimboStore::new(reader, dbs)?;

        let element_vault = ElementBuf::vault(reader, dbs, false)?;
        let meta_vault = MetadataBuf::vault(reader, dbs)?;
        let element_cache = ElementBuf::cache(reader, dbs)?;
        let meta_cache = MetadataBuf::cache(reader, dbs)?;

        Ok(Self {
            integration_limbo,
            integrated_dht_ops,
            validation_limbo,
            element_vault,
            meta_vault,
            element_cache,
            meta_cache,
        })
    }
    fn flush_to_txn(self, writer: &mut Writer) -> WorkspaceResult<()> {
        self.validation_limbo.0.flush_to_txn(writer)?;
        self.integration_limbo.flush_to_txn(writer)?;
        // Flush for cascade
        self.element_cache.flush_to_txn(writer)?;
        self.meta_cache.flush_to_txn(writer)?;
        Ok(())
    }
}
