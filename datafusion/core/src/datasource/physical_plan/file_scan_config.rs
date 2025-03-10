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

//! [`FileScanConfig`] to configure scanning of possibly partitioned
//! file sources.

use crate::datasource::{
    listing::{FileRange, PartitionedFile},
    object_store::ObjectStoreUrl,
};
use crate::physical_plan::ExecutionPlan;
use crate::{
    error::{DataFusionError, Result},
    scalar::ScalarValue,
};

use arrow::array::{ArrayData, BufferBuilder};
use arrow::buffer::Buffer;
use arrow::datatypes::{ArrowNativeType, UInt16Type};
use arrow_array::{ArrayRef, DictionaryArray, RecordBatch};
use arrow_schema::{DataType, Field, Schema, SchemaRef};
use datafusion_common::{
    exec_err,
    tree_node::{TreeNode, VisitRecursion},
};
use datafusion_common::{ColumnStatistics, Statistics};
use datafusion_physical_expr::LexOrdering;

use itertools::Itertools;
use log::warn;
use std::{
    borrow::Cow, cmp::min, collections::HashMap, fmt::Debug, marker::PhantomData,
    sync::Arc, vec,
};

use super::get_projected_output_ordering;

/// Convert type to a type suitable for use as a [`ListingTable`]
/// partition column. Returns `Dictionary(UInt16, val_type)`, which is
/// a reasonable trade off between a reasonable number of partition
/// values and space efficiency.
///
/// This use this to specify types for partition columns. However
/// you MAY also choose not to dictionary-encode the data or to use a
/// different dictionary type.
///
/// Use [`wrap_partition_value_in_dict`] to wrap a [`ScalarValue`] in the same say.
///
/// [`ListingTable`]: crate::datasource::listing::ListingTable
pub fn wrap_partition_type_in_dict(val_type: DataType) -> DataType {
    DataType::Dictionary(Box::new(DataType::UInt16), Box::new(val_type))
}

/// Convert a [`ScalarValue`] of partition columns to a type, as
/// decribed in the documentation of [`wrap_partition_type_in_dict`],
/// which can wrap the types.
pub fn wrap_partition_value_in_dict(val: ScalarValue) -> ScalarValue {
    ScalarValue::Dictionary(Box::new(DataType::UInt16), Box::new(val))
}

/// Get all of the [`PartitionedFile`] to be scanned for an [`ExecutionPlan`]
pub fn get_scan_files(
    plan: Arc<dyn ExecutionPlan>,
) -> Result<Vec<Vec<Vec<PartitionedFile>>>> {
    let mut collector: Vec<Vec<Vec<PartitionedFile>>> = vec![];
    plan.apply(&mut |plan| {
        if let Some(file_scan_config) = plan.file_scan_config() {
            collector.push(file_scan_config.file_groups.clone());
            Ok(VisitRecursion::Skip)
        } else {
            Ok(VisitRecursion::Continue)
        }
    })?;
    Ok(collector)
}

/// The base configurations to provide when creating a physical plan for
/// any given file format.
#[derive(Clone)]
pub struct FileScanConfig {
    /// Object store URL, used to get an [`ObjectStore`] instance from
    /// [`RuntimeEnv::object_store`]
    ///
    /// [`ObjectStore`]: object_store::ObjectStore
    /// [`RuntimeEnv::object_store`]: datafusion_execution::runtime_env::RuntimeEnv::object_store
    pub object_store_url: ObjectStoreUrl,
    /// Schema before `projection` is applied. It contains the all columns that may
    /// appear in the files. It does not include table partition columns
    /// that may be added.
    pub file_schema: SchemaRef,
    /// List of files to be processed, grouped into partitions
    ///
    /// Each file must have a schema of `file_schema` or a subset. If
    /// a particular file has a subset, the missing columns are
    /// padded with NULLs.
    ///
    /// DataFusion may attempt to read each partition of files
    /// concurrently, however files *within* a partition will be read
    /// sequentially, one after the next.
    pub file_groups: Vec<Vec<PartitionedFile>>,
    /// Estimated overall statistics of the files, taking `filters` into account.
    pub statistics: Statistics,
    /// Columns on which to project the data. Indexes that are higher than the
    /// number of columns of `file_schema` refer to `table_partition_cols`.
    pub projection: Option<Vec<usize>>,
    /// The maximum number of records to read from this plan. If `None`,
    /// all records after filtering are returned.
    pub limit: Option<usize>,
    /// The partitioning columns
    pub table_partition_cols: Vec<(String, DataType)>,
    /// All equivalent lexicographical orderings that describe the schema.
    pub output_ordering: Vec<LexOrdering>,
    /// Indicates whether this plan may produce an infinite stream of records.
    pub infinite_source: bool,
}

