//! Write set tracking for an active transaction
use std::collections::{
    BTreeMap,
    BTreeSet,
};

use common::{
    bootstrap_model::index::{
        database_index::IndexedFields,
        index_metadata_serialize_tablet_id,
        TABLE_ID_FIELD_PATH,
    },
    document::{
        DocumentUpdate,
        ResolvedDocument,
    },
    index::IndexKey,
    interval::{
        BinaryKey,
        Interval,
    },
    knobs::{
        TRANSACTION_MAX_NUM_USER_WRITES,
        TRANSACTION_MAX_SYSTEM_NUM_WRITES,
        TRANSACTION_MAX_SYSTEM_WRITE_SIZE_BYTES,
        TRANSACTION_MAX_USER_WRITE_SIZE_BYTES,
    },
    types::TabletIndexName,
    value::{
        ResolvedDocumentId,
        Size,
        TabletIdAndTableNumber,
    },
};
use errors::ErrorMetadata;
use value::{
    values_to_bytes,
    TableIdentifier,
};

use crate::{
    bootstrap_model::defaults::BootstrapTableIds,
    reads::TransactionReadSet,
};

#[derive(Clone, PartialEq)]
#[cfg_attr(any(test, feature = "testing"), derive(Debug))]
#[cfg_attr(any(test, feature = "testing"), derive(proptest_derive::Arbitrary))]
pub struct DocumentWrite {
    pub document: Option<ResolvedDocument>,
}

/// The write set for a transaction, maintained by `TransactionState`
#[derive(Clone, PartialEq)]
#[cfg_attr(any(test, feature = "testing"), derive(Debug))]
pub struct Writes {
    updates: BTreeMap<ResolvedDocumentId, DocumentUpdate>,

    // All of the new DocumentIds that were generated in this transaction.
    // TODO: Remove, `generated_ids` are included in `updates` as (None, None)
    pub generated_ids: BTreeSet<ResolvedDocumentId>,

    // Fields below can be recomputed from `updates`.

    // Size of writes to user tables
    user_tx_size: TransactionWriteSize,
    // Size of writes to system tables
    system_tx_size: TransactionWriteSize,
    // When we write to module versions we cannot use the module cache.
    written_tables: BTreeSet<TabletIdAndTableNumber>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct TransactionWriteSize {
    // Total number of writes (i.e. calls to `mutate`)
    pub num_writes: usize,

    // Total size of mutations. Writing to the same DocumentId twice will still count twice.
    pub size: usize,
}

impl Writes {
    /// Create an empty write set.
    pub fn new() -> Self {
        Self {
            updates: BTreeMap::new(),
            generated_ids: BTreeSet::new(),
            user_tx_size: TransactionWriteSize::default(),
            system_tx_size: TransactionWriteSize::default(),
            written_tables: BTreeSet::new(),
        }
    }

    /// Are there any writes in the active transaction?
    pub fn is_empty(&self) -> bool {
        self.updates.is_empty()
    }

    pub fn has_written_to(&self, table_id: &TabletIdAndTableNumber) -> bool {
        self.written_tables.contains(table_id)
    }

