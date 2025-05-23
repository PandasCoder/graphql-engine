use crate::state::AppState;

use axum::{http::StatusCode, Json};
use core::unimplemented;
use datafusion::{
    arrow::datatypes::{Field, SchemaBuilder, SchemaRef},
    common::DFSchema,
    datasource::{DefaultTableSource, MemTable, TableProvider},
    error::DataFusionError,
    execution::{runtime_env::RuntimeEnv, SessionStateBuilder},
    functions_aggregate::{count::Count, sum::Sum},
    functions_window::{expr_fn::row_number, ntile},
    logical_expr::{build_join_schema, AggregateUDF, ExprSchemable, Literal as _, SubqueryAlias},
    prelude::{ExprFunctionExt, SessionConfig, SessionContext},
    scalar::ScalarValue,
    sql::TableReference,
};
use plan_pushdown_types::{CaseWhen, Expression, JoinType, Literal, Rel, Sort};
use serde_arrow::from_record_batch;
use std::collections::BTreeMap;
use std::sync::Arc;

pub type Result<A> = std::result::Result<A, (StatusCode, Json<ndc_models::ErrorResponse>)>;

pub async fn execute_query_rel(
    state: &AppState,
    plan: &Rel,
) -> Result<Vec<Vec<serde_json::Value>>> {
    let logical_plan: datafusion::logical_expr::LogicalPlan =
        convert_plan_to_logical_plan(plan, state).map_err(|err| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ndc_models::ErrorResponse {
                    message: err.to_string(),
                    details: serde_json::Value::Null,
                }),
            )
        })?;

    let state = SessionStateBuilder::new()
        .with_config(SessionConfig::new())
        .with_runtime_env(Arc::new(RuntimeEnv::default()))
        .with_default_features()
        .build();

    let session_ctx = SessionContext::new();
    let task_ctx = session_ctx.task_ctx();

    let physical_plan = state
        .create_physical_plan(&logical_plan)
        .await
        .map_err(|err| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ndc_models::ErrorResponse {
                    message: err.to_string(),
                    details: serde_json::Value::Null,
                }),
            )
        })?;
    let results = datafusion::physical_plan::collect(physical_plan, task_ctx)
        .await
        .map_err(|err| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ndc_models::ErrorResponse {
                    message: err.to_string(),
                    details: serde_json::Value::Null,
                }),
            )
        })?;

    // TODO: stream the records back
    let mut rows: Vec<Vec<serde_json::Value>> = vec![];

    for batch in results {
        let schema = batch.schema();
        let new_rows = from_record_batch::<Vec<BTreeMap<String, serde_json::Value>>>(&batch)
            .map_err(|err| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ndc_models::ErrorResponse {
                        message: err.to_string(),
                        details: serde_json::Value::Null,
                    }),
                )
            })?;
        for new_row in new_rows {
            let mut row = vec![];
            for field in schema.fields() {
                row.push(new_row.get(field.name()).unwrap().clone());
            }
            rows.push(row);
        }
    }

    Ok(rows)
}

