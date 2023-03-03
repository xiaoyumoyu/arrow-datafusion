// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

//! In-memory data source for presenting a `Vec<RecordBatch>` as a data source that can be
//! queried by DataFusion. This allows data to be pre-loaded into memory and then
//! repeatedly queried without incurring additional file I/O overhead.

use core::panic;
use futures::StreamExt;

use std::any::Any;
use std::sync::Arc;

use arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use arrow::record_batch::RecordBatch;
use async_trait::async_trait;

use crate::datasource::{TableProvider, TableType};
use crate::error::{DataFusionError, Result};
use crate::execution::context::SessionState;
use crate::logical_expr::Expr;
use crate::physical_plan::common;
use crate::physical_plan::common::AbortOnDropSingle;
use crate::physical_plan::memory::MemoryExec;
use crate::physical_plan::ExecutionPlan;
use crate::physical_plan::{repartition::RepartitionExec, Partitioning};

/// In-memory table
#[derive(Debug)]
pub struct MemTable {
    schema: SchemaRef,
    batches: Vec<Vec<RecordBatch>>,
}

fn schema_contains(me: &Schema, other: &Schema) -> bool {
    // println!("\nSCHEMA CONTAINS\n me {:?} other {:?} \n", me, other);
    me.fields.len() == other.fields.len()
    && me.fields.iter().zip(other.fields.iter()).all(|(f1, f2)| field_contains(f1, f2))
    // make sure self.metadata is a superset of other.metadata
    && other.metadata.iter().all(|(k, v1)| match me.metadata.get(k) {
        Some(v2) => v1 == v2,
        _ => false,
    })
}

fn field_contains(me: &Field, other: &Field) -> bool {
    if me == other {
        return true;
    }

    // normalize names
    let mut me_name = me.name().to_owned();
    let mut other_name = other.name().to_owned();

    if is_qualified(&me_name) {
        if is_call(&me_name) {
            let fun = fun(&me_name);
            let atom = arg(&me_name);
            me_name = format!("{fun}({atom})");
        } else {
            me_name = unqualifify(&me_name);
        }
    }
    if is_qualified(&other_name) {
        if is_call(&other_name) {
            let fun = fun(&other_name);
            let atom = arg(&other_name);
            other_name = format!("{fun}({atom})");
        } else {
            other_name = unqualifify(&other_name);
        }
    }

    let mut res = me_name == other_name;
    res &= me.dict_id() == other.dict_id();
    res &= me.dict_is_ordered() == other.dict_is_ordered();
    res &= (me.data_type() == other.data_type())
        || (me.data_type() == &DataType::Int64 && other.data_type() == &DataType::UInt64);
    res &= me.is_nullable() || !other.is_nullable();

    // make sure self.metadata is a superset of other.metadata
    res &= match (&me.metadata().is_empty(), &other.metadata().is_empty()) {
        (_, true) => true,
        (true, false) => false,
        (false, false) => {
            other
                .metadata()
                .iter()
                .all(|(k, v)| match me.metadata().get(k) {
                    Some(s) => s == v,
                    None => false,
                })
        }
    };
    res
}

fn arg(name: &str) -> String {
    let mut res = match name.find('(') {
        Some(i) => name[i + 1..].to_owned(),
        None => panic!("'(' expected in {name}"),
    };
    res = match res.find(".") {
        Some(i) => res[i + 1..].to_owned(),
        None => panic!("'.' expected in {name}"),
    };
    res = match res.find(")") {
        Some(i) => res[0..i].to_owned(),
        None => panic!("'.' expected in {name}"),
    };
    res
}

fn fun(name: &str) -> String {
    match name.find('(') {
        Some(i) => name[0..i].to_owned(),
        None => panic!("'(' expected in {name}"),
    }
}

fn is_call(name: &str) -> bool {
    name.contains("(") && !name.contains(",")
}

fn is_qualified(name: &str) -> bool {
    name.contains(".")
}