    pub fn update(
        &mut self,
        bootstrap_tables: BootstrapTableIds,
        is_system_document: bool,
        reads: &mut TransactionReadSet,
        document_id: ResolvedDocumentId,
        document_update: DocumentUpdate,
    ) -> anyhow::Result<()> {
        if document_update.old_document.is_none() {
            anyhow::ensure!(self.updates.get(&document_id).is_none(), "Duplicate insert");
            self.register_new_id(reads, document_id)?;
        }
        Self::record_reads_for_write(bootstrap_tables, reads, *document_id.table())?;

        let id_size = document_id.size();
        let value_size = document_update
            .new_document
            .as_ref()
            .map(|d| d.value().size())
            .unwrap_or(0);

        self.written_tables.insert(*document_id.table());

        let tx_size = if is_system_document {
            &mut self.system_tx_size
        } else {
            &mut self.user_tx_size
        };

        // We always increment the size first, even if we throw,
        // we want the size to reflect the write, so that
        // we can tell that we threw and not issue a warning.
        tx_size.num_writes += 1;
        tx_size.size += id_size + value_size;

        if is_system_document {
            let tx_size = &self.system_tx_size;
            // If we exceed system limits, throw a system error and not a developer one.
            // Developers have no control over system tables. We should define feature
            // specific limit to avoid hitting the system table ones if needed.
            anyhow::ensure!(
                tx_size.num_writes <= *TRANSACTION_MAX_SYSTEM_NUM_WRITES,
                "Too many system document writes in a single transaction: {}",
                tx_size.num_writes
            );
            anyhow::ensure!(
                tx_size.size <= *TRANSACTION_MAX_SYSTEM_WRITE_SIZE_BYTES,
                "Too many bytes written in system tables in a single transaction: {}",
                tx_size.size
            );
            tx_size
        } else {
            let tx_size = &self.user_tx_size;
            anyhow::ensure!(
                tx_size.num_writes <= *TRANSACTION_MAX_NUM_USER_WRITES,
                ErrorMetadata::pagination_limit(
                    "TooManyWrites",
                    format!(
                        "Too many writes in a single function execution (limit: {})",
                        *TRANSACTION_MAX_NUM_USER_WRITES,
                    )
                ),
            );
            anyhow::ensure!(
                tx_size.size <= *TRANSACTION_MAX_USER_WRITE_SIZE_BYTES,
                ErrorMetadata::pagination_limit(
                    "TooManyBytesWritten",
                    format!(
                        "Too many bytes written in a single function execution (limit: {} bytes)",
                        *TRANSACTION_MAX_USER_WRITE_SIZE_BYTES,
                    )
                ),
            );
            tx_size
        };

        if let Some(old_update) = self.updates.get_mut(&document_id) {
            anyhow::ensure!(
                old_update.new_document == document_update.old_document,
                "Inconsistent update: The old update's new document does not match the new \
                 document's old update"
            );
            old_update.new_document = document_update.new_document;
        } else {
            self.updates.insert(document_id, document_update);
        }

        Ok(())
    }

    fn record_reads_for_write(
        table_mapping: BootstrapTableIds,
        reads: &mut TransactionReadSet,
        table: TabletIdAndTableNumber,
    ) -> anyhow::Result<()> {
        // by_name index on _indexes table.
        if table_mapping.is_index_table(table) || table_mapping.is_tables_table(table) {
            // Changes in _tables or _index cannot race with any other table or
            // index. This is because TableRegistry and IndexRegistry check a
            // number of invariants between tables and index records.
            // TODO(presley): This is probably the wrong layer to add this dependency.
            // We should be added by TableRegistry and IndexRegistry themselves.
            // For example, fast forwarding a vector search checkpoint does not
            // need this dependency.
            reads.record_indexed_derived(
                TabletIndexName::by_id(table_mapping.tables_id.tablet_id),
                IndexedFields::by_id(),
                Interval::all(),
            );
            reads.record_indexed_derived(
                TabletIndexName::by_id(table_mapping.index_id.tablet_id),
                IndexedFields::by_id(),
                Interval::all(),
            );
        } else {
            // Writes to a table require the table still exists.
            let table_id_bytes = IndexKey::new(
                vec![],
                table_mapping.tables_id.table_number.id(table.tablet_id.0),
            )
            .into_bytes();
            reads.record_indexed_derived(
                TabletIndexName::by_id(table_mapping.tables_id.tablet_id),
                IndexedFields::by_id(),
                Interval::prefix(table_id_bytes.into()),
            );

            // Inserts or updates also need all of the indexes they touch to
            // be stable. Thus we take read dependency on all indexes for that table_id.
            // TODO(presley): The _index.by_table_id index does not really exist.
            // Pretend it does since evaluating read dependencies do not actually
            // need to read the index. We only care about the name always mapping
            // to the same fields.
            let table_name_bytes =
                values_to_bytes(&[Some(index_metadata_serialize_tablet_id(&table.tablet_id)?)]);
            reads.record_indexed_derived(
                TabletIndexName::new(table_mapping.index_id.tablet_id, "by_table_id".parse()?)?,
                vec![TABLE_ID_FIELD_PATH.clone()].try_into()?,
                // Note that should really be exact point instead of a prefix,
                // but our read set interval does not support this.
                Interval::prefix(BinaryKey::from(table_name_bytes)),
            );
        };

        Ok(())
    }