fn convert_plan_to_logical_plan(
    plan: &Rel,
    state: &AppState,
) -> datafusion::error::Result<datafusion::logical_expr::LogicalPlan> {
    match plan {
        Rel::From {
            collection,
            columns,
        } => {
            let table_provider: Arc<dyn TableProvider> = get_table_provider(collection, state)?;
            let table_schema: SchemaRef = table_provider.as_ref().schema();

            let projection = columns
                .iter()
                .map(|column| {
                    table_schema
                        .as_ref()
                        .index_of(column.as_str())
                        .map_err(|e| DataFusionError::ArrowError(e, None))
                })
                .collect::<datafusion::error::Result<Vec<_>>>()?;
            let logical_plan = datafusion::logical_expr::LogicalPlan::TableScan(
                datafusion::logical_expr::TableScan::try_new(
                    TableReference::bare(collection.as_str()),
                    Arc::new(DefaultTableSource::new(table_provider)),
                    Some(projection),
                    vec![],
                    None,
                )?,
            );

            Ok(logical_plan)
        }
        Rel::Limit { input, fetch, skip } => {
            let input_plan = convert_plan_to_logical_plan(input, state)?;
            let logical_plan =
                datafusion::logical_expr::LogicalPlan::Limit(datafusion::logical_expr::Limit {
                    input: Arc::new(input_plan),
                    fetch: fetch.map(|fetch| {
                        Box::new(i64::try_from(fetch).expect("cast usize to i64").lit())
                    }),
                    skip: Some(Box::new(
                        i64::try_from(*skip).expect("cast usize to i64").lit(),
                    )),
                });
            Ok(logical_plan)
        }
        Rel::Project { input, exprs } => {
            let input_plan = convert_plan_to_logical_plan(input, state)?;
            let exprs = exprs
                .iter()
                .map(|e| convert_expression_to_logical_expr(e, input_plan.schema()))
                .collect::<datafusion::error::Result<Vec<_>>>()?;
            let logical_plan = datafusion::logical_expr::LogicalPlan::Projection(
                datafusion::logical_expr::Projection::try_new(exprs, Arc::new(input_plan))?,
            );
            Ok(logical_plan)
        }
        Rel::Filter { input, predicate } => {
            let input_plan = convert_plan_to_logical_plan(input, state)?;
            let predicate = convert_expression_to_logical_expr(predicate, input_plan.schema())?;
            let logical_plan = datafusion::logical_expr::LogicalPlan::Filter(
                datafusion::logical_expr::Filter::try_new(predicate, Arc::new(input_plan))?,
            );
            Ok(logical_plan)
        }
        Rel::Sort { input, exprs } => {
            let input_plan = convert_plan_to_logical_plan(input, state)?;
            let expr = exprs
                .iter()
                .map(|s| convert_sort_to_logical_sort(s, input_plan.schema()))
                .collect::<datafusion::error::Result<Vec<_>>>()?;
            let logical_plan =
                datafusion::logical_expr::LogicalPlan::Sort(datafusion::logical_expr::Sort {
                    expr,
                    input: Arc::new(input_plan),
                    fetch: None,
                });
            Ok(logical_plan)
        }
        Rel::Join {
            left,
            right,
            on,
            join_type,
        } => {
            let left_plan = convert_plan_to_logical_plan(left, state)?;
            let right_plan = convert_plan_to_logical_plan(right, state)?;

            let left_plan_alias = datafusion::logical_expr::LogicalPlan::SubqueryAlias(
                SubqueryAlias::try_new(Arc::new(left_plan), "left")?,
            );
            let right_plan_alias = datafusion::logical_expr::LogicalPlan::SubqueryAlias(
                SubqueryAlias::try_new(Arc::new(right_plan), "right")?,
            );

            let on_expr = on
                .iter()
                .map(|join_on| {
                    let left = convert_expression_to_logical_expr(
                        &join_on.left,
                        left_plan_alias.schema(),
                    )?;
                    let right = convert_expression_to_logical_expr(
                        &join_on.right,
                        right_plan_alias.schema(),
                    )?;
                    Ok((left, right))
                })
                .collect::<datafusion::error::Result<Vec<_>>>()?;

            let join_type = match join_type {
                JoinType::Left => datafusion::logical_expr::JoinType::Left,
                JoinType::Right => datafusion::logical_expr::JoinType::Right,
                JoinType::Inner => datafusion::logical_expr::JoinType::Inner,
                JoinType::Full => datafusion::logical_expr::JoinType::Full,
            };

            let join_schema = build_join_schema(
                left_plan_alias.schema(),
                right_plan_alias.schema(),
                &join_type,
            )?;

            let logical_plan =
                datafusion::logical_expr::LogicalPlan::Join(datafusion::logical_expr::Join {
                    left: Arc::new(left_plan_alias),
                    right: Arc::new(right_plan_alias),
                    on: on_expr,
                    filter: None,
                    join_type,
                    join_constraint: datafusion::common::JoinConstraint::On,
                    schema: Arc::new(join_schema),
                    null_equals_null: false,
                });
            Ok(logical_plan)
        }
        Rel::Aggregate {
            input,
            group_by,
            aggregates,
        } => {
            let input_plan = convert_plan_to_logical_plan(input, state)?;
            let group_by = group_by
                .iter()
                .map(|e| convert_expression_to_logical_expr(e, input_plan.schema()))
                .collect::<datafusion::error::Result<Vec<_>>>()?;
            let aggr_expr = aggregates
                .iter()
                .map(|e| convert_expression_to_logical_expr(e, input_plan.schema()))
                .collect::<datafusion::error::Result<Vec<_>>>()?;
            let logical_plan = datafusion::logical_expr::LogicalPlan::Aggregate(
                datafusion::logical_expr::Aggregate::try_new(
                    Arc::new(input_plan),
                    group_by,
                    aggr_expr,
                )?,
            );
            Ok(logical_plan)
        }
        Rel::Window { input, exprs } => {
            let input_plan = convert_plan_to_logical_plan(input, state)?;
            let exprs = exprs
                .iter()
                .map(|e| convert_expression_to_logical_expr(e, input_plan.schema()))
                .collect::<datafusion::error::Result<Vec<_>>>()?;
            let logical_plan = datafusion::logical_expr::LogicalPlan::Window(
                datafusion::logical_expr::Window::try_new(exprs, Arc::new(input_plan))?,
            );
            Ok(logical_plan)
        }
    }
}

