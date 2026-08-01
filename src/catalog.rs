use std::collections::BTreeMap;
use std::sync::Arc;

use crate::storage::Table;

/// An immutable catalog view identified by a monotonically increasing number.
#[derive(Debug)]
pub(crate) struct CatalogGeneration {
    pub(crate) id: u64,
    pub(crate) tables: BTreeMap<String, Arc<Table>>,
}

impl CatalogGeneration {
    pub(crate) fn empty() -> Self {
        Self {
            id: 0,
            tables: BTreeMap::new(),
        }
    }
}
