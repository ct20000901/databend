// Copyright 2021 Datafuse Labs
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

use common_exception::ErrorCode;
use common_exception::Result;
use common_expression::type_check::check_cast;
use common_expression::type_check::common_super_type;
use common_expression::types::DataType;
use common_expression::ConstantFolder;
use common_expression::DataField;
use common_expression::DataSchemaRef;
use common_expression::DataSchemaRefExt;
use common_expression::RemoteExpr;
use common_functions::BUILTIN_FUNCTIONS;

use crate::executor::explain::PlanStatsInfo;
use crate::executor::Exchange;
use crate::executor::PhysicalPlan;
use crate::executor::PhysicalPlanBuilder;
use crate::optimizer::ColumnSet;
use crate::optimizer::SExpr;
use crate::plans::Join;
use crate::plans::JoinType;
use crate::IndexType;
use crate::ScalarExpr;
use crate::TypeCheck;

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct HashJoin {
    // A unique id of operator in a `PhysicalPlan` tree, only used for display.
    pub plan_id: u32,
    // After building the probe key and build key, we apply probe_projections to probe_datablock
    // and build_projections to build_datablock, which can help us reduce memory usage and calls
    // of expensive functions (take_compacted_indices and gather), after processing other_conditions,
    // we will use projections for final column elimination.
    pub projections: ColumnSet,
    pub probe_projections: ColumnSet,
    pub build_projections: ColumnSet,

    pub build: Box<PhysicalPlan>,
    pub probe: Box<PhysicalPlan>,
    pub build_keys: Vec<RemoteExpr>,
    pub probe_keys: Vec<RemoteExpr>,
    pub non_equi_conditions: Vec<RemoteExpr>,
    pub join_type: JoinType,
    pub marker_index: Option<IndexType>,
    pub from_correlated_subquery: bool,
    // Use the column of probe side to construct build side column.
    // (probe index, (is probe column nullable, is build column nullable))
    pub probe_to_build: Vec<(usize, (bool, bool))>,
    pub output_schema: DataSchemaRef,
    // It means that join has a corresponding runtime filter
    pub contain_runtime_filter: bool,

    // Only used for explain
    pub stat_info: Option<PlanStatsInfo>,
}

impl HashJoin {
    pub fn output_schema(&self) -> Result<DataSchemaRef> {
        Ok(self.output_schema.clone())
    }
}