fn get_table_provider(
    collection_name: &ndc_models::CollectionName,
    state: &AppState,
) -> datafusion::error::Result<Arc<dyn TableProvider>> {
    let (rows, collection_fields) = match collection_name.as_str() {
        "actors" => (
            crate::collections::actors::rows(&BTreeMap::new(), state)
                .map_err(|e| DataFusionError::Internal(e.1 .0.message))?,
            crate::types::actor::definition().fields,
        ),
        "movies" => (
            crate::collections::movies::rows(&BTreeMap::new(), state)
                .map_err(|e| DataFusionError::Internal(e.1 .0.message))?
                .iter()
                .map(|row| {
                    BTreeMap::from_iter([
                        (
                            "id".into(),
                            row.get(&ndc_models::FieldName::from("id"))
                                .expect("'id' field missing")
                                .clone(),
                        ),
                        (
                            "title".into(),
                            row.get(&ndc_models::FieldName::from("title"))
                                .expect("'title' field missing")
                                .clone(),
                        ),
                        (
                            "release_date".into(),
                            row.get(&ndc_models::FieldName::from("release_date"))
                                .expect("'release_date' field missing")
                                .clone(),
                        ),
                    ])
                })
                .collect(),
            // Truncate the fields to avoid things we can't support
            BTreeMap::from_iter([
                (
                    "id".into(),
                    ndc_models::ObjectField {
                        description: Some("The movie's primary key".into()),
                        r#type: ndc_models::Type::Named { name: "Int".into() },
                        arguments: BTreeMap::new(),
                    },
                ),
                (
                    "title".into(),
                    ndc_models::ObjectField {
                        description: Some("The movie's title".into()),
                        r#type: ndc_models::Type::Named {
                            name: "String".into(),
                        },
                        arguments: BTreeMap::new(),
                    },
                ),
                (
                    "release_date".into(),
                    ndc_models::ObjectField {
                        description: Some("The movie's release date".into()),
                        r#type: ndc_models::Type::Named {
                            name: "Date".into(),
                        },
                        arguments: BTreeMap::new(),
                    },
                ),
            ]),
        ),
        _ => unimplemented!(),
    };
    let mut schema_builder = SchemaBuilder::new();
    for (field_name, object_field) in &collection_fields {
        let (data_type, nullable) = to_df_datatype(&object_field.r#type);
        schema_builder.push(Field::new(field_name.as_str(), data_type, nullable));
    }

    let schema = schema_builder.finish();
    let records = serde_arrow::to_record_batch(schema.fields(), &rows)
        .map_err(|e| DataFusionError::Internal(e.to_string()))?;

    let mem_table = MemTable::try_new(Arc::new(schema), vec![vec![records]])?;

    Ok(Arc::new(mem_table))
}

fn to_df_datatype(ty: &ndc_models::Type) -> (datafusion::arrow::datatypes::DataType, bool) {
    match ty {
        ndc_models::Type::Named { name } if name.as_str() == "Int" => {
            (datafusion::arrow::datatypes::DataType::Int64, false)
        }
        ndc_models::Type::Named { name } if name.as_str() == "Int64" => {
            (datafusion::arrow::datatypes::DataType::Utf8, false)
        }
        ndc_models::Type::Named { name } if name.as_str() == "BigInt" => {
            (datafusion::arrow::datatypes::DataType::Utf8, false)
        }
        ndc_models::Type::Named { name } if name.as_str() == "String" => {
            (datafusion::arrow::datatypes::DataType::Utf8, false)
        }
        ndc_models::Type::Named { name } if name.as_str() == "Date" => {
            (datafusion::arrow::datatypes::DataType::Utf8, false)
        }
        ndc_models::Type::Nullable { underlying_type } => {
            let (dt, _) = to_df_datatype(underlying_type);
            (dt, true)
        }
        _ => unimplemented!(),
    }
}