impl FileScanConfig {
    /// Project the schema and the statistics on the given column indices
    pub fn project(&self) -> (SchemaRef, Statistics, Vec<LexOrdering>) {
        if self.projection.is_none() && self.table_partition_cols.is_empty() {
            return (
                Arc::clone(&self.file_schema),
                self.statistics.clone(),
                self.output_ordering.clone(),
            );
        }

        let proj_iter: Box<dyn Iterator<Item = usize>> = match &self.projection {
            Some(proj) => Box::new(proj.iter().copied()),
            None => Box::new(
                0..(self.file_schema.fields().len() + self.table_partition_cols.len()),
            ),
        };

        let mut table_fields = vec![];
        let mut table_cols_stats = vec![];
        for idx in proj_iter {
            if idx < self.file_schema.fields().len() {
                table_fields.push(self.file_schema.field(idx).clone());
                if let Some(file_cols_stats) = &self.statistics.column_statistics {
                    table_cols_stats.push(file_cols_stats[idx].clone())
                } else {
                    table_cols_stats.push(ColumnStatistics::default())
                }
            } else {
                let partition_idx = idx - self.file_schema.fields().len();
                table_fields.push(Field::new(
                    &self.table_partition_cols[partition_idx].0,
                    self.table_partition_cols[partition_idx].1.to_owned(),
                    false,
                ));
                // TODO provide accurate stat for partition column (#1186)
                table_cols_stats.push(ColumnStatistics::default())
            }
        }

        let table_stats = Statistics {
            num_rows: self.statistics.num_rows,
            is_exact: self.statistics.is_exact,
            // TODO correct byte size?
            total_byte_size: None,
            column_statistics: Some(table_cols_stats),
        };

        let table_schema = Arc::new(
            Schema::new(table_fields).with_metadata(self.file_schema.metadata().clone()),
        );
        let projected_output_ordering =
            get_projected_output_ordering(self, &table_schema);
        (table_schema, table_stats, projected_output_ordering)
    }

    #[allow(unused)] // Only used by avro
    pub(crate) fn projected_file_column_names(&self) -> Option<Vec<String>> {
        self.projection.as_ref().map(|p| {
            p.iter()
                .filter(|col_idx| **col_idx < self.file_schema.fields().len())
                .map(|col_idx| self.file_schema.field(*col_idx).name())
                .cloned()
                .collect()
        })
    }

    pub(crate) fn file_column_projection_indices(&self) -> Option<Vec<usize>> {
        self.projection.as_ref().map(|p| {
            p.iter()
                .filter(|col_idx| **col_idx < self.file_schema.fields().len())
                .copied()
                .collect()
        })
    }

    /// Repartition all input files into `target_partitions` partitions, if total file size exceed
    /// `repartition_file_min_size`
    /// `target_partitions` and `repartition_file_min_size` directly come from configuration.
    ///
    /// This function only try to partition file byte range evenly, and let specific `FileOpener` to
    /// do actual partition on specific data source type. (e.g. `CsvOpener` will only read lines
    /// overlap with byte range but also handle boundaries to ensure all lines will be read exactly once)
    pub fn repartition_file_groups(
        file_groups: Vec<Vec<PartitionedFile>>,
        target_partitions: usize,
        repartition_file_min_size: usize,
    ) -> Option<Vec<Vec<PartitionedFile>>> {
        let flattened_files = file_groups.iter().flatten().collect::<Vec<_>>();

        // Perform redistribution only in case all files should be read from beginning to end
        let has_ranges = flattened_files.iter().any(|f| f.range.is_some());
        if has_ranges {
            return None;
        }

        let total_size = flattened_files
            .iter()
            .map(|f| f.object_meta.size as i64)
            .sum::<i64>();
        if total_size < (repartition_file_min_size as i64) || total_size == 0 {
            return None;
        }

        let target_partition_size =
            (total_size as usize + (target_partitions) - 1) / (target_partitions);

        let current_partition_index: usize = 0;
        let current_partition_size: usize = 0;

        // Partition byte range evenly for all `PartitionedFile`s
        let repartitioned_files = flattened_files
            .into_iter()
            .scan(
                (current_partition_index, current_partition_size),
                |state, source_file| {
                    let mut produced_files = vec![];
                    let mut range_start = 0;
                    while range_start < source_file.object_meta.size {
                        let range_end = min(
                            range_start + (target_partition_size - state.1),
                            source_file.object_meta.size,
                        );

                        let mut produced_file = source_file.clone();
                        produced_file.range = Some(FileRange {
                            start: range_start as i64,
                            end: range_end as i64,
                        });
                        produced_files.push((state.0, produced_file));

                        if state.1 + (range_end - range_start) >= target_partition_size {
                            state.0 += 1;
                            state.1 = 0;
                        } else {
                            state.1 += range_end - range_start;
                        }
                        range_start = range_end;
                    }
                    Some(produced_files)
                },
            )
            .flatten()
            .group_by(|(partition_idx, _)| *partition_idx)
            .into_iter()
            .map(|(_, group)| group.map(|(_, vals)| vals).collect_vec())
            .collect_vec();

        Some(repartitioned_files)
    }
}