fn unqualifify(name: &str) -> String {
    let res = match name.find('.') {
        Some(i) => name[i + 1..].to_owned(),
        None => panic!("'(' expected in {name}"),
    };
    res
}

impl MemTable {
    /// Create a new in-memory table from the provided schema and record batches
    pub fn try_new(schema: SchemaRef, partitions: Vec<Vec<RecordBatch>>) -> Result<Self> {
        // println!(
        //     "\nTRY NEW MEMTABLE\n  LP    {:?}\n  BATCH {:?} \n",
        //     schema,
        //     if partitions.len() > 0 && partitions[0].len() > 0 {
        //         partitions[0][0].schema()
        //     } else {
        //         Arc::new(Schema::new(vec![]))
        //     }
        // );

        if partitions
            .iter()
            .flatten()
            .all(|batches| schema_contains(&schema, &batches.schema()))
        // if partitions
        //     .iter()
        //     .flatten()
        //     .all(|batches| schema.contains(&batches.schema()))
        {
            if partitions.len() > 0 && partitions[0].len() > 0 {
                Ok(Self {
                    schema: partitions[0][0].schema(),
                    batches: partitions,
                })
            } else {
                Ok(Self {
                    schema,
                    batches: partitions,
                })
            }
        } else {
            Err(DataFusionError::Plan(format!(
                "Mismatch between schemas from\n  LP    {:?}\n  BATCH {:?} \n",
                schema,
                if partitions.len() > 0 && partitions[0].len() > 0 {
                    partitions[0][0].schema()
                } else {
                    Arc::new(Schema::new(vec![]))
                }
            )))
        }
    }

    /// Create a mem table by reading from another data source
    pub async fn load(
        t: Arc<dyn TableProvider>,
        output_partitions: Option<usize>,
        state: &SessionState,
    ) -> Result<Self> {
        let schema = t.schema();
        let exec = t.scan(state, None, &[], None).await?;
        let partition_count = exec.output_partitioning().partition_count();

        let tasks = (0..partition_count)
            .map(|part_i| {
                let task = state.task_ctx();
                let exec = exec.clone();
                let task = tokio::spawn(async move {
                    let stream = exec.execute(part_i, task)?;
                    common::collect(stream).await
                });

                AbortOnDropSingle::new(task)
            })
            // this collect *is needed* so that the join below can
            // switch between tasks
            .collect::<Vec<_>>();

        let mut data: Vec<Vec<RecordBatch>> =
            Vec::with_capacity(exec.output_partitioning().partition_count());

        for result in futures::future::join_all(tasks).await {
            data.push(result.map_err(|e| DataFusionError::External(Box::new(e)))??)
        }

        let exec = MemoryExec::try_new(&data, schema.clone(), None)?;

        if let Some(num_partitions) = output_partitions {
            let exec = RepartitionExec::try_new(
                Arc::new(exec),
                Partitioning::RoundRobinBatch(num_partitions),
            )?;

            // execute and collect results
            let mut output_partitions = vec![];
            for i in 0..exec.output_partitioning().partition_count() {
                // execute this *output* partition and collect all batches
                let task_ctx = state.task_ctx();
                let mut stream = exec.execute(i, task_ctx)?;
                let mut batches = vec![];
                while let Some(result) = stream.next().await {
                    batches.push(result?);
                }
                output_partitions.push(batches);
            }

            return MemTable::try_new(schema.clone(), output_partitions);
        }
        MemTable::try_new(schema.clone(), data)
    }
}

#[async_trait]
impl TableProvider for MemTable {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }

    fn table_type(&self) -> TableType {
        TableType::Base
    }

    async fn scan(
        &self,
        _state: &SessionState,
        projection: Option<&Vec<usize>>,
        _filters: &[Expr],
        _limit: Option<usize>,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        Ok(Arc::new(MemoryExec::try_new(
            &self.batches.clone(),
            self.schema(),
            projection.cloned(),
        )?))
    }
}