fn convert_expression_to_logical_expr(
    expr: &Expression,
    schema: &DFSchema,
) -> datafusion::error::Result<datafusion::logical_expr::Expr> {
    match expr {
        Expression::Literal { literal } => Ok(datafusion::prelude::Expr::Literal(
            convert_literal_to_logical_expr(literal),
        )),
        Expression::Column { index } => Ok(datafusion::prelude::Expr::Column(
            schema.columns()[*index].clone(),
        )),
        Expression::Cast { expr, as_type } => convert_expression_to_logical_expr(expr, schema)?
            .cast_to(&convert_data_type(as_type), schema),
        Expression::TryCast {
            expr: _,
            as_type: _,
        } => unimplemented!(),
        Expression::Case { when, default } => {
            let when_then_expr = when
                .iter()
                .map(|case_when: &CaseWhen| {
                    Ok((
                        Box::new(convert_expression_to_logical_expr(&case_when.when, schema)?),
                        Box::new(convert_expression_to_logical_expr(&case_when.then, schema)?),
                    ))
                })
                .collect::<datafusion::error::Result<Vec<_>>>()?;
            let else_expr = default
                .as_ref()
                .map(|e| -> datafusion::error::Result<_> {
                    Ok(Box::new(convert_expression_to_logical_expr(e, schema)?))
                })
                .transpose()?;
            Ok(datafusion::prelude::Expr::Case(
                datafusion::logical_expr::Case::new(None, when_then_expr, else_expr),
            ))
        }
        Expression::And { left, right } => Ok(datafusion::prelude::Expr::BinaryExpr(
            datafusion::logical_expr::BinaryExpr {
                left: Box::new(convert_expression_to_logical_expr(left, schema)?),
                op: datafusion::logical_expr::Operator::And,
                right: Box::new(convert_expression_to_logical_expr(right, schema)?),
            },
        )),
        Expression::Or { left, right } => Ok(datafusion::prelude::Expr::BinaryExpr(
            datafusion::logical_expr::BinaryExpr {
                left: Box::new(convert_expression_to_logical_expr(left, schema)?),
                op: datafusion::logical_expr::Operator::Or,
                right: Box::new(convert_expression_to_logical_expr(right, schema)?),
            },
        )),
        Expression::Eq { left, right } => Ok(datafusion::prelude::Expr::BinaryExpr(
            datafusion::logical_expr::BinaryExpr {
                left: Box::new(convert_expression_to_logical_expr(left, schema)?),
                op: datafusion::logical_expr::Operator::Eq,
                right: Box::new(convert_expression_to_logical_expr(right, schema)?),
            },
        )),
        Expression::NotEq { left, right } => Ok(datafusion::prelude::Expr::BinaryExpr(
            datafusion::logical_expr::BinaryExpr {
                left: Box::new(convert_expression_to_logical_expr(left, schema)?),
                op: datafusion::logical_expr::Operator::NotEq,
                right: Box::new(convert_expression_to_logical_expr(right, schema)?),
            },
        )),
        Expression::Lt { left, right } => Ok(datafusion::prelude::Expr::BinaryExpr(
            datafusion::logical_expr::BinaryExpr {
                left: Box::new(convert_expression_to_logical_expr(left, schema)?),
                op: datafusion::logical_expr::Operator::Lt,
                right: Box::new(convert_expression_to_logical_expr(right, schema)?),
            },
        )),
        Expression::LtEq { left, right } => Ok(datafusion::prelude::Expr::BinaryExpr(
            datafusion::logical_expr::BinaryExpr {
                left: Box::new(convert_expression_to_logical_expr(left, schema)?),
                op: datafusion::logical_expr::Operator::LtEq,
                right: Box::new(convert_expression_to_logical_expr(right, schema)?),
            },
        )),
        Expression::Gt { left, right } => Ok(datafusion::prelude::Expr::BinaryExpr(
            datafusion::logical_expr::BinaryExpr {
                left: Box::new(convert_expression_to_logical_expr(left, schema)?),
                op: datafusion::logical_expr::Operator::Gt,
                right: Box::new(convert_expression_to_logical_expr(right, schema)?),
            },
        )),
        Expression::GtEq { left, right } => Ok(datafusion::prelude::Expr::BinaryExpr(
            datafusion::logical_expr::BinaryExpr {
                left: Box::new(convert_expression_to_logical_expr(left, schema)?),
                op: datafusion::logical_expr::Operator::GtEq,
                right: Box::new(convert_expression_to_logical_expr(right, schema)?),
            },
        )),
        Expression::Plus { left, right } => Ok(datafusion::prelude::Expr::BinaryExpr(
            datafusion::logical_expr::BinaryExpr {
                left: Box::new(convert_expression_to_logical_expr(left, schema)?),
                op: datafusion::logical_expr::Operator::Plus,
                right: Box::new(convert_expression_to_logical_expr(right, schema)?),
            },
        )),
        Expression::Minus { left, right } => Ok(datafusion::prelude::Expr::BinaryExpr(
            datafusion::logical_expr::BinaryExpr {
                left: Box::new(convert_expression_to_logical_expr(left, schema)?),
                op: datafusion::logical_expr::Operator::Minus,
                right: Box::new(convert_expression_to_logical_expr(right, schema)?),
            },
        )),
        Expression::Multiply { left, right } => Ok(datafusion::prelude::Expr::BinaryExpr(
            datafusion::logical_expr::BinaryExpr {
                left: Box::new(convert_expression_to_logical_expr(left, schema)?),
                op: datafusion::logical_expr::Operator::Multiply,
                right: Box::new(convert_expression_to_logical_expr(right, schema)?),
            },
        )),
        Expression::Divide { left, right } => Ok(datafusion::prelude::Expr::BinaryExpr(
            datafusion::logical_expr::BinaryExpr {
                left: Box::new(convert_expression_to_logical_expr(left, schema)?),
                op: datafusion::logical_expr::Operator::Divide,
                right: Box::new(convert_expression_to_logical_expr(right, schema)?),
            },
        )),
        Expression::Modulo { left, right } => Ok(datafusion::prelude::Expr::BinaryExpr(
            datafusion::logical_expr::BinaryExpr {
                left: Box::new(convert_expression_to_logical_expr(left, schema)?),
                op: datafusion::logical_expr::Operator::Modulo,
                right: Box::new(convert_expression_to_logical_expr(right, schema)?),
            },
        )),
        Expression::Like { expr, pattern } => Ok(datafusion::prelude::Expr::BinaryExpr(
            datafusion::logical_expr::BinaryExpr {
                left: Box::new(convert_expression_to_logical_expr(expr, schema)?),
                op: datafusion::logical_expr::Operator::LikeMatch,
                right: Box::new(convert_expression_to_logical_expr(pattern, schema)?),
            },
        )),
        Expression::ILike { expr, pattern } => Ok(datafusion::prelude::Expr::BinaryExpr(
            datafusion::logical_expr::BinaryExpr {
                left: Box::new(convert_expression_to_logical_expr(expr, schema)?),
                op: datafusion::logical_expr::Operator::ILikeMatch,
                right: Box::new(convert_expression_to_logical_expr(pattern, schema)?),
            },
        )),
        Expression::NotLike { expr, pattern } => Ok(datafusion::prelude::Expr::BinaryExpr(
            datafusion::logical_expr::BinaryExpr {
                left: Box::new(convert_expression_to_logical_expr(expr, schema)?),
                op: datafusion::logical_expr::Operator::NotLikeMatch,
                right: Box::new(convert_expression_to_logical_expr(pattern, schema)?),
            },
        )),
        Expression::NotILike { expr, pattern } => Ok(datafusion::prelude::Expr::BinaryExpr(
            datafusion::logical_expr::BinaryExpr {
                left: Box::new(convert_expression_to_logical_expr(expr, schema)?),
                op: datafusion::logical_expr::Operator::NotILikeMatch,
                right: Box::new(convert_expression_to_logical_expr(pattern, schema)?),
            },
        )),
        Expression::Not { expr } => Ok(datafusion::prelude::Expr::Not(Box::new(
            convert_expression_to_logical_expr(expr, schema)?,
        ))),
        Expression::IsNotNull { expr } => Ok(datafusion::prelude::Expr::IsNotNull(Box::new(
            convert_expression_to_logical_expr(expr, schema)?,
        ))),
        Expression::IsNull { expr } => Ok(datafusion::prelude::Expr::IsNull(Box::new(
            convert_expression_to_logical_expr(expr, schema)?,
        ))),
        Expression::IsTrue { expr } => Ok(datafusion::prelude::Expr::IsTrue(Box::new(
            convert_expression_to_logical_expr(expr, schema)?,
        ))),
        Expression::IsFalse { expr } => Ok(datafusion::prelude::Expr::IsFalse(Box::new(
            convert_expression_to_logical_expr(expr, schema)?,
        ))),
        Expression::IsUnknown { expr } => Ok(datafusion::prelude::Expr::IsUnknown(Box::new(
            convert_expression_to_logical_expr(expr, schema)?,
        ))),
        Expression::IsNotTrue { expr } => Ok(datafusion::prelude::Expr::IsNotTrue(Box::new(
            convert_expression_to_logical_expr(expr, schema)?,
        ))),
        Expression::IsNotFalse { expr } => Ok(datafusion::prelude::Expr::IsNotFalse(Box::new(
            convert_expression_to_logical_expr(expr, schema)?,
        ))),
        Expression::IsNotUnknown { expr } => Ok(datafusion::prelude::Expr::IsNotUnknown(Box::new(
            convert_expression_to_logical_expr(expr, schema)?,
        ))),
        Expression::Negative { expr } => Ok(datafusion::prelude::Expr::Negative(Box::new(
            convert_expression_to_logical_expr(expr, schema)?,
        ))),
        Expression::Between { low, expr, high } => Ok(datafusion::prelude::Expr::between(
            convert_expression_to_logical_expr(expr, schema)?,
            convert_expression_to_logical_expr(low, schema)?,
            convert_expression_to_logical_expr(high, schema)?,
        )),
        Expression::NotBetween { low, expr, high } => Ok(datafusion::prelude::Expr::not_between(
            convert_expression_to_logical_expr(expr, schema)?,
            convert_expression_to_logical_expr(low, schema)?,
            convert_expression_to_logical_expr(high, schema)?,
        )),
        Expression::In { expr, list } => {
            let list = list
                .iter()
                .map(|e| convert_expression_to_logical_expr(e, schema))
                .collect::<datafusion::error::Result<Vec<_>>>()?;
            Ok(convert_expression_to_logical_expr(expr, schema)?.in_list(list, false))
        }
        Expression::NotIn { expr, list } => {
            let list = list
                .iter()
                .map(|e| convert_expression_to_logical_expr(e, schema))
                .collect::<datafusion::error::Result<Vec<_>>>()?;
            Ok(convert_expression_to_logical_expr(expr, schema)?.in_list(list, true))
        }
        Expression::ToLower { expr: _ } => unimplemented!(),
        Expression::ToUpper { expr: _ } => unimplemented!(),
        Expression::Average { expr: _ } => unimplemented!(),
        Expression::BoolAnd { expr: _ } => unimplemented!(),
        Expression::BoolOr { expr: _ } => unimplemented!(),
        Expression::Count { expr } => Ok(datafusion::prelude::Expr::AggregateFunction(
            datafusion::logical_expr::expr::AggregateFunction {
                func: Arc::new(AggregateUDF::from(Count::new())),
                args: vec![convert_expression_to_logical_expr(expr, schema)?],
                distinct: false,
                filter: None,
                order_by: None,
                null_treatment: None,
            },
        )),
        Expression::FirstValue { expr: _ } => unimplemented!(),
        Expression::LastValue { expr: _ } => unimplemented!(),
        Expression::Max { expr: _ } => unimplemented!(),
        Expression::Mean { expr: _ } => unimplemented!(),
        Expression::Median { expr: _ } => unimplemented!(),
        Expression::Min { expr: _ } => unimplemented!(),
        Expression::StringAgg { expr: _ } => unimplemented!(),
        Expression::Sum { expr } => Ok(datafusion::prelude::Expr::AggregateFunction(
            datafusion::logical_expr::expr::AggregateFunction {
                func: Arc::new(AggregateUDF::from(Sum::new())),
                args: vec![convert_expression_to_logical_expr(expr, schema)?],
                distinct: false,
                filter: None,
                order_by: None,
                null_treatment: None,
            },
        )),
        Expression::Var { expr: _ } => unimplemented!(),
        Expression::RowNumber {
            order_by,
            partition_by,
        } => {
            let order_by = order_by
                .iter()
                .map(|s| convert_sort_to_logical_sort(s, schema))
                .collect::<datafusion::error::Result<Vec<_>>>()?;
            let partition_by = partition_by
                .iter()
                .map(|s| convert_expression_to_logical_expr(s, schema))
                .collect::<datafusion::error::Result<Vec<_>>>()?;

            row_number()
                .order_by(order_by)
                .partition_by(partition_by)
                .build()
        }
        Expression::DenseRank {
            order_by: _,
            partition_by: _,
        } => unimplemented!(),
        Expression::NTile {
            order_by,
            partition_by,
            n,
        } => {
            let order_by = order_by
                .iter()
                .map(|s| convert_sort_to_logical_sort(s, schema))
                .collect::<datafusion::error::Result<Vec<_>>>()?;
            let partition_by = partition_by
                .iter()
                .map(|s| convert_expression_to_logical_expr(s, schema))
                .collect::<datafusion::error::Result<Vec<_>>>()?;

            ntile::ntile(n.lit())
                .order_by(order_by)
                .partition_by(partition_by)
                .build()
        }
        Expression::Rank {
            order_by: _,
            partition_by: _,
        } => unimplemented!(),
        Expression::CumeDist {
            order_by: _,
            partition_by: _,
        } => unimplemented!(),
        Expression::PercentRank {
            order_by: _,
            partition_by: _,
        } => unimplemented!(),
    }
}