/// A helper that projects partition columns into the file record batches.
///
/// One interesting trick is the usage of a cache for the key buffers of the partition column
/// dictionaries. Indeed, the partition columns are constant, so the dictionaries that represent them
/// have all their keys equal to 0. This enables us to re-use the same "all-zero" buffer across batches,
/// which makes the space consumption of the partition columns O(batch_size) instead of O(record_count).
pub struct PartitionColumnProjector {
    /// An Arrow buffer initialized to zeros that represents the key array of all partition
    /// columns (partition columns are materialized by dictionary arrays with only one
    /// value in the dictionary, thus all the keys are equal to zero).
    key_buffer_cache: ZeroBufferGenerators,
    /// Mapping between the indexes in the list of partition columns and the target
    /// schema. Sorted by index in the target schema so that we can iterate on it to
    /// insert the partition columns in the target record batch.
    projected_partition_indexes: Vec<(usize, usize)>,
    /// The schema of the table once the projection was applied.
    projected_schema: SchemaRef,
}

impl PartitionColumnProjector {
    // Create a projector to insert the partitioning columns into batches read from files
    // - `projected_schema`: the target schema with both file and partitioning columns
    // - `table_partition_cols`: all the partitioning column names
    pub fn new(projected_schema: SchemaRef, table_partition_cols: &[String]) -> Self {
        let mut idx_map = HashMap::new();
        for (partition_idx, partition_name) in table_partition_cols.iter().enumerate() {
            if let Ok(schema_idx) = projected_schema.index_of(partition_name) {
                idx_map.insert(partition_idx, schema_idx);
            }
        }

        let mut projected_partition_indexes: Vec<_> = idx_map.into_iter().collect();
        projected_partition_indexes.sort_by(|(_, a), (_, b)| a.cmp(b));

        Self {
            projected_partition_indexes,
            key_buffer_cache: Default::default(),
            projected_schema,
        }
    }

    // Transform the batch read from the file by inserting the partitioning columns
    // to the right positions as deduced from `projected_schema`
    // - `file_batch`: batch read from the file, with internal projection applied
    // - `partition_values`: the list of partition values, one for each partition column
    pub fn project(
        &mut self,
        file_batch: RecordBatch,
        partition_values: &[ScalarValue],
    ) -> Result<RecordBatch> {
        let expected_cols =
            self.projected_schema.fields().len() - self.projected_partition_indexes.len();

        if file_batch.columns().len() != expected_cols {
            return exec_err!(
                "Unexpected batch schema from file, expected {} cols but got {}",
                expected_cols,
                file_batch.columns().len()
            );
        }
        let mut cols = file_batch.columns().to_vec();
        for &(pidx, sidx) in &self.projected_partition_indexes {
            let mut partition_value = Cow::Borrowed(&partition_values[pidx]);

            // check if user forgot to dict-encode the partition value
            let field = self.projected_schema.field(sidx);
            let expected_data_type = field.data_type();
            let actual_data_type = partition_value.get_datatype();
            if let DataType::Dictionary(key_type, _) = expected_data_type {
                if !matches!(actual_data_type, DataType::Dictionary(_, _)) {
                    warn!("Partition value for column {} was not dictionary-encoded, applied auto-fix.", field.name());
                    partition_value = Cow::Owned(ScalarValue::Dictionary(
                        key_type.clone(),
                        Box::new(partition_value.as_ref().clone()),
                    ));
                }
            }

            cols.insert(
                sidx,
                create_output_array(
                    &mut self.key_buffer_cache,
                    partition_value.as_ref(),
                    file_batch.num_rows(),
                ),
            )
        }
        RecordBatch::try_new(Arc::clone(&self.projected_schema), cols).map_err(Into::into)
    }
}