impl PhysicalPlanBuilder {
    pub async fn build_hash_join(
        &mut self,
        join: &Join,
        s_expr: &SExpr,
        required: (ColumnSet, ColumnSet),
        mut pre_column_projections: Vec<IndexType>,
        column_projections: Vec<IndexType>,
        stat_info: PlanStatsInfo,
    ) -> Result<PhysicalPlan> {
        let mut probe_side = Box::new(self.build(s_expr.child(0)?, required.0).await?);
        let mut build_side = Box::new(self.build(s_expr.child(1)?, required.1).await?);

        // Unify the data types of the left and right exchange keys.
        if let (
            PhysicalPlan::Exchange(Exchange {
                keys: probe_keys, ..
            }),
            PhysicalPlan::Exchange(Exchange {
                keys: build_keys, ..
            }),
        ) = (probe_side.as_mut(), build_side.as_mut())
        {
            for (probe_key, build_key) in probe_keys.iter_mut().zip(build_keys.iter_mut()) {
                let probe_expr = probe_key.as_expr(&BUILTIN_FUNCTIONS);
                let build_expr = build_key.as_expr(&BUILTIN_FUNCTIONS);
                let common_ty = common_super_type(
                    probe_expr.data_type().clone(),
                    build_expr.data_type().clone(),
                    &BUILTIN_FUNCTIONS.default_cast_rules,
                )
                .ok_or_else(|| {
                    ErrorCode::IllegalDataType(format!(
                        "Cannot find common type for probe key {:?} and build key {:?}",
                        &probe_expr, &build_expr
                    ))
                })?;
                *probe_key = check_cast(
                    probe_expr.span(),
                    false,
                    probe_expr,
                    &common_ty,
                    &BUILTIN_FUNCTIONS,
                )?
                .as_remote_expr();
                *build_key = check_cast(
                    build_expr.span(),
                    false,
                    build_expr,
                    &common_ty,
                    &BUILTIN_FUNCTIONS,
                )?
                .as_remote_expr();
            }
        }

        let build_schema = match join.join_type {
            JoinType::Left | JoinType::LeftSingle | JoinType::Full => {
                let build_schema = build_side.output_schema()?;
                // Wrap nullable type for columns in build side.
                let build_schema = DataSchemaRefExt::create(
                    build_schema
                        .fields()
                        .iter()
                        .map(|field| {
                            DataField::new(field.name(), field.data_type().wrap_nullable())
                        })
                        .collect::<Vec<_>>(),
                );
                build_schema
            }
            _ => build_side.output_schema()?,
        };

        let probe_schema = match join.join_type {
            JoinType::Right | JoinType::RightSingle | JoinType::Full => {
                let probe_schema = probe_side.output_schema()?;
                // Wrap nullable type for columns in probe side.
                let probe_schema = DataSchemaRefExt::create(
                    probe_schema
                        .fields()
                        .iter()
                        .map(|field| {
                            DataField::new(field.name(), field.data_type().wrap_nullable())
                        })
                        .collect::<Vec<_>>(),
                );
                probe_schema
            }
            _ => probe_side.output_schema()?,
        };

        assert_eq!(join.left_conditions.len(), join.right_conditions.len());
        let mut left_join_conditions = Vec::new();
        let mut right_join_conditions = Vec::new();
        let mut probe_to_build_index = Vec::new();
        for (left_condition, right_condition) in join
            .left_conditions
            .iter()
            .zip(join.right_conditions.iter())
        {
            let left_expr = left_condition
                .resolve_and_check(probe_schema.as_ref())?
                .project_column_ref(|index| probe_schema.index_of(&index.to_string()).unwrap());
            let right_expr = right_condition
                .resolve_and_check(build_schema.as_ref())?
                .project_column_ref(|index| build_schema.index_of(&index.to_string()).unwrap());
            if join.join_type == JoinType::Inner {
                if let (ScalarExpr::BoundColumnRef(left), ScalarExpr::BoundColumnRef(right)) =
                    (left_condition, right_condition)
                {
                    if column_projections.contains(&right.column.index) {
                        if let (Ok(probe_index), Ok(build_index)) = (
                            probe_schema.index_of(&left.column.index.to_string()),
                            build_schema.index_of(&right.column.index.to_string()),
                        ) {
                            if probe_schema
                                .field(probe_index)
                                .data_type()
                                .remove_nullable()
                                == build_schema
                                    .field(build_index)
                                    .data_type()
                                    .remove_nullable()
                            {
                                probe_to_build_index.push(((probe_index, false), build_index));
                                if !pre_column_projections.contains(&left.column.index) {
                                    pre_column_projections.push(left.column.index);
                                }
                            }
                        }
                    }
                }
            }
            // Unify the data types of the left and right expressions.
            let left_type = left_expr.data_type();
            let right_type = right_expr.data_type();
            let common_ty = common_super_type(
                left_type.clone(),
                right_type.clone(),
                &BUILTIN_FUNCTIONS.default_cast_rules,
            )
            .ok_or_else(|| {
                ErrorCode::IllegalDataType(format!(
                    "Cannot find common type for {:?} and {:?}",
                    left_type, right_type
                ))
            })?;
            let left_expr = check_cast(
                left_expr.span(),
                false,
                left_expr,
                &common_ty,
                &BUILTIN_FUNCTIONS,
            )?;
            let right_expr = check_cast(
                right_expr.span(),
                false,
                right_expr,
                &common_ty,
                &BUILTIN_FUNCTIONS,
            )?;

            let (left_expr, _) =
                ConstantFolder::fold(&left_expr, &self.func_ctx, &BUILTIN_FUNCTIONS);
            let (right_expr, _) =
                ConstantFolder::fold(&right_expr, &self.func_ctx, &BUILTIN_FUNCTIONS);

            left_join_conditions.push(left_expr.as_remote_expr());
            right_join_conditions.push(right_expr.as_remote_expr());
        }

        let mut probe_projections = ColumnSet::new();
        let mut build_projections = ColumnSet::new();
        for column in pre_column_projections {
            if let Ok(index) = probe_schema.index_of(&column.to_string()) {
                probe_projections.insert(index);
            }
            if let Ok(index) = build_schema.index_of(&column.to_string()) {
                build_projections.insert(index);
            }
        }

        let mut merged_fields =
            Vec::with_capacity(probe_projections.len() + build_projections.len());
        let mut probe_fields = Vec::with_capacity(probe_projections.len());
        let mut build_fields = Vec::with_capacity(build_projections.len());
        let mut probe_to_build = Vec::new();
        let mut tail_fields = Vec::new();
        for (i, field) in probe_schema.fields().iter().enumerate() {
            if probe_projections.contains(&i) {
                for ((probe_index, updated), _) in probe_to_build_index.iter_mut() {
                    if probe_index == &i && !*updated {
                        *probe_index = probe_fields.len();
                        *updated = true;
                    }
                }
                probe_fields.push(field.clone());
                merged_fields.push(field.clone());
            }
        }
        for (i, field) in build_schema.fields().iter().enumerate() {
            if build_projections.contains(&i) {
                let mut is_tail = false;
                for ((probe_index, _), build_index) in probe_to_build_index.iter() {
                    if build_index == &i {
                        tail_fields.push(field.clone());
                        probe_to_build.push((
                            *probe_index,
                            (
                                probe_fields[*probe_index].data_type().is_nullable(),
                                field.data_type().is_nullable(),
                            ),
                        ));
                        build_projections.remove(&i);
                        is_tail = true;
                    }
                }
                if !is_tail {
                    build_fields.push(field.clone());
                }
                merged_fields.push(field.clone());
            }
        }
        build_fields.extend(tail_fields.clone());
        merged_fields.extend(tail_fields);
        let merged_schema = DataSchemaRefExt::create(merged_fields);

        let merged_fields = match join.join_type {
            JoinType::Cross
            | JoinType::Inner
            | JoinType::Left
            | JoinType::LeftSingle
            | JoinType::Right
            | JoinType::RightSingle
            | JoinType::Full => {
                probe_fields.extend(build_fields);
                probe_fields
            }
            JoinType::LeftSemi | JoinType::LeftAnti => probe_fields,
            JoinType::RightSemi | JoinType::RightAnti => build_fields,
            JoinType::LeftMark => {
                let name = if let Some(idx) = join.marker_index {
                    idx.to_string()
                } else {
                    "marker".to_string()
                };
                build_fields.push(DataField::new(
                    name.as_str(),
                    DataType::Nullable(Box::new(DataType::Boolean)),
                ));
                build_fields
            }
            JoinType::RightMark => {
                let name = if let Some(idx) = join.marker_index {
                    idx.to_string()
                } else {
                    "marker".to_string()
                };
                probe_fields.push(DataField::new(
                    name.as_str(),
                    DataType::Nullable(Box::new(DataType::Boolean)),
                ));
                probe_fields
            }
        };
        let mut projections = ColumnSet::new();
        let projected_schema = DataSchemaRefExt::create(merged_fields.clone());
        for column in column_projections.iter() {
            if let Ok(index) = projected_schema.index_of(&column.to_string()) {
                projections.insert(index);
            }
        }

        let mut output_fields = Vec::with_capacity(column_projections.len());
        for (i, field) in merged_fields.iter().enumerate() {
            if projections.contains(&i) {
                output_fields.push(field.clone());
            }
        }
        let output_schema = DataSchemaRefExt::create(output_fields);

        Ok(PhysicalPlan::HashJoin(HashJoin {
            plan_id: self.next_plan_id(),
            projections,
            build_projections,
            probe_projections,
            build: build_side,
            probe: probe_side,
            join_type: join.join_type.clone(),
            build_keys: right_join_conditions,
            probe_keys: left_join_conditions,
            non_equi_conditions: join
                .non_equi_conditions
                .iter()
                .map(|scalar| {
                    let expr = scalar
                        .resolve_and_check(merged_schema.as_ref())?
                        .project_column_ref(|index| {
                            merged_schema.index_of(&index.to_string()).unwrap()
                        });
                    let (expr, _) = ConstantFolder::fold(&expr, &self.func_ctx, &BUILTIN_FUNCTIONS);
                    Ok(expr.as_remote_expr())
                })
                .collect::<Result<_>>()?,
            marker_index: join.marker_index,
            from_correlated_subquery: join.from_correlated_subquery,
            probe_to_build,
            output_schema,
            contain_runtime_filter: join.contain_runtime_filter,
            stat_info: Some(stat_info),
        }))
    }
}