fn convert_data_type(
    scalar_type: &plan_pushdown_types::ScalarType,
) -> datafusion::arrow::datatypes::DataType {
    match scalar_type {
        plan_pushdown_types::ScalarType::Null => datafusion::arrow::datatypes::DataType::Null,
        plan_pushdown_types::ScalarType::Boolean => datafusion::arrow::datatypes::DataType::Boolean,
        plan_pushdown_types::ScalarType::Float32 => datafusion::arrow::datatypes::DataType::Float32,
        plan_pushdown_types::ScalarType::Float64 => datafusion::arrow::datatypes::DataType::Float64,
        plan_pushdown_types::ScalarType::Int8 => datafusion::arrow::datatypes::DataType::Int8,
        plan_pushdown_types::ScalarType::Int16 => datafusion::arrow::datatypes::DataType::Int16,
        plan_pushdown_types::ScalarType::Int32 => datafusion::arrow::datatypes::DataType::Int32,
        plan_pushdown_types::ScalarType::Int64 => datafusion::arrow::datatypes::DataType::Int64,
        plan_pushdown_types::ScalarType::UInt8 => datafusion::arrow::datatypes::DataType::UInt8,
        plan_pushdown_types::ScalarType::UInt16 => datafusion::arrow::datatypes::DataType::UInt16,
        plan_pushdown_types::ScalarType::UInt32 => datafusion::arrow::datatypes::DataType::UInt32,
        plan_pushdown_types::ScalarType::UInt64 => datafusion::arrow::datatypes::DataType::UInt64,
        plan_pushdown_types::ScalarType::Decimal128 { scale, prec } => {
            datafusion::arrow::datatypes::DataType::Decimal128(*scale, *prec)
        }
        plan_pushdown_types::ScalarType::Decimal256 { scale, prec } => {
            datafusion::arrow::datatypes::DataType::Decimal256(*scale, *prec)
        }
        plan_pushdown_types::ScalarType::Utf8 => datafusion::arrow::datatypes::DataType::Utf8,
        plan_pushdown_types::ScalarType::Date32 => datafusion::arrow::datatypes::DataType::Date32,
        plan_pushdown_types::ScalarType::Date64 => datafusion::arrow::datatypes::DataType::Date64,
        plan_pushdown_types::ScalarType::Time32Second => {
            datafusion::arrow::datatypes::DataType::Time32(
                datafusion::arrow::datatypes::TimeUnit::Second,
            )
        }
        plan_pushdown_types::ScalarType::Time32Millisecond => {
            datafusion::arrow::datatypes::DataType::Time32(
                datafusion::arrow::datatypes::TimeUnit::Millisecond,
            )
        }
        plan_pushdown_types::ScalarType::Time64Microsecond => {
            datafusion::arrow::datatypes::DataType::Time64(
                datafusion::arrow::datatypes::TimeUnit::Microsecond,
            )
        }
        plan_pushdown_types::ScalarType::Time64Nanosecond => {
            datafusion::arrow::datatypes::DataType::Time64(
                datafusion::arrow::datatypes::TimeUnit::Nanosecond,
            )
        }
        plan_pushdown_types::ScalarType::TimestampSecond => {
            datafusion::arrow::datatypes::DataType::Timestamp(
                datafusion::arrow::datatypes::TimeUnit::Second,
                None,
            )
        }
        plan_pushdown_types::ScalarType::TimestampMillisecond => {
            datafusion::arrow::datatypes::DataType::Timestamp(
                datafusion::arrow::datatypes::TimeUnit::Millisecond,
                None,
            )
        }
        plan_pushdown_types::ScalarType::TimestampMicrosecond => {
            datafusion::arrow::datatypes::DataType::Timestamp(
                datafusion::arrow::datatypes::TimeUnit::Microsecond,
                None,
            )
        }
        plan_pushdown_types::ScalarType::TimestampNanosecond => {
            datafusion::arrow::datatypes::DataType::Timestamp(
                datafusion::arrow::datatypes::TimeUnit::Nanosecond,
                None,
            )
        }
        plan_pushdown_types::ScalarType::DurationSecond => {
            datafusion::arrow::datatypes::DataType::Duration(
                datafusion::arrow::datatypes::TimeUnit::Second,
            )
        }
        plan_pushdown_types::ScalarType::DurationMillisecond => {
            datafusion::arrow::datatypes::DataType::Duration(
                datafusion::arrow::datatypes::TimeUnit::Millisecond,
            )
        }
        plan_pushdown_types::ScalarType::DurationMicrosecond => {
            datafusion::arrow::datatypes::DataType::Duration(
                datafusion::arrow::datatypes::TimeUnit::Microsecond,
            )
        }
        plan_pushdown_types::ScalarType::DurationNanosecond => {
            datafusion::arrow::datatypes::DataType::Duration(
                datafusion::arrow::datatypes::TimeUnit::Nanosecond,
            )
        }
    }
}