#[derive(Debug, Default)]
struct ZeroBufferGenerators {
    gen_i8: ZeroBufferGenerator<i8>,
    gen_i16: ZeroBufferGenerator<i16>,
    gen_i32: ZeroBufferGenerator<i32>,
    gen_i64: ZeroBufferGenerator<i64>,
    gen_u8: ZeroBufferGenerator<u8>,
    gen_u16: ZeroBufferGenerator<u16>,
    gen_u32: ZeroBufferGenerator<u32>,
    gen_u64: ZeroBufferGenerator<u64>,
}

/// Generate a arrow [`Buffer`] that contains zero values.
#[derive(Debug, Default)]
struct ZeroBufferGenerator<T>
where
    T: ArrowNativeType,
{
    cache: Option<Buffer>,
    _t: PhantomData<T>,
}

impl<T> ZeroBufferGenerator<T>
where
    T: ArrowNativeType,
{
    const SIZE: usize = std::mem::size_of::<T>();

    fn get_buffer(&mut self, n_vals: usize) -> Buffer {
        match &mut self.cache {
            Some(buf) if buf.len() >= n_vals * Self::SIZE => {
                buf.slice_with_length(0, n_vals * Self::SIZE)
            }
            _ => {
                let mut key_buffer_builder = BufferBuilder::<T>::new(n_vals);
                key_buffer_builder.advance(n_vals); // keys are all 0
                self.cache.insert(key_buffer_builder.finish()).clone()
            }
        }
    }
}

fn create_dict_array<T>(
    buffer_gen: &mut ZeroBufferGenerator<T>,
    dict_val: &ScalarValue,
    len: usize,
    data_type: DataType,
) -> ArrayRef
where
    T: ArrowNativeType,
{
    let dict_vals = dict_val.to_array();

    let sliced_key_buffer = buffer_gen.get_buffer(len);

    // assemble pieces together
    let mut builder = ArrayData::builder(data_type)
        .len(len)
        .add_buffer(sliced_key_buffer);
    builder = builder.add_child_data(dict_vals.to_data());
    Arc::new(DictionaryArray::<UInt16Type>::from(
        builder.build().unwrap(),
    ))
}