    /// Register a newly allocated DocumentId.
    /// This enables us to check for reuse on commit.
    pub(crate) fn register_new_id(
        &mut self,
        reads: &mut TransactionReadSet,
        document_id: ResolvedDocumentId,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.generated_ids.contains(&document_id),
            "Transaction allocated the same DocumentId twice: {document_id}"
        );
        self.generated_ids.insert(document_id);

        // New ID creation requires the ID to have never existed before.
        // We check in CommitterClient that it never existed before the transaction's
        // begin timestamp, and here we take a dependency on the ID to make sure
        // it cannot be created by a parallel commit.
        let index_name = TabletIndexName::by_id(document_id.table().tablet_id);
        let id_bytes = IndexKey::new(vec![], document_id.into()).into_bytes();
        reads.record_indexed_derived(
            index_name,
            IndexedFields::by_id(),
            Interval::prefix(id_bytes.into()),
        );
        Ok(())
    }

    /// How large is the given write transaction?
    pub fn user_size(&self) -> &TransactionWriteSize {
        &self.user_tx_size
    }

    pub fn system_size(&self) -> &TransactionWriteSize {
        &self.system_tx_size
    }

    /// Iterate over the coalesced writes (so no `DocumentId` appears twice).
    pub fn coalesced_writes(&self) -> impl Iterator<Item = (&ResolvedDocumentId, &DocumentUpdate)> {
        self.updates.iter()
    }

    pub fn into_coalesced_writes(
        self,
    ) -> impl Iterator<Item = (ResolvedDocumentId, DocumentUpdate)> {
        self.updates.into_iter()
    }

    pub fn into_updates_and_generated_ids(
        self,
    ) -> (
        BTreeMap<ResolvedDocumentId, DocumentUpdate>,
        BTreeSet<ResolvedDocumentId>,
    ) {
        (self.updates, self.generated_ids)
    }
}

#[cfg(test)]
mod tests {
    use common::{
        bootstrap_model::{
            index::{
                database_index::IndexedFields,
                IndexMetadata,
                INDEX_TABLE,
            },
            tables::TableMetadata,
        },
        document::{
            CreationTime,
            DocumentUpdate,
            PackedDocument,
            ResolvedDocument,
        },
        testing::TestIdGenerator,
        types::{
            PersistenceVersion,
            TabletIndexName,
        },
    };
    use maplit::btreeset;
    use sync_types::Timestamp;
    use value::{
        assert_obj,
        ResolvedDocumentId,
    };

    use super::Writes;
    use crate::{
        bootstrap_model::defaults::BootstrapTableIds,
        reads::TransactionReadSet,
    };

    #[test]
    fn test_write_read_dependencies() -> anyhow::Result<()> {
        // Create table mapping.
        let mut id_generator = TestIdGenerator::new();
        let user_table1 = id_generator.user_table_id(&"user_table1".parse()?);
        let user_table2 = id_generator.user_table_id(&"user_table2".parse()?);
        let bootstrap_tables = BootstrapTableIds::new(&id_generator);

        // Writes to a table should OCC with modification of the table metadata
        // or an index of the same table.
        let mut user_table1_write = TransactionReadSet::new();
        Writes::record_reads_for_write(bootstrap_tables, &mut user_table1_write, user_table1)?;

        let user_table1_table_metadata_change = PackedDocument::pack(ResolvedDocument::new(
            ResolvedDocumentId::new(bootstrap_tables.tables_id, user_table1.tablet_id.0),
            CreationTime::ONE,
            TableMetadata::new("big_table".parse()?, user_table1.table_number).try_into()?,
        )?);
        assert!(user_table1_write
            .read_set()
            .overlaps(
                &user_table1_table_metadata_change,
                PersistenceVersion::default()
            )
            .is_some());

        let user_table1_index_change = PackedDocument::pack(ResolvedDocument::new(
            id_generator.system_generate(&INDEX_TABLE),
            CreationTime::ONE,
            IndexMetadata::new_backfilling(
                Timestamp::MIN,
                TabletIndexName::new(user_table1.tablet_id, "by_likes".parse()?)?,
                IndexedFields::by_id(),
            )
            .try_into()?,
        )?);
        assert!(user_table1_write
            .read_set()
            .overlaps(&user_table1_index_change, PersistenceVersion::default())
            .is_some());

        // Writes to a table should *not* OCC with modification of the table metadata
        // or an index of unrelated same table.
        let user_table2_table_metadata_change = PackedDocument::pack(ResolvedDocument::new(
            ResolvedDocumentId::new(bootstrap_tables.tables_id, user_table2.tablet_id.0),
            CreationTime::ONE,
            TableMetadata::new("small_table".parse()?, user_table2.table_number).try_into()?,
        )?);
        assert!(user_table1_write
            .read_set()
            .overlaps(
                &user_table2_table_metadata_change,
                PersistenceVersion::default()
            )
            .is_none());

        let user_table2_index_change = PackedDocument::pack(ResolvedDocument::new(
            id_generator.system_generate(&INDEX_TABLE),
            CreationTime::ONE,
            IndexMetadata::new_backfilling(
                Timestamp::MIN,
                TabletIndexName::new(user_table2.tablet_id, "by_likes".parse()?)?,
                IndexedFields::by_id(),
            )
            .try_into()?,
        )?);
        assert!(user_table1_write
            .read_set()
            .overlaps(&user_table2_index_change, PersistenceVersion::default())
            .is_none());

        // Changes to any index metadata should conflict with changes to any
        // other table or index metadata.
        let mut metadata_write = TransactionReadSet::new();
        let index_table_id = bootstrap_tables.index_id;
        Writes::record_reads_for_write(bootstrap_tables, &mut metadata_write, index_table_id)?;

        assert!(metadata_write
            .read_set()
            .overlaps(
                &user_table1_table_metadata_change,
                PersistenceVersion::default()
            )
            .is_some());

        assert!(metadata_write
            .read_set()
            .overlaps(&user_table1_index_change, PersistenceVersion::default())
            .is_some());

        assert!(metadata_write
            .read_set()
            .overlaps(
                &user_table2_table_metadata_change,
                PersistenceVersion::default()
            )
            .is_some());

        assert!(metadata_write
            .read_set()
            .overlaps(&user_table2_index_change, PersistenceVersion::default())
            .is_some());

        Ok(())
    }

