// Copyright 2023 Datafuse Labs.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::collections::HashMap;
use std::sync::Arc;

use common_base::base::GlobalInstance;
use common_catalog::plan::VirtualColumn;
use common_catalog::plan::VirtualColumnType;
use common_catalog::plan::ROW_ID;
use common_exception::Result;
use common_expression::types::DataType;

pub struct VirtualColumnMap {
    virtual_columns: HashMap<String, VirtualColumn>,
}

impl VirtualColumnMap {
    pub fn init() -> Result<()> {
        let mut virtual_columns = HashMap::new();

        virtual_columns.insert(
            ROW_ID.to_string(),
            VirtualColumn::new(ROW_ID, VirtualColumnType::RowId),
        );

        GlobalInstance::set(Arc::new(VirtualColumnMap { virtual_columns }));
        Ok(())
    }

    pub fn instance() -> Arc<VirtualColumnMap> {
        GlobalInstance::get()
    }

    pub fn get_data_type(&self, name: &str) -> Option<DataType> {
        self.virtual_columns
            .get(name)
            .map(|virtual_column| virtual_column.data_type())
    }

    pub fn get_virtual_column(&self, name: &str) -> Option<VirtualColumn> {
        self.virtual_columns
            .get(name)
            .map(|virtual_column| virtual_column.to_owned())
    }

    pub fn all_virtual_columns(&self) -> Vec<VirtualColumn> {
        self.virtual_columns.values().cloned().collect()
    }

    pub fn virtual_columns(&self) -> &HashMap<String, VirtualColumn> {
        &self.virtual_columns
    }
}