#[cfg(test)]
mod tests {
    use super::{Arc, DataFusionError, MemTable, RecordBatch, Result, TableProvider};
    use crate::from_slice::FromSlice;
    use crate::prelude::SessionContext;
    use arrow::array::Int32Array;
    use arrow::datatypes::{DataType, Field, Schema};
    use arrow::error::ArrowError;
    use futures::StreamExt;
    use std::collections::HashMap;

    #[tokio::test]
    async fn test_with_projection() -> Result<()> {
        let session_ctx = SessionContext::new();
        let task_ctx = session_ctx.task_ctx();
        let schema = Arc::new(Schema::new(vec![
            Field::new("a", DataType::Int32, false),
            Field::new("b", DataType::Int32, false),
            Field::new("c", DataType::Int32, false),
            Field::new("d", DataType::Int32, true),
        ]));

        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(Int32Array::from_slice([1, 2, 3])),
                Arc::new(Int32Array::from_slice([4, 5, 6])),
                Arc::new(Int32Array::from_slice([7, 8, 9])),
                Arc::new(Int32Array::from(vec![None, None, Some(9)])),
            ],
        )?;

        let provider = MemTable::try_new(schema, vec![vec![batch]])?;

        // scan with projection
        let exec = provider
            .scan(&session_ctx.state(), Some(&vec![2, 1]), &[], None)
            .await?;

        let mut it = exec.execute(0, task_ctx)?;
        let batch2 = it.next().await.unwrap()?;
        assert_eq!(2, batch2.schema().fields().len());
        assert_eq!("c", batch2.schema().field(0).name());
        assert_eq!("b", batch2.schema().field(1).name());
        assert_eq!(2, batch2.num_columns());

        Ok(())
    }

    #[tokio::test]
    async fn test_without_projection() -> Result<()> {
        let session_ctx = SessionContext::new();
        let task_ctx = session_ctx.task_ctx();
        let schema = Arc::new(Schema::new(vec![
            Field::new("a", DataType::Int32, false),
            Field::new("b", DataType::Int32, false),
            Field::new("c", DataType::Int32, false),
        ]));

        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(Int32Array::from_slice([1, 2, 3])),
                Arc::new(Int32Array::from_slice([4, 5, 6])),
                Arc::new(Int32Array::from_slice([7, 8, 9])),
            ],
        )?;

        let provider = MemTable::try_new(schema, vec![vec![batch]])?;

        let exec = provider.scan(&session_ctx.state(), None, &[], None).await?;
        let mut it = exec.execute(0, task_ctx)?;
        let batch1 = it.next().await.unwrap()?;
        assert_eq!(3, batch1.schema().fields().len());
        assert_eq!(3, batch1.num_columns());

        Ok(())
    }

    #[tokio::test]
    async fn test_invalid_projection() -> Result<()> {
        let session_ctx = SessionContext::new();

        let schema = Arc::new(Schema::new(vec![
            Field::new("a", DataType::Int32, false),
            Field::new("b", DataType::Int32, false),
            Field::new("c", DataType::Int32, false),
        ]));

        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(Int32Array::from_slice([1, 2, 3])),
                Arc::new(Int32Array::from_slice([4, 5, 6])),
                Arc::new(Int32Array::from_slice([7, 8, 9])),
            ],
        )?;

        let provider = MemTable::try_new(schema, vec![vec![batch]])?;

        let projection: Vec<usize> = vec![0, 4];

        match provider
            .scan(&session_ctx.state(), Some(&projection), &[], None)
            .await
        {
            Err(DataFusionError::ArrowError(ArrowError::SchemaError(e))) => {
                assert_eq!(
                    "\"project index 4 out of bounds, max field 3\"",
                    format!("{e:?}")
                )
            }
            res => panic!("Scan should failed on invalid projection, got {res:?}"),
        };

        Ok(())
    }

    #[test]
    fn test_schema_validation_incompatible_column() -> Result<()> {
        let schema1 = Arc::new(Schema::new(vec![
            Field::new("a", DataType::Int32, false),
            Field::new("b", DataType::Int32, false),
            Field::new("c", DataType::Int32, false),
        ]));

        let schema2 = Arc::new(Schema::new(vec![
            Field::new("a", DataType::Int32, false),
            Field::new("b", DataType::Float64, false),
            Field::new("c", DataType::Int32, false),
        ]));

        let batch = RecordBatch::try_new(
            schema1,
            vec![
                Arc::new(Int32Array::from_slice([1, 2, 3])),
                Arc::new(Int32Array::from_slice([4, 5, 6])),
                Arc::new(Int32Array::from_slice([7, 8, 9])),
            ],
        )?;

        match MemTable::try_new(schema2, vec![vec![batch]]) {
            Err(DataFusionError::Plan(e)) => {
                assert!(e.contains("Mismatch between schema"))
            }
            _ => panic!("MemTable::new should have failed due to schema mismatch"),
        }

        Ok(())
    }

    #[test]
    fn test_schema_validation_different_column_count() -> Result<()> {
        let schema1 = Arc::new(Schema::new(vec![
            Field::new("a", DataType::Int32, false),
            Field::new("c", DataType::Int32, false),
        ]));

        let schema2 = Arc::new(Schema::new(vec![
            Field::new("a", DataType::Int32, false),
            Field::new("b", DataType::Int32, false),
            Field::new("c", DataType::Int32, false),
        ]));

        let batch = RecordBatch::try_new(
            schema1,
            vec![
                Arc::new(Int32Array::from_slice([1, 2, 3])),
                Arc::new(Int32Array::from_slice([7, 5, 9])),
            ],
        )?;

        match MemTable::try_new(schema2, vec![vec![batch]]) {
            Err(DataFusionError::Plan(e)) => {
                assert!(e.contains("Mismatch between schema"))
            }
            _ => panic!("MemTable::new should have failed due to schema mismatch"),
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_merged_schema() -> Result<()> {
        let session_ctx = SessionContext::new();
        let task_ctx = session_ctx.task_ctx();
        let mut metadata = HashMap::new();
        metadata.insert("foo".to_string(), "bar".to_string());

        let schema1 = Schema::new_with_metadata(
            vec![
                Field::new("a", DataType::Int32, false),
                Field::new("b", DataType::Int32, false),
                Field::new("c", DataType::Int32, false),
            ],
            // test for comparing metadata
            metadata,
        );

        let schema2 = Schema::new(vec![
            // test for comparing nullability
            Field::new("a", DataType::Int32, true),
            Field::new("b", DataType::Int32, false),
            Field::new("c", DataType::Int32, false),
        ]);

        let merged_schema = Schema::try_merge(vec![schema1.clone(), schema2.clone()])?;

        let batch1 = RecordBatch::try_new(
            Arc::new(schema1),
            vec![
                Arc::new(Int32Array::from_slice([1, 2, 3])),
                Arc::new(Int32Array::from_slice([4, 5, 6])),
                Arc::new(Int32Array::from_slice([7, 8, 9])),
            ],
        )?;

        let batch2 = RecordBatch::try_new(
            Arc::new(schema2),
            vec![
                Arc::new(Int32Array::from_slice([1, 2, 3])),
                Arc::new(Int32Array::from_slice([4, 5, 6])),
                Arc::new(Int32Array::from_slice([7, 8, 9])),
            ],
        )?;

        let provider =
            MemTable::try_new(Arc::new(merged_schema), vec![vec![batch1, batch2]])?;

        let exec = provider.scan(&session_ctx.state(), None, &[], None).await?;
        let mut it = exec.execute(0, task_ctx)?;
        let batch1 = it.next().await.unwrap()?;
        assert_eq!(3, batch1.schema().fields().len());
        assert_eq!(3, batch1.num_columns());

        Ok(())
    }
}