    #[test]
    fn test_register_new_id() -> anyhow::Result<()> {
        let mut id_generator = TestIdGenerator::new();
        let table_name = "table".parse()?;
        let _ = id_generator.user_table_id(&table_name);
        let bootstrap_tables = BootstrapTableIds::new(&id_generator);
        let mut writes = Writes::new();
        let mut reads = TransactionReadSet::new();
        let id = id_generator.user_generate(&table_name);
        let document =
            ResolvedDocument::new(id, CreationTime::ONE, assert_obj!("hello" => "world"))?;
        writes.update(
            bootstrap_tables,
            false,
            &mut reads,
            id,
            DocumentUpdate {
                id,
                old_document: None,
                new_document: Some(document),
            },
        )?;
        assert_eq!(writes.generated_ids, btreeset! {id});
        Ok(())
    }

    #[test]
    fn test_document_updates_are_combined() -> anyhow::Result<()> {
        let mut id_generator = TestIdGenerator::new();
        let table_name = "table".parse()?;
        let _ = id_generator.user_table_id(&table_name);
        let bootstrap_tables = BootstrapTableIds::new(&id_generator);

        let mut writes = Writes::new();
        let mut reads = TransactionReadSet::new();
        let id = id_generator.user_generate(&table_name);
        let old_document = ResolvedDocument::new(id, CreationTime::ONE, assert_obj!())?;
        let new_document =
            ResolvedDocument::new(id, CreationTime::ONE, assert_obj!("hello" => "world"))?;
        writes.update(
            bootstrap_tables,
            false,
            &mut reads,
            id,
            DocumentUpdate {
                id,
                old_document: Some(old_document.clone()),
                new_document: Some(new_document.clone()),
            },
        )?;
        let newer_document = ResolvedDocument::new(
            id,
            CreationTime::ONE,
            assert_obj!("hello" => "world", "foo" => "bar"),
        )?;
        writes.update(
            bootstrap_tables,
            false,
            &mut reads,
            id,
            DocumentUpdate {
                id,
                old_document: Some(new_document),
                new_document: Some(newer_document.clone()),
            },
        )?;

        assert_eq!(writes.updates.len(), 1);
        assert_eq!(
            writes.updates.pop_first().unwrap(),
            (
                id,
                DocumentUpdate {
                    id,
                    old_document: Some(old_document),
                    new_document: Some(newer_document),
                }
            )
        );
        assert_eq!(writes.generated_ids, btreeset! {});
        Ok(())
    }
}