fn convert_literal_to_logical_expr(literal: &Literal) -> ScalarValue {
    match literal {
        Literal::Null => ScalarValue::Null,
        Literal::Boolean { value } => ScalarValue::Boolean(*value),
        Literal::Float32 { value } => ScalarValue::Float32(*value),
        Literal::Float64 { value } => ScalarValue::Float64(*value),
        Literal::Int8 { value } => ScalarValue::Int8(*value),
        Literal::Int16 { value } => ScalarValue::Int16(*value),
        Literal::Int32 { value } => ScalarValue::Int32(*value),
        Literal::Int64 { value } => ScalarValue::Int64(*value),
        Literal::UInt8 { value } => ScalarValue::UInt8(*value),
        Literal::UInt16 { value } => ScalarValue::UInt16(*value),
        Literal::UInt32 { value } => ScalarValue::UInt32(*value),
        Literal::UInt64 { value } => ScalarValue::UInt64(*value),
        Literal::Decimal128 { value, scale, prec } => {
            ScalarValue::Decimal128(*value, *scale, *prec)
        }
        Literal::Decimal256 { value, scale, prec } => ScalarValue::Decimal256(
            value
                .as_ref()
                .and_then(|s| datafusion::arrow::datatypes::i256::from_string(s)),
            *scale,
            *prec,
        ),
        Literal::Utf8 { value } => ScalarValue::Utf8(value.clone()),
        Literal::Date32 { value } => ScalarValue::Date32(*value),
        Literal::Date64 { value } => ScalarValue::Date64(*value),
        Literal::Time32Second { value } => ScalarValue::Time32Second(*value),
        Literal::Time32Millisecond { value } => ScalarValue::Time32Millisecond(*value),
        Literal::Time64Microsecond { value } => ScalarValue::Time64Microsecond(*value),
        Literal::Time64Nanosecond { value } => ScalarValue::Time64Nanosecond(*value),
        Literal::TimestampSecond { value } => ScalarValue::TimestampSecond(*value, None),
        Literal::TimestampMillisecond { value } => ScalarValue::TimestampMillisecond(*value, None),
        Literal::TimestampMicrosecond { value } => ScalarValue::TimestampMicrosecond(*value, None),
        Literal::TimestampNanosecond { value } => ScalarValue::TimestampNanosecond(*value, None),
        Literal::DurationSecond { value } => ScalarValue::DurationSecond(*value),
        Literal::DurationMillisecond { value } => ScalarValue::DurationMillisecond(*value),
        Literal::DurationMicrosecond { value } => ScalarValue::DurationMicrosecond(*value),
        Literal::DurationNanosecond { value } => ScalarValue::DurationNanosecond(*value),
    }
}

fn convert_sort_to_logical_sort(
    sort: &Sort,
    schema: &DFSchema,
) -> datafusion::error::Result<datafusion::logical_expr::SortExpr> {
    Ok(datafusion::logical_expr::SortExpr {
        expr: convert_expression_to_logical_expr(&sort.expr, schema)?,
        asc: sort.asc,
        nulls_first: sort.nulls_first,
    })
}