fn create_output_array(
    key_buffer_cache: &mut ZeroBufferGenerators,
    val: &ScalarValue,
    len: usize,
) -> ArrayRef {
    if let ScalarValue::Dictionary(key_type, dict_val) = &val {
        match key_type.as_ref() {
            DataType::Int8 => {
                return create_dict_array(
                    &mut key_buffer_cache.gen_i8,
                    dict_val,
                    len,
                    val.get_datatype(),
                );
            }
            DataType::Int16 => {
                return create_dict_array(
                    &mut key_buffer_cache.gen_i16,
                    dict_val,
                    len,
                    val.get_datatype(),
                );
            }
            DataType::Int32 => {
                return create_dict_array(
                    &mut key_buffer_cache.gen_i32,
                    dict_val,
                    len,
                    val.get_datatype(),
                );
            }
            DataType::Int64 => {
                return create_dict_array(
                    &mut key_buffer_cache.gen_i64,
                    dict_val,
                    len,
                    val.get_datatype(),
                );
            }
            DataType::UInt8 => {
                return create_dict_array(
                    &mut key_buffer_cache.gen_u8,
                    dict_val,
                    len,
                    val.get_datatype(),
                );
            }
            DataType::UInt16 => {
                return create_dict_array(
                    &mut key_buffer_cache.gen_u16,
                    dict_val,
                    len,
                    val.get_datatype(),
                );
            }
            DataType::UInt32 => {
                return create_dict_array(
                    &mut key_buffer_cache.gen_u32,
                    dict_val,
                    len,
                    val.get_datatype(),
                );
            }
            DataType::UInt64 => {
                return create_dict_array(
                    &mut key_buffer_cache.gen_u64,
                    dict_val,
                    len,
                    val.get_datatype(),
                );
            }
            _ => {}
        }
    }

    val.to_array_of_size(len)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        test::{build_table_i32, columns},
        test_util::aggr_test_schema,
    };

    #[test]
    fn physical_plan_config_no_projection() {
        let file_schema = aggr_test_schema();
        let conf = config_for_projection(
            Arc::clone(&file_schema),
            None,
            Statistics::default(),
            vec![(
                "date".to_owned(),
                wrap_partition_type_in_dict(DataType::Utf8),
            )],
        );

        let (proj_schema, proj_statistics, _) = conf.project();
        assert_eq!(proj_schema.fields().len(), file_schema.fields().len() + 1);
        assert_eq!(
            proj_schema.field(file_schema.fields().len()).name(),
            "date",
            "partition columns are the last columns"
        );
        assert_eq!(
            proj_statistics
                .column_statistics
                .expect("projection creates column statistics")
                .len(),
            file_schema.fields().len() + 1
        );
        // TODO implement tests for partition column statistics once implemented

        let col_names = conf.projected_file_column_names();
        assert_eq!(col_names, None);

        let col_indices = conf.file_column_projection_indices();
        assert_eq!(col_indices, None);
    }

    #[test]
    fn physical_plan_config_with_projection() {
        let file_schema = aggr_test_schema();
        let conf = config_for_projection(
            Arc::clone(&file_schema),
            Some(vec![file_schema.fields().len(), 0]),
            Statistics {
                num_rows: Some(10),
                // assign the column index to distinct_count to help assert
                // the source statistic after the projection
                column_statistics: Some(
                    (0..file_schema.fields().len())
                        .map(|i| ColumnStatistics {
                            distinct_count: Some(i),
                            ..Default::default()
                        })
                        .collect(),
                ),
                ..Default::default()
            },
            vec![(
                "date".to_owned(),
                wrap_partition_type_in_dict(DataType::Utf8),
            )],
        );

        let (proj_schema, proj_statistics, _) = conf.project();
        assert_eq!(
            columns(&proj_schema),
            vec!["date".to_owned(), "c1".to_owned()]
        );
        let proj_stat_cols = proj_statistics
            .column_statistics
            .expect("projection creates column statistics");
        assert_eq!(proj_stat_cols.len(), 2);
        // TODO implement tests for proj_stat_cols[0] once partition column
        // statistics are implemented
        assert_eq!(proj_stat_cols[1].distinct_count, Some(0));

        let col_names = conf.projected_file_column_names();
        assert_eq!(col_names, Some(vec!["c1".to_owned()]));

        let col_indices = conf.file_column_projection_indices();
        assert_eq!(col_indices, Some(vec![0]));
    }

    #[test]
    fn partition_column_projector() {
        let file_batch = build_table_i32(
            ("a", &vec![0, 1, 2]),
            ("b", &vec![-2, -1, 0]),
            ("c", &vec![10, 11, 12]),
        );
        let partition_cols = vec![
            (
                "year".to_owned(),
                wrap_partition_type_in_dict(DataType::Utf8),
            ),
            (
                "month".to_owned(),
                wrap_partition_type_in_dict(DataType::Utf8),
            ),
            (
                "day".to_owned(),
                wrap_partition_type_in_dict(DataType::Utf8),
            ),
        ];
        // create a projected schema
        let conf = config_for_projection(
            file_batch.schema(),
            // keep all cols from file and 2 from partitioning
            Some(vec![
                0,
                1,
                2,
                file_batch.schema().fields().len(),
                file_batch.schema().fields().len() + 2,
            ]),
            Statistics::default(),
            partition_cols.clone(),
        );
        let (proj_schema, ..) = conf.project();
        // created a projector for that projected schema
        let mut proj = PartitionColumnProjector::new(
            proj_schema,
            &partition_cols
                .iter()
                .map(|x| x.0.clone())
                .collect::<Vec<_>>(),
        );

        // project first batch
        let projected_batch = proj
            .project(
                // file_batch is ok here because we kept all the file cols in the projection
                file_batch,
                &[
                    wrap_partition_value_in_dict(ScalarValue::Utf8(Some(
                        "2021".to_owned(),
                    ))),
                    wrap_partition_value_in_dict(ScalarValue::Utf8(Some(
                        "10".to_owned(),
                    ))),
                    wrap_partition_value_in_dict(ScalarValue::Utf8(Some(
                        "26".to_owned(),
                    ))),
                ],
            )
            .expect("Projection of partition columns into record batch failed");
        let expected = [
            "+---+----+----+------+-----+",
            "| a | b  | c  | year | day |",
            "+---+----+----+------+-----+",
            "| 0 | -2 | 10 | 2021 | 26  |",
            "| 1 | -1 | 11 | 2021 | 26  |",
            "| 2 | 0  | 12 | 2021 | 26  |",
            "+---+----+----+------+-----+",
        ];
        crate::assert_batches_eq!(expected, &[projected_batch]);

        // project another batch that is larger than the previous one
        let file_batch = build_table_i32(
            ("a", &vec![5, 6, 7, 8, 9]),
            ("b", &vec![-10, -9, -8, -7, -6]),
            ("c", &vec![12, 13, 14, 15, 16]),
        );
        let projected_batch = proj
            .project(
                // file_batch is ok here because we kept all the file cols in the projection
                file_batch,
                &[
                    wrap_partition_value_in_dict(ScalarValue::Utf8(Some(
                        "2021".to_owned(),
                    ))),
                    wrap_partition_value_in_dict(ScalarValue::Utf8(Some(
                        "10".to_owned(),
                    ))),
                    wrap_partition_value_in_dict(ScalarValue::Utf8(Some(
                        "27".to_owned(),
                    ))),
                ],
            )
            .expect("Projection of partition columns into record batch failed");
        let expected = [
            "+---+-----+----+------+-----+",
            "| a | b   | c  | year | day |",
            "+---+-----+----+------+-----+",
            "| 5 | -10 | 12 | 2021 | 27  |",
            "| 6 | -9  | 13 | 2021 | 27  |",
            "| 7 | -8  | 14 | 2021 | 27  |",
            "| 8 | -7  | 15 | 2021 | 27  |",
            "| 9 | -6  | 16 | 2021 | 27  |",
            "+---+-----+----+------+-----+",
        ];
        crate::assert_batches_eq!(expected, &[projected_batch]);

        // project another batch that is smaller than the previous one
        let file_batch = build_table_i32(
            ("a", &vec![0, 1, 3]),
            ("b", &vec![2, 3, 4]),
            ("c", &vec![4, 5, 6]),
        );
        let projected_batch = proj
            .project(
                // file_batch is ok here because we kept all the file cols in the projection
                file_batch,
                &[
                    wrap_partition_value_in_dict(ScalarValue::Utf8(Some(
                        "2021".to_owned(),
                    ))),
                    wrap_partition_value_in_dict(ScalarValue::Utf8(Some(
                        "10".to_owned(),
                    ))),
                    wrap_partition_value_in_dict(ScalarValue::Utf8(Some(
                        "28".to_owned(),
                    ))),
                ],
            )
            .expect("Projection of partition columns into record batch failed");
        let expected = [
            "+---+---+---+------+-----+",
            "| a | b | c | year | day |",
            "+---+---+---+------+-----+",
            "| 0 | 2 | 4 | 2021 | 28  |",
            "| 1 | 3 | 5 | 2021 | 28  |",
            "| 3 | 4 | 6 | 2021 | 28  |",
            "+---+---+---+------+-----+",
        ];
        crate::assert_batches_eq!(expected, &[projected_batch]);

        // forgot to dictionary-wrap the scalar value
        let file_batch = build_table_i32(
            ("a", &vec![0, 1, 2]),
            ("b", &vec![-2, -1, 0]),
            ("c", &vec![10, 11, 12]),
        );
        let projected_batch = proj
            .project(
                // file_batch is ok here because we kept all the file cols in the projection
                file_batch,
                &[
                    ScalarValue::Utf8(Some("2021".to_owned())),
                    ScalarValue::Utf8(Some("10".to_owned())),
                    ScalarValue::Utf8(Some("26".to_owned())),
                ],
            )
            .expect("Projection of partition columns into record batch failed");
        let expected = [
            "+---+----+----+------+-----+",
            "| a | b  | c  | year | day |",
            "+---+----+----+------+-----+",
            "| 0 | -2 | 10 | 2021 | 26  |",
            "| 1 | -1 | 11 | 2021 | 26  |",
            "| 2 | 0  | 12 | 2021 | 26  |",
            "+---+----+----+------+-----+",
        ];
        crate::assert_batches_eq!(expected, &[projected_batch]);
    }

    // sets default for configs that play no role in projections
    fn config_for_projection(
        file_schema: SchemaRef,
        projection: Option<Vec<usize>>,
        statistics: Statistics,
        table_partition_cols: Vec<(String, DataType)>,
    ) -> FileScanConfig {
        FileScanConfig {
            file_schema,
            file_groups: vec![vec![]],
            limit: None,
            object_store_url: ObjectStoreUrl::parse("test:///").unwrap(),
            projection,
            statistics,
            table_partition_cols,
            output_ordering: vec![],
            infinite_source: false,
        }
    }
}
